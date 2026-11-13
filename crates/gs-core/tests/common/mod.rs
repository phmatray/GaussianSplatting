//! Shared by the integration tests and the `snap_recon` example: a minimal
//! wire client, the held-out viewpoints and the image metrics of the
//! novel-view quality check.
//!
//! The reconstruction is always judged the way a viewer would see it: the
//! pipeline's own [`Outgoing`] messages are decoded with [`codec::decode`],
//! the two layers are maintained exactly as a client does, and the resulting
//! cloud is rendered with the reference rasterizer from cameras that are
//! *not* part of the rig or on the scan path.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::path::Path;

use gs_core::capture::render_reference;
use gs_core::codec::{self, Body, LayerMode, PreviewMsg};
use gs_core::glam::Vec3;
use gs_core::pipeline::{Outgoing, Pipeline};
use gs_core::protocol::{ControlMsg, ScanState, SessionState, Stats};
use gs_core::raster::{psnr, rasterize, Image, RasterOptions};
use gs_core::scene::{house, SceneSnapshot};
use gs_core::{Camera, Splat};

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

/// Image size of the held-out views.
pub const VIEW_WIDTH: u32 = 320;
pub const VIEW_HEIGHT: u32 = 240;

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

/// House viewpoints: in every room two standing positions off the scan path
/// (which walks at 1.4 m), each looking in two directions.
pub fn house_views() -> Vec<View> {
    // (room, eye x, eye z, [target x, target y, target z] ...)
    #[rustfmt::skip]
    let spots: &[(&str, [f32; 2], [[f32; 3]; 2])] = &[
        ("living_a",   [-1.2, -3.2], [[-4.5, 0.6, -0.8], [-3.0, 1.2, -4.0]]),
        ("living_b",   [-4.6, -0.6], [[-1.5, 0.5, -3.0], [0.4, 1.3, -1.0]]),
        ("kitchen_a",  [1.3, -0.8],  [[4.5, 0.9, -3.5], [2.0, 1.1, -4.0]]),
        ("kitchen_b",  [4.9, -3.2],  [[1.5, 0.7, -1.0], [4.0, 0.8, 2.5]]),
        ("dining_a",   [3.5, 0.9],   [[5.0, 0.8, 3.5], [4.5, 1.2, -2.0]]),
        ("dining_b",   [5.0, 3.3],   [[3.6, 0.7, 1.0], [4.2, 1.6, 0.4]]),
        ("hall_a",     [-0.3, 3.6],  [[-0.3, 1.0, 0.0], [-0.9, 1.5, 1.5]]),
        ("hall_b",     [0.0, 0.8],   [[-0.4, 1.0, 3.9], [-0.9, 1.3, 2.0]]),
        ("bedroom_a",  [-1.6, 0.8],  [[-4.8, 0.6, 3.0], [-3.0, 1.0, 3.9]]),
        ("bedroom_b",  [-4.8, 3.5],  [[-1.8, 0.8, 1.0], [-3.2, 1.4, 0.3]]),
        ("bathroom_a", [0.9, 0.8],   [[2.6, 0.7, 3.4], [1.0, 0.9, 3.9]]),
        ("bathroom_b", [2.5, 3.5],   [[0.6, 0.9, 0.8], [2.9, 1.2, 0.9]]),
    ];
    let mut views = Vec::new();
    for (name, eye, targets) in spots {
        let eye = Vec3::new(eye[0], 1.62, eye[1]);
        for (k, target) in targets.iter().enumerate() {
            views.push(view(format!("{name}{k}"), eye, Vec3::from_array(*target), 70.0));
        }
    }
    views
}

/// Result of comparing one held-out view.
pub struct Score {
    /// PSNR of the splat render against the ray-cast reference, in dB.
    pub psnr: f32,
    /// Fraction of the pixels where the reference shows scene and the splats
    /// accumulate at least half opacity.
    pub coverage: f32,
    /// Fraction of the pixels where the reference shows scene.
    pub scene_fraction: f32,
    pub reference: Image,
    pub reconstruction: Image,
}

/// Renders `splats` and the true scene from `cam` and compares them.
///
/// The splats are rasterized over black and over white: the difference gives
/// the accumulated opacity of every pixel, from which both the coverage and
/// the image over [`BACKGROUND`] follow.
pub fn evaluate(snap: &SceneSnapshot, splats: &[Splat], cam: &Camera, shadows: bool) -> Score {
    let reference_frame = render_reference(snap, cam, shadows, 2, BACKGROUND);
    let reference = Image::from_frame(&reference_frame);
    let black = rasterize(splats, cam, &RasterOptions { background: [0, 0, 0], ..Default::default() });
    let white = rasterize(splats, cam, &RasterOptions { background: [255, 255, 255], ..Default::default() });

    let pixels = (cam.width * cam.height) as usize;
    let mut reconstruction = Image::new(cam.width, cam.height, BACKGROUND);
    let (mut scene, mut covered) = (0usize, 0usize);
    for i in 0..pixels {
        // Per channel: white − black = 255·(1 − alpha).
        let transparency: f32 =
            (0..3).map(|c| white.rgb[i * 3 + c] as f32 - black.rgb[i * 3 + c] as f32).sum::<f32>() / (3.0 * 255.0);
        for c in 0..3 {
            let v = black.rgb[i * 3 + c] as f32 + transparency * BACKGROUND[c] as f32;
            reconstruction.rgb[i * 3 + c] = v.round().clamp(0.0, 255.0) as u8;
        }
        if reference_frame.depth[i] > 0.0 {
            scene += 1;
            covered += usize::from(transparency < 0.5);
        }
    }
    Score {
        psnr: psnr(&reference, &reconstruction),
        coverage: if scene > 0 { covered as f32 / scene as f32 } else { 1.0 },
        scene_fraction: scene as f32 / pixels.max(1) as f32,
        reference,
        reconstruction,
    }
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

/// Smallest distance from `eye` to the scene over a ring of directions:
/// tells whether a viewpoint stands in free space.
pub fn clearance(snap: &SceneSnapshot, eye: Vec3) -> f32 {
    let mut nearest = f32::INFINITY;
    for i in 0..32 {
        let a = std::f32::consts::TAU * i as f32 / 32.0;
        for pitch in [-0.6f32, 0.0, 0.6] {
            let dir = Vec3::new(a.cos() * pitch.cos(), pitch.sin(), a.sin() * pitch.cos());
            if let Some(hit) = snap.raycast(eye, dir, 100.0) {
                nearest = nearest.min(hit.t);
            }
        }
    }
    nearest
}

/// True when `p` is inside the house interior.
pub fn inside_house(p: Vec3) -> bool {
    p.x.abs() < house::HALF_X && p.z.abs() < house::HALF_Z && p.y > 0.0 && p.y < house::CEILING_HEIGHT
}
