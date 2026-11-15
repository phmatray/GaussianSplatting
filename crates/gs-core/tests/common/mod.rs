//! Shared by the integration tests and the `snap_recon` example: a minimal
//! wire client, the held-out viewpoints and the image metrics of the
//! novel-view quality check.
//!
//! The reconstruction is always judged the way a viewer would see it: the
//! pipeline's own [`Outgoing`] messages are decoded with [`codec::decode`],
//! the two layers are maintained exactly as a client does, and the resulting
//! cloud is drawn by the CPU references of the viewer's two blending modes
//! ([`Renderer`]) from cameras that are *not* part of the rig or on the scan
//! path.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::Path;

use gs_core::capture::{render_reference, render_rgbd, CaptureOptions};
use gs_core::codec::{self, Body, LayerMode, PreviewMsg};
use gs_core::experimental::metrics::{self, classify, MaskParams, INTERIOR, SILHOUETTE, TEXTURE, UNOBSERVED};
use gs_core::experimental::wblend::{self, Layer, WblendOptions, LIVE_LAYER_WEIGHT};
use gs_core::glam::Vec3;
use gs_core::pipeline::{scan_frame, scan_frame_count, scan_sensor, Outgoing, Pipeline, ScanFrame};
use gs_core::protocol::{ControlMsg, PipelineConfig, ScanState, SessionState, Stats, SORTED_SPLAT_SCALE};
use gs_core::raster::Image;
use gs_core::rig::ScanPath;
use gs_core::scene::{house, SceneSnapshot};
use gs_core::{Camera, RgbdFrame, Splat};

/// Flat color of everything that is not scene (reference misses) or not
/// covered by splats (reconstruction).
pub const BACKGROUND: [u8; 3] = [18, 20, 28];

/// What a viewer holds after applying a stream of pipeline messages.
#[derive(Default)]
pub struct Client {
    pub state: Option<SessionState>,
    /// Layer 0 (static) and layer 1 (live).
    pub layers: [Vec<Splat>; 2],
    pub stats: Option<Stats>,
    /// Latest preview per camera id.
    pub previews: BTreeMap<u8, PreviewMsg>,
    /// Total bytes received.
    pub bytes: usize,
    /// Messages received per kind: state, layer, preview, stats.
    pub counts: [usize; 4],
    last_seq: Option<u32>,
}

impl Client {
    /// Decodes and applies messages in order. Panics on anything a real
    /// client could not make sense of.
    pub fn apply(&mut self, messages: &[Outgoing]) {
        for out in messages {
            self.bytes += out.bytes.len();
            let msg = codec::decode(&out.bytes).expect("pipeline emitted an undecodable message");
            if let Some(last) = self.last_seq {
                assert!(msg.seq > last, "sequence numbers must increase: {} after {last}", msg.seq);
            }
            self.last_seq = Some(msg.seq);
            match msg.body {
                Body::State(json) => {
                    assert!(out.reliable, "State must be reliable");
                    self.state = Some(serde_json::from_str(&json).expect("State is not a SessionState"));
                    self.counts[0] += 1;
                }
                Body::Layer(layer) => {
                    let id = layer.layer_id as usize;
                    assert!(id < 2, "unknown layer {id}");
                    assert_eq!(out.reliable, id == 0, "layer 0 is reliable, layer 1 is not");
                    match layer.mode {
                        LayerMode::Replace => self.layers[id] = layer.splats,
                        LayerMode::Append => self.layers[id].extend_from_slice(&layer.splats),
                    }
                    self.counts[1] += 1;
                }
                Body::Preview(preview) => {
                    assert!(!out.reliable);
                    self.previews.insert(preview.camera_id, preview);
                    self.counts[2] += 1;
                }
                Body::Stats(json) => {
                    assert!(!out.reliable);
                    self.stats = Some(serde_json::from_str(&json).expect("Stats is not a Stats"));
                    self.counts[3] += 1;
                }
            }
        }
    }

    /// Both layers in one cloud, as a viewer draws them.
    pub fn cloud(&self) -> Vec<Splat> {
        let mut all = self.layers[0].clone();
        all.extend_from_slice(&self.layers[1]);
        all
    }

    pub fn stats(&self) -> &Stats {
        self.stats.as_ref().expect("no Stats received yet")
    }

