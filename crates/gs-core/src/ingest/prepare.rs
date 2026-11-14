//! From a decoded capture frame to the RGB-D image the reconstruction
//! consumes.
//!
//! The reconstruction of this crate was written for a simulated sensor:
//! colour and depth on the same grid, exact poses, depth that is either
//! right or absent. A phone delivers something else, and this module is
//! where the difference is absorbed:
//!
//! * the depth map (256x192 from ARKit) is the working grid; the intrinsics,
//!   which refer to the colour image, are scaled to it;
//! * the colour image, several times finer, is averaged over the footprint
//!   of every depth pixel ([`super::image`]);
//! * depth outside the range of the sensor or of low confidence is removed
//!   before any surface is estimated. LiDAR depth is interpolated across
//!   silhouettes ("flying pixels" that hang in mid-air between the object
//!   and what is behind it); ARKit gives those a low confidence, and for
//!   frames without a confidence plane a depth-step filter stands in;
//! * a pose that is not a rigid transform, or intrinsics that are not a
//!   camera, make the frame unusable instead of poisoning the map.

use glam::{Affine3A, Mat3, Mat4, Vec3};
use serde::{Deserialize, Serialize};

use super::frame::CaptureFrame;
use super::image::{decode_jpeg, resample_area, ColorImage};
use crate::protocol::CaptureMode;
use crate::reconstruct::{ReconstructParams, VoxelFusion};
use crate::types::{Camera, RgbdFrame};

/// `value` when it is a finite number, `fallback` otherwise, then clamped.
#[inline]
fn finite_clamp(value: f32, fallback: f32, lo: f32, hi: f32) -> f32 {
    if value.is_finite() { value.clamp(lo, hi) } else { fallback }
}

/// What depends on the capture mode.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModeParams {
    /// Edge length of a fusion cell in meters (not used by `live`).
    pub voxel_size: f32,
    /// Depth closer than this is not used (meters). ARKit's LiDAR does not
    /// measure below about 0.2 m.
    pub near: f32,
    /// Depth beyond this is not used (meters).
    pub far: f32,
    /// Radius of the region of interest until the phone sends one
    /// (`object` and `person`; meters).
    pub default_radius: f32,
}

impl Default for ModeParams {
    fn default() -> Self {
        IngestParams::default().room
    }
}

/// Parameters of the ingest: how frames of the phone become splats.
///
/// Fields missing from serialized data take their [`Default`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct IngestParams {
    /// Depth pixels with a confidence below this are dropped (ARKit: 0 low,
    /// 1 medium, 2 high). Frames without a confidence plane keep all.
    pub min_confidence: u8,
    /// Frames without a confidence plane only: a depth pixel is dropped
    /// when one of its eight neighbours is nearer or farther by more than
    /// this fraction of the nearer depth, because it lies on, or next to, a
    /// depth discontinuity, where LiDAR depth hangs between the object and
    /// what is behind it. The limit is raised to what a plane at the
    /// grazing limit steps per pixel (4 % for ARKit's 256x192), so that
    /// floors and walls seen at a slant are never taken for edges. `0`
    /// keeps every pixel.
    ///
    /// With a confidence plane this filter is not applied: ARKit marks
    /// exactly those pixels as low confidence, and removing the rim of
    /// every silhouette on top of that costs 1 to 2 dB in held-out views
    /// (`tests/ingest_capture.rs`).
    pub edge_jump: f32,
    /// `room`: 2.4 cm cells (those of the simulated home scan: finer ones
    /// gain nothing measurable at LiDAR resolution and a flat of 90 m2
    /// would need more than a million of them), 0.25 to 5 m.
    pub room: ModeParams,
    /// `object`: 8 mm cells inside the region of interest.
    pub object: ModeParams,
    /// `person`: 1 cm cells inside the region of interest.
    pub person: ModeParams,
    /// `live`: the range of the single view.
    pub live: ModeParams,
    /// Most cells (splats) the fusion holds. When the bound is reached new
    /// surface is no longer added; what is there is still refined.
    pub max_voxels: usize,
    /// Frames a cell averages before it only changes for a clearly better
    /// view ([`VoxelFusion::blend_frames`]).
    pub blend_frames: u32,
    /// Compensate the exposure and white balance drift of the phone's
    /// camera ([`VoxelFusion::exposure_compensation`]).
    pub exposure_compensation: bool,
    /// Tangential standard deviation of a fused splat in cells.
    pub kernel: f32,
    /// The fusion keeps what lies within this multiple of the region's
    /// radius, so that a region that is nudged or enlarged later still has
    /// its surroundings.
    pub region_margin: f32,
    /// `person` and `live`: when the frame carries a person mask, keep only
    /// the person. `person` always does; this switch is for `live`, where
    /// the default shows the whole view.
    pub live_person_only: bool,
    /// A fused cell built by fewer frames than this, and without neighbours,
    /// is dropped by the final pass of `finish` (what a flying pixel or a
    /// burst of noise leaves in mid-air). `1` keeps everything.
    pub min_frames: u32,
    /// Width of the preview thumbnails in pixels (before they are turned
    /// upright).
    pub preview_width: u32,
    /// Depth-to-Gaussian parameters (edge threshold, smoothing; footprint
    /// and opacity for the `live` mode).
    pub recon: ReconstructParams,
}

