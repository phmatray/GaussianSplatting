//! Evaluation harness of the quality lab: evaluation views with cached
//! references, scoring, Markdown rows, PNG output.
//!
//! Shared with `examples/quality_report.rs`, which uses a part of it.

#![allow(dead_code)]

use std::path::PathBuf;
use std::time::Instant;

use gs_core::capture::render_reference;
use gs_core::experimental::lab::{observed_mask, ScanExposures};
use gs_core::experimental::sheet;
use gs_core::experimental::metrics::{classify, score, MaskParams, Score, Summary, INTERIOR, SILHOUETTE, TEXTURE, UNOBSERVED};
use gs_core::experimental::wblend::{render, FragmentStats, Layer, WblendOptions};
use gs_core::glam::{Quat, Vec3};
use gs_core::raster::Image;
use gs_core::scene::stage::StageScene;
use gs_core::scene::SceneSnapshot;
use gs_core::{Camera, RgbdFrame};

use crate::common::{self, BACKGROUND};

/// Evaluation resolutions; the last one is the one the owner looks at.
pub const RESOLUTIONS: [(u32, u32); 3] = [(320, 240), (640, 480), (1280, 960)];
/// The resolution that counts.
pub const FULL: (u32, u32) = (1280, 960);

/// What an evaluation view is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// One of the existing held-out views of the novel-view tests.
    Held,
    /// A close-up.
    Close,
    /// A view along a surface.
    Graze,
}

/// A viewpoint, at the base resolution of 320x240.
#[derive(Clone)]
pub struct Viewpoint {
    pub name: String,
    pub kind: Kind,
    pub cam: Camera,
}

/// A viewpoint at one resolution with everything needed to score a render.
pub struct EvalView {
    pub name: String,
    pub kind: Kind,
    pub cam: Camera,
    pub reference: RgbdFrame,
    /// Regions with bands of constant width on the scene (2 px at 320 wide).
    pub regions: Vec<u8>,
    /// Regions with bands of 2 px whatever the resolution.
    pub regions_2px: Vec<u8>,
}

/// Output directory, cache and switches.
pub struct Lab {
    pub out: PathBuf,
    /// Skip the most expensive variants.
    pub quick: bool,
    /// Run only this part of the experiment (`--only stage`).
    only: Option<String>,
    started: Instant,
}

impl Lab {
    pub fn new(out: PathBuf, quick: bool, only: Option<String>) -> Self {
        std::fs::create_dir_all(out.join("cache")).expect("cannot create the output directory");
        Self { out, quick, only, started: Instant::now() }
    }

    /// Should the part `name` of the experiment run?
    pub fn wants(&self, name: &str) -> bool {
        self.only.as_deref().is_none_or(|only| only == name)
    }

    /// Progress note on standard error (tables go to standard output).
    pub fn note(&self, text: &str) {
        eprintln!("[{:7.1} s] {text}", self.started.elapsed().as_secs_f64());
    }

    fn cache_file(&self, key: &str) -> PathBuf {
        self.out.join("cache").join(format!("{key}.bin"))
    }

    /// The ray-cast ground truth of a view, from the cache when present.
    pub fn reference(&self, key: &str, snap: &SceneSnapshot, cam: &Camera, shadows: bool) -> RgbdFrame {
        let path = self.cache_file(&format!("ref_{key}_{}x{}", cam.width, cam.height));
        let n = cam.width as usize * cam.height as usize;
        if let Ok(bytes) = std::fs::read(&path) {
            if bytes.len() == n * 7 {
                let depth = bytes[n * 3..].chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
                return RgbdFrame { width: cam.width, height: cam.height, rgb: bytes[..n * 3].to_vec(), depth };
            }
        }
        let frame = render_reference(snap, cam, shadows, 2, BACKGROUND);
        let mut bytes = frame.rgb.clone();
        bytes.extend(frame.depth.iter().flat_map(|z| z.to_le_bytes()));
        std::fs::write(&path, bytes).expect("cannot write the cache");
        frame
    }