    pub fn scan_state(&self) -> Option<ScanState> {
        self.stats.as_ref().and_then(|s| s.scan).map(|s| s.state)
    }
}

/// Runs the whole home scan as fast as possible and returns the client that
/// followed it, plus the number of ticks it took.
pub fn run_full_scan(pipeline: &mut Pipeline, client: &mut Client) -> usize {
    client.apply(&pipeline.snapshot());
    client.apply(&pipeline.handle_control(ControlMsg::ScanFinish));
    let mut ticks = 0;
    while client.scan_state() != Some(ScanState::Done) {
        client.apply(&pipeline.tick(1.0 / 30.0));
        ticks += 1;
        assert!(ticks < 100_000, "the scan never finishes");
    }
    ticks
}

/// A held-out viewpoint.
pub struct View {
    pub name: String,
    pub cam: Camera,
}

/// Image size of the held-out views. Large enough that a splat of the
/// default clouds covers a few pixels: at 320x240 the views are coarser
/// than the sensors, and a finer reconstruction scores no better.
pub const VIEW_WIDTH: u32 = 640;
pub const VIEW_HEIGHT: u32 = 480;

fn view(name: impl Into<String>, eye: Vec3, target: Vec3, fov_deg: f32) -> View {
    View { name: name.into(), cam: Camera::look_at(eye, target, Vec3::Y, fov_deg.to_radians(), VIEW_WIDTH, VIEW_HEIGHT) }
}

/// Stage viewpoints: four at standing eye height, half-way between rig
/// cameras and a little farther out than the rig, one close-up of the dancer
/// and one from above. The default rig has its cameras every 45 degrees at
/// 1.3 m and 2.5 m, 3.2 m from the axis.
pub fn stage_views() -> Vec<View> {
    let mut views = Vec::new();
    for k in 0..4 {
        let angle = (22.5f32 + 90.0 * k as f32).to_radians();
        let eye = Vec3::new(3.6 * angle.sin(), 1.65, 3.6 * angle.cos());
        views.push(view(format!("ring{k}"), eye, Vec3::new(0.0, 0.9, 0.0), 50.0));
    }
    views.push(view("closeup", Vec3::new(1.25, 1.45, 1.45), Vec3::new(0.0, 1.05, 0.0), 50.0));
    views.push(view("above", Vec3::new(0.5, 5.2, 0.9), Vec3::new(0.0, 0.3, 0.0), 50.0));
    views
}

/// The stage viewpoint that looks down on the floor (shadow checks).
pub fn stage_view_above() -> View {
    stage_views().pop().unwrap()
}

/// Smallest free distance around a house viewpoint, and its smallest
/// horizontal distance to the scan path, except in the hallway.
pub const HOUSE_VIEW_CLEARANCE: f32 = 0.6;
pub const HOUSE_VIEW_PATH_DISTANCE: f32 = 0.4;
/// The hallway is 1.3 m wide with the scan path in its middle: there the
/// viewpoints differ from the path mostly in height.
pub const HALL_VIEW_CLEARANCE: f32 = 0.3;

