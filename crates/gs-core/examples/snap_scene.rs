//! Renders contact sheets of the built-in scenes for visual inspection.
//!
//! ```text
//! cargo run -p gs-core --release --features par --example snap_scene -- <out_dir>
//! ```
//!
//! Writes into `<out_dir>`:
//! - `stage_sheet.png`: the dancer from three viewpoints at six instants of the loop
//! - `stage_closeup.png`: larger renders at four instants
//! - `stage_ring.png`: what every ring camera delivers (RGB above, depth below)
//! - `house_rooms.png`: one view per room
//! - `house_tour.png`: handheld frames along the scan path
//! - `house_plan.png`: top-down view without the ceiling, with the scan path drawn in

use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use gs_core::capture::{render_reference, render_rgbd, CaptureOptions, NoiseModel};
use gs_core::glam::Vec3;
use gs_core::rig::{RingRig, ScanPath};
use gs_core::scene::house::{HouseOptions, HouseScene, EYE_HEIGHT, ROOMS};
use gs_core::scene::{make_scene, Scene, SceneKind};
use gs_core::{Camera, RgbdFrame};

/// A simple RGB canvas that frames are pasted into.
struct Canvas {
    width: u32,
    height: u32,
    rgb: Vec<u8>,
}

impl Canvas {
    fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            rgb: vec![24; (width * height * 3) as usize],
        }
    }

    fn from_frame(frame: &RgbdFrame) -> Self {
        Self {
            width: frame.width,
            height: frame.height,
            rgb: frame.rgb.clone(),
        }
    }

    fn put(&mut self, x: i32, y: i32, c: [u8; 3]) {
        if x >= 0 && y >= 0 && (x as u32) < self.width && (y as u32) < self.height {
            let i = ((y as u32 * self.width + x as u32) * 3) as usize;
            self.rgb[i..i + 3].copy_from_slice(&c);
        }
    }

    /// Pastes `rgb` (w x h) at `(x0, y0)`, magnified by an integer factor.
    fn paste(&mut self, x0: u32, y0: u32, w: u32, h: u32, rgb: &[u8], zoom: u32) {
        for y in 0..h * zoom {
            for x in 0..w * zoom {
                let s = (((y / zoom) * w + x / zoom) * 3) as usize;
                self.put((x0 + x) as i32, (y0 + y) as i32, [rgb[s], rgb[s + 1], rgb[s + 2]]);
            }
        }
    }

    fn dot(&mut self, x: f32, y: f32, radius: i32, c: [u8; 3]) {
        for dy in -radius..=radius {
            for dx in -radius..=radius {
                if dx * dx + dy * dy <= radius * radius {
                    self.put(x as i32 + dx, y as i32 + dy, c);
                }
            }
        }
    }

    fn save(&self, path: &Path) {
        let file = BufWriter::new(File::create(path).expect("create png"));
        let mut encoder = png::Encoder::new(file, self.width, self.height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().expect("png header");
        writer.write_image_data(&self.rgb).expect("png data");
        println!("wrote {}", path.display());
    }
}

/// Lays frames out on a grid with a small gutter.
fn contact_sheet(frames: &[RgbdFrame], columns: usize) -> Canvas {
    let (w, h) = (frames[0].width, frames[0].height);
    let rows = frames.len().div_ceil(columns);
    let gap = 4;
    let mut canvas = Canvas::new(columns as u32 * (w + gap) + gap, rows as u32 * (h + gap) + gap);
    for (i, f) in frames.iter().enumerate() {
        let (cx, cy) = ((i % columns) as u32, (i / columns) as u32);
        canvas.paste(gap + cx * (w + gap), gap + cy * (h + gap), w, h, &f.rgb, 1);
    }
    canvas
}