    /// The observation mask of a house view, from the cache when present.
    pub fn observed(&self, key: &str, snap: &SceneSnapshot, exposures: &ScanExposures, cam: &Camera) -> Vec<bool> {
        let path = self.cache_file(&format!("obs_{key}_{}x{}", cam.width, cam.height));
        let n = cam.width as usize * cam.height as usize;
        if let Ok(bytes) = std::fs::read(&path) {
            if bytes.len() == n {
                return bytes.iter().map(|&b| b != 0).collect();
            }
        }
        let mask = observed_mask(snap, exposures, cam);
        std::fs::write(&path, mask.iter().map(|&m| u8::from(m)).collect::<Vec<u8>>()).expect("cannot write the cache");
        mask
    }

    /// Builds the evaluation view of a viewpoint at a resolution.
    pub fn eval_view(
        &self,
        key: &str,
        point: &Viewpoint,
        resolution: (u32, u32),
        snap: &SceneSnapshot,
        shadows: bool,
        exposures: Option<&ScanExposures>,
    ) -> EvalView {
        let cam = point.cam.resized(resolution.0, resolution.1);
        let key = format!("{key}_{}", point.name);
        let reference = self.reference(&key, snap, &cam, shadows);
        let observed = exposures.map(|e| self.observed(&key, snap, e, &cam));
        let params = MaskParams::default();
        EvalView {
            name: point.name.clone(),
            kind: point.kind,
            regions: classify(&reference, observed.as_deref(), &params.scaled_to(cam.width)),
            regions_2px: classify(&reference, observed.as_deref(), &params),
            cam,
            reference,
        }
    }

    pub fn save(&self, name: &str, image: &Image) {
        common::save_png(&self.out.join(name), image);
        self.note(&format!("wrote {}", self.out.join(name).display()));
    }
}

fn viewpoint(name: &str, kind: Kind, eye: Vec3, target: Vec3, fov_deg: f32) -> Viewpoint {
    Viewpoint { name: name.into(), kind, cam: Camera::look_at(eye, target, Vec3::Y, fov_deg.to_radians(), 320, 240) }
}

/// The six held-out stage views of the novel-view tests, a close-up 0.6 m
/// from the dancer's upper body and a view grazing the stage floor.
pub fn stage_viewpoints(t: f32) -> Vec<Viewpoint> {
    let mut points: Vec<Viewpoint> =
        common::stage_views().into_iter().map(|v| Viewpoint { name: v.name, kind: Kind::Held, cam: v.cam }).collect();
    let joints = StageScene::new().dancer_joints(t);
    // 0.6 m from the chest, 25 degrees off the direction the dancer faces
    // (no rig camera is that close or at that height).
    let toward = (Quat::from_rotation_y(25f32.to_radians()) * joints.facing).normalize_or(Vec3::Z);
    let eye = joints.chest + toward * 0.6 + Vec3::Y * 0.12;
    points.push(viewpoint("near", Kind::Close, eye, 0.5 * (joints.chest + joints.neck), 50.0));
    // 20 cm above the stage level, looking across the checkered floor.
    let angle = 20f32.to_radians();
    points.push(viewpoint("graze", Kind::Graze, Vec3::new(3.0 * angle.sin(), 0.2, 3.0 * angle.cos()), Vec3::new(0.0, 0.3, 0.0), 50.0));
    points
}

/// The 24 held-out house views of the novel-view tests, a close look at the
/// bookshelf of the living room and a view along its south wall.
pub fn house_viewpoints() -> Vec<Viewpoint> {
    let mut points: Vec<Viewpoint> =
        common::house_views().into_iter().map(|v| Viewpoint { name: v.name, kind: Kind::Held, cam: v.cam }).collect();
    points.push(viewpoint("furniture", Kind::Close, Vec3::new(-1.3, 1.2, -2.95), Vec3::new(-0.95, 1.1, -3.85), 60.0));
    points.push(viewpoint("graze", Kind::Graze, Vec3::new(0.1, 1.5, -0.12), Vec3::new(-5.4, 1.3, 0.15), 60.0));
    points
}