/// House viewpoints: in every room two positions in free space (at least
/// [`HOUSE_VIEW_CLEARANCE`] from any surface) away from the scan path, which
/// walks at 1.4 m, each looking in two directions.
pub fn house_views() -> Vec<View> {
    /// Name, eye and two targets.
    type Spot = (&'static str, [f32; 3], [[f32; 3]; 2]);
    #[rustfmt::skip]
    let spots: &[Spot] = &[
        ("living_a",   [-1.3, 1.62, -1.7],  [[-4.5, 0.7, -1.0], [-3.0, 1.0, -4.0]]),
        ("living_b",   [-4.0, 1.62, -0.7],  [[-1.5, 0.6, -3.0], [0.5, 1.2, -1.5]]),
        ("kitchen_a",  [0.9, 1.62, -1.3],   [[4.5, 0.9, -3.5], [2.0, 1.1, -4.0]]),
        ("kitchen_b",  [2.8, 1.62, -3.2],   [[3.5, 0.8, 0.5], [-2.0, 0.9, -1.5]]),
        ("dining_a",   [3.7, 1.62, 1.1],    [[5.0, 0.8, 3.5], [4.5, 1.2, -2.0]]),
        ("dining_b",   [3.7, 1.62, 3.1],    [[5.2, 0.9, 1.5], [4.3, 1.0, -1.0]]),
        ("hall_a",     [-0.05, 1.75, 1.0],  [[-0.3, 1.0, 3.9], [-0.9, 1.4, 2.2]]),
        ("hall_b",     [-0.55, 1.15, 3.2],  [[-0.2, 1.3, 0.3], [0.4, 1.2, 2.0]]),
        ("bedroom_a",  [-3.6, 1.62, 1.5],   [[-4.8, 0.6, 3.2], [-1.6, 0.9, 3.6]]),
        ("bedroom_b",  [-2.4, 1.62, 3.1],   [[-4.8, 0.8, 1.0], [-1.2, 1.2, 1.0]]),
        ("bathroom_a", [1.5, 1.62, 0.9],    [[2.6, 0.7, 3.4], [0.9, 0.9, 3.8]]),
        ("bathroom_b", [2.1, 1.62, 2.9],    [[0.7, 0.9, 0.9], [2.8, 1.1, 0.8]]),
    ];
    let mut views = Vec::new();
    for (name, eye, targets) in spots {
        for (k, target) in targets.iter().enumerate() {
            views.push(view(format!("{name}{k}"), Vec3::from_array(*eye), Vec3::from_array(*target), 70.0));
        }
    }
    views
}

/// More house viewpoints, looking where a visitor in a headset looks but a
/// scan easily misses: straight up at ceilings and beams, into the corners
/// under the ceiling and down at the floor.
pub fn house_look_around_views() -> Vec<View> {
    #[rustfmt::skip]
    let spots: &[(&str, [f32; 3], [f32; 3], f32)] = &[
        ("living_ceiling",  [-2.5, 1.2, -2.0], [-2.6, 2.5, -2.2], 90.0),
        ("living_corner",   [-3.5, 1.6, -2.0], [-5.4, 2.4, -3.9], 70.0),
        ("living_floor",    [-2.5, 1.6, -1.5], [-2.3, 0.0, -1.8], 80.0),
        ("kitchen_ceiling", [2.5, 1.2, -2.2],  [2.6, 2.5, -2.4],  90.0),
        ("bedroom_ceiling", [-3.0, 1.2, 2.2],  [-3.1, 2.5, 2.3],  90.0),
        ("bathroom_corner", [1.6, 1.6, 1.8],   [2.9, 0.2, 3.8],   80.0),
        ("bathroom_up",     [1.8, 1.0, 2.0],   [2.9, 2.4, 0.6],   80.0),
        ("hall_up",         [-0.4, 1.3, 2.0],  [-0.3, 2.5, 2.6],  90.0),
    ];
    spots.iter().map(|(name, eye, target, fov)| view(*name, Vec3::from_array(*eye), Vec3::from_array(*target), *fov)).collect()
}

/// Bird's-eye view of the whole house (ceiling to be clipped by the caller).
pub fn top_view() -> View {
    let eye = Vec3::new(0.0, 15.0, 0.0);
    View {
        name: "top".into(),
        cam: Camera::look_at(eye, Vec3::ZERO, Vec3::NEG_Z, 36f32.to_radians(), 2 * VIEW_WIDTH, 2 * VIEW_HEIGHT),
    }
}

/// The two blending modes of a viewer (`docs/ARCHITECTURE.md`), drawn by
/// their CPU references.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Renderer {
    /// Normalised blending with soft silhouettes, the mode the clouds are
    /// produced for and a viewer's default: splat scale 1, the live layer
    /// weighted [`LIVE_LAYER_WEIGHT`] to 1.
    Normalized,
    /// Sorted "over" compositing, the fallback: splat scale
    /// [`SORTED_SPLAT_SCALE`].
    Sorted,
}

impl Renderer {
    pub const BOTH: [Self; 2] = [Self::Normalized, Self::Sorted];

    pub fn name(self) -> &'static str {
        match self {
            Self::Normalized => "normalised",
            Self::Sorted => "sorted",
        }
    }

    fn options(self) -> WblendOptions {
        match self {
            Self::Normalized => WblendOptions::normalized_soft(),
            Self::Sorted => WblendOptions { kernel_scale: SORTED_SPLAT_SCALE, ..WblendOptions::sorted() },
        }
    }

    /// Draws the static and the live layer; returns the picture over
    /// [`BACKGROUND`] and the coverage of every pixel.
    pub fn draw(self, layers: &[Vec<Splat>; 2], cam: &Camera) -> (Image, Vec<f32>) {
        let layers = [Layer { splats: &layers[0], weight: 1.0 }, Layer { splats: &layers[1], weight: LIVE_LAYER_WEIGHT }];
        let rendered = wblend::render(&layers, cam, &self.options());
        (rendered.over(BACKGROUND), rendered.alpha)
    }
}