impl Default for IngestParams {
    fn default() -> Self {
        Self {
            min_confidence: 1,
            edge_jump: 0.1,
            room: ModeParams { voxel_size: 0.024, near: 0.25, far: 5.0, default_radius: 0.0 },
            object: ModeParams { voxel_size: 0.008, near: 0.15, far: 2.5, default_radius: 0.6 },
            person: ModeParams { voxel_size: 0.01, near: 0.25, far: 4.0, default_radius: 1.2 },
            live: ModeParams { voxel_size: 0.02, near: 0.25, far: 5.0, default_radius: 0.0 },
            max_voxels: VoxelFusion::DEFAULT_MAX_VOXELS,
            blend_frames: 24,
            exposure_compensation: true,
            kernel: 0.45,
            region_margin: 1.5,
            live_person_only: false,
            min_frames: 2,
            preview_width: 96,
            recon: ReconstructParams::default(),
        }
    }
}

impl IngestParams {
    /// Hard upper bound on [`IngestParams::max_voxels`]: 4 million cells
    /// are 400 MB of fusion state and a 64 MB layer.
    pub const MAX_VOXELS_LIMIT: usize = 4_000_000;

    /// The parameters of a mode.
    pub fn mode(&self, mode: CaptureMode) -> &ModeParams {
        match mode {
            CaptureMode::Room => &self.room,
            CaptureMode::Object => &self.object,
            CaptureMode::Person => &self.person,
            CaptureMode::Live => &self.live,
        }
    }

    /// The same parameters with every field forced into a range the ingest
    /// can serve with bounded memory and time.
    pub fn sanitized(self) -> Self {
        let d = Self::default();
        let mode = |m: ModeParams, d: ModeParams| {
            let near = finite_clamp(m.near, d.near, 0.05, 5.0);
            ModeParams {
                voxel_size: finite_clamp(m.voxel_size, d.voxel_size, VoxelFusion::MIN_VOXEL_SIZE, 0.2),
                near,
                far: finite_clamp(m.far, d.far, near + 0.1, 20.0),
                default_radius: finite_clamp(m.default_radius, d.default_radius, 0.0, 10.0),
            }
        };
        Self {
            min_confidence: self.min_confidence.min(2),
            edge_jump: finite_clamp(self.edge_jump, d.edge_jump, 0.0, 1.0),
            room: mode(self.room, d.room),
            object: mode(self.object, d.object),
            person: mode(self.person, d.person),
            live: mode(self.live, d.live),
            max_voxels: self.max_voxels.clamp(1_000, Self::MAX_VOXELS_LIMIT),
            blend_frames: self.blend_frames.clamp(1, VoxelFusion::MAX_BLEND_FRAMES),
            exposure_compensation: self.exposure_compensation,
            kernel: finite_clamp(self.kernel, d.kernel, 0.2, 1.5),
            region_margin: finite_clamp(self.region_margin, d.region_margin, 1.0, 4.0),
            live_person_only: self.live_person_only,
            min_frames: self.min_frames.clamp(1, 16),
            preview_width: self.preview_width.clamp(16, 640),
            recon: self.recon.sanitized(),
        }
    }
}

