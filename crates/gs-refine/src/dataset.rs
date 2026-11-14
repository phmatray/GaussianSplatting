//! A recorded capture as a training set.
//!
//! [`Capture::open`] reads only the 128-byte header of every frame of a
//! `.gscap` and remembers where each record lies; photographs and depth
//! planes are read and decoded on demand. [`select_views`] decides which
//! frames to train on and which to hold out; [`ViewCache`] hands out the
//! photograph of a view at the size the optimiser currently works at and
//! keeps the decoded ones within a memory budget, so a capture of thousands
//! of 1920x1440 frames never sits decoded in memory.
//!
//! Coordinates: the optimiser works in the world viewers see, which is the
//! world of the capture moved vertically so that the floor is `y = 0`
//! (`docs/ARCHITECTURE.md`, "Details fixed by `gs_core::ingest`").

use std::collections::HashMap;
use std::fmt;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use glam::Vec3;
use gs_core::ingest::file::{FILE_MAGIC, MAX_HEADER_JSON};
use gs_core::ingest::frame::{decode_frame, CaptureFrame, FrameHeader, Tracking, FRAME_HEADER_SIZE, MAX_FRAME_BYTES};
use gs_core::ingest::image::{decode_jpeg, resample_area, ColorImage};
use gs_core::ingest::prepare::frame_camera;
use gs_core::ingest::{CaptureRegion, FormatError, GscapHeader};
use gs_core::protocol::CaptureMode;
use gs_core::Camera;
use rayon::prelude::*;

use crate::loss::{Target, BACKGROUND, IGNORE, SCENE};

/// Why a capture cannot be used.
#[derive(Debug)]
pub enum DatasetError {
    Io(std::io::Error),
    Format(FormatError),
    /// The capture is valid but the offline mode cannot work with it.
    Unusable(String),
}

