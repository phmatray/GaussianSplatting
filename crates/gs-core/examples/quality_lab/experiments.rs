//! The experiments of the quality lab, one function per command.

use gs_core::experimental::lab::{
    ring_candidates, run_scan, stage_cloud, RingCapture, RingNormals, Scan, ScanExposures, Sensor, StageCloud,
};
use gs_core::experimental::metrics::{score, Summary, INTERIOR, SILHOUETTE, TEXTURE, UNOBSERVED};
use gs_core::experimental::multiview::{FusionParams, Selection};
use gs_core::experimental::upsample::Upsampling;
use gs_core::experimental::wblend::{
    render, Blend, Coverage, DepthModel, Footprint, Layer, LowPass, Normalized, Silhouette, WblendOptions,
};
use gs_core::pipeline::Pipeline;
use gs_core::protocol::{PipelineConfig, ScanConfig};
use gs_core::reconstruct::ReconstructParams;
use gs_core::rig::RingRig;
use gs_core::scene::{make_scene, Scene, SceneKind, SceneSnapshot};
use gs_core::Splat;

use crate::common::BACKGROUND;
use crate::harness::{
    bytes, evaluate_set, house_viewpoints, stage_viewpoints, table_header, table_header_overdraw, table_row, table_row_overdraw,
    thousands, EvalView, Kind, Lab, SetResult,
    FULL, RESOLUTIONS,
};

/// The two instants of the dance the stage is judged at.
pub const STAGE_TIMES: [f32; 2] = [1.3, 4.1];
/// Bytes of one splat on the wire.
const WIRE: usize = 16;

// ---------------------------------------------------------------------------
// Stage machinery
// ---------------------------------------------------------------------------

/// How the stage is captured and reconstructed in one experiment.
#[derive(Clone)]
pub struct StageRecipe {
    /// Multiplies the resolution of the ring cameras (colour grid).
    pub scale: u32,
    /// Overrides the image size of the ring cameras (`scale` is then only
    /// used by sensors with a coarser depth map).
    pub size: Option<(u32, u32)>,
    pub sensor: Sensor,
    pub recon: ReconstructParams,
    /// Use the simulator's normals instead of estimated ones.
    pub exact_normals: bool,
    /// Multi-view fusion of both layers (A1); `None` concatenates.
    pub fusion: Option<FusionParams>,
    /// Subset of the ring cameras; all when `None`.
    pub cameras: Option<Vec<usize>>,
}

impl Default for StageRecipe {
    fn default() -> Self {
        Self {
            scale: 1,
            size: None,
            sensor: Sensor::Native,
            recon: ReconstructParams::default(),
            exact_normals: false,
            fusion: None,
            cameras: None,
        }
    }
}

impl StageRecipe {
    pub fn rig(&self) -> RingRig {
        let rig = RingRig::default();
        let (width, height) = self.size.unwrap_or((rig.width * self.scale, rig.height * self.scale));
        RingRig { width, height, ..rig }
    }

    /// Captures and reconstructs the stage at time `t`.
    pub fn build(&self, scene: &dyn Scene, t: f32) -> (RingCapture, StageCloud) {
        let (empty, now) = (scene.snapshot_static(), scene.snapshot(t));
        let cameras = self.rig().cameras();
        let capture = RingCapture::new(&empty, &now, &cameras, self.sensor, true, self.recon.edge_threshold);
        let normals = self.exact_normals.then(|| RingNormals::new(&empty, &now, &cameras));
        let candidates = ring_candidates(&capture, &self.recon, normals.as_ref());
        let cloud = stage_cloud(&capture, &candidates, self.fusion.as_ref(), self.cameras.as_deref());
        (capture, cloud)
    }
}

/// The evaluation views of the stage at several instants, at one resolution.
pub struct StageViews {
    pub views: Vec<EvalView>,
    /// Index into the list of instants for every view.
    pub instant: Vec<usize>,
}

impl StageViews {
    pub fn new(lab: &Lab, scene: &dyn Scene, times: &[f32], resolution: (u32, u32)) -> Self {
        let (mut views, mut instant) = (Vec::new(), Vec::new());
        for (k, &t) in times.iter().enumerate() {
            let snap = scene.snapshot(t);
            for point in stage_viewpoints(t) {
                views.push(lab.eval_view(&format!("stage_t{t}"), &point, resolution, &snap, true, None));
                instant.push(k);
            }
        }
        Self { views, instant }
    }
}

/// How a cloud is drawn in one experiment.
#[derive(Clone, Copy)]
pub struct Renderer {
    pub opts: WblendOptions,
    /// Blending weight of the live layer relative to the static one.
    pub live_weight: f32,
}

impl Default for Renderer {
    /// Today's viewer.
    fn default() -> Self {
        Self { opts: WblendOptions::default(), live_weight: 1.0 }
    }
}

/// Scores stage clouds (one per instant) on the views.
pub fn score_stage(views: &StageViews, clouds: &[StageCloud], renderer: &Renderer) -> SetResult {
    let layers = |i: usize| {
        let cloud = &clouds[views.instant[i]];
        vec![Layer { splats: &cloud.fixed, weight: 1.0 }, Layer { splats: &cloud.live, weight: renderer.live_weight }]
    };
    evaluate_set(&views.views, &layers, &renderer.opts)
}

/// "69.1k + 9.2k" and the wire cost of stage clouds (mean over instants).
pub fn stage_budget(clouds: &[StageCloud]) -> (String, String) {
    let n = clouds.len().max(1);
    let fixed = clouds.iter().map(|c| c.fixed.len()).sum::<usize>() / n;
    let live = clouds.iter().map(|c| c.live.len()).sum::<usize>() / n;
    (
        format!("{} + {}", thousands(fixed), thousands(live)),
        format!("{} + {}/tick ({:.1} MB/s)", bytes(fixed * WIRE), bytes(live * WIRE), (live * WIRE * 30) as f64 / 1e6),
    )
}

/// Builds the clouds of a recipe at the standard instants.
pub fn stage_clouds(scene: &dyn Scene, recipe: &StageRecipe, times: &[f32]) -> Vec<StageCloud> {
    times.iter().map(|&t| recipe.build(scene, t).1).collect()
}

// ---------------------------------------------------------------------------
// House machinery
// ---------------------------------------------------------------------------

/// The house scene and the views a scan is judged from.
pub struct HouseBench {
    pub snap: SceneSnapshot,
    pub config: PipelineConfig,
    /// Today's scan (with its exposures: they define "observed").
    pub today: Scan,
}

impl HouseBench {
    pub fn new(lab: &Lab) -> Self {
        let snap = make_scene(SceneKind::House).snapshot(0.0);
        let config = PipelineConfig::for_scene(SceneKind::House);
        lab.note("scanning the house with today's settings");
        let today = run_scan(&snap, &config.scan, &config.recon, Sensor::Native, config.shadows, 1_000_000, true);
        Self { snap, config, today }
    }

    pub fn exposures(&self) -> &ScanExposures {
        self.today.exposures.as_ref().expect("today's scan keeps its exposures")
    }

    /// The evaluation views at one resolution: all of them, or every third
    /// held-out view plus the close-up and the grazing view.
    pub fn views(&self, lab: &Lab, resolution: (u32, u32), subset: bool) -> Vec<EvalView> {
        house_viewpoints()
            .iter()
            .enumerate()
            .filter(|(i, p)| !subset || p.kind != Kind::Held || i % 3 == 0)
            .map(|(_, p)| lab.eval_view("house", p, resolution, &self.snap, self.config.shadows, Some(self.exposures())))
            .collect()
    }

    /// A scan with another sensor resolution, voxel size or depth sensor.
    pub fn scan(&self, lab: &Lab, scale: u32, voxel: f32, sensor: Sensor) -> Scan {
        let base = self.config.scan;
        let scan = ScanConfig { width: base.width * scale, height: base.height * scale, voxel_size: voxel, ..base };
        lab.note(&format!("scanning the house at {}x{}, {:.1} cm cells, {sensor:?}", scan.width, scan.height, 100.0 * voxel));
        run_scan(&self.snap, &scan, &self.config.recon, sensor, self.config.shadows, 20_000_000, false)
    }
}

/// Scores a house cloud on the views.
pub fn score_house(views: &[EvalView], splats: &[Splat], renderer: &Renderer) -> SetResult {
    evaluate_set(views, &|_| vec![Layer { splats, weight: 1.0 }], &renderer.opts)
}

// ---------------------------------------------------------------------------
// Step 0: baseline
// ---------------------------------------------------------------------------

/// Region shares with bands of 2 px, for the baseline.
fn shares_2px<'a>(views: &[EvalView], layers_of: &dyn Fn(usize) -> Vec<Layer<'a>>) -> Summary {
    let scores: Vec<_> = views
        .iter()
        .enumerate()
        .filter(|(_, v)| v.kind == Kind::Held)
        .map(|(i, v)| {
            let rendered = render(&layers_of(i), &v.cam, &WblendOptions::default());
            score(&v.reference, &rendered.over(BACKGROUND), &rendered.alpha, &v.regions_2px)
        })
        .collect();
    Summary::of(&scores)
}

fn share_row(label: &str, resolution: (u32, u32), bands: &str, s: &Summary) -> String {
    format!(
        "| {label} | {}x{} | {bands} | {:.1} / {:.1} / {:.1} / {:.1} | {:.1} / {:.1} / {:.1} / {:.1} | {:.1} / {:.1} / {:.1} / {:.1} |",
        resolution.0,
        resolution.1,
        100.0 * s.area[INTERIOR],
        100.0 * s.area[TEXTURE],
        100.0 * s.area[SILHOUETTE],
        100.0 * s.area[UNOBSERVED],
        100.0 * s.share[INTERIOR],
        100.0 * s.share[TEXTURE],
        100.0 * s.share[SILHOUETTE],
        100.0 * s.share[UNOBSERVED],
        s.psnr_region[INTERIOR],
        s.psnr_region[TEXTURE],
        s.psnr_region[SILHOUETTE],
        s.psnr_region[UNOBSERVED],
    )
}

/// PSNR of the only view of a one-view result.
fn single_psnr(one: &SetResult) -> f32 {
    one.close.iter().chain(&one.graze).map(|s| s.psnr).next().unwrap_or(one.held.psnr)
}

const SHARE_HEADER: &str = "| scene | res | bands | area int/tex/sil/unobs % | error share int/tex/sil/unobs % | PSNR int/tex/sil/unobs dB |\n|---|---|---|---|---|---|";

