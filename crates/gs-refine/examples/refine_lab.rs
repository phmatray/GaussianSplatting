//! Measures the offline mode against exact ground truth.
//!
//! Films a procedural scene with the simulated phone of gs-core, writes the
//! `.gscap`, refines it, and scores the real-time fusion and the refined
//! result from viewpoints the phone never took, against the ray-cast scene:
//! PSNR overall and by region, SSIM, as a function of iterations and time.
//!
//! ```text
//! cargo run --release -p gs-refine --example refine_lab -- bedroom out/lab \
//!     [--frames N] [--color-width W] [--realistic] [--exposure X] [--depth-noise X] [--pose-jitter X] [--pose-drift X]
//!     [--preset rapide|standard|fin] [--set train.iterations=3000]... [--eval-at 0,500,1000] [--eval-width 1280]
//!     [--sheets] [--diff] [--ply] [--label text] [--probe] [--bench]
//! ```
//!
//! Scenarios: `bedroom`, `house`, `object`, `person` (as `gscap_write`) and
//! `living` (a sweep of the living room on a path of its own).
//! `--seed N` changes the noise of the simulated phone, `--stage-time T` the
//! pose of the subject of `object` and `person`. `--train-photos` scores
//! training and held-out photographs with one instrument; `--floaters`
//! measures what hangs in front of the true surfaces from viewpoints the
//! phone was never near and writes sheets of them.
//! `--set` patches any field of the job's configuration by its JSON path.
//! Everything here is simulated; nothing has seen a real phone.

#[path = "../../gs-core/tests/common/mod.rs"]
mod common;
#[path = "../../gs-core/tests/ingest_sim/mod.rs"]
mod ingest_sim;

use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::time::Instant;

use common::{house_look_around_views, house_views, observed_mask, save_png, Exposures, BACKGROUND};
use gs_core::capture::{render_reference, render_rgbd, CaptureOptions};
use gs_core::experimental::metrics::{self, classify, MaskParams, Summary, INTERIOR, SILHOUETTE, TEXTURE};
use gs_core::experimental::sheet;
use gs_core::experimental::wblend::{self, Layer, WblendOptions};
use gs_core::glam::Vec3;
use gs_core::ingest::frame::encode_frame;
use gs_core::ingest::{CaptureRegion, GscapHeader, GscapWriter};
use gs_core::raster::Image;
use gs_core::scene::stage::StageScene;
use gs_core::scene::{make_scene, Scene, SceneKind, SceneSnapshot};
use gs_core::{Camera, RgbdFrame, Splat};
use gs_refine::job::{CancelToken, Preset, RefineConfig, RefineJob};
use gs_refine::model::Gaussians;
use gs_refine::raster::{CpuRasterizer, Rasterizer, Rendered, View};
use ingest_sim::{orbit_pose, room_sweep, CaptureWorld, HouseWalk, Phone, PhoneSim};

const BEDROOM_LOOP: [[f32; 2]; 7] = [[-1.3, 2.0], [-1.7, 1.78], [-2.35, 1.36], [-2.92, 1.78], [-2.8, 2.38], [-2.25, 2.6], [-1.68, 2.28]];
const DANCER: Vec3 = Vec3::new(0.0, 1.0, 0.0);
/// Around the coffee table of the living room (waypoints of the house walk,
/// which is known to be free of furniture), filmed as a spiral sweep.
const LIVING_LOOP: [[f32; 2]; 8] = [[-1.3, -1.12], [-2.4, -1.18], [-3.4, -1.18], [-4.08, -1.5], [-4.08, -2.45], [-3.45, -2.92], [-2.3, -2.92], [-1.15, -2.55]];

struct Args(Vec<String>);

impl Args {
    fn flag(&self, name: &str) -> bool {
        self.0.iter().any(|a| a == name)
    }

    fn text(&self, name: &str) -> Option<&str> {
        self.0.iter().position(|a| a == name).and_then(|i| self.0.get(i + 1)).map(String::as_str)
    }

    fn number<T: std::str::FromStr>(&self, name: &str) -> Option<T> {
        self.text(name).and_then(|v| v.parse().ok())
    }

    fn all(&self, name: &str) -> Vec<&str> {
        self.0.windows(2).filter(|w| w[0] == name).map(|w| w[1].as_str()).collect()
    }
}

/// What the lab knows about a scenario and the phone does not.
struct Scenario {
    mode: &'static str,
    frames: usize,
    snap: SceneSnapshot,
    /// The scene without the person (person captures).
    empty: Option<SceneSnapshot>,
    /// Region of interest in scene coordinates.
    region: Option<(Vec3, f32)>,
    cameras: Vec<Camera>,
}