impl fmt::Display for DatasetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "{e}"),
            Self::Format(e) => write!(f, "{}: {}", e.code.as_str(), e.detail),
            Self::Unusable(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for DatasetError {}

impl From<std::io::Error> for DatasetError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<FormatError> for DatasetError {
    fn from(e: FormatError) -> Self {
        Self::Format(e)
    }
}

/// One frame record of the file.
#[derive(Clone, Debug)]
pub struct FrameEntry {
    /// Where the frame (its 128-byte header first) starts in the file.
    pub offset: u64,
    pub length: u32,
    pub header: FrameHeader,
}

/// An indexed `.gscap`.
pub struct Capture {
    pub path: PathBuf,
    pub header: GscapHeader,
    pub mode: CaptureMode,
    pub frames: Vec<FrameEntry>,
    /// The file ends inside a record: a recording cut short.
    pub truncated: bool,
    pub bytes: u64,
    file: Mutex<File>,
}

fn read_fully(reader: &mut impl Read, buffer: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

impl Capture {
    /// Indexes the file: one pass that reads the headers and skips the
    /// payloads.
    pub fn open(path: &Path) -> Result<Self, DatasetError> {
        use gs_core::ingest::ErrorCode;
        let bad = |code: ErrorCode, detail: &str| DatasetError::Format(FormatError { code, detail: detail.to_string() });
        let file = File::open(path)?;
        let bytes = file.metadata()?.len();
        let mut reader = BufReader::with_capacity(1 << 16, file);
        let mut start = [0u8; 12];
        if read_fully(&mut reader, &mut start)? < 12 || start[..8] != FILE_MAGIC {
            return Err(bad(ErrorCode::BadFileMagic, "not a .gscap file"));
        }
        let json_length = u32::from_le_bytes([start[8], start[9], start[10], start[11]]) as usize;
        if json_length > MAX_HEADER_JSON || 12 + json_length as u64 > bytes {
            return Err(bad(ErrorCode::BadHeader, "header length"));
        }
        let mut json = vec![0u8; json_length];
        reader.read_exact(&mut json)?;
        let text = std::str::from_utf8(&json).map_err(|_| bad(ErrorCode::BadHeader, "the header is not UTF-8"))?;
        let header = GscapHeader::from_json(text)?;
        let mode = header.capture_mode().ok_or_else(|| bad(ErrorCode::BadHeader, "unknown capture mode"))?;

        let mut frames = Vec::new();
        let mut truncated = false;
        let mut position = 12 + json_length as u64;
        loop {
            let mut length = [0u8; 4];
            let got = read_fully(&mut reader, &mut length)?;
            if got == 0 {
                break;
            }
            let length = u32::from_le_bytes(length);
            if got < 4 || position + 4 + length as u64 > bytes {
                truncated = true;
                break;
            }
            if (length as usize) < FRAME_HEADER_SIZE || length as usize > MAX_FRAME_BYTES {
                return Err(bad(ErrorCode::BadLength, "frame record length"));
            }
            let mut head = [0u8; FRAME_HEADER_SIZE];
            reader.read_exact(&mut head)?;
            let frame_header = FrameHeader::parse(&head)?;
            if frame_header.total_length() != length as u64 {
                return Err(bad(ErrorCode::BadLength, "frame lengths do not add up to the record"));
            }
            frames.push(FrameEntry { offset: position + 4, length, header: frame_header });
            position += 4 + length as u64;
            reader.seek(SeekFrom::Start(position))?;
        }
        Ok(Self { path: path.to_path_buf(), header, mode, frames, truncated, bytes, file: Mutex::new(reader.into_inner()) })
    }

    fn read_at(&self, offset: u64, length: usize) -> Result<Vec<u8>, DatasetError> {
        let mut bytes = vec![0u8; length];
        let mut file = self.file.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut bytes)?;
        Ok(bytes)
    }

    /// The bytes of frame `i` exactly as recorded.
    pub fn frame_bytes(&self, i: usize) -> Result<Vec<u8>, DatasetError> {
        let entry = &self.frames[i];
        self.read_at(entry.offset, entry.length as usize)
    }

    /// Frame `i`, decoded (the JPEG stays compressed).
    pub fn frame(&self, i: usize) -> Result<CaptureFrame, DatasetError> {
        Ok(decode_frame(&self.frame_bytes(i)?)?)
    }

    /// Only the JPEG of frame `i`.
    pub fn jpeg(&self, i: usize) -> Result<Vec<u8>, DatasetError> {
        let entry = &self.frames[i];
        self.read_at(entry.offset + FRAME_HEADER_SIZE as u64, entry.header.color_length as usize)
    }

    /// Seconds between the first and the last frame.
    pub fn duration(&self) -> f64 {
        match (self.frames.first(), self.frames.last()) {
            (Some(a), Some(b)) => (b.header.timestamp - a.header.timestamp).max(0.0),
            _ => 0.0,
        }
    }
}

/// The colour camera of a frame in the world of the capture, or `None`
/// when its pose or intrinsics are unusable.
pub fn color_camera(header: &FrameHeader) -> Option<Camera> {
    // `frame_camera` validates pose and intrinsics; it only needs the
    // header fields, so an empty frame carries them.
    let probe = CaptureFrame {
        index: header.index,
        timestamp: header.timestamp,
        world_from_cam: header.world_from_cam,
        intrinsics: header.intrinsics,
        tracking: header.tracking,
        orientation: header.orientation,
        color_width: header.color_width,
        color_height: header.color_height,
        color: Vec::new(),
        depth_width: header.depth_width.min(64),
        depth_height: header.depth_height.min(64),
        depth_mm: Vec::new(),
        confidence: None,
        mask: None,
        compressed: false,
        reserved: 0,
    };
    let depth_cam = frame_camera(&probe).ok()?;
    let [fx, fy, cx, cy] = header.intrinsics;
    Some(Camera { width: header.color_width as u32, height: header.color_height as u32, fx, fy, cx, cy, world_from_cam: depth_cam.world_from_cam })
}

/// How views are chosen.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SelectParams {
    /// At most this many training views (`0`: no limit). A long capture is
    /// thinned evenly along its path.
    pub max_views: usize,
    /// Every n-th kept view is held out for evaluation (`0`: none).
    pub holdout_every: usize,
    /// A frame closer than this to the previous kept one, in meters and in
    /// radians, repeats it.
    pub min_move: f32,
    pub min_turn: f32,
    /// A frame whose [`sharpness`] is below this fraction of the median of
    /// the three frames before and after it is blurred (`0`: keep all).
    pub blur_ratio: f32,
}