pub fn baseline(lab: &Lab) {
    let scene = make_scene(SceneKind::Stage);
    let today = Renderer::default();

    // The lab's stage must be the pipeline's stage.
    let mut pipeline = Pipeline::new(PipelineConfig::for_scene(SceneKind::Stage));
    pipeline.tick(STAGE_TIMES[0]);
    let (_, cloud) = StageRecipe::default().build(scene.as_ref(), pipeline.scene_time());
    let same = cloud.fixed.len() == pipeline.static_splats().len() && cloud.live.len() == pipeline.dynamic_splats().len();
    println!(
        "Cross-check with `Pipeline` at t = {:.1} s: lab {} + {} splats, pipeline {} + {} ({}).\n",
        pipeline.scene_time(),
        cloud.fixed.len(),
        cloud.live.len(),
        pipeline.static_splats().len(),
        pipeline.dynamic_splats().len(),
        if same { "same" } else { "DIFFERENT" }
    );

    println!("### Stage, today's defaults\n\n{}", table_header("instant"));
    let mut share_rows = Vec::new();
    let mut per_view = Vec::new();
    for &resolution in &RESOLUTIONS {
        for &t in &STAGE_TIMES {
            let views = StageViews::new(lab, scene.as_ref(), &[t], resolution);
            let clouds = stage_clouds(scene.as_ref(), &StageRecipe::default(), &[t]);
            let result = score_stage(&views, &clouds, &today);
            let (splats, wire) = stage_budget(&clouds);
            println!("{}", table_row(&format!("t = {t} s"), resolution, &splats, &wire, &result));
            let layers = |_: usize| vec![Layer { splats: &clouds[0].fixed, weight: 1.0 }, Layer { splats: &clouds[0].live, weight: 1.0 }];
            share_rows.push(share_row(&format!("stage t = {t} s"), resolution, "scaled", &result.held));
            share_rows.push(share_row(&format!("stage t = {t} s"), resolution, "2 px", &shares_2px(&views.views, &layers)));
            if resolution == FULL {
                for (i, view) in views.views.iter().enumerate() {
                    let one = evaluate_set(std::slice::from_ref(view), &|_| layers(i), &today.opts);
                    per_view.push(format!("stage t={t} {}: {:.2} dB", view.name, single_psnr(&one)));
                }
            }
        }
        lab.note(&format!("stage baseline at {resolution:?} done"));
    }

    let bench = HouseBench::new(lab);
    println!("\n### House, today's defaults (finished scan)\n\n{}", table_header("scan"));
    for &resolution in &RESOLUTIONS {
        let views = bench.views(lab, resolution, false);
        let result = score_house(&views, &bench.today.splats, &today);
        let n = bench.today.splats.len();
        println!("{}", table_row("192x144, 3.2 cm", resolution, &thousands(n), &bytes(n * WIRE), &result));
        let layers = |_: usize| vec![Layer { splats: &bench.today.splats, weight: 1.0 }];
        share_rows.push(share_row("house", resolution, "scaled", &result.held));
        share_rows.push(share_row("house", resolution, "2 px", &shares_2px(&views, &layers)));
        if resolution == FULL {
            for (i, view) in views.iter().enumerate() {
                let one = evaluate_set(std::slice::from_ref(view), &|_| layers(i), &today.opts);
                let observed = one.close.iter().chain(&one.graze).map(|s| s.psnr_observed).next().unwrap_or(one.held.psnr_observed);
                per_view.push(format!("house {}: {:.2} dB (observed {observed:.2})", view.name, single_psnr(&one)));
            }
        }
        lab.note(&format!("house baseline at {resolution:?} done"));
    }
    println!(
        "\nHouse scan: {} frames, {:.1} M sensor pixels.\n\n### Error by region of the reference (held-out views)\n\n\
         \"scaled\" bands are 2 px wide at 320x240 and proportionally wider above (constant width on the scene); \
         \"2 px\" bands are 2 px at every resolution.\n\n{SHARE_HEADER}",
        bench.today.frame_count,
        bench.today.pixels as f64 / 1e6
    );
    for row in share_rows {
        println!("{row}");
    }
    println!("\n### Per view at 1280x960\n");
    for line in per_view {
        println!("- {line}");
    }
}

// ---------------------------------------------------------------------------
// Step 1: oracles
// ---------------------------------------------------------------------------