fn scenario(name: &str, sim: &PhoneSim, frames: Option<usize>, stage_time: f32) -> Scenario {
    let house = || make_scene(SceneKind::House).snapshot(0.0);
    match name {
        "living" => {
            let n = frames.unwrap_or(300);
            Scenario { mode: "room", frames: n, snap: house(), empty: None, region: None, cameras: (0..n).map(|i| sim.camera(room_sweep(&LIVING_LOOP, i, n, 47.0))).collect() }
        }
        "bedroom" => {
            let n = frames.unwrap_or(220);
            Scenario { mode: "room", frames: n, snap: house(), empty: None, region: None, cameras: (0..n).map(|i| sim.camera(room_sweep(&BEDROOM_LOOP, i, n, 55.0))).collect() }
        }
        "house" => {
            let walk = HouseWalk::new(10.0);
            let n = frames.unwrap_or(walk.frame_count()).min(walk.frame_count());
            Scenario { mode: "room", frames: n, snap: house(), empty: None, region: None, cameras: (0..n).map(|i| sim.camera(walk.pose(walk.time(i)))).collect() }
        }
        "object" | "person" => {
            let stage = StageScene::new();
            let n = frames.unwrap_or(120);
            let masked = name == "person";
            let radius = if masked { 1.2 } else { 1.1 };
            let cameras = (0..n)
                .map(|i| {
                    let (eye, target) = orbit_pose(DANCER, 2.3, 1.0, 1.75, i as f32 / n as f32, 1.0);
                    sim.look_at(eye, target)
                })
                .collect();
            Scenario {
                mode: if masked { "person" } else { "object" },
                frames: n,
                snap: stage.snapshot(stage_time),
                empty: masked.then(|| stage.snapshot_static()),
                region: Some((DANCER, radius)),
                cameras,
            }
        }
        other => panic!("unknown scenario {other}"),
    }
}

/// Writes the capture and returns its world. With `hole`, a sphere in
/// scene coordinates, the depth sensor returns nothing from inside it (a
/// window, a mirror, a black sofa): the photographs still show it.
fn write_capture(path: &Path, sc: &Scenario, sim: PhoneSim, hole: Option<(Vec3, f32)>) -> CaptureWorld {
    let mut phone = Phone::new(sim, &sc.cameras[0]);
    let world = phone.world;
    if path.exists() {
        return world;
    }
    let header = GscapHeader {
        protocol: Some(1),
        device: "simulateur".into(),
        app: "lab".into(),
        mode: sc.mode.into(),
        name: "Simulation".into(),
        created: "2026-11-14T12:00:00Z".into(),
        region: sc.region.map(|(center, radius)| CaptureRegion { center: world.to_capture(center).to_array(), radius }),
        floor: Some(world.floor_y()),
    };
    let tmp = path.with_extension("tmp");
    let file = std::fs::File::create(&tmp).expect("cannot create the capture");
    let mut writer = GscapWriter::new(BufWriter::new(file), &header, gs_core::ingest::file::HEADER_PAD).expect("header");
    for (i, cam) in sc.cameras.iter().enumerate() {
        let mut shot = phone.shoot(&sc.snap, sc.empty.as_ref(), cam, i as f64 / 10.0);
        if let Some((center, radius)) = hole {
            let frame = &mut shot.frame;
            let (dw, dh) = (frame.depth_width as usize, frame.depth_height as usize);
            let depth_cam = shot.cam.resized(dw as u32, dh as u32);
            for at in 0..dw * dh {
                let z = frame.depth_mm[at] as f32 * 1e-3;
                if z > 0.0 && depth_cam.unproject((at % dw) as f32 + 0.5, (at / dw) as f32 + 0.5, z).distance(center) < radius {
                    frame.depth_mm[at] = 0;
                    if let Some(confidence) = frame.confidence.as_mut() {
                        confidence[at] = 0;
                    }
                }
            }
        }
        writer.append(&encode_frame(&shot.frame).expect("frame")).expect("write");
    }
    drop(writer);
    std::fs::rename(&tmp, path).expect("rename");
    world
}

/// A ground-truth view: reference picture, region classes, mask of what
/// the scores count.
struct TruthView {
    name: String,
    cam: Camera,
    reference: RgbdFrame,
    regions: Vec<u8>,
    observed: Vec<bool>,
    /// Classes restricted to where the fused cloud is closed, away from its
    /// holes: what both clouds can be compared on (set by `inner_regions`).
    inner: Vec<u8>,
    observed_fraction: f32,
}