impl Default for SelectParams {
    fn default() -> Self {
        Self { max_views: 600, holdout_every: 8, min_move: 0.02, min_turn: 0.02, blur_ratio: 0.6 }
    }
}

/// The outcome of [`select_views`]: indices into [`Capture::frames`].
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Selection {
    pub train: Vec<usize>,
    pub holdout: Vec<usize>,
    /// Frames the starting cloud is fused from: every usable frame that is
    /// not held out, including those thinned out of `train` (the fusion is
    /// cheap and every frame closes holes and averages noise).
    pub fusion: Vec<usize>,
    pub dropped_tracking: usize,
    pub dropped_pose: usize,
    pub dropped_color: usize,
    pub dropped_blurred: usize,
    pub dropped_duplicate: usize,
    pub dropped_thinned: usize,
}

/// Sharpness of a photograph, as a number that depends little on what it
/// shows: the energy of the luma differences between neighbouring pixels
/// at about 480 pixels across, divided by the same energy at half that
/// size. Crisp edges give about 0.5 and fine texture more; blur removes the
/// finest scale first and pushes the ratio toward 0.25. `None` when the
/// picture does not decode or has too little structure to tell (a blank
/// wall): such a frame is kept.
pub fn sharpness(jpeg: &[u8]) -> Option<Option<f32>> {
    let image = decode_jpeg(jpeg, 480, 360).ok()?;
    let (w, h) = (image.width as usize & !1, image.height as usize & !1);
    if w < 8 || h < 8 {
        return Some(None);
    }
    let stride = image.width as usize;
    let luma = |x: usize, y: usize| {
        let p = &image.rgb[(y * stride + x) * 3..];
        0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32
    };
    let fine: Vec<f32> = (0..w * h).map(|i| luma(i % w, i / w)).collect();
    let (cw, ch) = (w / 2, h / 2);
    let coarse: Vec<f32> = (0..cw * ch)
        .map(|i| {
            let (x, y) = (2 * (i % cw), 2 * (i / cw));
            0.25 * (fine[y * w + x] + fine[y * w + x + 1] + fine[(y + 1) * w + x] + fine[(y + 1) * w + x + 1])
        })
        .collect();
    let energy = |v: &[f32], w: usize, h: usize| {
        let mut sum = 0.0f64;
        for y in 0..h - 1 {
            for x in 0..w - 1 {
                let i = y * w + x;
                sum += ((v[i + 1] - v[i]).powi(2) + (v[i + w] - v[i]).powi(2)) as f64;
            }
        }
        sum / ((w - 1) * (h - 1)) as f64
    };
    let (fine_energy, coarse_energy) = (energy(&fine, w, h), energy(&coarse, cw, ch));
    // Below about one grey level of structure per pixel there is nothing
    // to measure but the noise of the encoder.
    Some((coarse_energy > 1.0).then(|| (fine_energy / coarse_energy) as f32))
}