/// One render of one view.
pub struct ViewResult {
    pub score: Score,
    pub image: Image,
    pub fragments: FragmentStats,
}

/// Renders `layers` from the view and scores the picture.
pub fn evaluate(view: &EvalView, layers: &[Layer], opts: &WblendOptions) -> ViewResult {
    let rendered = render(layers, &view.cam, opts);
    let image = rendered.over(BACKGROUND);
    ViewResult { score: score(&view.reference, &image, &rendered.alpha, &view.regions), image, fragments: rendered.stats }
}

/// The scores of a set of views, by kind.
pub struct SetResult {
    pub held: Summary,
    pub close: Vec<Score>,
    pub graze: Vec<Score>,
    /// Splat fragments per pixel: visibility pass, colour pass.
    pub overdraw: (f32, f32),
}

/// Renders and scores every view.
pub fn evaluate_set<'a>(views: &[EvalView], layers_of: &dyn Fn(usize) -> Vec<Layer<'a>>, opts: &WblendOptions) -> SetResult {
    let (mut held, mut close, mut graze) = (Vec::new(), Vec::new(), Vec::new());
    let (mut visibility, mut shaded, mut pixels) = (0u64, 0u64, 0u64);
    for (i, view) in views.iter().enumerate() {
        let result = evaluate(view, &layers_of(i), opts);
        visibility += result.fragments.visibility;
        shaded += result.fragments.shaded;
        pixels += view.cam.width as u64 * view.cam.height as u64;
        match view.kind {
            Kind::Held => held.push(result.score),
            Kind::Close => close.push(result.score),
            Kind::Graze => graze.push(result.score),
        }
    }
    let per_pixel = |count: u64| count as f32 / pixels.max(1) as f32;
    SetResult { held: Summary::of(&held), close, graze, overdraw: (per_pixel(visibility), per_pixel(shaded)) }
}

/// Mean PSNR and SSIM of a few scores as `dB / ssim`.
fn pair(scores: &[Score]) -> String {
    if scores.is_empty() {
        return "-".into();
    }
    let n = scores.len() as f32;
    format!("{:.2} / {:.3}", scores.iter().map(|s| s.psnr).sum::<f32>() / n, scores.iter().map(|s| s.ssim).sum::<f32>() / n)
}

/// Header of the standard result table.
pub fn table_header(first: &str) -> String {
    format!(
        "| {first} | res | splats | wire | PSNR | PSNR obs | SSIM | cover % | error share int/tex/sil/unobs % | PSNR int/tex/sil | HF | close dB/SSIM | graze dB/SSIM |\n\
         |---|---|---|---|---|---|---|---|---|---|---|---|---|"
    )
}

/// Human-readable byte count.
pub fn bytes(n: usize) -> String {
    if n >= 1_000_000 {
        format!("{:.2} MB", n as f64 / 1e6)
    } else {
        format!("{:.0} kB", n as f64 / 1e3)
    }
}

/// One row of the standard result table. `splats` is a free-form count
/// ("69k + 9k"), `wire` the bytes on the wire.
pub fn table_row(label: &str, resolution: (u32, u32), splats: &str, wire: &str, result: &SetResult) -> String {
    let h = &result.held;
    format!(
        "| {label} | {}x{} | {splats} | {wire} | {:.2} | {:.2} | {:.4} | {:.2} | {:.0} / {:.0} / {:.0} / {:.0} | {:.1} / {:.1} / {:.1} | {:.2} | {} | {} |",
        resolution.0,
        resolution.1,
        h.psnr,
        h.psnr_observed,
        h.ssim,
        100.0 * h.coverage,
        100.0 * h.share[INTERIOR],
        100.0 * h.share[TEXTURE],
        100.0 * h.share[SILHOUETTE],
        100.0 * h.share[UNOBSERVED],
        h.psnr_region[INTERIOR],
        h.psnr_region[TEXTURE],
        h.psnr_region[SILHOUETTE],
        h.hf_interior,
        pair(&result.close),
        pair(&result.graze),
    )
}