/// Depth map as a near-bright / far-dark color ramp; invalid pixels are black.
fn depth_image(frame: &RgbdFrame) -> Vec<u8> {
    let valid = frame.depth.iter().copied().filter(|&z| z > 0.0);
    let (lo, hi) = valid.fold((f32::INFINITY, 0.0f32), |(lo, hi), z| (lo.min(z), hi.max(z)));
    let mut out = Vec::with_capacity(frame.depth.len() * 3);
    for &z in &frame.depth {
        if z <= 0.0 {
            out.extend_from_slice(&[0, 0, 0]);
            continue;
        }
        let s = 1.0 - ((z - lo) / (hi - lo).max(1e-6)).clamp(0.0, 1.0);
        let c = Vec3::new(0.15, 0.1, 0.45).lerp(Vec3::new(1.0, 0.95, 0.4), s) * (0.35 + 0.65 * s);
        out.extend_from_slice(&[(c.x * 255.0) as u8, (c.y * 255.0) as u8, (c.z * 255.0) as u8]);
    }
    out
}

const BACKDROP: [u8; 3] = [38, 40, 46];

fn stage(out: &Path) {
    let scene = make_scene(SceneKind::Stage);
    let period = scene.loop_duration().unwrap();

    // Three viewpoints x six instants.
    let views = [
        (Vec3::new(0.0, 1.5, 4.3), Vec3::new(0.0, 0.95, 0.0)),
        (Vec3::new(3.4, 2.4, 2.2), Vec3::new(0.0, 0.9, 0.0)),
        (Vec3::new(-3.6, 1.1, -2.0), Vec3::new(0.0, 0.95, 0.0)),
    ];
    let columns = 8;
    let mut frames = Vec::new();
    for (eye, target) in views {
        let cam = Camera::look_at(eye, target, Vec3::Y, 36f32.to_radians(), 230, 300);
        for k in 0..columns {
            let snap = scene.snapshot(period * (k as f32 + 0.3) / columns as f32);
            frames.push(render_reference(&snap, &cam, true, 2, BACKDROP));
        }
    }
    contact_sheet(&frames, columns).save(&out.join("stage_sheet.png"));

    // Larger renders.
    let cam = Camera::look_at(
        Vec3::new(1.2, 1.5, 3.3),
        Vec3::new(0.0, 0.95, 0.0),
        Vec3::Y,
        38f32.to_radians(),
        440,
        560,
    );
    let closeups: Vec<RgbdFrame> = [0.1, 0.36, 0.6, 0.86]
        .iter()
        .map(|u| render_reference(&scene.snapshot(period * u), &cam, true, 2, BACKDROP))
        .collect();
    contact_sheet(&closeups, 4).save(&out.join("stage_closeup.png"));

    // The whole stage from the default viewer position.
    let (eye, target) = scene.viewer_start();
    let cam = Camera::look_at(
        eye + Vec3::new(1.5, 1.2, 1.5),
        target,
        Vec3::Y,
        55f32.to_radians(),
        640,
        420,
    );
    Canvas::from_frame(&render_reference(
        &scene.snapshot(period * 0.3),
        &cam,
        true,
        2,
        BACKDROP,
    ))
    .save(&out.join("stage_wide.png"));

    // What the ring cameras deliver: RGB on top, depth below, realistic noise.
    let rig = RingRig::default();
    let cams = rig.cameras();
    let snap = scene.snapshot(period * 0.3);
    let zoom = 2;
    let (w, h) = (rig.width * zoom, rig.height * zoom);
    let per_row = 4;
    let rows = cams.len().div_ceil(per_row) as u32;
    let mut canvas = Canvas::new(per_row as u32 * (w + 4) + 4, rows * (2 * h + 12) + 4);
    for (i, cam) in cams.iter().enumerate() {
        let opts = CaptureOptions {
            noise: NoiseModel::realistic(),
            seed: i as u32,
            ..Default::default()
        };
        let frame = render_rgbd(&snap, cam, &opts);
        let (x, y) = (
            4 + (i % per_row) as u32 * (w + 4),
            4 + (i / per_row) as u32 * (2 * h + 12),
        );
        canvas.paste(x, y, rig.width, rig.height, &frame.rgb, zoom);
        canvas.paste(x, y + h + 4, rig.width, rig.height, &depth_image(&frame), zoom);
    }
    canvas.save(&out.join("stage_ring.png"));
}