fn truth_views(name: &str, sc: &Scenario, width: u32, far: f32, probe: bool) -> Vec<TruthView> {
    let height = width * 3 / 4;
    // What the phone saw, at a coarse grid: enough to tell seen from unseen.
    let step = (sc.cameras.len() / 400).max(1);
    let quick = CaptureOptions { shadows: false, min_range: 0.05, max_range: 50.0, ..CaptureOptions::default() };
    let frames = sc.cameras.iter().step_by(step).map(|cam| (cam.resized(128, 96), render_rgbd(&sc.snap, &cam.resized(128, 96), &quick))).collect();
    let exposures = Exposures { frames, max_range: far };

    let cams: Vec<(String, Camera)> = match name {
        "bedroom" | "living" => house_views().into_iter().filter(|v| v.name.starts_with(name)).map(|v| (v.name, v.cam)).collect(),
        "house" => house_views().into_iter().map(|v| (v.name, v.cam)).collect(),
        _ => (0..8)
            .map(|k| {
                // Between the phone's positions, closer, lower and higher.
                let angle = std::f32::consts::TAU * (k as f32 + 0.37) / 8.0;
                let (radius, eye_y) = if k % 2 == 0 { (1.9, 1.25) } else { (2.1, 1.9) };
                let eye = Vec3::new(DANCER.x + radius * angle.sin(), eye_y, DANCER.z + radius * angle.cos());
                (format!("orbit{k}"), Camera::look_at(eye, DANCER, Vec3::Y, 50f32.to_radians(), 640, 480))
            })
            .collect(),
    };
    let mut cams = cams;
    if probe {
        // The same instrument at poses the phone took, and a step beside them.
        let n = sc.cameras.len();
        for k in 0..6 {
            let cam = sc.cameras[(2 * k + 1) * n / 12];
            cams.push((format!("probe-train{k}"), cam));
            let mut moved = cam;
            let side = Vec3::from(cam.world_from_cam.matrix3.x_axis);
            moved.world_from_cam.translation += gs_core::glam::Vec3A::from(side * 0.25 + Vec3::Y * 0.1);
            cams.push((format!("probe-near{k}"), moved));
        }
    }
    let mut views = Vec::new();
    for (view_name, cam) in cams {
        let cam = cam.resized(width, height);
        let mut reference = render_reference(&sc.snap, &cam, true, 2, BACKGROUND);
        let mut observed = observed_mask(&sc.snap, &exposures, &cam);
        // Object and person captures hold only the subject: the reference
        // shows the background colour everywhere else.
        if let Some((center, radius)) = sc.region {
            let without = sc.empty.as_ref().map(|empty| render_rgbd(empty, &cam, &quick));
            for (i, seen) in observed.iter_mut().enumerate() {
                let (x, y) = ((i as u32 % width) as f32 + 0.5, (i as u32 / width) as f32 + 0.5);
                let z = reference.depth[i];
                let inside = z > 0.0 && cam.unproject(x, y, z).distance(center) <= radius * 0.97;
                let subject = without.as_ref().is_none_or(|w| (w.depth[i] - z).abs() > 0.02);
                if !(inside && subject) {
                    reference.depth[i] = 0.0;
                    reference.rgb[i * 3..i * 3 + 3].copy_from_slice(&BACKGROUND);
                    // Where the true surface lies just outside the region
                    // the capture's own cut is not exactly ours.
                    *seen = !(z > 0.0 && (cam.unproject(x, y, z).distance(center) - radius).abs() < 0.08 * radius && subject);
                }
            }
        }
        let fraction = observed.iter().filter(|o| **o).count() as f32 / observed.len() as f32;
        let regions = classify(&reference, Some(&observed), &MaskParams::default().scaled_to(width));
        views.push(TruthView { name: view_name, cam, reference, inner: regions.clone(), regions, observed, observed_fraction: fraction });
    }
    // Rooms: only views the phone has mostly looked at.
    if sc.region.is_none() {
        views.retain(|v| v.observed_fraction >= 0.6 || v.name.starts_with("probe"));
    }
    views
}

#[derive(Clone, Copy, PartialEq)]
enum Draw {
    /// The viewer's normalised blending: how a capture session is shown.
    Viewer,
    /// Sorted blending with the plain dilation: how the viewer shows an
    /// imported splat file.
    Imported,
}

fn draw(splats: &[Splat], cam: &Camera, mode: Draw) -> (Image, Vec<f32>) {
    let opts = match mode {
        Draw::Viewer => WblendOptions::normalized_soft(),
        Draw::Imported => WblendOptions::default(),
    };
    let rendered = wblend::render(&[Layer { splats, weight: 1.0 }], cam, &opts);
    (rendered.over(BACKGROUND), rendered.alpha)
}

/// Scores of a cloud: with the project's usual instrument, and restricted
/// to the inner mask.
struct Scores {
    all: Summary,
    inner: Summary,
    /// Inner PSNR at the poses the phone took and a step beside them
    /// (`--probe`).
    probes: [f32; 2],
    pictures: Vec<Image>,
    alphas: Vec<Vec<f32>>,
}

/// Scores a cloud (scene coordinates) on the ground-truth views.
fn score(views: &[TruthView], splats: &[Splat], mode: Draw) -> Scores {
    let (mut all, mut inner, mut pictures, mut alphas) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut probes = [Vec::new(), Vec::new()];
    for view in views {
        let (image, alpha) = draw(splats, &view.cam, mode);
        let inner_score = metrics::score(&view.reference, &image, &alpha, &view.inner);
        if view.name.starts_with("probe-train") {
            probes[0].push(inner_score);
        } else if view.name.starts_with("probe-near") {
            probes[1].push(inner_score);
        } else {
            all.push(metrics::score(&view.reference, &image, &alpha, &view.regions));
            inner.push(inner_score);
        }
        pictures.push(image);
        alphas.push(alpha);
    }
    let probes = probes.map(|p| Summary::of(&p).psnr_observed);
    Scores { all: Summary::of(&all), inner: Summary::of(&inner), probes, pictures, alphas }
}

/// Restricts the comparison to pixels the phone saw and the fused cloud
/// closed, a few pixels away from every hole: the holes of a capture are
/// the same before and after, and their ragged rims otherwise drown the
/// differences in the rest of the picture.
fn inner_regions(views: &mut [TruthView], fusion_alpha: &[Vec<f32>]) {
    for (view, alpha) in views.iter_mut().zip(fusion_alpha) {
        let (w, h) = (view.cam.width as usize, view.cam.height as usize);
        let hole: Vec<bool> = (0..w * h).map(|i| !view.observed[i] || (view.reference.depth[i] > 0.0 && alpha[i] < 0.9)).collect();
        let near_hole = metrics::dilate(&hole, w, h, (w / 100).max(2));
        let inner: Vec<bool> = near_hole.iter().map(|n| !n).collect();
        view.inner = classify(&view.reference, Some(&inner), &MaskParams::default().scaled_to(w as u32));
    }
}