/// Why a frame did not go into the reconstruction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SkipReason {
    /// The capture is paused.
    Paused,
    /// The capture is finished.
    Finished,
    /// ARKit's tracking was limited or unavailable (fusing modes).
    Tracking,
    /// The pose is not a finite rigid transform.
    Pose,
    /// The intrinsics or the image sizes do not describe a camera.
    Intrinsics,
    /// The frame carries no colour image.
    NoColor,
    /// The colour image is not a JPEG that can be decoded.
    Color,
    /// No depth pixel survived the filters.
    NoDepth,
    /// `person` mode with masks: nobody in this frame.
    NoPerson,
    /// The depth map is larger than [`MAX_FUSED_DEPTH_PIXELS`].
    DepthSize,
}

impl SkipReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Paused => "paused",
            Self::Finished => "finished",
            Self::Tracking => "tracking",
            Self::Pose => "pose",
            Self::Intrinsics => "intrinsics",
            Self::NoColor => "no_color",
            Self::Color => "color",
            Self::NoDepth => "no_depth",
            Self::NoPerson => "no_person",
            Self::DepthSize => "depth_size",
        }
    }
}

/// Largest depth map a frame may carry to be reconstructed (1024 x 1024;
/// ARKit delivers 256 x 192). The frame format allows four times as much,
/// and a hostile frame of a few kilobytes can announce it: every pixel
/// costs reconstruction time, so the bound on the work per frame is here.
pub const MAX_FUSED_DEPTH_PIXELS: usize = 1 << 20;

/// The camera of a frame on its depth grid, or why it has none.
///
/// The intrinsics of a frame refer to its colour image; they are scaled by
/// depth size over colour size. The pose must be a rigid transform within
/// round-off: ARKit delivers one, and anything else (a damaged file, a
/// phone that lost track and sends zeros) must not reach the fusion.
pub fn frame_camera(frame: &CaptureFrame) -> Result<Camera, SkipReason> {
    if frame.depth_pixels() > MAX_FUSED_DEPTH_PIXELS {
        return Err(SkipReason::DepthSize);
    }
    let m = &frame.world_from_cam;
    if !m.iter().all(|v| v.is_finite()) {
        return Err(SkipReason::Pose);
    }
    let bottom_row_ok = m[3].abs() < 1e-3 && m[7].abs() < 1e-3 && m[11].abs() < 1e-3 && (m[15] - 1.0).abs() < 1e-3;
    let mat = Mat4::from_cols_array(m);
    let rotation = Mat3::from_mat4(mat);
    let translation = mat.w_axis.truncate();
    let orthonormal = (rotation * rotation.transpose() - Mat3::IDENTITY).to_cols_array().iter().all(|e| e.abs() < 1e-2);
    if !bottom_row_ok || !orthonormal || rotation.determinant() < 0.0 || translation.abs().max_element() > 1.0e4 {
        return Err(SkipReason::Pose);
    }
    let (cw, ch) = (frame.color_width as f32, frame.color_height as f32);
    let (dw, dh) = (frame.depth_width as f32, frame.depth_height as f32);
    let [fx, fy, cx, cy] = frame.intrinsics;
    // A focal length between a tenth of and fifty times the image size is a
    // field of view between 157 and 1 degrees; the principal point of a
    // real camera is near the middle of its image.
    let focal_ok = |f: f32, size: f32| f.is_finite() && f >= 0.1 * size && f <= 50.0 * size;
    let center_ok = |c: f32, size: f32| c.is_finite() && c >= -0.5 * size && c <= 1.5 * size;
    if cw < 1.0 || ch < 1.0 || !focal_ok(fx, cw) || !focal_ok(fy, ch) || !center_ok(cx, cw) || !center_ok(cy, ch) {
        return Err(SkipReason::Intrinsics);
    }
    let (sx, sy) = (dw / cw, dh / ch);
    Ok(Camera {
        width: frame.depth_width as u32,
        height: frame.depth_height as u32,
        fx: fx * sx,
        fy: fy * sy,
        cx: cx * sx,
        cy: cy * sy,
        world_from_cam: Affine3A::from_mat3_translation(rotation, translation),
    })
}