/// Chooses the training and the held-out views of a capture: frames with
/// normal tracking, a valid pose and a decodable photograph, minus blurred
/// ones and near-repeats, thinned to `max_views`.
pub fn select_views(capture: &Capture, params: &SelectParams) -> Selection {
    let mut selection = Selection::default();
    let mut usable = Vec::new();
    for (i, entry) in capture.frames.iter().enumerate() {
        if entry.header.tracking != Tracking::Normal {
            selection.dropped_tracking += 1;
        } else if color_camera(&entry.header).is_none() {
            selection.dropped_pose += 1;
        } else {
            usable.push(i);
        }
    }
    // Sharpness of every usable frame; a frame that does not decode is out.
    let sharp: Vec<Option<Option<f32>>> = usable.par_iter().map(|&i| capture.jpeg(i).ok().and_then(|jpeg| sharpness(&jpeg))).collect();
    let decoded: Vec<(usize, Option<f32>)> = usable.iter().zip(&sharp).filter_map(|(&i, s)| s.map(|s| (i, s))).collect();
    selection.dropped_color = usable.len() - decoded.len();

    let mut kept: Vec<usize> = Vec::new();
    let mut last: Option<Camera> = None;
    for (k, &(i, s)) in decoded.iter().enumerate() {
        let mut around: Vec<f32> = (k.saturating_sub(3)..(k + 4).min(decoded.len())).filter(|&j| j != k).filter_map(|j| decoded[j].1).collect();
        around.sort_by(f32::total_cmp);
        if around.len() >= 4 && s.is_some_and(|s| s < params.blur_ratio * around[around.len() / 2]) {
            selection.dropped_blurred += 1;
            continue;
        }
        let cam = color_camera(&capture.frames[i].header).expect("checked above");
        if let Some(previous) = &last {
            let moved = cam.position().distance(previous.position());
            let turned = cam.forward().dot(previous.forward()).clamp(-1.0, 1.0).acos();
            if moved < params.min_move && turned < params.min_turn {
                selection.dropped_duplicate += 1;
                continue;
            }
        }
        last = Some(cam);
        kept.push(i);
    }

    // Hold out first, so that the held-out views do not depend on the cap.
    let mut train = Vec::new();
    for (k, &i) in kept.iter().enumerate() {
        if params.holdout_every > 0 && k % params.holdout_every == params.holdout_every / 2 {
            selection.holdout.push(i);
        } else {
            train.push(i);
        }
    }
    selection.fusion = train.clone();
    if params.max_views > 0 && train.len() > params.max_views {
        let n = train.len();
        let thinned: Vec<usize> = (0..params.max_views).map(|k| train[k * n / params.max_views]).collect();
        selection.dropped_thinned = n - thinned.len();
        train = thinned;
    }
    selection.train = train;
    selection
}

/// One view the optimiser renders: a frame of the capture with its camera
/// in viewer coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewInfo {
    /// Index into [`Capture::frames`].
    pub frame: usize,
    /// The colour camera at full size, floor at `y = 0`.
    pub cam: Camera,
}

/// The views of a selection, in viewer coordinates.
pub fn views_of(capture: &Capture, frames: &[usize], floor_y: f32) -> Vec<ViewInfo> {
    frames
        .iter()
        .filter_map(|&frame| {
            let mut cam = color_camera(&capture.frames[frame].header)?;
            cam.world_from_cam.translation.y -= floor_y;
            Some(ViewInfo { frame, cam })
        })
        .collect()
}

/// What decides the class of a pixel in object and person captures.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RegionMask {
    /// Region of interest in viewer coordinates.
    pub center: Vec3,
    pub radius: f32,
    /// Person captures: trust the person mask of the frames that have one.
    pub use_person_mask: bool,
}

impl RegionMask {
    pub fn new(region: &CaptureRegion, floor_y: f32, mode: CaptureMode) -> Self {
        Self {
            center: Vec3::from_array(region.center) - Vec3::new(0.0, floor_y, 0.0),
            radius: region.radius,
            use_person_mask: mode == CaptureMode::Person,
        }
    }
}

/// Smallest confidence whose depth is trusted at all, and the weights of
/// ARKit's three levels (low, medium, high).
const CONFIDENCE_WEIGHT: [u8; 3] = [0, 96, 255];

/// A view decoded at one size.
#[derive(Debug)]
pub struct ViewData {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
    pub depth_width: u32,
    pub depth_height: u32,
    pub depth_mm: Vec<u16>,
    pub depth_weight: Vec<u8>,
    /// Empty in room captures (everything is scene).
    pub class: Vec<u8>,
}