fn row(label: &str, iteration: u32, wall: f64, cpu: f64, gaussians: usize, s: &Scores, holdout: Option<f32>) -> String {
    format!(
        "| {label} | {iteration} | {wall:.0} | {cpu:.0} | {gaussians} | {:.2} | {:.2} | {:.3} | {:.1} | {:.2} | {:.2} | {:.2} | {:.2} | {} | {:.2} | {:.2} |",
        s.all.psnr,
        s.all.psnr_observed,
        s.all.ssim,
        100.0 * s.all.coverage_observed,
        s.inner.psnr_observed,
        s.inner.psnr_region[INTERIOR],
        s.inner.psnr_region[TEXTURE],
        s.inner.psnr_region[SILHOUETTE],
        holdout.map_or("-".to_string(), |p| format!("{p:.2}")),
        s.probes[0],
        s.probes[1],
    )
}

fn patch(config: &mut RefineConfig, assignments: &[&str]) {
    let mut value = serde_json::to_value(&*config).expect("config");
    for assignment in assignments {
        let (path, text) = assignment.split_once('=').unwrap_or_else(|| panic!("--set {assignment}: expected path=value"));
        let new: serde_json::Value = serde_json::from_str(text).unwrap_or_else(|_| serde_json::Value::String(text.to_string()));
        let mut at = &mut value;
        for key in path.split('.') {
            at = at.get_mut(key).unwrap_or_else(|| panic!("--set {path}: no field {key}"));
        }
        *at = new;
    }
    *config = serde_json::from_value(value).expect("patched config");
}

fn load() -> String {
    std::fs::read_to_string("/proc/loadavg").map(|s| s.split_whitespace().take(3).collect::<Vec<_>>().join(" ")).unwrap_or_default()
}

/// Time per iteration at fixed sizes and thread counts, with where it goes.
/// `--bench-widths 320,480,640,960`, `--bench-threads 1,4`, `--bench-steps 30`.
fn bench(capture: &Path, config: &RefineConfig, args: &Args) {
    let list = |name: &str, default: &str| -> Vec<u32> { args.text(name).unwrap_or(default).split(',').filter_map(|v| v.parse().ok()).collect() };
    let steps = args.number::<u32>("--bench-steps").unwrap_or(30);
    println!("\n### time per iteration, {}", capture.file_name().unwrap().to_string_lossy());
    println!("| width | threads | Gaussians | pairs | wall ms | cpu ms | view | project | bin | sort | composite | loss | of which blurs | backward pixels | reduce | chain | update | load |");
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|");
    for width in list("--bench-widths", "320,480,640,960") {
        for threads in list("--bench-threads", "1,4") {
            let mut config = config.clone();
            config.threads = threads as usize;
            config.train.width = width;
            config.train.levels = vec![gs_refine::train::Level { until: 1.0, scale: 1.0 }];
            config.train.densify.every = 0;
            config.train.iterations = 1_000_000;
            config.evals = 0;
            config.eval_views = 0;
            let mut job = RefineJob::prepare(capture, config, &CancelToken::default(), &mut |_| {}).expect("prepare");
            // A few steps first: buffers allocated, pool awake.
            job.run_to(4, &CancelToken::default(), &mut |_| {}).expect("run");
            let (before, t0, r0) = (job.report(), job.trainer.timing, job.trainer.raster.timing);
            job.run_to(4 + steps, &CancelToken::default(), &mut |_| {}).expect("run");
            let (after, t1, r1) = (job.report(), job.trainer.timing, job.trainer.raster.timing);
            let ms = |a: f64, b: f64| 1000.0 * (b - a) / steps as f64;
            println!(
                "| {width} | {threads} | {} | {} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} | {} |",
                job.trainer.model.len(),
                job.trainer.raster.pairs(),
                ms(before.wall_s, after.wall_s),
                ms(before.cpu_s, after.cpu_s),
                ms(t0.view, t1.view),
                ms(r0.project, r1.project),
                ms(r0.bin, r1.bin),
                ms(r0.sort, r1.sort),
                ms(r0.composite, r1.composite),
                ms(t0.loss, t1.loss),
                ms(t0.loss_blurs, t1.loss_blurs),
                ms(r0.backward_pixels, r1.backward_pixels),
                ms(r0.reduce, r1.reduce),
                ms(r0.chain, r1.chain),
                ms(t0.update, t1.update),
                load()
            );
        }
    }
}

/// Colour, mean depth and coverage of a cloud (scene coordinates) drawn the
/// way the optimiser and the viewer of an imported file draw it.
fn sorted_picture(splats: &[Splat], cam: &Camera) -> (Image, Vec<f32>, Vec<f32>) {
    let model = Gaussians::from_splats(splats, 1.0, 1.0, 0.0);
    let mut out = Rendered::default();
    CpuRasterizer::default().forward(&model, &View::from_camera(cam, BACKGROUND.map(|v| v as f32 / 255.0)), &mut out);
    let depth = out.depth.iter().zip(&out.alpha).map(|(d, a)| if *a > 1e-3 { d / a } else { 0.0 }).collect();
    (out.to_image(), depth, out.alpha)
}

/// Share of the pixels with a true surface where the cloud is closed
/// (coverage of a half or more) and its mean depth is more than 10 % short
/// of the truth, and the mean shortfall over those pixels: what hangs in the
/// air in front of the scene.
fn in_front(depth: &[f32], alpha: &[f32], truth: &RgbdFrame) -> (f32, f32) {
    let (mut surface, mut front, mut shortfall) = (0usize, 0usize, 0.0f64);
    for i in 0..depth.len() {
        let z = truth.depth[i];
        if z <= 0.0 {
            continue;
        }
        surface += 1;
        if alpha[i] >= 0.5 && depth[i] < 0.9 * z {
            front += 1;
            shortfall += (1.0 - depth[i] / z) as f64;
        }
    }
    (100.0 * front as f32 / surface.max(1) as f32, (shortfall / front.max(1) as f64) as f32)
}