/// Result of comparing one held-out view.
pub struct Score {
    /// PSNR of the splat render against the ray-cast reference, in dB, over
    /// the whole image.
    pub psnr: f32,
    /// PSNR over the pixels of the `observed` mask only (equal to `psnr`
    /// without a mask).
    pub psnr_observed: f32,
    /// Mean SSIM of the luma.
    pub ssim: f32,
    /// PSNR on flat surfaces, around color edges and around depth edges
    /// (bands of 4 pixels at 640x480), in dB.
    pub interior: f32,
    pub texture: f32,
    pub silhouette: f32,
    /// Fraction of the pixels where the reference shows scene and the splats
    /// cover at least half of the pixel.
    pub coverage: f32,
    /// The same, among the pixels of the `observed` mask.
    pub coverage_observed: f32,
    /// Fraction of the pixels that are in the `observed` mask.
    pub observed_fraction: f32,
    pub reference: Image,
    pub reconstruction: Image,
}

/// Draws the two layers of a cloud with `renderer` and the true scene from
/// `cam`, and compares them.
///
/// `observed`, when given, marks the pixels whose surface point the sensors
/// have seen at all (see [`observed_mask`]); the `*_observed` scores are
/// restricted to them, which separates the quality of the reconstruction
/// from the reach of the cameras.
pub fn evaluate(
    snap: &SceneSnapshot,
    layers: &[Vec<Splat>; 2],
    cam: &Camera,
    shadows: bool,
    observed: Option<&[bool]>,
    renderer: Renderer,
) -> Score {
    let reference = render_reference(snap, cam, shadows, 2, BACKGROUND);
    let (reconstruction, alpha) = renderer.draw(layers, cam);
    let regions = classify(&reference, observed, &MaskParams::default().scaled_to(cam.width));
    let score = metrics::score(&reference, &reconstruction, &alpha, &regions);
    Score {
        psnr: score.psnr,
        psnr_observed: score.psnr_observed,
        ssim: score.ssim,
        interior: score.psnr_region(INTERIOR),
        texture: score.psnr_region(TEXTURE),
        silhouette: score.psnr_region(SILHOUETTE),
        coverage: score.coverage,
        coverage_observed: score.coverage_observed,
        observed_fraction: 1.0 - score.pixels[UNOBSERVED] as f32 / score.pixels.iter().sum::<u64>().max(1) as f32,
        reference: Image::from_frame(&reference),
        reconstruction,
    }
}

/// The frames a depth sensor took, for [`observed_mask`].
pub struct Exposures {
    pub frames: Vec<(Camera, RgbdFrame)>,
    /// Depth returns beyond this distance were not used.
    pub max_range: f32,
}

impl Exposures {
    /// Re-takes every sensor frame of the home scan described by `config`
    /// (noise-free), exactly the frames the pipeline fuses.
    pub fn of_scan(snap: &SceneSnapshot, config: &PipelineConfig) -> Self {
        let scan = &config.scan;
        let path = ScanPath::house_default().with_speed(scan.speed);
        let frames = (0..scan_frame_count(&path, scan))
            .map(|i| {
                let ScanFrame { sensor, time } = scan_frame(&path, scan, i);
                let cam = scan_sensor(&path, time, scan, sensor);
                let frame = render_rgbd(snap, &cam, &CaptureOptions { shadows: config.shadows, ..Default::default() });
                (cam, frame)
            })
            .collect();
        Self { frames, max_range: scan.max_range }
    }
}