impl ViewData {
    pub fn target(&self) -> Target<'_> {
        Target {
            width: self.width,
            height: self.height,
            rgb: &self.rgb,
            depth_width: self.depth_width,
            depth_height: self.depth_height,
            depth_mm: &self.depth_mm,
            depth_weight: &self.depth_weight,
            class: &self.class,
        }
    }

    fn bytes(&self) -> usize {
        self.rgb.len() + 2 * self.depth_mm.len() + self.depth_weight.len() + self.class.len() + 128
    }
}

/// Height that goes with `width` for a view of this aspect.
pub fn level_height(cam: &Camera, width: u32) -> u32 {
    ((width as u64 * cam.height as u64 + cam.width as u64 / 2) / cam.width.max(1) as u64).max(1) as u32
}

/// Decodes a frame for the optimiser: the photograph area-averaged to
/// `width`, the depth planes and, with a region, the class of every depth
/// pixel. `cam` is the view's colour camera in viewer coordinates.
pub fn decode_view(frame: &CaptureFrame, cam: &Camera, width: u32, region: Option<&RegionMask>) -> Result<ViewData, DatasetError> {
    decode_view_measured(frame, cam, width, region, None)
}

/// [`decode_view`] that, without a region and with `measured = (near, far)`
/// in meters, marks every depth pixel the fusion could not use (no value,
/// no confidence, outside that range) as [`IGNORE`]: the photograph is then
/// compared only where the sensor measured the surface it shows. What lies
/// beyond the sensor's range (the next room through a door, the garden
/// through a window) has no geometry in the cloud; asked to paint it, the
/// optimiser stretches the nearest Gaussians into halos.
pub fn decode_view_measured(frame: &CaptureFrame, cam: &Camera, width: u32, region: Option<&RegionMask>, measured: Option<(f32, f32)>) -> Result<ViewData, DatasetError> {
    let width = width.clamp(16, cam.width.max(16));
    let height = level_height(cam, width);
    let decoded = decode_jpeg(&frame.color, width, height).map_err(|e| DatasetError::Unusable(format!("frame {}: {e}", frame.index)))?;
    let image: ColorImage = resample_area(&decoded, width, height);
    let (dw, dh) = (frame.depth_width as usize, frame.depth_height as usize);
    let n = dw * dh;
    let mut depth_mm = frame.depth_mm.clone();
    depth_mm.resize(n, 0);
    let confidence = frame.confidence.as_deref().filter(|c| c.len() == n);
    let mut depth_weight: Vec<u8> = (0..n)
        .map(|i| if depth_mm[i] == 0 { 0 } else { confidence.map_or(255, |c| CONFIDENCE_WEIGHT[c[i].min(2) as usize]) })
        .collect();
    // Without a confidence plane nothing marks the pixels that hang between
    // two surfaces: distrust every pixel next to a depth step.
    if confidence.is_none() {
        let raw = depth_weight.clone();
        for y in 0..dh {
            for x in 0..dw {
                let i = y * dw + x;
                let z = depth_mm[i] as f32;
                let step = |j: usize| raw[j] != 0 && (depth_mm[j] as f32 - z).abs() > 0.05 * z;
                if (x > 0 && step(i - 1)) || (x + 1 < dw && step(i + 1)) || (y > 0 && step(i - dw)) || (y + 1 < dh && step(i + dw)) {
                    depth_weight[i] = 0;
                }
            }
        }
    }

    let mut class = Vec::new();
    if let Some(region) = region {
        let mask = frame.mask.as_deref().filter(|m| m.len() == n && region.use_person_mask);
        let depth_cam = cam.resized(dw as u32, dh as u32);
        let eye = cam.position();
        // Distance at which the ray through a pixel enters the region.
        let to_center = region.center - eye;
        class = (0..n)
            .map(|i| {
                if let Some(mask) = mask {
                    return if mask[i] >= 128 { SCENE } else { BACKGROUND };
                }
                if depth_mm[i] == 0 || depth_weight[i] == 0 {
                    return IGNORE;
                }
                let (x, y) = ((i % dw) as f32 + 0.5, (i / dw) as f32 + 0.5);
                let z = depth_mm[i] as f32 * 1.0e-3;
                let point = depth_cam.unproject(x, y, z);
                if point.distance(region.center) <= region.radius {
                    return SCENE;
                }
                // Outside the region: behind it (or beside it) nothing may
                // be drawn; in front of it something hides the subject and
                // says nothing about it.
                let dir = (point - eye).normalize_or_zero();
                let along = to_center.dot(dir);
                let miss2 = to_center.length_squared() - along * along;
                let entry = if miss2 < region.radius * region.radius { along - (region.radius * region.radius - miss2).sqrt() } else { f32::NEG_INFINITY };
                if (point - eye).length() < entry { IGNORE } else { BACKGROUND }
            })
            .collect();
        // The planes are several photo pixels coarse and a mask is never
        // exact at an outline: where background touches scene, do not
        // insist that nothing is there.
        let at = |x: isize, y: isize| (x >= 0 && y >= 0 && x < dw as isize && y < dh as isize).then(|| class[y as usize * dw + x as usize]);
        let softened: Vec<u8> = (0..n)
            .map(|i| {
                let (x, y) = ((i % dw) as isize, (i / dw) as isize);
                let touches_scene = [(-1, 0), (1, 0), (0, -1), (0, 1)].iter().any(|&(dx, dy)| at(x + dx, y + dy) == Some(SCENE));
                if class[i] == BACKGROUND && touches_scene { IGNORE } else { class[i] }
            })
            .collect();
        class = softened;
    } else if let Some((near, far)) = measured {
        class = (0..n)
            .map(|i| {
                let z = depth_mm[i] as f32 * 1.0e-3;
                if depth_weight[i] > 0 && z >= near && z <= far { SCENE } else { IGNORE }
            })
            .collect();
    }
    Ok(ViewData { width, height, rgb: image.rgb, depth_width: dw as u32, depth_height: dh as u32, depth_mm, depth_weight, class })
}