/// Floaters: viewpoints the phone was never near (ceilings, corners, the
/// floor, low and high in the room), scored against the true depth, and
/// viewpoints outside the room (from above with the ceiling cut away, from
/// behind a wall) as pictures only. Three clouds each: the fusion as the
/// viewer shows it, the optimiser's start, the refined result.
#[allow(clippy::too_many_arguments)]
fn floaters(name: &str, sc: &Scenario, out: &Path, tag: &str, fused: &[Splat], start: &[Splat], refined: &[Splat], views: &[TruthView], fusion: &Scores, width: u32) {
    let height = width * 3 / 4;
    println!("\n### floaters, {tag}");
    if sc.region.is_some() {
        // Subjects: coverage where the truth shows nothing at all.
        let ghost = |alphas: &dyn Fn(usize) -> Vec<f32>| {
            let (mut sum, mut n) = (0.0f64, 0usize);
            for (i, view) in views.iter().enumerate() {
                let alpha = alphas(i);
                for (k, a) in alpha.iter().enumerate() {
                    if view.reference.depth[k] <= 0.0 && view.observed[k] {
                        sum += *a as f64;
                        n += 1;
                    }
                }
            }
            100.0 * sum / n.max(1) as f64
        };
        println!(
            "mean coverage where the truth is empty, %: fusion (viewer) {:.3}, start {:.3}, refined {:.3}",
            ghost(&|i| fusion.alphas[i].clone()),
            ghost(&|i| sorted_picture(start, &views[i].cam).2),
            ghost(&|i| sorted_picture(refined, &views[i].cam).2)
        );
    }
    // Viewpoints with a ground truth.
    let (lo, hi) = match name {
        "bedroom" => ([-5.5f32, 0.2], [-1.0f32, 4.0]),
        "living" => ([-5.5, -4.0], [0.5, 0.2]),
        _ => ([-5.5, -4.0], [5.5, 4.0]),
    };
    let center = Vec3::new(0.5 * (lo[0] + hi[0]), 1.2, 0.5 * (lo[1] + hi[1]));
    let mut cams: Vec<(String, Camera, bool)> = Vec::new();
    if sc.region.is_none() {
        for v in house_look_around_views() {
            if name == "house" || v.name.starts_with(name) {
                cams.push((v.name, v.cam, true));
            }
        }
        // Low in one corner and high in the opposite one, looking across.
        let low = Vec3::new(lo[0] + 0.7, 0.3, lo[1] + 0.7);
        let high = Vec3::new(hi[0] - 0.7, 2.25, hi[1] - 0.7);
        cams.push(("low_corner".into(), Camera::look_at(low, center, Vec3::Y, 75f32.to_radians(), 640, 480), true));
        cams.push(("high_corner".into(), Camera::look_at(high, Vec3::new(center.x, 0.6, center.z), Vec3::Y, 75f32.to_radians(), 640, 480), true));
        // From behind the wall and from above: no truth to compare with.
        let behind = Vec3::new(lo[0] - 0.5, 1.5, center.z);
        cams.push(("behind_wall".into(), Camera::look_at(behind, center, Vec3::Y, 75f32.to_radians(), 640, 480), false));
        cams.push(("from_above".into(), Camera::look_at(Vec3::new(center.x, 7.5, center.z), Vec3::new(center.x, 0.0, center.z), Vec3::NEG_Z, 55f32.to_radians(), 640, 480), false));
    } else {
        for (k, (eye, target)) in [(Vec3::new(0.4, 4.6, 0.7), Vec3::new(0.0, 0.6, 0.0)), (Vec3::new(3.4, 0.35, 0.6), DANCER), (Vec3::new(-0.9, 1.5, -0.9), DANCER)].into_iter().enumerate() {
            cams.push((format!("far{k}"), Camera::look_at(eye, target, Vec3::Y, 50f32.to_radians(), 640, 480), false));
        }
    }
    println!("| view | fusion (viewer) in front % | start in front % | refined in front % | refined mean shortfall | refined coverage % |");
    println!("|---|---|---|---|---|---|");
    let (mut sums, mut scored) = ([0.0f32; 3], 0);
    for (view_name, cam, has_truth) in cams {
        let cam = cam.resized(width, height);
        // The room without its ceiling for the view from above.
        let cut = |splats: &[Splat]| -> Vec<Splat> { splats.iter().filter(|s| view_name != "from_above" || s.pos[1] < 2.2).copied().collect() };
        let (fused_cut, start_cut, refined_cut) = (cut(fused), cut(start), cut(refined));
        let shown = wblend::render(&[Layer { splats: &fused_cut, weight: 1.0 }], &cam, &WblendOptions::normalized_soft());
        let (start_image, start_depth, start_alpha) = sorted_picture(&start_cut, &cam);
        let (refined_image, refined_depth, refined_alpha) = sorted_picture(&refined_cut, &cam);
        let mut panels = Vec::new();
        if has_truth {
            let truth = render_reference(&sc.snap, &cam, true, 2, BACKGROUND);
            // The viewer's blending has no mean depth: its own depth is
            // the visible surface, which cannot float. Scored on coverage
            // and the sorted clouds only.
            let fusion_sorted = sorted_picture(&fused_cut, &cam);
            let (a, b, c) = (in_front(&fusion_sorted.1, &fusion_sorted.2, &truth), in_front(&start_depth, &start_alpha, &truth), in_front(&refined_depth, &refined_alpha, &truth));
            let coverage = 100.0 * refined_alpha.iter().filter(|a| **a >= 0.5).count() as f32 / refined_alpha.len() as f32;
            println!("| {view_name} | {:.2} | {:.2} | {:.2} | {:.3} | {coverage:.1} |", a.0, b.0, c.0, c.1);
            sums = [sums[0] + a.0, sums[1] + b.0, sums[2] + c.0];
            scored += 1;
            panels.push(("ground truth".to_string(), Image::from_frame(&truth)));
        }
        panels.push(("fusion (viewer)".to_string(), shown.over(BACKGROUND)));
        panels.push(("start (sorted)".to_string(), start_image));
        panels.push(("refined".to_string(), refined_image));
        let labelled: Vec<Image> = panels.iter().map(|(text, image)| sheet::labelled(image, text, 2)).collect();
        save_png(&out.join(format!("floaters/{tag}_{view_name}.png")), &sheet::beside(&labelled, 6, [255, 255, 255]));
    }
    if scored > 0 {
        let n = scored as f32;
        println!("| mean of {scored} | {:.2} | {:.2} | {:.2} | | |", sums[0] / n, sums[1] / n, sums[2] / n);
    }
}

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let (Some(name), Some(out)) = (raw.first().cloned(), raw.get(1).map(PathBuf::from)) else {
        eprintln!("usage: refine_lab <bedroom|house|object|person> <out dir> [options], see the source");
        std::process::exit(2);
    };
    let args = Args(raw);
    std::fs::create_dir_all(&out).expect("cannot create the output directory");

    let mut sim = if args.flag("--realistic") { PhoneSim::realistic() } else { PhoneSim::default() };
    if let Some(width) = args.number::<u32>("--color-width") {
        (sim.color_width, sim.color_height) = (width, width * 3 / 4);
    }
    sim.exposure_drift = args.number("--exposure").unwrap_or(sim.exposure_drift);
    sim.depth_noise = args.number("--depth-noise").unwrap_or(sim.depth_noise);
    sim.pose_jitter = args.number("--pose-jitter").unwrap_or(sim.pose_jitter);
    sim.pose_drift = args.number("--pose-drift").unwrap_or(sim.pose_drift);
    sim.seed = args.number("--seed").unwrap_or(sim.seed);
    let stage_time = args.number("--stage-time").unwrap_or(1.0);
    let sc = scenario(&name, &sim, args.number("--frames"), stage_time);
    let mut tag = format!("{name}_{}f_{}w_e{}_d{}_j{}_r{}", sc.frames, sim.color_width, sim.exposure_drift, sim.depth_noise, sim.pose_jitter, sim.pose_drift);
    if sim.seed != 1 {
        tag.push_str(&format!("_s{}", sim.seed));
    }
    if stage_time != 1.0 {
        tag.push_str(&format!("_t{stage_time}"));
    }
    // `--depth-hole R`: no depth within R meters of the point the first
    // ground-truth view looks at.
    let hole = args.number::<f32>("--depth-hole").map(|radius| {
        let cam = truth_views(&name, &sc, 64, 5.0, false).first().expect("a ground-truth view").cam.resized(3, 3);
        let depth = render_rgbd(&sc.snap, &cam, &CaptureOptions { shadows: false, min_range: 0.05, max_range: 50.0, ..CaptureOptions::default() }).depth[4];
        tag.push_str(&format!("_h{radius}"));
        (cam.unproject(1.5, 1.5, depth), radius)
    });
    let capture_path = out.join(format!("{tag}.gscap"));
    let started = Instant::now();
    let world = write_capture(&capture_path, &sc, sim, hole);
    eprintln!("capture {} ({:.0} s)", capture_path.display(), started.elapsed().as_secs_f64());

    let mut config = RefineConfig::preset(args.text("--preset").map_or(Preset::Rapide, |p| Preset::from_name(p).expect("unknown preset")));
    patch(&mut config, &args.all("--set"));
    let label = args.text("--label").unwrap_or("refined").to_string();

    let cancel = CancelToken::default();
    if args.flag("--bench") {
        bench(&capture_path, &config, &args);
        return;
    }
    let started = Instant::now();
    let mut job = RefineJob::prepare(&capture_path, config.clone(), &cancel, &mut |_| {}).expect("prepare");
    eprintln!(
        "prepared in {:.0} s: {} training views, {} held out, {} fused splats at {:.1} mm cells, {} threads",
        started.elapsed().as_secs_f64(),
        job.info.selection.train.len(),
        job.info.selection.holdout.len(),
        job.info.fused_splats,
        job.info.voxel_size * 1000.0,
        job.threads()
    );
    let floor_y = job.info.floor_y;

    let eval_width = args.number("--eval-width").unwrap_or(1280);
    let far = if sc.region.is_some() { 4.0 } else { 5.0 };
    let mut views = truth_views(&name, &sc, eval_width, far, args.flag("--probe"));
    eprintln!("{} ground-truth views at {eval_width}x{}", views.len(), eval_width * 3 / 4);

    println!("\n### {tag}, {label}: {}", args.0[2..].join(" "));
    println!("load at start {}", load());
    println!("| cloud | iteration | wall s | cpu s | Gaussians | PSNR | PSNR seen | SSIM | coverage % | inner PSNR | inner interior | inner texture | inner silhouette | held-out photo PSNR | at phone poses | 27 cm beside |");
    println!("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|");
    let fused_scene = world.to_scene(job.fused(), floor_y);
    let alphas = score(&views, &fused_scene, Draw::Viewer).alphas;
    inner_regions(&mut views, &alphas);
    let fusion = score(&views, &fused_scene, Draw::Viewer);
    if args.flag("--fusion-sorted") {
        // The same cloud as the optimiser first sees it.
        let grown: Vec<Splat> = fused_scene.iter().map(|s| Splat { scale: s.scale.map(|v| v * config.init.splat_scale), ..*s }).collect();
        println!("{}", row("fusion (sorted, scaled)", 0, 0.0, 0.0, grown.len(), &score(&views, &grown, Draw::Imported), None));
    }
    let fusion_holdout = job.evaluate_fusion().expect("evaluation").map(|s| s.psnr);
    println!("{}", row("fusion (viewer)", 0, 0.0, 0.0, fused_scene.len(), &fusion, fusion_holdout));

    let mut eval_at: Vec<u32> = args.text("--eval-at").map(|t| t.split(',').filter_map(|v| v.parse().ok()).collect()).unwrap_or_default();
    // The job has settled how many iterations an automatic preset runs.
    let iterations = job.config.train.iterations;
    eval_at.retain(|&at| at < iterations);
    eval_at.push(iterations);
    eval_at.sort_unstable();
    eval_at.dedup();
    let mut last_pictures = Vec::new();
    let mut last_psnr = fusion.inner.psnr_observed;
    // The cloud the optimiser starts from, in scene coordinates.
    let start_scene = world.to_scene(&job.trainer.model.to_splats(), floor_y);
    for &stop in &eval_at {
        if stop > 0 {
            job.run_to(stop, &cancel, &mut |_| {}).expect("run");
        }
        let report = job.report();
        let splats = world.to_scene(&job.trainer.model.to_splats(), floor_y);
        let summary = score(&views, &splats, Draw::Imported);
        let holdout = report.evals.iter().rev().find(|e| e.iteration == job.trainer.iteration).map(|e| e.score.psnr);
        let name = if stop == 0 { "start" } else { label.as_str() };
        println!("{}", row(name, job.trainer.iteration, report.wall_s, report.cpu_s, splats.len(), &summary, holdout));
        last_psnr = summary.inner.psnr_observed;
        last_pictures = summary.pictures;
    }
    let report = job.report();
    println!(
        "load at end {}; {:.0} ms per iteration wall, {:.0} ms cpu, {} threads",
        load(),
        1000.0 * report.wall_s / report.iterations_done.max(1) as f64,
        1000.0 * report.cpu_s / report.iterations_done.max(1) as f64,
        report.threads,
    );
    std::fs::write(out.join(format!("{tag}_{label}.json")), serde_json::to_string_pretty(&report).expect("report")).expect("write report");
    if args.flag("--ply") {
        std::fs::write(out.join(format!("{tag}_{label}.ply")), gs_core::io::write_ply(&job.splats())).expect("write ply");
    }

    if args.flag("--train-photos") {
        // The classic check for memorising: the same score on photographs
        // the optimiser trained on and on those it never saw.
        let n = job.trainer.views.len();
        let some: Vec<_> = (0..12.min(n)).map(|k| job.trainer.views[k * n / 12.min(n)]).collect();
        let train = job.evaluate_views(&job.trainer.model, &some).expect("evaluation");
        let held = job.evaluate_views(&job.trainer.model, job.holdout()).expect("evaluation");
        if let (Some(train), Some(held)) = (train, held) {
            println!(
                "photographs, final state: training views {:.2} dB (SSIM {:.3}, {} views), held-out views {:.2} dB (SSIM {:.3}, {} views), gap {:.2} dB",
                train.psnr,
                train.ssim,
                train.views,
                held.psnr,
                held.ssim,
                held.views,
                train.psnr - held.psnr
            );
        }
    }

    if args.flag("--floaters") {
        let refined_scene = world.to_scene(&job.trainer.model.to_splats(), floor_y);
        floaters(&name, &sc, &out, &format!("{tag}_{label}"), &fused_scene, &start_scene, &refined_scene, &views, &fusion, eval_width.min(640));
    }

    if args.flag("--exposure-stats") {
        // The learned corrections next to what the simulated phone did.
        let e = sim.exposure_drift;
        let (mut mean_gain, mut mean_truth) = ([0.0f32; 3], [0.0f32; 3]);
        let n = job.trainer.views.len();
        for (k, (view, state)) in job.trainer.views.iter().zip(&job.trainer.view_state).enumerate() {
            let t = view.frame as f32 / 10.0;
            let exposure = 1.0 + e * (0.9 * t).sin();
            let warmth = e / 3.0 * (0.37 * t + 1.0).sin();
            let truth = [exposure * (1.0 + warmth), exposure, exposure * (1.0 - warmth)];
            for c in 0..3 {
                mean_gain[c] += state.correction.gain[c] / n as f32;
                mean_truth[c] += truth[c] / n as f32;
            }
            if k % (n / 8).max(1) == 0 {
                println!("view {:3}: gain {:.3?} offset {:.3?}, the phone's gain {:.3?}", view.frame, state.correction.gain, state.correction.offset, truth);
            }
        }
        println!("mean learned gain {mean_gain:.3?}, mean true gain {mean_truth:.3?}");
    }

    if args.flag("--shape-stats") {
        // What the optimiser did to the surfels it was given.
        let percentiles = |mut v: Vec<f32>| {
            v.sort_by(f32::total_cmp);
            let at = |q: f32| v[((v.len() - 1) as f32 * q) as usize];
            format!("p10 {:.4} p50 {:.4} p90 {:.4} p99 {:.4}", at(0.1), at(0.5), at(0.9), at(0.99))
        };
        let model = job.trainer.model.to_splats();
        let sorted = |s: &Splat| {
            let mut v = s.scale;
            v.sort_by(f32::total_cmp);
            v
        };
        println!("sigma min  (m): {}", percentiles(model.iter().map(|s| sorted(s)[0]).collect()));
        println!("sigma mid  (m): {}", percentiles(model.iter().map(|s| sorted(s)[1]).collect()));
        println!("sigma max  (m): {}", percentiles(model.iter().map(|s| sorted(s)[2]).collect()));
        println!("opacity       : {}", percentiles(model.iter().map(|s| s.rgba[3] as f32 / 255.0).collect()));
        if model.len() == job.fused().len() {
            let normal = |s: &Splat| gs_core::splat::surfel_normal(s);
            let (mut along, mut across, mut tilt) = (Vec::new(), Vec::new(), Vec::new());
            for (now, before) in model.iter().zip(job.fused()) {
                let d = now.position() - before.position();
                let n = normal(before);
                along.push(d.dot(n).abs());
                across.push((d - n * d.dot(n)).length());
                tilt.push(normal(now).dot(n).abs().clamp(0.0, 1.0).acos().to_degrees());
            }
            println!("moved along the normal (m): {}", percentiles(along));
            println!("moved in the surface   (m): {}", percentiles(across));
            println!("normal turned (degrees)   : {}", percentiles(tilt));
        }
    }

    if args.flag("--shift-probe") {
        // Is the cloud registered with the ground truth? The translation of
        // the picture that fits best, per view, and what it gains.
        for (i, view) in views.iter().enumerate() {
            let (w, h) = (view.cam.width as i64, view.cam.height as i64);
            let mse = |image: &Image, dx: i64, dy: i64| {
                let (mut sum, mut n) = (0.0f64, 0u64);
                for y in 8..h - 8 {
                    for x in 8..w - 8 {
                        let at = (y * w + x) as usize;
                        if view.inner[at] == metrics::UNOBSERVED as u8 {
                            continue;
                        }
                        let from = ((y + dy) * w + x + dx) as usize;
                        for c in 0..3 {
                            let d = image.rgb[from * 3 + c] as f64 - view.reference.rgb[at * 3 + c] as f64;
                            sum += d * d;
                        }
                        n += 3;
                    }
                }
                sum / n.max(1) as f64
            };
            for (what, image) in [("fusion", &fusion.pictures[i]), ("refined", &last_pictures[i])] {
                let mut best = (f64::INFINITY, 0, 0);
                for dy in -5..=5 {
                    for dx in -5..=5 {
                        let m = mse(image, dx, dy);
                        if m < best.0 {
                            best = (m, dx, dy);
                        }
                    }
                }
                let psnr = |m: f64| 10.0 * (255.0f64 * 255.0 / m).log10();
                println!("shift {} {what}: {:.2} dB as is, {:.2} dB moved by ({}, {})", view.name, psnr(mse(image, 0, 0)), psnr(best.0), best.1, best.2);
            }
        }
    }

    if args.flag("--sheets") {
        for (i, view) in views.iter().enumerate() {
            let panels = [
                ("ground truth".to_string(), Image::from_frame(&view.reference)),
                (format!("fusion (viewer)  {:.2} dB", fusion.inner.psnr_observed), fusion.pictures[i].clone()),
                (format!("{label}  {:.2} dB", last_psnr), last_pictures[i].clone()),
            ];
            let labelled: Vec<Image> = panels.iter().map(|(text, image)| sheet::labelled(image, text, 3)).collect();
            let mut rows = vec![sheet::beside(&labelled, 6, [255, 255, 255])];
            if args.flag("--diff") {
                // Four times the absolute difference to the ground truth;
                // what the inner mask leaves out is dimmed.
                let difference = |image: &Image| {
                    let mut out = image.clone();
                    for (i, pixel) in out.rgb.chunks_exact_mut(3).enumerate() {
                        let dim = if view.inner[i] == metrics::UNOBSERVED as u8 { 4 } else { 1 };
                        for (c, value) in pixel.iter_mut().enumerate() {
                            *value = ((value.abs_diff(view.reference.rgb[i * 3 + c]) as u32 * 4).min(255) / dim) as u8;
                        }
                    }
                    out
                };
                let diffs = [sheet::labelled(&panels[0].1, "ground truth", 3), sheet::labelled(&difference(&panels[1].1), "fusion, 4 x difference", 3), sheet::labelled(&difference(&panels[2].1), "refined, 4 x difference", 3)];
                rows.push(sheet::beside(&diffs, 6, [255, 255, 255]));
            }
            // Two magnified crops: the middle of the view and its lower left.
            let (w, h) = (view.cam.width, view.cam.height);
            for (x, y) in [(w * 3 / 8, h * 3 / 8), (w / 8, h / 2)] {
                let crops: Vec<Image> = panels.iter().map(|(text, image)| sheet::labelled(&sheet::magnify(&sheet::crop(image, x, y, w / 4, h / 4), 4), text, 2)).collect();
                rows.push(sheet::beside(&crops, 6, [255, 255, 255]));
            }
            let sheet_image = sheet::stacked(&rows, 8, [255, 255, 255]);
            save_png(&out.join(format!("sheets/{tag}_{label}_{}.png", view.name)), &sheet::shrink(&sheet_image, 2));
        }
    }
}