/// `count` as "69.1k".
pub fn thousands(count: usize) -> String {
    format!("{:.1}k", count as f64 / 1e3)
}

/// CPU seconds (user + system, all threads) this process has used so far,
/// from `/proc/self/stat`; 0 where that does not exist.
fn cpu_seconds() -> f64 {
    let Ok(stat) = std::fs::read_to_string("/proc/self/stat") else { return 0.0 };
    // Fields 14 and 15 after the parenthesised command name, in clock ticks
    // (100 per second on Linux).
    let rest = stat.rsplit(')').next().unwrap_or("");
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let ticks = |i: usize| fields.get(i).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
    (ticks(11) + ticks(12)) / 100.0
}

/// Cost of `f` on a machine that is shared with other work: runs it
/// repeatedly for at least `seconds` and returns the CPU milliseconds per
/// run (summed over all threads, independent of what else is running) and
/// the best wall-clock milliseconds of a single run.
pub fn cost_ms<T>(seconds: f64, mut f: impl FnMut() -> T) -> (f64, f64) {
    let (cpu_start, start) = (cpu_seconds(), Instant::now());
    let (mut runs, mut best) = (0u32, f64::INFINITY);
    while runs < 5 || start.elapsed().as_secs_f64() < seconds {
        let begin = Instant::now();
        std::hint::black_box(f());
        best = best.min(begin.elapsed().as_secs_f64() * 1e3);
        runs += 1;
    }
    ((cpu_seconds() - cpu_start) * 1e3 / runs as f64, best)
}

/// Runs `f` on a single thread (a one-thread rayon pool with the `par`
/// feature; the code is serial anyway without it).
pub fn single_threaded<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    #[cfg(feature = "par")]
    {
        rayon::ThreadPoolBuilder::new().num_threads(1).build().expect("cannot build a thread pool").install(f)
    }
    #[cfg(not(feature = "par"))]
    f()
}

/// Labelled panels side by side.
pub fn panel_row(panels: &[(String, Image)], label_scale: u32) -> Image {
    let labelled: Vec<Image> = panels.iter().map(|(text, image)| sheet::labelled(image, text, label_scale)).collect();
    sheet::beside(&labelled, 6, [255, 255, 255])
}

/// The same rectangle `(x, y, width, height)` of every panel, magnified and
/// labelled, side by side.
pub fn crop_row(panels: &[(String, Image)], rect: (u32, u32, u32, u32), factor: u32, label_scale: u32) -> Image {
    let crops: Vec<(String, Image)> = panels
        .iter()
        .map(|(text, image)| (text.clone(), sheet::magnify(&sheet::crop(image, rect.0, rect.1, rect.2, rect.3), factor)))
        .collect();
    panel_row(&crops, label_scale)
}

/// Absolute difference to the reference, times `gain`.
pub fn difference(reference: &Image, image: &Image, gain: u32) -> Image {
    let rgb = reference.rgb.iter().zip(&image.rgb).map(|(a, b)| (a.abs_diff(*b) as u32 * gain).min(255) as u8).collect();
    Image { width: image.width, height: image.height, rgb }
}

/// [`table_header`] with a column for the overdraw.
pub fn table_header_overdraw(first: &str) -> String {
    let base = table_header(first);
    let (titles, rule) = base.split_once('\n').unwrap();
    format!("{titles} fragments per pixel: visibility + colour |\n{rule}---|")
}

/// [`table_row`] with the overdraw of the render.
pub fn table_row_overdraw(label: &str, resolution: (u32, u32), splats: &str, wire: &str, result: &SetResult) -> String {
    format!("{} {:.1} + {:.1} |", table_row(label, resolution, splats, wire, result), result.overdraw.0, result.overdraw.1)
}