/// Decoded views within a memory budget, least recently used out first.
pub struct ViewCache {
    budget: usize,
    used: usize,
    clock: u64,
    entries: HashMap<(usize, u32), (Arc<ViewData>, u64)>,
    /// Decodes so far (cache misses).
    pub decodes: u64,
    /// Passed to [`decode_view_measured`]; set before the first [`Self::get`].
    pub measured: Option<(f32, f32)>,
}

impl ViewCache {
    pub fn new(budget_bytes: usize) -> Self {
        Self { budget: budget_bytes, used: 0, clock: 0, entries: HashMap::new(), decodes: 0, measured: None }
    }

    /// Bytes of decoded views held right now.
    pub fn bytes(&self) -> usize {
        self.used
    }

    /// The view at `width`, from memory or from the file.
    pub fn get(&mut self, capture: &Capture, view: &ViewInfo, width: u32, region: Option<&RegionMask>) -> Result<Arc<ViewData>, DatasetError> {
        self.clock += 1;
        let key = (view.frame, width);
        if let Some((data, stamp)) = self.entries.get_mut(&key) {
            *stamp = self.clock;
            return Ok(data.clone());
        }
        let data = Arc::new(decode_view_measured(&capture.frame(view.frame)?, &view.cam, width, region, self.measured)?);
        self.decodes += 1;
        let size = data.bytes();
        // Make room; the new entry itself always fits, however small the
        // budget is.
        while self.used + size > self.budget && !self.entries.is_empty() {
            let oldest = *self.entries.iter().min_by_key(|(_, (_, stamp))| *stamp).expect("not empty").0;
            if let Some((old, _)) = self.entries.remove(&oldest) {
                self.used -= old.bytes();
            }
        }
        self.used += size;
        self.entries.insert(key, (data.clone(), self.clock));
        Ok(data)
    }

    /// Forgets every view decoded at another width than `width`.
    pub fn keep_only(&mut self, width: u32) {
        let used = &mut self.used;
        self.entries.retain(|key, (data, _)| {
            let keep = key.1 == width;
            if !keep {
                *used -= data.bytes();
            }
            keep
        });
    }
}