/// A frame ready for the reconstruction.
#[derive(Clone, Debug, PartialEq)]
pub struct Prepared {
    /// Colour and cleaned depth (meters, `0` = not usable) on the depth grid.
    pub frame: RgbdFrame,
    /// The camera of that grid.
    pub cam: Camera,
    /// Depth pixels that survived the filters.
    pub valid: u32,
}

/// Relative depth step per pixel of a plane at the grazing limit of the
/// reconstruction (a cosine of 0.2 between normal and viewing ray), for a
/// focal length of one pixel, along a diagonal, with a margin.
const GRAZING_STEP: f32 = 1.3 * 4.9 * std::f32::consts::SQRT_2;

/// Cleans the depth of a frame: millimeters to meters, range, confidence
/// and depth discontinuities. `cam` is the camera of the depth grid
/// ([`frame_camera`]). Returns the map and how many pixels are left.
pub fn clean_depth(frame: &CaptureFrame, cam: &Camera, near: f32, far: f32, params: &IngestParams) -> (Vec<f32>, u32) {
    let (w, h) = (frame.depth_width as usize, frame.depth_height as usize);
    let n = w * h;
    let confidence = frame.confidence.as_deref().filter(|c| c.len() == n);
    let mut depth: Vec<f32> = frame
        .depth_mm
        .iter()
        .take(n)
        .enumerate()
        .map(|(i, &mm)| {
            let z = mm as f32 * 1.0e-3;
            let trusted = confidence.is_none_or(|c| c[i] >= params.min_confidence);
            if mm != 0 && trusted && z >= near && z <= far { z } else { 0.0 }
        })
        .collect();
    depth.resize(n, 0.0);
    if confidence.is_none() && params.edge_jump > 0.0 && w >= 3 && h >= 3 {
        // Steps are measured on the raw map: a pixel next to one that the
        // range or the confidence removed is still next to an edge.
        let raw = |i: usize| frame.depth_mm.get(i).copied().unwrap_or(0) as f32 * 1.0e-3;
        let limit = 1.0 + params.edge_jump.max(GRAZING_STEP / cam.fx.min(cam.fy).max(1.0));
        let mut on_edge = vec![false; n];
        for y in 0..h {
            for x in 0..w {
                let i = y * w + x;
                let z = depth[i];
                if z == 0.0 {
                    continue;
                }
                'around: for yy in y.saturating_sub(1)..=(y + 1).min(h - 1) {
                    for xx in x.saturating_sub(1)..=(x + 1).min(w - 1) {
                        let other = raw(yy * w + xx);
                        if other > 0.0 && (other > z * limit || z > other * limit) {
                            on_edge[i] = true;
                            break 'around;
                        }
                    }
                }
            }
        }
        for (z, edge) in depth.iter_mut().zip(&on_edge) {
            if *edge {
                *z = 0.0;
            }
        }
    }
    let valid = depth.iter().filter(|&&z| z > 0.0).count() as u32;
    (depth, valid)
}

/// The colour image of a frame averaged onto its depth grid.
pub fn color_on_depth_grid(frame: &CaptureFrame) -> Result<ColorImage, SkipReason> {
    if frame.color.is_empty() {
        return Err(SkipReason::NoColor);
    }
    let (w, h) = (frame.depth_width as u32, frame.depth_height as u32);
    let decoded = decode_jpeg(&frame.color, w, h).map_err(|_| SkipReason::Color)?;
    let image = resample_area(&decoded, w, h);
    if image.rgb.len() != w as usize * h as usize * 3 {
        return Err(SkipReason::Color);
    }
    Ok(image)
}