/// For every pixel of `view`: has any exposure seen the surface point this
/// pixel shows (inside its image, not hidden, within range)? Pixels that
/// show no scene count as observed. This is ground truth the pipeline never
/// gets; it only tells the test which pixels a reconstruction can be held
/// responsible for.
pub fn observed_mask(snap: &SceneSnapshot, exposures: &Exposures, view: &Camera) -> Vec<bool> {
    let mut mask = Vec::with_capacity((view.width * view.height) as usize);
    for y in 0..view.height {
        for x in 0..view.width {
            let (origin, dir) = view.ray(x as f32 + 0.5, y as f32 + 0.5);
            let Some(hit) = snap.raycast(origin, dir, f32::INFINITY) else {
                mask.push(true);
                continue;
            };
            mask.push(exposures.frames.iter().any(|(cam, frame)| {
                let Some((px, py, z)) = cam.project(hit.pos) else { return false };
                if px < 0.0 || py < 0.0 || px >= cam.width as f32 || py >= cam.height as f32 || z > exposures.max_range {
                    return false;
                }
                let depth = frame.depth_at(px as u32, py as u32);
                // Within a pixel the depth of a slanted surface varies.
                depth > 0.0 && (depth - z).abs() <= 0.04 * z + 0.02
            }));
        }
    }
    mask
}

/// `reference | reconstruction | 4 x absolute difference`.
pub fn side_by_side(score: &Score) -> Image {
    let (w, h) = (score.reference.width as usize, score.reference.height as usize);
    let mut out = Image::new(3 * w as u32, h as u32, [0, 0, 0]);
    for y in 0..h {
        for x in 0..w {
            let src = (y * w + x) * 3;
            for c in 0..3 {
                let (a, b) = (score.reference.rgb[src + c], score.reconstruction.rgb[src + c]);
                let row = y * 3 * w;
                out.rgb[(row + x) * 3 + c] = a;
                out.rgb[(row + w + x) * 3 + c] = b;
                out.rgb[(row + 2 * w + x) * 3 + c] = (a.abs_diff(b) as u32 * 4).min(255) as u8;
            }
        }
    }
    out
}

/// Stacks images of equal width on top of each other.
pub fn stack(images: &[Image]) -> Image {
    let width = images.first().map_or(0, |i| i.width);
    let mut out = Image { width, height: 0, rgb: Vec::new() };
    for image in images.iter().filter(|i| i.width == width) {
        out.height += image.height;
        out.rgb.extend_from_slice(&image.rgb);
    }
    out
}

pub fn save_png(path: &Path, image: &Image) {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).expect("cannot create the output directory");
    }
    let file = std::io::BufWriter::new(std::fs::File::create(path).expect("cannot create the PNG"));
    let mut encoder = png::Encoder::new(file, image.width, image.height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header().and_then(|mut w| w.write_image_data(&image.rgb)).expect("cannot write the PNG");
}

/// Where the tests and the example drop their pictures: `$GS_RECON_OUT`, if
/// set. Nothing is written otherwise.
pub fn output_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("GS_RECON_OUT").map(std::path::PathBuf::from)
}

/// Mean of a list (0 for an empty one).
pub fn mean(values: &[f32]) -> f32 {
    if values.is_empty() { 0.0 } else { values.iter().sum::<f32>() / values.len() as f32 }
}

/// Smallest distance from `eye` to the scene over a sphere of directions:
/// tells whether a viewpoint stands in free space.
pub fn clearance(snap: &SceneSnapshot, eye: Vec3) -> f32 {
    const DIRECTIONS: usize = 1500;
    let mut nearest = f32::INFINITY;
    for i in 0..DIRECTIONS {
        // Fibonacci sphere.
        let y = 1.0 - 2.0 * (i as f32 + 0.5) / DIRECTIONS as f32;
        let r = (1.0 - y * y).sqrt();
        let a = i as f32 * 2.399_963;
        if let Some(hit) = snap.raycast(eye, Vec3::new(r * a.cos(), y, r * a.sin()), 100.0) {
            nearest = nearest.min(hit.t);
        }
    }
    nearest
}

/// Horizontal distance from `p` to the default scan path of the house.
pub fn scan_path_distance(p: Vec3) -> f32 {
    ScanPath::house_default()
        .polyline(600)
        .iter()
        .map(|q| (Vec3::new(q.x, 0.0, q.z) - Vec3::new(p.x, 0.0, p.z)).length())
        .fold(f32::INFINITY, f32::min)
}

/// True when `p` is inside the house interior.
pub fn inside_house(p: Vec3) -> bool {
    p.x.abs() < house::HALF_X && p.z.abs() < house::HALF_Z && p.y > 0.0 && p.y < house::CEILING_HEIGHT
}