fn house(out: &Path) {
    let scene = HouseScene::new();
    let snap = scene.snapshot(0.0);

    // One view per room, from a corner toward the opposite one.
    let views: [(&str, [f32; 3], [f32; 3]); 6] = [
        ("living", [-0.3, 1.65, -0.45], [-3.6, 0.7, -2.4]),
        ("kitchen", [0.9, 1.65, -0.35], [3.8, 0.8, -3.0]),
        ("dining", [3.35, 1.65, 0.45], [4.7, 0.7, 3.2]),
        ("hall", [-0.3, 1.6, 3.6], [-0.2, 1.1, 0.0]),
        ("bedroom", [-1.35, 1.7, 1.2], [-4.4, 0.6, 3.0]),
        ("bathroom", [0.75, 1.7, 3.6], [2.4, 0.6, 0.6]),
    ];
    let frames: Vec<RgbdFrame> = views
        .iter()
        .map(|(_, eye, target)| {
            let cam = Camera::look_at(
                Vec3::from(*eye),
                Vec3::from(*target),
                Vec3::Y,
                72f32.to_radians(),
                480,
                360,
            );
            render_reference(&snap, &cam, true, 2, [255, 0, 255])
        })
        .collect();
    contact_sheet(&frames, 3).save(&out.join("house_rooms.png"));

    // Second set: the opposite direction in every room.
    let views2: [([f32; 3], [f32; 3]); 6] = [
        ([-5.1, 1.6, -3.5], [-1.0, 0.8, -0.6]),
        ([5.1, 1.6, -0.3], [1.5, 0.8, -3.2]),
        ([5.1, 1.6, 3.5], [3.4, 0.8, 0.6]),
        ([-0.3, 1.6, 0.6], [-0.3, 1.0, 3.9]),
        ([-5.1, 1.7, 0.6], [-1.6, 0.7, 3.4]),
        ([2.6, 1.7, 2.3], [0.7, 0.7, 0.4]),
    ];
    let frames: Vec<RgbdFrame> = views2
        .iter()
        .map(|(eye, target)| {
            let cam = Camera::look_at(
                Vec3::from(*eye),
                Vec3::from(*target),
                Vec3::Y,
                72f32.to_radians(),
                480,
                360,
            );
            render_reference(&snap, &cam, true, 2, [255, 0, 255])
        })
        .collect();
    contact_sheet(&frames, 3).save(&out.join("house_rooms_reverse.png"));

    // Handheld footage: a frame every 2.5 seconds of the tour.
    let path = ScanPath::house_default();
    let step = 2.5;
    let count = (path.duration() / step) as usize + 1;
    let frames: Vec<RgbdFrame> = (0..count)
        .map(|i| {
            let cam = path.camera_at(i as f32 * step, 70.0, 320, 240);
            render_rgbd(&snap, &cam, &CaptureOptions::default())
        })
        .collect();
    contact_sheet(&frames, 6).save(&out.join("house_tour.png"));
    println!(
        "tour: {:.1} m, {:.1} s, {} waypoints",
        path.length(),
        path.duration(),
        path.waypoints().len()
    );

    // Floor plan: ceiling removed, seen from far above (nearly orthographic).
    let open = HouseScene::with_options(HouseOptions { ceiling: false }).snapshot(0.0);
    let cam = Camera::look_at(
        Vec3::new(0.0, 60.0, 0.0),
        Vec3::ZERO,
        Vec3::NEG_Z,
        12.2f32.to_radians(),
        1200,
        900,
    );
    let mut plan = Canvas::from_frame(&render_reference(&open, &cam, true, 2, [20, 20, 24]));
    for (i, p) in path.polyline(900).iter().enumerate() {
        if let Some((px, py, _)) = cam.project(*p) {
            let s = i as f32 / 899.0;
            plan.dot(px, py, 2, [(255.0 * s) as u8, 40, (255.0 * (1.0 - s)) as u8]);
        }
    }
    for p in path.waypoints() {
        if let Some((px, py, _)) = cam.project(*p) {
            plan.dot(px, py, 4, [255, 230, 0]);
        }
    }
    for room in ROOMS {
        if let Some((px, py, _)) = cam.project(room.center() + Vec3::Y * EYE_HEIGHT) {
            plan.dot(px, py, 3, [255, 255, 255]);
        }
    }
    plan.save(&out.join("house_plan.png"));
}

fn main() {
    let out = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| "snap_out".to_string()));
    std::fs::create_dir_all(&out).expect("create output directory");
    stage(&out);
    house(&out);
}