/// Turns a decoded frame into the RGB-D image and camera the
/// reconstruction consumes, keeping depth between `near` and `far`.
pub fn prepare(frame: &CaptureFrame, near: f32, far: f32, params: &IngestParams) -> Result<Prepared, SkipReason> {
    let cam = frame_camera(frame)?;
    let (depth, valid) = clean_depth(frame, &cam, near, far, params);
    if valid == 0 {
        return Err(SkipReason::NoDepth);
    }
    let color = color_on_depth_grid(frame)?;
    Ok(Prepared { frame: RgbdFrame { width: cam.width, height: cam.height, rgb: color.rgb, depth }, cam, valid })
}

/// The point the frame looks at: where its optical axis meets the scene
/// (the median depth of the middle of the image), for a region of interest
/// nobody has placed yet.
pub fn focus_point(prepared: &Prepared) -> Option<Vec3> {
    let (w, h) = (prepared.frame.width as usize, prepared.frame.height as usize);
    let mut middle: Vec<f32> = Vec::new();
    for y in h * 3 / 8..(h * 5).div_ceil(8) {
        for x in w * 3 / 8..(w * 5).div_ceil(8) {
            let z = prepared.frame.depth[y * w + x];
            if z > 0.0 {
                middle.push(z);
            }
        }
    }
    if middle.is_empty() {
        return None;
    }
    let mid = middle.len() / 2;
    let (_, z, _) = middle.select_nth_unstable_by(mid, f32::total_cmp);
    Some(prepared.cam.unproject(prepared.cam.cx, prepared.cam.cy, *z))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ingest::frame::{Orientation, Tracking};

    fn frame(depth_mm: Vec<u16>, w: u16, h: u16) -> CaptureFrame {
        CaptureFrame {
            color_width: 16,
            color_height: 12,
            intrinsics: [12.0, 12.0, 8.0, 6.0],
            depth_width: w,
            depth_height: h,
            depth_mm,
            tracking: Tracking::Normal,
            orientation: Orientation::Portrait,
            ..CaptureFrame::default()
        }
    }

    #[test]
    fn intrinsics_are_scaled_to_the_depth_grid() {
        let cam = frame_camera(&frame(vec![1000; 12], 4, 3)).unwrap();
        assert_eq!((cam.width, cam.height, cam.fx, cam.fy, cam.cx, cam.cy), (4, 3, 3.0, 3.0, 2.0, 1.5));
        // A depth grid of another aspect ratio scales each axis by its own
        // factor.
        let wide = frame_camera(&frame(vec![1000; 16], 8, 2)).unwrap();
        assert_eq!((wide.fx, wide.fy, wide.cx, wide.cy), (6.0, 2.0, 4.0, 1.0));
    }

    #[test]
    fn broken_poses_and_intrinsics_are_refused() {
        let base = frame(vec![1000; 12], 4, 3);
        let with_pose = |f: &dyn Fn(&mut [f32; 16])| {
            let mut frame = base.clone();
            f(&mut frame.world_from_cam);
            frame_camera(&frame)
        };
        assert!(with_pose(&|_| {}).is_ok());
        assert!(with_pose(&|m| m[12] = 3.5).is_ok());
        assert_eq!(with_pose(&|m| m[0] = f32::NAN), Err(SkipReason::Pose));
        assert_eq!(with_pose(&|m| m[13] = f32::INFINITY), Err(SkipReason::Pose));
        assert_eq!(with_pose(&|m| *m = [0.0; 16]), Err(SkipReason::Pose));
        assert_eq!(with_pose(&|m| m[0] = 2.0), Err(SkipReason::Pose));
        assert_eq!(with_pose(&|m| m[0] = -1.0), Err(SkipReason::Pose), "a mirror is not a rotation");
        assert_eq!(with_pose(&|m| m[15] = 2.0), Err(SkipReason::Pose));
        assert_eq!(with_pose(&|m| m[3] = 0.5), Err(SkipReason::Pose));
        assert_eq!(with_pose(&|m| m[12] = 1.0e6), Err(SkipReason::Pose));
        let huge = CaptureFrame { depth_width: 2048, depth_height: 1024, depth_mm: Vec::new(), ..base.clone() };
        assert_eq!(frame_camera(&huge), Err(SkipReason::DepthSize));
        // A rotation with round-off is fine.
        let turned = Mat4::from_rotation_y(0.7) * Mat4::from_rotation_x(-0.3);
        assert!(with_pose(&|m| {
            *m = turned.to_cols_array();
            for i in [0, 1, 2, 4, 5, 6, 8, 9, 10] {
                m[i] *= 1.001;
            }
        })
        .is_ok());

        let with_k = |k: [f32; 4], cw: u16, ch: u16| {
            frame_camera(&CaptureFrame { intrinsics: k, color_width: cw, color_height: ch, ..base.clone() })
        };
        for (k, cw, ch) in [
            ([0.0, 12.0, 8.0, 6.0], 16, 12),
            ([-12.0, 12.0, 8.0, 6.0], 16, 12),
            ([f32::NAN, 12.0, 8.0, 6.0], 16, 12),
            ([12.0, f32::INFINITY, 8.0, 6.0], 16, 12),
            ([12.0, 12.0, 800.0, 6.0], 16, 12),
            ([12.0, 12.0, 8.0, f32::NAN], 16, 12),
            ([1.0e-30, 12.0, 8.0, 6.0], 16, 12),
            ([12.0, 12.0, 8.0, 6.0], 0, 12),
            ([12.0, 12.0, 8.0, 6.0], 16, 0),
            ([1.0, 1.0, 0.0, 0.0], 65535, 65535),
        ] {
            assert_eq!(with_k(k, cw, ch), Err(SkipReason::Intrinsics), "{k:?} {cw}x{ch}");
        }
    }

    #[test]
    fn depth_is_cleaned_by_range_confidence_and_edges() {
        let params = IngestParams::default();
        // The focal length of ARKit's depth map (256 pixels for 60 degrees).
        let frame = |depth_mm: Vec<u16>, w: u16, h: u16| CaptureFrame {
            color_width: w,
            color_height: h,
            intrinsics: [221.0, 221.0, w as f32 / 2.0, h as f32 / 2.0],
            ..frame(depth_mm, w, h)
        };
        let clean_depth = |f: &CaptureFrame, near: f32, far: f32, params: &IngestParams| {
            clean_depth(f, &frame_camera(f).unwrap(), near, far, params)
        };
        // A wall at 2 m with a hole, a near and a far outlier column.
        let (w, h) = (8usize, 6usize);
        let mut mm = vec![2000u16; w * h];
        mm[w + 1] = 0;
        let mut conf = vec![2u8; w * h];
        conf[3 * w + 6] = 0;
        conf[4 * w + 6] = 1;
        let mut f = frame(mm.clone(), w as u16, h as u16);
        f.confidence = Some(conf);
        let (depth, valid) = clean_depth(&f, 0.25, 5.0, &params);
        assert_eq!(depth[w + 1], 0.0, "a hole stays a hole");
        assert_eq!(depth[3 * w + 6], 0.0, "low confidence is dropped");
        assert_eq!(depth[4 * w + 6], 2.0, "medium confidence is kept by default");
        assert_eq!(valid as usize, w * h - 2);
        let strict = IngestParams { min_confidence: 2, ..params };
        assert_eq!(clean_depth(&f, 0.25, 5.0, &strict).1 as usize, w * h - 3);
        // Range.
        assert_eq!(clean_depth(&f, 2.5, 5.0, &params).1, 0);
        assert_eq!(clean_depth(&f, 0.25, 1.5, &params).1, 0);

        // A step from 1 m to 2 m with one interpolated pixel in between:
        // the flying pixel and both rims go, the rest stays.
        let row: [u16; 8] = [1000, 1000, 1000, 1500, 2000, 2000, 2000, 2000];
        let step = frame(row.iter().copied().cycle().take(w * h).collect(), w as u16, h as u16);
        let (depth, _) = clean_depth(&step, 0.25, 5.0, &params);
        for y in 0..h {
            let kept: Vec<bool> = (0..w).map(|x| depth[y * w + x] > 0.0).collect();
            assert_eq!(kept, [true, true, false, false, false, true, true, true], "row {y}");
        }
        // Without the filter everything in range is kept; and with a
        // confidence plane it is the confidence that speaks for the edges.
        let lax = IngestParams { edge_jump: 0.0, ..params };
        assert_eq!(clean_depth(&step, 0.25, 5.0, &lax).1 as usize, w * h);
        let mut rated = step.clone();
        rated.confidence = Some((0..w * h).map(|i| if i % w == 3 { 0 } else { 2 }).collect());
        assert_eq!(clean_depth(&rated, 0.25, 5.0, &params).1 as usize, (w - 1) * h);
        // A floor seen at a slant steps a few percent per pixel and stays.
        let slanted: Vec<u16> = (0..w * h).map(|i| 1000 + 25 * (i / w) as u16).collect();
        assert_eq!(clean_depth(&frame(slanted.clone(), w as u16, h as u16), 0.25, 5.0, &params).1 as usize, w * h);
        // On a coarse depth map a floor steps several times as much per
        // pixel: the limit follows the resolution.
        let coarse: Vec<u16> = (0..w * h).map(|i| 1000 + 150 * (i / w) as u16).collect();
        let mut coarse = frame(coarse, w as u16, h as u16);
        assert!((clean_depth(&coarse, 0.25, 5.0, &params).1 as usize) < w * h);
        coarse.intrinsics = [55.0, 55.0, 4.0, 3.0];
        assert_eq!(clean_depth(&coarse, 0.25, 5.0, &params).1 as usize, w * h);
        // Planes of the wrong length are ignored, never indexed.
        let mut odd = frame(vec![2000; 5], w as u16, h as u16);
        odd.confidence = Some(vec![0; 3]);
        assert_eq!(clean_depth(&odd, 0.25, 5.0, &params).1, 5);
    }

    #[test]
    fn parameters_are_sane_and_hostile_ones_are_tamed() {
        let d = IngestParams::default();
        assert_eq!(d.sanitized(), d);
        assert_eq!(serde_json::from_str::<IngestParams>("{}").unwrap(), d);
        let partial: IngestParams = serde_json::from_str(r#"{"min_confidence":2,"room":{"voxel_size":0.03}}"#).unwrap();
        assert_eq!((partial.min_confidence, partial.room.voxel_size, partial.room.far), (2, 0.03, 5.0));
        let wild = r#"{"min_confidence":200,"edge_jump":-1,"room":{"voxel_size":1e-30,"near":1e30,"far":-5},
            "max_voxels":18446744073709551615,"blend_frames":4000000000,"kernel":1e30,"region_margin":0,
            "min_frames":0,"preview_width":0}"#;
        let tame = serde_json::from_str::<IngestParams>(wild).unwrap().sanitized();
        assert_eq!(tame.min_confidence, 2);
        assert_eq!(tame.edge_jump, 0.0);
        assert!(tame.room.voxel_size >= VoxelFusion::MIN_VOXEL_SIZE && tame.room.far > tame.room.near);
        assert_eq!(tame.max_voxels, IngestParams::MAX_VOXELS_LIMIT);
        assert_eq!(tame.blend_frames, VoxelFusion::MAX_BLEND_FRAMES);
        assert!(tame.kernel <= 1.5 && tame.region_margin >= 1.0 && tame.min_frames == 1 && tame.preview_width == 16);
        assert_eq!(tame.sanitized(), tame);
        let nan = IngestParams { edge_jump: f32::NAN, kernel: f32::NAN, ..d };
        assert_eq!(nan.sanitized(), d);
        for mode in CaptureMode::ALL {
            let m = d.mode(mode);
            assert!(m.near < m.far && m.voxel_size >= VoxelFusion::MIN_VOXEL_SIZE);
            assert_eq!(m.default_radius > 0.0, mode.uses_region());
        }
    }
}