fn upsamplings() -> [(&'static str, Upsampling); 3] {
    [("nearest", Upsampling::Nearest), ("bilinear", Upsampling::Bilinear), ("guided", Upsampling::Guided { sigma_color: 15.0 })]
}

pub fn oracle(lab: &Lab) {
    let scene = make_scene(SceneKind::Stage);
    let today = Renderer::default();
    let base = StageRecipe::default();

    // --- Stage: sensor resolution, colour finer than depth, exact normals.
    let mut recipes: Vec<(String, StageRecipe)> = vec![
        ("today: colour and depth 160x120".into(), base.clone()),
        ("oracle: exact normals, 160x120".into(), StageRecipe { exact_normals: true, ..base.clone() }),
        ("oracle: colour and depth 320x240".into(), StageRecipe { scale: 2, ..base.clone() }),
    ];
    for (name, mode) in upsamplings() {
        let sensor = Sensor::CoarseDepth { factor: 2, mode };
        recipes.push((format!("colour 320x240, depth 160x120 {name}"), StageRecipe { scale: 2, sensor, ..base.clone() }));
    }
    if !lab.quick {
        recipes.push(("oracle: colour and depth 640x480".into(), StageRecipe { scale: 4, ..base.clone() }));
        for (name, mode) in upsamplings() {
            let sensor = Sensor::CoarseDepth { factor: 4, mode };
            recipes.push((format!("colour 640x480, depth 160x120 {name}"), StageRecipe { scale: 4, sensor, ..base.clone() }));
        }
        let sensor = Sensor::CoarseDepth { factor: 2, mode: Upsampling::Guided { sigma_color: 15.0 } };
        recipes.push(("colour 640x480, depth 320x240 guided".into(), StageRecipe { scale: 4, sensor, ..base.clone() }));
    }
    if lab.wants("stage") {
        let clouds: Vec<Vec<StageCloud>> = recipes
            .iter()
            .map(|(name, recipe)| {
                lab.note(&format!("stage: {name}"));
                stage_clouds(scene.as_ref(), recipe, &STAGE_TIMES)
            })
            .collect();
        println!("### Stage oracles (both instants, today's renderer)\n\n{}", table_header("sensors"));
        for &resolution in RESOLUTIONS.iter().rev() {
            let views = StageViews::new(lab, scene.as_ref(), &STAGE_TIMES, resolution);
            for ((name, _), clouds) in recipes.iter().zip(&clouds) {
                let (splats, wire) = stage_budget(clouds);
                println!("{}", table_row(name, resolution, &splats, &wire, &score_stage(&views, clouds, &today)));
            }
            lab.note(&format!("stage oracles at {resolution:?} done"));
        }
    }

    // --- Stage: one camera against eight where one suffices.
    if lab.wants("one") {
        one_camera(lab, scene.as_ref());
    }
    if !lab.wants("house") {
        return;
    }

    // --- House.
    let bench = HouseBench::new(lab);
    let voxel = bench.config.scan.voxel_size;
    let mut scans: Vec<(String, Scan)> = Vec::new();
    scans.push(("192x144 sensor, 1.6 cm cells".into(), bench.scan(lab, 1, voxel / 2.0, Sensor::Native)));
    scans.push(("384x288 sensor, 3.2 cm cells".into(), bench.scan(lab, 2, voxel, Sensor::Native)));
    scans.push(("oracle: 384x288 sensor, 1.6 cm cells".into(), bench.scan(lab, 2, voxel / 2.0, Sensor::Native)));
    for (name, mode) in upsamplings() {
        let sensor = Sensor::CoarseDepth { factor: 2, mode };
        scans.push((format!("colour 384x288, depth 192x144 {name}, 1.6 cm cells"), bench.scan(lab, 2, voxel / 2.0, sensor)));
    }
    if !lab.quick {
        scans.push(("oracle: 768x576 sensor, 1.6 cm cells".into(), bench.scan(lab, 4, voxel / 2.0, Sensor::Native)));
        scans.push(("oracle: 768x576 sensor, 0.8 cm cells".into(), bench.scan(lab, 4, voxel / 4.0, Sensor::Native)));
        let sensor = Sensor::CoarseDepth { factor: 4, mode: Upsampling::Guided { sigma_color: 15.0 } };
        scans.push(("colour 768x576, depth 192x144 guided, 0.8 cm cells".into(), bench.scan(lab, 4, voxel / 4.0, sensor)));
    }
    println!(
        "\n### House oracles (finished scan, today's renderer; every third held-out view, the close-up and the grazing view)\n\n{}",
        table_header("scan")
    );
    for &resolution in RESOLUTIONS.iter().rev() {
        let views = bench.views(lab, resolution, true);
        let n = bench.today.splats.len();
        let result = score_house(&views, &bench.today.splats, &today);
        println!("{}", table_row("today: 192x144 sensor, 3.2 cm cells", resolution, &thousands(n), &bytes(n * WIRE), &result));
        for (name, scan) in &scans {
            let n = scan.splats.len();
            let result = score_house(&views, &scan.splats, &today);
            println!("{}", table_row(name, resolution, &thousands(n), &bytes(n * WIRE), &result));
        }
        lab.note(&format!("house oracles at {resolution:?} done"));
    }
}

/// Views next to a rig camera, where that camera alone sees nearly
/// everything the view shows: what does stacking seven more cameras cost?
fn one_camera(lab: &Lab, scene: &dyn Scene) {
    use gs_core::glam::Vec3;
    let today = Renderer::default();
    let t = STAGE_TIMES[0];
    let snap = scene.snapshot(t);
    let base = StageRecipe::default();
    let cameras = base.rig().cameras();
    println!(
        "\n### Stage: one camera against eight (t = {t} s)\n\nViews 25 cm beside and 20 cm behind a rig camera; scores on the pixels \
         whose surface point that camera sees.\n\n| view | cloud | res | splats | PSNR on pixels seen by the camera | PSNR all | SSIM | coverage of seen % | share int/tex/sil % |\n|---|---|---|---|---|---|---|---|---|"
    );
    for index in [0usize, 1] {
        let cam = &cameras[index];
        let right = Vec3::from(cam.world_from_cam.matrix3.x_axis);
        let eye = cam.position() + right * 0.25 - cam.forward() * 0.2;
        let point = crate::harness::Viewpoint {
            name: format!("beside_cam{index}"),
            kind: Kind::Held,
            cam: gs_core::Camera::look_at(eye, RingRig::default().target, Vec3::Y, cam.fov_y(), 320, 240),
        };
        let (capture, _) = base.build(scene, t);
        let exposures = ScanExposures { frames: vec![(*cam, capture.frames[index].depth.clone())], max_range: 20.0 };
        let wide = ReconstructParams { footprint: 0.6, ..base.recon };
        let variants: Vec<(String, StageRecipe)> = vec![
            (format!("camera {index} alone, footprint 0.45"), StageRecipe { cameras: Some(vec![index]), ..base.clone() }),
            (format!("camera {index} alone, footprint 0.6"), StageRecipe { cameras: Some(vec![index]), recon: wide, ..base.clone() }),
            ("all eight, footprint 0.45 (today)".into(), base.clone()),
            ("all eight fused (A1 greedy), footprint 0.45".into(), StageRecipe { fusion: Some(FusionParams::default()), ..base.clone() }),
            ("all eight fused (A1 greedy), footprint 0.6".into(), StageRecipe { fusion: Some(FusionParams::default()), recon: wide, ..base.clone() }),
        ];
        for &resolution in &RESOLUTIONS {
            let view = lab.eval_view(&format!("stage_t{t}_one{index}"), &point, resolution, &snap, true, Some(&exposures));
            let views = StageViews { views: vec![view], instant: vec![0] };
            for (name, recipe) in &variants {
                let cloud = recipe.build(scene, t).1;
                let h = score_stage(&views, std::slice::from_ref(&cloud), &today).held;
                println!(
                    "| beside camera {index} | {name} | {}x{} | {} + {} | {:.2} | {:.2} | {:.4} | {:.2} | {:.0} / {:.0} / {:.0} |",
                    resolution.0,
                    resolution.1,
                    thousands(cloud.fixed.len()),
                    thousands(cloud.live.len()),
                    h.psnr_observed,
                    h.psnr,
                    h.ssim,
                    100.0 * h.coverage_observed,
                    100.0 * h.share[INTERIOR] / (1.0 - h.share[UNOBSERVED]).max(1e-6),
                    100.0 * h.share[TEXTURE] / (1.0 - h.share[UNOBSERVED]).max(1e-6),
                    100.0 * h.share[SILHOUETTE] / (1.0 - h.share[UNOBSERVED]).max(1e-6),
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Step 2: A1, multi-view fusion
// ---------------------------------------------------------------------------

fn greedy(margin: f32) -> FusionParams {
    FusionParams { margin, ..FusionParams::default() }
}

fn dominated(margin: f32) -> FusionParams {
    FusionParams { select: Some(Selection::Dominated), margin, ..FusionParams::default() }
}

fn fusion_row(label: &str, clouds: &[StageCloud]) -> String {
    let n = clouds.len().max(1);
    let mean = |f: &dyn Fn(&StageCloud) -> usize| clouds.iter().map(f).sum::<usize>() / n;
    let percent = |a: usize, b: usize| if b > 0 { 100.0 * a as f32 / b as f32 } else { 0.0 };
    let (fi, li) = (mean(&|c| c.fixed_stats.input), mean(&|c| c.live_stats.input));
    format!(
        "| {label} | {fi} | {} | {} | {} ({:.0} %) | {li} | {} | {} | {} ({:.0} %) |",
        mean(&|c| c.fixed_stats.carved),
        mean(&|c| c.fixed_stats.dominated),
        mean(&|c| c.fixed.len()),
        percent(mean(&|c| c.fixed.len()), fi),
        mean(&|c| c.live_stats.carved),
        mean(&|c| c.live_stats.dominated),
        mean(&|c| c.live.len()),
        percent(mean(&|c| c.live.len()), li),
    )
}

pub fn a1(lab: &Lab) {
    let scene = make_scene(SceneKind::Stage);
    let base = StageRecipe::default();
    let with = |fusion: FusionParams| StageRecipe { fusion: Some(fusion), ..base.clone() };

    // --- Variants of the fusion itself.
    let variants: Vec<(String, StageRecipe)> = vec![
        ("today: concatenated".into(), base.clone()),
        ("carve only".into(), with(FusionParams { select: None, ..FusionParams::default() })),
        ("dominated, m = 0".into(), with(dominated(0.0))),
        ("dominated, m = 0.25".into(), with(dominated(0.25))),
        ("dominated, m = 0.5".into(), with(dominated(0.5))),
        ("greedy, m = 0".into(), with(greedy(0.0))),
        ("greedy, m = 0.1".into(), with(greedy(0.1))),
        ("greedy, m = 0.25".into(), with(greedy(0.25))),
        ("greedy, m = 0.5".into(), with(greedy(0.5))),
        ("greedy, m = 1".into(), with(greedy(1.0))),
        ("greedy, m = 0, no edge guard".into(), with(FusionParams { edge_guard: false, ..greedy(0.0) })),
        ("greedy, m = 0, no carving".into(), with(FusionParams { carve: false, ..greedy(0.0) })),
        ("greedy, m = 0, tolerance 1.5 %".into(), with(FusionParams { tolerance: 0.015, ..greedy(0.0) })),
        ("greedy, m = 0, tolerance 6 %".into(), with(FusionParams { tolerance: 0.06, ..greedy(0.0) })),
        ("greedy, m = 0, any normals".into(), with(FusionParams { min_normal_dot: -1.0, ..greedy(0.0) })),
    ];
    if lab.wants("timing") {
        a1_timing(lab, scene.as_ref());
    }
    if lab.wants("variants") {
        a1_variants(lab, scene.as_ref(), &variants);
    }
    if lab.wants("sweep") {
        a1_sweep(lab, scene.as_ref());
    }
}

fn a1_variants(lab: &Lab, scene_box: &dyn Scene, variants: &[(String, StageRecipe)]) {
    let today = Renderer::default();
    let scene = &scene_box;
    let clouds: Vec<Vec<StageCloud>> = variants.iter().map(|(_, r)| stage_clouds(*scene, r, &STAGE_TIMES)).collect();
    println!(
        "### A1: what the fusion drops (mean of the two instants)\n\n| fusion | plate in | carved | dominated | plate out | live in | carved | dominated | live out |\n|---|---|---|---|---|---|---|---|---|"
    );
    for ((name, _), clouds) in variants.iter().zip(&clouds).skip(1) {
        println!("{}", fusion_row(name, clouds));
    }
    println!("\n### A1: quality (both instants, today's renderer)\n\n{}", table_header("fusion"));
    for &resolution in RESOLUTIONS.iter().rev() {
        let views = StageViews::new(lab, *scene, &STAGE_TIMES, resolution);
        for ((name, _), clouds) in variants.iter().zip(&clouds) {
            if resolution != FULL && !(name.starts_with("today") || name == "greedy, m = 0" || name == "greedy, m = 0.25" || name == "dominated, m = 0.25") {
                continue;
            }
            let (splats, wire) = stage_budget(clouds);
            println!("{}", table_row(name, resolution, &splats, &wire, &score_stage(&views, clouds, &today)));
        }
        if resolution == FULL {
            // One layer at a time.
            let (plain, fused) = (&clouds[0], &clouds[5]);
            let mix = |fixed: &[StageCloud], live: &[StageCloud]| -> Vec<StageCloud> {
                fixed.iter().zip(live).map(|(f, l)| StageCloud { fixed: f.fixed.clone(), live: l.live.clone(), ..StageCloud::default() }).collect()
            };
            for (name, clouds) in [("greedy, m = 0, plate only", mix(fused, plain)), ("greedy, m = 0, live layer only", mix(plain, fused))] {
                let (splats, wire) = stage_budget(&clouds);
                println!("{}", table_row(name, resolution, &splats, &wire, &score_stage(&views, &clouds, &today)));
            }
        }
        lab.note(&format!("A1 variants at {resolution:?} done"));
    }
}

/// The optimum of the reconstruction parameters moves with the fusion.
fn a1_sweep(lab: &Lab, scene_box: &dyn Scene) {
    let today = Renderer::default();
    let base = StageRecipe::default();
    let scene = &scene_box;
    let views = StageViews::new(lab, *scene, &STAGE_TIMES, FULL);
    println!(
        "\n### Reconstruction parameters, re-swept at 1280x960 (both instants, today's renderer)\n\nOne parameter at a time around the defaults \
         (footprint 0.45, min_cos 0.2, max_anisotropy 4, edge_shrink 0.5).\n\n{}",
        table_header("parameters")
    );
    let d = ReconstructParams::default();
    let mut sweeps: Vec<(String, ReconstructParams)> = vec![("defaults".into(), d)];
    for k in [0.35, 0.55, 0.65, 0.8] {
        sweeps.push((format!("footprint {k}"), ReconstructParams { footprint: k, ..d }));
    }
    for c in [0.1, 0.35, 0.5] {
        sweeps.push((format!("min_cos {c}"), ReconstructParams { min_cos: c, ..d }));
    }
    for a in [1.5, 2.5, 8.0] {
        sweeps.push((format!("max_anisotropy {a}"), ReconstructParams { max_anisotropy: a, ..d }));
    }
    for e in [0.75, 1.0] {
        sweeps.push((format!("edge_shrink {e}"), ReconstructParams { edge_shrink: e, ..d }));
    }
    sweeps.push(("opacity 1.0".into(), ReconstructParams { opacity: 1.0, ..d }));
    for (fusion_name, fusion) in [("concatenated", None), ("A1 greedy", Some(greedy(0.0)))] {
        for (name, recon) in &sweeps {
            let recipe = StageRecipe { recon: *recon, fusion, ..base.clone() };
            let clouds = stage_clouds(*scene, &recipe, &STAGE_TIMES);
            let (splats, wire) = stage_budget(&clouds);
            println!("{}", table_row(&format!("{fusion_name}: {name}"), FULL, &splats, &wire, &score_stage(&views, &clouds, &today)));
        }
        lab.note(&format!("sweep with {fusion_name} done"));
    }
    // A small grid where the one-at-a-time sweep points.
    for (k, c, a) in [(0.55, 0.35, 4.0), (0.55, 0.35, 2.5), (0.65, 0.35, 2.5), (0.55, 0.5, 2.5), (0.65, 0.5, 4.0), (0.65, 0.2, 2.5)] {
        let recon = ReconstructParams { footprint: k, min_cos: c, max_anisotropy: a, ..d };
        let recipe = StageRecipe { recon, fusion: Some(greedy(0.0)), ..base.clone() };
        let clouds = stage_clouds(*scene, &recipe, &STAGE_TIMES);
        let (splats, wire) = stage_budget(&clouds);
        let name = format!("A1 greedy: footprint {k}, min_cos {c}, max_anisotropy {a}");
        println!("{}", table_row(&name, FULL, &splats, &wire, &score_stage(&views, &clouds, &today)));
    }
}

/// Reconstruction and fusion time of the live layer for one tick.
fn a1_timing(lab: &Lab, scene: &dyn Scene) {
    use gs_core::experimental::lab::map_indices;
    use gs_core::experimental::multiview::{fuse, FusionView};
    use gs_core::experimental::recon::{frame_to_pixel_splats, ReconHooks};
    use gs_core::reconstruct::{foreground_mask, frame_to_splats, BackgroundPlate};

    println!(
        "\n### A1: time per tick for the live layer (t = {} s)\n\nThe machine is shared, so every cell gives the CPU time per run (summed over threads, measured over 2 s of repetitions) \
         and, after the slash, the best wall-clock time of a single run.\n\n\
         | ring | threads | product today: mask + splats, CPU ms / best wall ms | lab: mask + splats with provenance | fusion (A1) | live splats in | out |\n|---|---|---|---|---|---|---|",
        STAGE_TIMES[0]
    );
    let params = ReconstructParams::default();
    for scale in [1u32, 2, 4] {
        let recipe = StageRecipe { scale, ..StageRecipe::default() };
        let (capture, _) = recipe.build(scene, STAGE_TIMES[0]);
        let plates: Vec<BackgroundPlate> = capture.plates.iter().map(BackgroundPlate::from_frame).collect();
        let count = capture.cameras.len();
        let product = || {
            map_indices(count, |i| {
                let mask = foreground_mask(&capture.frames[i], &plates[i], &params);
                let mut out = Vec::new();
                frame_to_splats(&capture.frames[i], &capture.cameras[i], &params, Some(&mask), &mut out);
                out
            })
        };
        let candidates = || {
            map_indices(count, |i| {
                let mask = foreground_mask(&capture.frames[i], &plates[i], &params);
                frame_to_pixel_splats(&capture.frames[i], &capture.cameras[i], &params, &ReconHooks { mask: Some(&mask), normals: None })
            })
        };
        let lists = candidates();
        let fusion = FusionParams::default();
        let fused = || {
            let views: Vec<FusionView> =
                (0..count).map(|i| FusionView { cam: &capture.cameras[i], depth: &capture.frames[i].depth, splats: &lists[i] }).collect();
            fuse(&views, &fusion)
        };
        let label = format!("8 x {}x{}", 160 * scale, 120 * scale);
        let input: usize = lists.iter().map(Vec::len).sum();
        for threads in ["1", "all"] {
            let run = |f: &(dyn Fn() -> (f64, f64) + Sync)| if threads == "1" { crate::harness::single_threaded(f) } else { f() };
            let cell = |(cpu, wall): (f64, f64)| format!("{cpu:.2} / {wall:.2}");
            let a = cell(run(&|| crate::harness::cost_ms(2.0, product)));
            let b = cell(run(&|| crate::harness::cost_ms(2.0, candidates)));
            let c = cell(run(&|| crate::harness::cost_ms(2.0, fused)));
            println!("| {label} | {threads} | {a} | {b} | {c} | {input} | {} |", fused().stats.kept);
        }
    }
    lab.note("A1 timing done");
}

// ---------------------------------------------------------------------------
// Capture feasibility
// ---------------------------------------------------------------------------

pub fn capture(lab: &Lab) {
    use gs_core::capture::{render_rgbd, CaptureOptions};
    use gs_core::reconstruct::{foreground_mask, BackgroundPlate, MASK_FOREGROUND, MASK_SKIP};

    let scene = make_scene(SceneKind::Stage);
    let empty = scene.snapshot_static();
    let params = ReconstructParams::default();
    let opts = CaptureOptions::default();
    let loop_duration = scene.loop_duration().unwrap_or(9.6);
    let instants = 24;

    println!(
        "### Foreground per camera over the dance loop ({instants} instants)\n\nFractions of a camera's pixels. \"nearer\" differs from the plate in depth, \
         \"changed\" only in colour (shadows); the boxes are the bounding boxes of the marked pixels grown by 2 px.\n\n\
         | ring | nearer mean / max % | changed mean / max % | box of nearer mean / max % | box of all marked mean / max % | per-tick mean over the 8 cameras: marked %, box of all % (worst tick) |\n|---|---|---|---|---|---|"
    );
    let mut box_fraction = [(0.0f64, 0.0f64); 3];
    for (slot, scale) in [1u32, 2, 4].into_iter().enumerate() {
        let recipe = StageRecipe { scale, ..StageRecipe::default() };
        let cameras = recipe.rig().cameras();
        let plates: Vec<_> = cameras.iter().map(|c| BackgroundPlate::from_frame(&render_rgbd(&empty, c, &opts))).collect();
        // (nearer, changed, box of nearer, box of all) per camera and instant.
        let mut samples: Vec<[f64; 4]> = Vec::new();
        let mut worst_tick = (0.0f64, 0.0f64);
        for k in 0..instants {
            let snap = scene.snapshot(loop_duration * k as f32 / instants as f32);
            let mut tick = (0.0f64, 0.0f64);
            for (cam, plate) in cameras.iter().zip(&plates) {
                let frame = render_rgbd(&snap, cam, &opts);
                let mask = foreground_mask(&frame, plate, &params);
                let (w, h) = (cam.width as usize, cam.height as usize);
                let count = |f: &dyn Fn(u8) -> bool| mask.iter().filter(|&&m| f(m)).count() as f64 / mask.len() as f64;
                let bounds = |f: &dyn Fn(u8) -> bool| {
                    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0usize, 0usize);
                    for (i, &m) in mask.iter().enumerate() {
                        if f(m) {
                            x0 = x0.min(i % w);
                            x1 = x1.max(i % w + 1);
                            y0 = y0.min(i / w);
                            y1 = y1.max(i / w + 1);
                        }
                    }
                    if x1 <= x0 {
                        return 0.0;
                    }
                    let grow = 2;
                    ((x1 + grow).min(w) - x0.saturating_sub(grow)) as f64 * ((y1 + grow).min(h) - y0.saturating_sub(grow)) as f64 / (w * h) as f64
                };
                let sample = [
                    count(&|m| m == MASK_FOREGROUND),
                    count(&|m| m != MASK_SKIP && m != MASK_FOREGROUND),
                    bounds(&|m| m == MASK_FOREGROUND),
                    bounds(&|m| m != MASK_SKIP),
                ];
                tick.0 += (sample[0] + sample[1]) / cameras.len() as f64;
                tick.1 += sample[3] / cameras.len() as f64;
                samples.push(sample);
            }
            if tick.1 > worst_tick.1 {
                worst_tick = tick;
            }
        }
        let stat = |c: usize| {
            let mean = samples.iter().map(|s| s[c]).sum::<f64>() / samples.len() as f64;
            let max = samples.iter().map(|s| s[c]).fold(0.0, f64::max);
            (mean, max)
        };
        let cell = |c: usize| format!("{:.1} / {:.1}", 100.0 * stat(c).0, 100.0 * stat(c).1);
        println!(
            "| 8 x {}x{} | {} | {} | {} | {} | {:.1}, {:.1} |",
            160 * scale,
            120 * scale,
            cell(0),
            cell(1),
            cell(2),
            cell(3),
            100.0 * worst_tick.0,
            100.0 * worst_tick.1
        );
        box_fraction[slot] = (stat(3).0, worst_tick.1);
        lab.note(&format!("foreground at scale {scale} done"));
    }

    println!(
        "\n### Simulated capture cost of one tick (8 cameras, shadows on)\n\nThe machine is shared: \"CPU ms\" is the CPU time per tick summed over threads (measured over 3 s of repetitions), \
         \"wall\" the best single run. \"boxes only\" is an estimate: the CPU time times the fraction of pixels inside the bounding boxes of \
         everything that differs from the plate (mean over the loop, and the worst tick), i.e. a simulator that ray casts only those boxes at this \
         resolution and reuses the plate elsewhere. Divide CPU ms by the number of free cores for the wall-clock time on an idle machine.\n\n\
         | ring | threads | full frames: CPU ms | best wall ms | CPU ns per ray | boxes only, mean tick: CPU ms | boxes only, worst tick: CPU ms |\n|---|---|---|---|---|---|---|"
    );
    let snap = scene.snapshot(STAGE_TIMES[0]);
    for (slot, scale) in [1u32, 2, 4].into_iter().enumerate() {
        let cameras = StageRecipe { scale, ..StageRecipe::default() }.rig().cameras();
        let rays = cameras.len() as f64 * (160 * scale) as f64 * (120 * scale) as f64;
        let shoot = || gs_core::experimental::lab::map_indices(cameras.len(), |i| render_rgbd(&snap, &cameras[i], &opts)).len();
        for threads in ["1", "all"] {
            let (ms, wall) = if threads == "1" {
                crate::harness::single_threaded(|| crate::harness::cost_ms(3.0, shoot))
            } else {
                crate::harness::cost_ms(3.0, shoot)
            };
            println!(
                "| 8 x {}x{} | {threads} | {ms:.1} | {wall:.1} | {:.0} | {:.1} | {:.1} |",
                160 * scale,
                120 * scale,
                ms * 1e6 / rays,
                ms * box_fraction[slot].0,
                ms * box_fraction[slot].1
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Step 3: C1, normalised depth-tested blending
// ---------------------------------------------------------------------------

/// The normalised renderer the sweeps start from.
pub fn normalized_base() -> (WblendOptions, Normalized) {
    let params = Normalized {
        vis_cutoff: 2.0,
        depth_tol: 0.01,
        depth_tol_sigma: 0.0,
        coverage: Coverage::Linear(0.5),
        taper: true,
        silhouette: Silhouette::Hard,
        near_fringe: true,
    };
    (WblendOptions { blend: Blend::Normalized(params), ..WblendOptions::default() }, params)
}

fn with_blend(opts: WblendOptions, params: Normalized) -> WblendOptions {
    WblendOptions { blend: Blend::Normalized(params), ..opts }
}

/// One-at-a-time variations of the normalised renderer around a base.
fn c1_variations(base: WblendOptions, n: Normalized, kernels: &[f32]) -> Vec<(String, WblendOptions)> {
    let mut list: Vec<(String, WblendOptions)> = vec![("base".into(), base)];
    for &k in kernels {
        list.push((format!("kernel x{k}"), WblendOptions { kernel_scale: base.kernel_scale * k, ..base }));
    }
    for c in [2.0, 2.5] {
        list.push((format!("cutoff {c}"), WblendOptions { cutoff: c, ..base }));
    }
    list.push(("cutoff 2, no taper".into(), with_blend(WblendOptions { cutoff: 2.0, ..base }, Normalized { taper: false, ..n })));
    list.push(("no taper".into(), with_blend(base, Normalized { taper: false, ..n })));
    for v in [1.0, 1.5, 2.5, 3.0] {
        list.push((format!("visibility radius {v}"), with_blend(base, Normalized { vis_cutoff: v, ..n })));
    }
    for t in [0.002, 0.005, 0.02, 0.05] {
        list.push((format!("depth tolerance {} %", 100.0 * t), with_blend(base, Normalized { depth_tol: t, ..n })));
    }
    for t in [0.002, 0.005, 0.01] {
        let opts = WblendOptions { depth: DepthModel::Plane, ..with_blend(base, Normalized { depth_tol: t, ..n }) };
        list.push((format!("plane depth, tolerance {} %", 100.0 * t), opts));
    }
    list.push(("tolerance 0.2 % + 2 sigma".into(), with_blend(base, Normalized { depth_tol: 0.002, depth_tol_sigma: 2.0, ..n })));
    for (name, coverage) in [("none", Coverage::None), ("linear 0.2", Coverage::Linear(0.2)), ("linear 1.0", Coverage::Linear(1.0)), ("smooth 0.5", Coverage::Smooth(0.5))] {
        list.push((format!("fringe coverage {name}"), with_blend(base, Normalized { coverage, ..n })));
    }
    list.push(("cull back faces".into(), WblendOptions { cull_backfaces: true, ..base }));
    list
}

/// Silhouette rules against coverage thresholds and visibility radii.
fn silhouette_variations(base: WblendOptions, n: Normalized) -> Vec<(String, WblendOptions)> {
    let mut list = Vec::new();
    for (rule_name, silhouette) in [("soft over background", Silhouette::SoftOverBackground), ("soft over next surface", Silhouette::SoftOverNextSurface)] {
        for tau in [0.3, 0.5, 0.8, 1.2] {
            let params = Normalized { silhouette, coverage: Coverage::Linear(tau), ..n };
            list.push((format!("{rule_name}, coverage W / {tau}"), with_blend(base, params)));
        }
    }
    let soft = Normalized { silhouette: Silhouette::SoftOverNextSurface, ..n };
    for tau in [0.5, 0.8, 1.2] {
        list.push((format!("soft over next surface, coverage smoothstep {tau}"), with_blend(base, Normalized { coverage: Coverage::Smooth(tau), ..soft })));
    }
    for v in [1.5, 2.5, 3.0] {
        for tau in [0.5, 0.8] {
            let params = Normalized { vis_cutoff: v, coverage: Coverage::Linear(tau), ..soft };
            list.push((format!("soft over next surface, coverage W / {tau}, visibility radius {v}"), with_blend(base, params)));
        }
    }
    for t in [0.02, 0.05] {
        let params = Normalized { depth_tol: t, coverage: Coverage::Linear(0.8), ..soft };
        list.push((format!("soft over next surface, coverage W / 0.8, depth tolerance {} %", 100.0 * t), with_blend(base, params)));
    }
    for t in [0.005, 0.01, 0.02] {
        let params = Normalized { depth_tol: t, coverage: Coverage::Linear(0.8), ..soft };
        let opts = WblendOptions { depth: DepthModel::Plane, ..with_blend(base, params) };
        list.push((format!("soft over next surface, coverage W / 0.8, plane depth, tolerance {} %", 100.0 * t), opts));
    }
    for k in [0.8, 1.25, 1.5] {
        let params = Normalized { coverage: Coverage::Linear(0.8), ..soft };
        list.push((format!("soft over next surface, coverage W / 0.8, kernel x{k}"), WblendOptions { kernel_scale: k, ..with_blend(base, params) }));
    }
    list
}

pub fn c1(lab: &Lab) {
    let today = Renderer::default();
    let (base, n) = normalized_base();
    if lab.wants("silhouette") {
        let scene = make_scene(SceneKind::Stage);
        let views = StageViews::new(lab, scene.as_ref(), &STAGE_TIMES, FULL);
        let clouds: Vec<(&str, Vec<StageCloud>)> = vec![
            ("concatenated", stage_clouds(scene.as_ref(), &StageRecipe::default(), &STAGE_TIMES)),
            ("A1", stage_clouds(scene.as_ref(), &StageRecipe { fusion: Some(greedy(0.0)), ..StageRecipe::default() }, &STAGE_TIMES)),
        ];
        println!("### C1: silhouette rules on the stage at 1280x960 (both instants)\n\nOtherwise the base renderer of the C1 sweep.\n\n{}", table_header_overdraw("cloud: renderer"));
        for (cloud_name, clouds) in &clouds {
            let (splats, wire) = stage_budget(clouds);
            for (name, opts) in silhouette_variations(base, n) {
                let result = score_stage(&views, clouds, &Renderer { opts, live_weight: 16.0 });
                println!("{}", table_row_overdraw(&format!("{cloud_name}: {name}"), FULL, &splats, &wire, &result));
            }
            lab.note(&format!("silhouette rules on {cloud_name} done"));
        }
    }
    if lab.wants("silhouette-house") {
        let bench = HouseBench::new(lab);
        let views = bench.views(lab, FULL, true);
        let n_splats = bench.today.splats.len();
        let (splats, wire) = (thousands(n_splats), bytes(n_splats * WIRE));
        println!("\n### C1: silhouette rules on the house at 1280x960 ({} views)\n\n{}", views.len(), table_header_overdraw("renderer"));
        for (name, opts) in silhouette_variations(base, n) {
            let result = score_house(&views, &bench.today.splats, &Renderer { opts, live_weight: 1.0 });
            println!("{}", table_row_overdraw(&name, FULL, &splats, &wire, &result));
        }
        lab.note("silhouette rules on the house done");
    }
    if lab.wants("stage") {
        let scene = make_scene(SceneKind::Stage);
        let views = StageViews::new(lab, scene.as_ref(), &STAGE_TIMES, FULL);
        let clouds: Vec<(&str, Vec<StageCloud>)> = vec![
            ("concatenated", stage_clouds(scene.as_ref(), &StageRecipe::default(), &STAGE_TIMES)),
            ("A1", stage_clouds(scene.as_ref(), &StageRecipe { fusion: Some(greedy(0.0)), ..StageRecipe::default() }, &STAGE_TIMES)),
        ];
        println!(
            "### C1 on the stage at 1280x960 (both instants)\n\nBase: visibility radius 2 sigma, cutoff 3 sigma with taper, depth tolerance 1 % on center depths, \
             fringe coverage min(1, W / 0.5), live layer weight 16.\n\n{}",
            table_header_overdraw("cloud: renderer")
        );
        for (cloud_name, clouds) in &clouds {
            let (splats, wire) = stage_budget(clouds);
            let over = score_stage(&views, clouds, &today);
            println!("{}", table_row_overdraw(&format!("{cloud_name}: sorted over (today's renderer)"), FULL, &splats, &wire, &over));
            let kernels: &[f32] = if *cloud_name == "A1" { &[0.8, 1.3, 1.6, 2.0] } else { &[0.7, 0.85, 1.3, 1.6] };
            for (name, opts) in c1_variations(base, n, kernels) {
                let result = score_stage(&views, clouds, &Renderer { opts, live_weight: 16.0 });
                println!("{}", table_row_overdraw(&format!("{cloud_name}: {name}"), FULL, &splats, &wire, &result));
            }
            for w in [1.0, 4.0, 64.0] {
                let result = score_stage(&views, clouds, &Renderer { opts: base, live_weight: w });
                println!("{}", table_row_overdraw(&format!("{cloud_name}: live weight {w}"), FULL, &splats, &wire, &result));
            }
            lab.note(&format!("C1 stage sweep on {cloud_name} done"));
        }
        // Kernel width against visibility radius on the fused cloud.
        let (splats, wire) = stage_budget(&clouds[1].1);
        for k in [1.0, 1.3, 1.6] {
            for v in [1.2, 1.6, 2.0] {
                for plane in [false, true] {
                    let opts = WblendOptions {
                        kernel_scale: k,
                        depth: if plane { DepthModel::Plane } else { DepthModel::Center },
                        ..with_blend(base, Normalized { vis_cutoff: v, ..n })
                    };
                    let result = score_stage(&views, &clouds[1].1, &Renderer { opts, live_weight: 16.0 });
                    let name = format!("A1: kernel x{k}, visibility radius {v}{}", if plane { ", plane depth" } else { "" });
                    println!("{}", table_row_overdraw(&name, FULL, &splats, &wire, &result));
                }
            }
        }
    }
    if lab.wants("house2") {
        let bench = HouseBench::new(lab);
        let views = bench.views(lab, FULL, true);
        let n_splats = bench.today.splats.len();
        let (splats, wire) = (thousands(n_splats), bytes(n_splats * WIRE));
        println!(
            "\n### C1 on the house at 1280x960, second round ({} views)\n\nHard silhouettes, 1 % tolerance, tapered 3 sigma kernel; the kernel width is relative to the scan's 0.6 cell.\n\n{}",
            views.len(),
            table_header_overdraw("renderer")
        );
        let hard = c1_renderer(2.0, 1.0);
        let Blend::Normalized(two_sided) = hard.opts.blend else { unreachable!() };
        for k in [0.45, 0.6, 0.75] {
            for v in [1.6, 2.0, 2.6] {
                // Below a product of 1.18 the visibility discs leave gaps.
                let params = Normalized { vis_cutoff: v, ..two_sided };
                let variants = [
                    ("center depth, one-sided", WblendOptions { kernel_scale: k, depth: DepthModel::Center, ..with_blend(hard.opts, Normalized { near_fringe: true, ..params }) }),
                    ("plane depth, two-sided", WblendOptions { kernel_scale: k, ..with_blend(hard.opts, params) }),
                ];
                for (name, opts) in variants {
                    let result = score_house(&views, &bench.today.splats, &Renderer { opts, live_weight: 1.0 });
                    println!("{}", table_row_overdraw(&format!("kernel x{k}, visibility radius {v}, {name}"), FULL, &splats, &wire, &result));
                }
            }
        }
        let best = |k: f32, v: f32| WblendOptions { kernel_scale: k, ..with_blend(hard.opts, Normalized { vis_cutoff: v, ..two_sided }) };
        let extra: Vec<(&str, WblendOptions)> = vec![
            ("kernel x0.6, visibility radius 2, cull back faces", WblendOptions { cull_backfaces: true, ..best(0.6, 2.0) }),
            ("kernel x0.6, visibility radius 2, planar (C2)", with_planar(Renderer { opts: best(0.6, 2.0), live_weight: 1.0 }).opts),
            ("kernel x0.6, visibility radius 2, tolerance 2 %", with_blend(best(0.6, 2.0), Normalized { depth_tol: 0.02, ..two_sided })),
            ("kernel x0.6, visibility radius 2, tolerance 0.5 %", with_blend(best(0.6, 2.0), Normalized { depth_tol: 0.005, ..two_sided })),
            ("kernel x0.6, visibility radius 2, cutoff 2", WblendOptions { cutoff: 2.0, ..best(0.6, 2.0) }),
            ("kernel x0.6, soft over next surface W / 0.5", WblendOptions { kernel_scale: 0.6, ..c1_soft_renderer(0.5, 1.0).opts }),
            ("kernel x0.6, soft over next surface W / 1.0", WblendOptions { kernel_scale: 0.6, ..c1_soft_renderer(1.0, 1.0).opts }),
            ("sorted over, kernel x0.8", WblendOptions { kernel_scale: 0.8, ..WblendOptions::default() }),
            ("sorted over, kernel x0.6", WblendOptions { kernel_scale: 0.6, ..WblendOptions::default() }),
        ];
        for (name, opts) in extra {
            let result = score_house(&views, &bench.today.splats, &Renderer { opts, live_weight: 1.0 });
            println!("{}", table_row_overdraw(name, FULL, &splats, &wire, &result));
        }
        lab.note("C1 house second round done");
    }
    if lab.wants("house") {
        let bench = HouseBench::new(lab);
        let views = bench.views(lab, FULL, true);
        let n_splats = bench.today.splats.len();
        let (splats, wire) = (thousands(n_splats), bytes(n_splats * WIRE));
        println!(
            "\n### C1 on the house at 1280x960 ({} of the views: every third held-out view, the close-up, the grazing view)\n\nBase as for the stage.\n\n{}",
            views.len(),
            table_header_overdraw("renderer")
        );
        println!("{}", table_row_overdraw("sorted over (today's renderer)", FULL, &splats, &wire, &score_house(&views, &bench.today.splats, &today)));
        for (name, opts) in c1_variations(base, n, &[0.6, 0.8, 1.25, 1.5]) {
            let result = score_house(&views, &bench.today.splats, &Renderer { opts, live_weight: 1.0 });
            println!("{}", table_row_overdraw(&name, FULL, &splats, &wire, &result));
        }
        lab.note("C1 house sweep done");
    }
}
// ---------------------------------------------------------------------------
// Step 4: C2, planar splats and the mip filter
// ---------------------------------------------------------------------------

/// Footprint and filter variations of a renderer.
fn c2_variations(over: WblendOptions, normalized: WblendOptions) -> Vec<(String, WblendOptions)> {
    let planar = |o: WblendOptions, floor: f32| WblendOptions { footprint: Footprint::Planar, depth: DepthModel::Plane, screen_floor_px: floor, ..o };
    vec![
        ("over, EWA, dilation 0.3 px2 (today)".into(), over),
        ("over, EWA, dilation 0.1 px2".into(), WblendOptions { low_pass: LowPass::Dilate(0.1), ..over }),
        ("over, EWA, normalised mip filter 0.1 px2".into(), WblendOptions { low_pass: LowPass::Normalized(0.1), ..over }),
        ("over, EWA, normalised mip filter 0.3 px2".into(), WblendOptions { low_pass: LowPass::Normalized(0.3), ..over }),
        ("over, planar, no floor".into(), planar(over, 0.0)),
        ("over, planar, floor 0.5 px".into(), planar(over, 0.5)),
        ("normalised, EWA, center depth".into(), WblendOptions { depth: DepthModel::Center, ..normalized }),
        ("normalised, EWA, plane depth".into(), WblendOptions { depth: DepthModel::Plane, ..normalized }),
        ("normalised, EWA, plane depth, mip filter 0.1 px2".into(), WblendOptions { depth: DepthModel::Plane, low_pass: LowPass::Normalized(0.1), ..normalized }),
        ("normalised, planar, no floor".into(), planar(normalized, 0.0)),
        ("normalised, planar, floor 0.5 px".into(), planar(normalized, 0.5)),
    ]
}

/// Viewpoints far from the stage (what the miniature mode of the headset
/// shows): the whole stage is some 60 pixels wide.
fn far_views(lab: &Lab, scene: &dyn Scene, t: f32) -> StageViews {
    use gs_core::glam::Vec3;
    let snap = scene.snapshot(t);
    let mut views = Vec::new();
    for (name, eye) in [("far_a", Vec3::new(4.0, 7.0, 16.0)), ("far_b", Vec3::new(-12.0, 3.0, -9.0))] {
        let point = crate::harness::Viewpoint {
            name: name.into(),
            kind: Kind::Held,
            cam: gs_core::Camera::look_at(eye, Vec3::new(0.0, 0.8, 0.0), Vec3::Y, 50f32.to_radians(), 320, 240),
        };
        views.push(lab.eval_view(&format!("stage_t{t}"), &point, (320, 240), &snap, true, None));
    }
    StageViews { instant: vec![0; views.len()], views }
}

pub fn c2(lab: &Lab) {
    let over = WblendOptions::default();
    let best = best_stage_renderer();
    if lab.wants("stage") {
        let scene = make_scene(SceneKind::Stage);
        let views = StageViews::new(lab, scene.as_ref(), &STAGE_TIMES, FULL);
        let far = far_views(lab, scene.as_ref(), STAGE_TIMES[0]);
        let clouds: Vec<(&str, Vec<StageCloud>)> = vec![
            ("concatenated", stage_clouds(scene.as_ref(), &StageRecipe::default(), &STAGE_TIMES)),
            ("A1", stage_clouds(scene.as_ref(), &best_stage_recipe(1), &STAGE_TIMES)),
        ];
        println!("### C2 on the stage at 1280x960 (both instants)\n\n{}", table_header_overdraw("cloud: renderer"));
        let mut far_rows = Vec::new();
        for (cloud_name, clouds) in &clouds {
            let (splats, wire) = stage_budget(clouds);
            // "Over" variants on the stacked cloud, normalised ones on the fused cloud.
            let stacked = *cloud_name == "concatenated";
            for (name, opts) in c2_variations(over, best.opts).into_iter().filter(|(name, _)| name.starts_with("over") == stacked) {
                let renderer = Renderer { opts, live_weight: best.live_weight };
                let result = score_stage(&views, clouds, &renderer);
                println!("{}", table_row_overdraw(&format!("{cloud_name}: {name}"), FULL, &splats, &wire, &result));
                let h = score_stage(&far, &clouds[..1], &renderer).held;
                far_rows.push(format!("| {cloud_name}: {name} | {:.2} | {:.4} | {:.1} / {:.1} / {:.1} |", h.psnr, h.ssim, h.psnr_region[INTERIOR], h.psnr_region[TEXTURE], h.psnr_region[SILHOUETTE]));
            }
            lab.note(&format!("C2 stage on {cloud_name} done"));
        }
        println!("\n### C2: the stage from far away (two views at 14 to 17 m, 320x240, t = {} s)\n\n| cloud: renderer | PSNR | SSIM | PSNR int/tex/sil |\n|---|---|---|---|", STAGE_TIMES[0]);
        for row in far_rows {
            println!("{row}");
        }
    }
    if lab.wants("house") {
        let bench = HouseBench::new(lab);
        let views = bench.views(lab, FULL, true);
        let n_splats = bench.today.splats.len();
        let (splats, wire) = (thousands(n_splats), bytes(n_splats * WIRE));
        println!("\n### C2 on the house at 1280x960 ({} views)\n\n{}", views.len(), table_header_overdraw("renderer"));
        for (name, opts) in c2_variations(over, hard_house_renderer().opts) {
            let result = score_house(&views, &bench.today.splats, &Renderer { opts, live_weight: 1.0 });
            println!("{}", table_row_overdraw(&name, FULL, &splats, &wire, &result));
        }
        lab.note("C2 house done");
    }
}

// ---------------------------------------------------------------------------
// Combinations
// ---------------------------------------------------------------------------

/// Normalised blending as the sweeps settled it: hard silhouettes at
/// `vis` standard deviations, two-sided depth test on plane depths with 1 %
/// tolerance, tapered 3 sigma kernel.
pub fn c1_renderer(vis: f32, live_weight: f32) -> Renderer {
    let (base, n) = normalized_base();
    let params = Normalized { vis_cutoff: vis, near_fringe: false, ..n };
    Renderer { opts: WblendOptions { depth: DepthModel::Plane, ..with_blend(base, params) }, live_weight }
}

/// The same with soft silhouettes over the next surface.
pub fn c1_soft_renderer(tau: f32, live_weight: f32) -> Renderer {
    let hard = c1_renderer(2.0, live_weight);
    let Blend::Normalized(n) = hard.opts.blend else { unreachable!() };
    let params = Normalized { silhouette: Silhouette::SoftOverNextSurface, coverage: Coverage::Linear(tau), ..n };
    Renderer { opts: with_blend(hard.opts, params), ..hard }
}

pub fn combo(lab: &Lab) {
    let scene = make_scene(SceneKind::Stage);
    let views = StageViews::new(lab, scene.as_ref(), &STAGE_TIMES, FULL);
    let d = ReconstructParams::default();
    let guided = Sensor::CoarseDepth { factor: 2, mode: Upsampling::Guided { sigma_color: 15.0 } };
    let sensors: Vec<(&str, u32, Sensor, bool)> = vec![
        ("160x120", 1, Sensor::Native, true),
        ("320x240", 2, Sensor::Native, false),
        ("colour 320x240 + depth 160x120 guided", 2, guided, false),
        ("640x480", 4, Sensor::Native, false),
    ];
    let renderers: Vec<(&str, Renderer)> = vec![
        ("over", Renderer::default()),
        ("C1", c1_renderer(1.6, 1.0)),
        ("C1, live weight 16", c1_renderer(1.6, 16.0)),
        ("C1 + C2", with_planar(c1_renderer(1.6, 1.0))),
        ("C1 soft + C2", with_planar(c1_soft_renderer(0.5, 1.0))),
    ];
    println!("### Combinations on the stage at 1280x960 (both instants)\n\n{}", table_header_overdraw("sensors, cloud: renderer"));
    for (sensor_name, scale, sensor, full) in sensors {
        if scale == 4 && lab.quick {
            continue;
        }
        let base = StageRecipe { scale, sensor, ..StageRecipe::default() };
        let fused = StageRecipe { fusion: Some(greedy(0.0)), ..base.clone() };
        let mut clouds: Vec<(&str, StageRecipe, Vec<usize>)> = vec![
            ("concatenated", base.clone(), vec![0, 1, 3]),
            ("A1, edge_shrink 0.3", StageRecipe { recon: ReconstructParams { edge_shrink: 0.3, ..d }, ..fused.clone() }, vec![1, 3, 4]),
            ("A1, footprint 0.65", StageRecipe { recon: ReconstructParams { footprint: 0.65, ..d }, ..fused.clone() }, vec![0]),
        ];
        if full {
            clouds[0].2 = vec![0, 1, 2, 3, 4];
            clouds[1].2 = vec![0, 1, 2, 3, 4];
            clouds.push(("A1", fused.clone(), vec![1, 2, 3, 4]));
            let unguarded = FusionParams { edge_guard: false, ..greedy(0.0) };
            clouds.push(("A1 without edge guard", StageRecipe { fusion: Some(unguarded), ..base.clone() }, vec![1, 2, 3]));
            let relaxed = StageRecipe { fusion: Some(greedy(0.25)), recon: ReconstructParams { edge_shrink: 0.3, ..d }, ..base.clone() };
            clouds.push(("A1 m = 0.25, edge_shrink 0.3", relaxed, vec![1, 3]));
        }
        if scale == 4 {
            clouds.truncate(2);
            clouds[0].2 = vec![0, 1];
            clouds[1].2 = vec![1, 3];
        }
        for (cloud_name, recipe, which) in &clouds {
            let built = stage_clouds(scene.as_ref(), recipe, &STAGE_TIMES);
            let (splats, wire) = stage_budget(&built);
            for &r in which {
                let (renderer_name, renderer) = &renderers[r];
                let result = score_stage(&views, &built, renderer);
                println!("{}", table_row_overdraw(&format!("{sensor_name}, {cloud_name}: {renderer_name}"), FULL, &splats, &wire, &result));
            }
        }
        lab.note(&format!("combinations with {sensor_name} sensors done"));
    }
}

// ---------------------------------------------------------------------------
// The configurations the measurements point to
// ---------------------------------------------------------------------------

/// The normalised renderer the sweeps settled on for the stage: hard
/// silhouettes at 1.6 sigma, plane depths with a two-sided 1 % test, and no
/// extra weight for the live layer.
pub fn best_stage_renderer() -> Renderer {
    c1_renderer(1.6, 1.0)
}

/// The same for the house, with the kernel narrowed to 0.45 cell (0.75 of
/// the scan's 0.6): the visibility discs then just close the cell grid.
pub fn hard_house_renderer() -> Renderer {
    let renderer = c1_renderer(1.6, 1.0);
    Renderer { opts: WblendOptions { kernel_scale: 0.75, ..renderer.opts }, ..renderer }
}

/// The best renderer of the house sweeps: soft silhouettes over the next
/// surface, kernel 0.36 cell (0.6 of the scan's), visibility radius 2 sigma.
pub fn best_house_renderer() -> Renderer {
    let renderer = c1_soft_renderer(0.5, 1.0);
    Renderer { opts: WblendOptions { kernel_scale: 0.6, ..renderer.opts }, ..renderer }
}

/// The renderer with exact planar splats and per-fragment depth (C2).
pub fn with_planar(renderer: Renderer) -> Renderer {
    let opts = WblendOptions { footprint: Footprint::Planar, depth: DepthModel::Plane, screen_floor_px: 0.5, ..renderer.opts };
    Renderer { opts, ..renderer }
}

/// The stage reconstruction with multi-view fusion (A1, greedy with a
/// margin of 0.25: the setting that drops a good third of the splats without
/// costing quality), at `scale` times today's sensor resolution.
pub fn best_stage_recipe(scale: u32) -> StageRecipe {
    StageRecipe { scale, fusion: Some(greedy(0.25)), ..StageRecipe::default() }
}
// ---------------------------------------------------------------------------
// Headline table
// ---------------------------------------------------------------------------

/// Today against the configurations the measurements point to, at the three
/// evaluation resolutions.
pub fn headline(lab: &Lab) {
    if lab.wants("stage") {
        let scene = make_scene(SceneKind::Stage);
        let variants = stage_variants();
        let chosen = [0usize, 1, 2, 14, 3, 4, 5, 12, 11, 6, 13, 7, 8];
        let clouds: Vec<Vec<StageCloud>> = chosen.iter().map(|&i| stage_clouds(scene.as_ref(), &variants[i].recipe, &STAGE_TIMES)).collect();
        println!("### Stage: today against the best combinations (both instants)\n\n{}", table_header_overdraw("configuration"));
        for &resolution in RESOLUTIONS.iter().rev() {
            let views = StageViews::new(lab, scene.as_ref(), &STAGE_TIMES, resolution);
            for (&i, clouds) in chosen.iter().zip(&clouds) {
                let (splats, wire) = stage_budget(clouds);
                let result = score_stage(&views, clouds, &variants[i].renderer);
                println!("{}", table_row_overdraw(variants[i].label, resolution, &splats, &wire, &result));
            }
            lab.note(&format!("stage headline at {resolution:?} done"));
        }
    }
    if lab.wants("house") {
        let bench = HouseBench::new(lab);
        let over = Renderer::default();
        let c1 = best_house_renderer();
        let voxel = bench.config.scan.voxel_size;
        let medium = bench.scan(lab, 1, 0.024, Sensor::Native);
        let fine = bench.scan(lab, 1, voxel / 2.0, Sensor::Native);
        let fine_sensor = bench.scan(lab, 2, voxel / 2.0, Sensor::Native);
        let variants: Vec<(&str, &[Splat], Renderer)> = vec![
            ("today: 192x144, 3.2 cm cells, sorted over", &bench.today.splats, over),
            ("same scan, C1", &bench.today.splats, hard_house_renderer()),
            ("same scan, C1 soft", &bench.today.splats, c1),
            ("same scan, C1 soft + C2", &bench.today.splats, with_planar(c1)),
            ("192x144, 2.4 cm cells, sorted over", &medium.splats, over),
            ("192x144, 2.4 cm cells, C1 soft + C2", &medium.splats, with_planar(c1)),
            ("192x144, 1.6 cm cells, sorted over", &fine.splats, over),
            ("192x144, 1.6 cm cells, C1 soft + C2", &fine.splats, with_planar(c1)),
            ("384x288, 1.6 cm cells, C1 soft + C2", &fine_sensor.splats, with_planar(c1)),
        ];
        println!("\n### House: today against the best combinations (all 24 held-out views)\n\n{}", table_header_overdraw("configuration"));
        for &resolution in RESOLUTIONS.iter().rev() {
            let views = bench.views(lab, resolution, false);
            for (label, splats, renderer) in &variants {
                let result = score_house(&views, splats, renderer);
                println!("{}", table_row_overdraw(label, resolution, &thousands(splats.len()), &bytes(splats.len() * WIRE), &result));
            }
            lab.note(&format!("house headline at {resolution:?} done"));
        }
    }
}

// ---------------------------------------------------------------------------
// Pictures
// ---------------------------------------------------------------------------

/// A named way of capturing, reconstructing and drawing the stage.
struct StageVariant {
    label: &'static str,
    recipe: StageRecipe,
    renderer: Renderer,
}

fn stage_variants() -> Vec<StageVariant> {
    let d = ReconstructParams::default();
    let base = StageRecipe::default();
    let fused = |scale: u32, sensor: Sensor| StageRecipe { scale, sensor, ..best_stage_recipe(1) };
    let guided = Sensor::CoarseDepth { factor: 2, mode: Upsampling::Guided { sigma_color: 15.0 } };
    let over = Renderer::default();
    let c1 = best_stage_renderer();
    let best = with_planar(c1_soft_renderer(0.5, c1.live_weight));
    let variant = |label: &'static str, recipe: StageRecipe, renderer: Renderer| StageVariant { label, recipe, renderer };
    let selected = StageRecipe { recon: ReconstructParams { footprint: 0.65, ..d }, fusion: Some(greedy(0.0)), ..base.clone() };
    vec![
        variant("today: 8x160x120, stacked, sorted over", base.clone(), over),
        variant("A1 best view only (footprint 0.65), sorted over", selected, over),
        variant("stacked, C1", base.clone(), c1),
        variant("A1 + C1", fused(1, Sensor::Native), c1),
        variant("A1 + C1 + C2", fused(1, Sensor::Native), with_planar(c1)),
        variant("A1 + C1 soft + C2", fused(1, Sensor::Native), best),
        variant("8x320x240, stacked, sorted over", StageRecipe { scale: 2, ..base.clone() }, over),
        variant("8x320x240: A1 + C1 soft + C2", fused(2, Sensor::Native), best),
        variant("colour 320x240, depth 160x120 guided: A1 + C1 soft + C2", fused(2, guided), best),
        variant("8x640x480: A1 + C1 soft + C2", fused(4, Sensor::Native), best),
        variant("8x640x480, stacked, sorted over", StageRecipe { scale: 4, ..base.clone() }, over),
        variant("8x240x180: A1 + C1 soft + C2", StageRecipe { size: Some((240, 180)), ..best_stage_recipe(1) }, best),
        variant("8x240x180, stacked, sorted over", StageRecipe { size: Some((240, 180)), ..base.clone() }, over),
        variant("8x320x240: A1 + C1", fused(2, Sensor::Native), c1),
        variant("stacked, C1 soft + C2", base, best),
    ]
}

/// `label  xx.xx dB  SSIM 0.xxx`.
fn caption(label: &str, score: &gs_core::experimental::metrics::Score) -> String {
    format!("{label}  {:.2} dB  SSIM {:.3}", score.psnr, score.ssim)
}

pub fn sheets(lab: &Lab) {
    use crate::harness::{crop_row, evaluate, panel_row};
    use gs_core::experimental::sheet::{shrink, stacked};
    use gs_core::raster::Image;
    let white = [255u8; 3];
    let save_with_preview = |name: &str, image: &Image| {
        lab.save(&format!("sheets/{name}.png"), image);
        lab.save(&format!("sheets/preview/{name}.png"), &shrink(image, 2));
    };

    if lab.wants("stage") {
        let scene = make_scene(SceneKind::Stage);
        let t = STAGE_TIMES[0];
        let views = StageViews::new(lab, scene.as_ref(), &[t], FULL);
        let variants = stage_variants();
        let clouds: Vec<StageCloud> = variants
            .iter()
            .map(|v| {
                lab.note(&format!("building {}", v.label));
                v.recipe.build(scene.as_ref(), t).1
            })
            .collect();
        let view_named = |name: &str| views.views.iter().find(|v| v.name == name).expect("no such view");
        let shot = |view: &EvalView, index: usize| {
            let cloud = &clouds[index];
            let renderer = &variants[index].renderer;
            let layers = [Layer { splats: &cloud.fixed, weight: 1.0 }, Layer { splats: &cloud.live, weight: renderer.live_weight }];
            let result = evaluate(view, &layers, &renderer.opts);
            (caption(variants[index].label, &result.score), result.image)
        };
        let panels = |view: &EvalView, which: &[usize]| -> Vec<(String, Image)> {
            let mut list = vec![("ground truth (ray cast)".to_string(), Image::from_frame(&view.reference))];
            list.extend(which.iter().map(|&i| shot(view, i)));
            list
        };
        // The two headline sheets: truth | today | best with today's sensors | best with the recommended sensors.
        for (file, view) in [("stage_normal", "ring0"), ("stage_closeup", "near"), ("stage_above", "above"), ("stage_graze", "graze")] {
            save_with_preview(file, &panel_row(&panels(view_named(view), &[0, 5, 7]), 3));
        }
        // One sheet per technique: the same crops before and after.
        type Rows<'a> = Vec<(&'a str, (u32, u32, u32, u32))>;
        let techniques: Vec<(&str, Vec<usize>, Rows)> = vec![
            ("tech_a1_c1_stage", vec![0, 1, 2, 3], vec![("ring0", (500, 300, 300, 220)), ("ring0", (200, 560, 300, 220)), ("above", (420, 330, 300, 220))]),
            ("tech_c1_silhouette_stage", vec![0, 4, 5], vec![("ring0", (420, 240, 400, 300)), ("closeup", (440, 300, 400, 300))]),
            ("tech_c2_stage", vec![0, 3, 4], vec![("graze", (340, 420, 400, 300)), ("near", (300, 200, 400, 300))]),
            ("tech_resolution_stage", vec![5, 8, 7, 9], vec![("ring0", (500, 300, 300, 220)), ("near", (300, 200, 300, 220))]),
            ("tech_resolution_today_renderer_stage", vec![0, 6, 10], vec![("ring0", (500, 300, 300, 220)), ("near", (300, 200, 300, 220))]),
        ];
        for (file, which, rows) in techniques {
            let strips: Vec<Image> = rows.iter().map(|(view, rect)| crop_row(&panels(view_named(view), &which), *rect, 2, 2)).collect();
            lab.save(&format!("sheets/{file}.png"), &stacked(&strips, 8, white));
        }
    }

    if lab.wants("house") {
        let bench = HouseBench::new(lab);
        let over = Renderer::default();
        let c1 = best_house_renderer();
        let voxel = bench.config.scan.voxel_size;
        let medium = bench.scan(lab, 1, 0.024, Sensor::Native);
        let fine = bench.scan(lab, 1, voxel / 2.0, Sensor::Native);
        let variants: Vec<(&str, &[Splat], Renderer)> = vec![
            ("today: 192x144, 3.2 cm cells, sorted over", &bench.today.splats, over),
            ("same scan, C1", &bench.today.splats, hard_house_renderer()),
            ("same scan, C1 soft", &bench.today.splats, c1),
            ("same scan, C1 soft + C2", &bench.today.splats, with_planar(c1)),
            ("2.4 cm cells, C1 soft + C2", &medium.splats, with_planar(c1)),
            ("1.6 cm cells, C1 soft + C2", &fine.splats, with_planar(c1)),
            ("1.6 cm cells, sorted over", &fine.splats, over),
        ];
        let points = house_viewpoints();
        let view_named = |name: &str| {
            let point = points.iter().find(|p| p.name == name).expect("no such view");
            lab.eval_view("house", point, FULL, &bench.snap, bench.config.shadows, Some(bench.exposures()))
        };
        let panels = |view: &EvalView, which: &[usize]| -> Vec<(String, Image)> {
            let mut list = vec![("ground truth (ray cast)".to_string(), Image::from_frame(&view.reference))];
            for &i in which {
                let (label, splats, renderer) = &variants[i];
                let result = evaluate(view, &[Layer { splats, weight: 1.0 }], &renderer.opts);
                list.push((caption(label, &result.score), result.image));
            }
            list
        };
        for (file, view) in [("house_normal", "living_a0"), ("house_closeup", "furniture"), ("house_graze", "graze"), ("house_kitchen", "kitchen_a1")] {
            save_with_preview(file, &panel_row(&panels(&view_named(view), &[0, 3, 4, 5]), 3));
        }
        type Rows<'a> = Vec<(&'a str, (u32, u32, u32, u32))>;
        let techniques: Vec<(&str, Vec<usize>, Rows)> = vec![
            ("tech_c1_house", vec![0, 1, 2], vec![("living_a0", (440, 330, 400, 300)), ("kitchen_a1", (440, 330, 400, 300)), ("furniture", (440, 330, 400, 300))]),
            ("tech_c2_house", vec![2, 3], vec![("graze", (440, 330, 400, 300)), ("furniture", (200, 200, 400, 300))]),
            ("tech_cells_house", vec![0, 6, 3, 5], vec![("furniture", (440, 330, 300, 220)), ("living_a0", (440, 330, 300, 220))]),
        ];
        for (file, which, rows) in techniques {
            let strips: Vec<Image> = rows.iter().map(|(view, rect)| crop_row(&panels(&view_named(view), &which), *rect, 2, 2)).collect();
            lab.save(&format!("sheets/{file}.png"), &stacked(&strips, 8, white));
        }
    }
}

// ---------------------------------------------------------------------------
// Exploration
// ---------------------------------------------------------------------------

/// Renders one stage view with a recipe and a renderer.
fn stage_picture(view: &EvalView, cloud: &StageCloud, renderer: &Renderer) -> (gs_core::raster::Image, f32) {
    let layers = [Layer { splats: &cloud.fixed, weight: 1.0 }, Layer { splats: &cloud.live, weight: renderer.live_weight }];
    let result = crate::harness::evaluate(view, &layers, &renderer.opts);
    (result.image, result.score.psnr)
}

/// Diagnostic pictures: truth, a few variants and their errors for the
/// stage views.
pub fn look(lab: &Lab) {
    use crate::harness::{crop_row, difference, panel_row};
    use gs_core::experimental::sheet::{shrink, stacked};
    use gs_core::raster::Image;
    let scene = make_scene(SceneKind::Stage);
    let t = STAGE_TIMES[0];
    let views = StageViews::new(lab, scene.as_ref(), &[t], FULL);
    let today = Renderer::default();
    let (base, n) = normalized_base();
    let fused = StageRecipe { fusion: Some(FusionParams::default()), ..StageRecipe::default() };
    let hard = Renderer { opts: WblendOptions { depth: DepthModel::Plane, ..with_blend(base, Normalized { vis_cutoff: 1.6, ..n }) }, live_weight: 16.0 };
    let soft = Renderer {
        opts: with_blend(base, Normalized { silhouette: Silhouette::SoftOverNextSurface, coverage: Coverage::Linear(0.5), ..n }),
        live_weight: 16.0,
    };
    let variants: Vec<(&str, StageRecipe, Renderer)> = vec![
        ("today", StageRecipe::default(), today),
        ("A1, C1 hard", fused.clone(), hard),
        ("A1, C1 soft", fused.clone(), soft),
        ("A1 exact normals, C1 hard", StageRecipe { exact_normals: true, ..fused.clone() }, hard),
    ];
    let clouds: Vec<StageCloud> = variants.iter().map(|(_, r, _)| r.build(scene.as_ref(), t).1).collect();
    for view in &views.views {
        let reference = Image::from_frame(&view.reference);
        let mut panels = vec![("ground truth".to_string(), reference.clone())];
        let mut errors = Vec::new();
        for ((name, _, renderer), cloud) in variants.iter().zip(&clouds) {
            let (image, psnr) = stage_picture(view, cloud, renderer);
            errors.push((format!("{name}: error x4"), difference(&reference, &image, 4)));
            panels.push((format!("{name} {psnr:.2} dB"), image));
        }
        let small = |list: &[(String, Image)]| -> Vec<(String, Image)> { list.iter().map(|(n, i)| (n.clone(), shrink(i, 2))).collect() };
        let mut rows = vec![panel_row(&small(&panels), 2)];
        errors.insert(0, ("".into(), Image::new(FULL.0, FULL.1, [0; 3])));
        rows.push(panel_row(&small(&errors), 2));
        rows.push(crop_row(&panels, (520, 330, 240, 180), 2, 2));
        rows.push(crop_row(&errors, (520, 330, 240, 180), 2, 2));
        lab.save(&format!("look/stage_{}.png", view.name), &stacked(&rows, 6, [255, 255, 255]));
    }
}

/// Does a renderer fatten or thin the outline of what moves on the stage?
/// The live layer is rendered alone and its coverage compared with the true
/// shape of everything that differs from the empty stage (the dancer, the
/// balls and their shadows), over the held-out views of both instants.
pub fn outline(lab: &Lab) {
    use gs_core::experimental::metrics::{outline_bias, OutlineBias};
    let scene = make_scene(SceneKind::Stage);
    let empty = scene.snapshot_static();
    let views = StageViews::new(lab, scene.as_ref(), &STAGE_TIMES, FULL);
    // The true shape per view.
    let truths: Vec<Vec<bool>> = views
        .views
        .iter()
        .map(|view| {
            let plate = lab.reference(&format!("stage_empty_{}", view.name), &empty, &view.cam, true);
            (0..plate.depth.len())
                .map(|i| {
                    let (a, b) = (view.reference.depth[i], plate.depth[i]);
                    let moved = (a > 0.0) != (b > 0.0) || (a - b).abs() > 0.01;
                    moved || (0..3).any(|c| view.reference.rgb[i * 3 + c].abs_diff(plate.rgb[i * 3 + c]) > 12)
                })
                .collect()
        })
        .collect();
    let (base, n) = normalized_base();
    let d = ReconstructParams::default();
    let fused = StageRecipe { fusion: Some(FusionParams::default()), ..StageRecipe::default() };
    let wide = StageRecipe { recon: ReconstructParams { footprint: 0.65, ..d }, ..fused.clone() };
    let hard = |vis: f32| WblendOptions { depth: DepthModel::Plane, ..with_blend(base, Normalized { vis_cutoff: vis, ..n }) };
    let soft = |tau: f32| with_blend(base, Normalized { silhouette: Silhouette::SoftOverNextSurface, coverage: Coverage::Linear(tau), ..n });
    let planar = |o: WblendOptions| WblendOptions { footprint: Footprint::Planar, depth: DepthModel::Plane, screen_floor_px: 0.5, ..o };
    let cases: Vec<(&str, StageRecipe, WblendOptions)> = vec![
        ("concatenated, sorted over (today)", StageRecipe::default(), WblendOptions::default()),
        ("A1 footprint 0.65, sorted over", wide.clone(), WblendOptions::default()),
        ("A1, C1 hard, visibility radius 2", fused.clone(), hard(2.0)),
        ("A1, C1 hard, visibility radius 1.6", fused.clone(), hard(1.6)),
        ("A1, C1 hard, visibility radius 1.2", fused.clone(), hard(1.2)),
        ("A1, C1 hard, visibility radius 1.6, planar", fused.clone(), planar(hard(1.6))),
        ("A1, C1 soft over next surface, W / 0.5", fused.clone(), soft(0.5)),
        ("A1, C1 soft over next surface, W / 0.8", fused.clone(), soft(0.8)),
        ("A1, C1 soft over next surface, W / 0.8, planar", fused.clone(), planar(soft(0.8))),
        ("concatenated, C1 hard, visibility radius 1.6", StageRecipe::default(), hard(1.6)),
    ];
    println!(
        "### Outline of the live layer at 1280x960 (held-out views, both instants)\n\nMean displacement of the outline in pixels of the 1280x960 view \
         (one sensor pixel is about 7 of them at the dancer): pixels covered outside the true shape, and true pixels left uncovered, per pixel of outline length.\n\n\
         | cloud, renderer | fattened by px | thinned by px | net px |\n|---|---|---|---|"
    );
    for (name, recipe, opts) in &cases {
        let clouds = stage_clouds(scene.as_ref(), recipe, &STAGE_TIMES);
        let mut total = OutlineBias::default();
        for (i, view) in views.views.iter().enumerate() {
            if view.kind != Kind::Held {
                continue;
            }
            let cloud = &clouds[views.instant[i]];
            let rendered = render(&[Layer { splats: &cloud.live, weight: 1.0 }], &view.cam, opts);
            total.add(&outline_bias(&truths[i], &rendered.alpha, view.cam.width as usize, view.cam.height as usize));
        }
        let (fat, thin) = total.displacement();
        println!("| {name} | {fat:.2} | {thin:.2} | {:+.2} |", fat - thin);
    }
}

/// Quality-weighted blending instead of best-view selection: every
/// observation stays, its opacity (the blending weight of the normalised
/// renderer) scaled by `(q / q_best)^p`.
fn explore_weights(lab: &Lab) {
    let scene = make_scene(SceneKind::Stage);
    let views = StageViews::new(lab, scene.as_ref(), &STAGE_TIMES, FULL);
    let base = StageRecipe::default();
    let soft = |power: f32| StageRecipe { fusion: Some(FusionParams { soft: Some(power), ..greedy(0.0) }), ..base.clone() };
    let recipes: Vec<(&str, StageRecipe)> = vec![
        ("stacked", base.clone()),
        ("weighted by q / q_best", soft(1.0)),
        ("weighted by (q / q_best)^2", soft(2.0)),
        ("weighted by (q / q_best)^4", soft(4.0)),
        ("A1 greedy, m = 0.5", StageRecipe { fusion: Some(greedy(0.5)), ..base.clone() }),
        ("A1 greedy, m = 0", StageRecipe { fusion: Some(greedy(0.0)), ..base.clone() }),
    ];
    let renderers: Vec<(&str, Renderer)> = vec![
        ("C1", c1_renderer(1.6, 1.0)),
        ("C1, visibility radius 1.2", c1_renderer(1.2, 1.0)),
        ("C1 + C2", with_planar(c1_renderer(1.6, 1.0))),
        ("C1 soft + C2", with_planar(c1_soft_renderer(0.5, 1.0))),
    ];
    println!("### Quality-weighted blending against best-view selection at 1280x960 (both instants)\n\n{}", table_header_overdraw("cloud: renderer"));
    for (recipe_name, recipe) in &recipes {
        let clouds = stage_clouds(scene.as_ref(), recipe, &STAGE_TIMES);
        let (splats, wire) = stage_budget(&clouds);
        for (renderer_name, renderer) in &renderers {
            let result = score_stage(&views, &clouds, renderer);
            println!("{}", table_row_overdraw(&format!("{recipe_name}: {renderer_name}"), FULL, &splats, &wire, &result));
        }
    }
}

/// Free-form exploration: recipes against renderers at 1280x960.
pub fn explore(lab: &Lab) {
    if lab.wants("weights") {
        explore_weights(lab);
        if !lab.wants("fringe") {
            return;
        }
    }
    let scene = make_scene(SceneKind::Stage);
    let views = StageViews::new(lab, scene.as_ref(), &STAGE_TIMES, FULL);
    let (base, n) = normalized_base();
    let d = ReconstructParams::default();
    let fused = StageRecipe { fusion: Some(FusionParams::default()), ..StageRecipe::default() };
    let recon = |r: ReconstructParams| StageRecipe { recon: r, ..fused.clone() };
    let recipes: Vec<(&str, StageRecipe)> = vec![
        ("A1", fused.clone()),
        ("A1, exact normals", StageRecipe { exact_normals: true, ..fused.clone() }),
        ("A1, max_anisotropy 2", recon(ReconstructParams { max_anisotropy: 2.0, ..d })),
        ("A1, min_cos 0.35", recon(ReconstructParams { min_cos: 0.35, ..d })),
        ("A1, edge_shrink 0.3", recon(ReconstructParams { edge_shrink: 0.3, ..d })),
        ("A1, edge_shrink 0.75", recon(ReconstructParams { edge_shrink: 0.75, ..d })),
        ("A1, footprint 0.38", recon(ReconstructParams { footprint: 0.38, ..d })),
        ("A1, footprint 0.55", recon(ReconstructParams { footprint: 0.55, ..d })),
        ("A1, smooth_passes 0", recon(ReconstructParams { smooth_passes: 0, ..d })),
        ("concatenated, exact normals", StageRecipe { exact_normals: true, ..StageRecipe::default() }),
    ];
    let plane = |params: Normalized| WblendOptions { depth: DepthModel::Plane, ..with_blend(base, params) };
    let two_sided = Normalized { near_fringe: false, ..n };
    let soft = Normalized { silhouette: Silhouette::SoftOverNextSurface, ..two_sided };
    let mut renderers: Vec<(String, Renderer)> = vec![("over".into(), Renderer::default())];
    for vis in [1.2, 1.6, 2.0] {
        for weight in [1.0, 16.0] {
            renderers.push((format!("C1 hard two-sided, vis {vis}, live weight {weight}"), Renderer { opts: plane(Normalized { vis_cutoff: vis, ..two_sided }), live_weight: weight }));
        }
    }
    for tau in [0.5, 0.8] {
        renderers.push((format!("C1 soft next two-sided, W / {tau}"), Renderer { opts: plane(Normalized { coverage: Coverage::Linear(tau), ..soft }), live_weight: 16.0 }));
    }
    renderers.push(("C1 hard one-sided, vis 1.6, live weight 16".into(), Renderer { opts: plane(Normalized { vis_cutoff: 1.6, ..n }), live_weight: 16.0 }));
    let recipes: Vec<(&str, StageRecipe)> = if lab.wants("fringe") { recipes.into_iter().filter(|(name, _)| *name == "A1" || name.contains("edge_shrink 0.3")).chain([("concatenated", StageRecipe::default())]).collect() } else { recipes };
    println!("### Exploration at 1280x960 (both instants)\n\n{}", table_header_overdraw("cloud: renderer"));
    for (recipe_name, recipe) in &recipes {
        let clouds = stage_clouds(scene.as_ref(), recipe, &STAGE_TIMES);
        let (splats, wire) = stage_budget(&clouds);
        for (renderer_name, renderer) in &renderers {
            let result = score_stage(&views, &clouds, renderer);
            println!("{}", table_row_overdraw(&format!("{recipe_name}: {renderer_name}"), FULL, &splats, &wire, &result));
        }
    }
}
