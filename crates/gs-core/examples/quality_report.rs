//! Before/after instrument of the reconstruction quality work.
//!
//! It judges what a viewer receives. `dump` runs the real [`Pipeline`] for
//! every scenario (stage and house, default and light presets, with and
//! without sensor noise) and stores the decoded layers on disk under a
//! label. `score` and `sheets` then draw the stored clouds of any label with
//! the same renderers, against the same ray-cast ground truth: two states of
//! the code are compared with one instrument, whatever changed in between.
//!
//! ```text
//! cargo run -p gs-core --release --features par --example quality_report -- dump   <dir> <label> [--only <text>]
//! cargo run -p gs-core --release --features par --example quality_report -- score  <dir> <label> [--only <text>]
//!       [--res 1280x960] [--variants sorted:1,soft:1,hard:1] [--every <n>] [--no-temporal]
//! cargo run -p gs-core --release --features par --example quality_report -- sheets <dir> <before> <after>
//!       [--before sorted:1] [--after sorted:1,soft:1]
//! ```
//!
//! A variant is `<mode>:<splat scale>`: `sorted` is back-to-front "over"
//! compositing, `soft` and `hard` are the two quality levels of the
//! normalised mode (see "Blending modes" in `docs/ARCHITECTURE.md`); the
//! splat scale multiplies every standard deviation, as a viewer's splat
//! scale setting does.
//!
//! The temporal metric compares consecutive ticks drawn from one fixed
//! held-out view with the change of the ground truth between the same two
//! instants: what remains is flicker.

#[path = "../tests/common/mod.rs"]
mod common;
#[path = "quality_lab/harness.rs"]
mod harness;

use std::path::{Path, PathBuf};

use common::{run_full_scan, Client, BACKGROUND};
use gs_core::capture::{render_rgbd, CaptureOptions, NoiseModel};
use gs_core::experimental::lab::ScanExposures;
use gs_core::experimental::metrics::{classify, MaskParams};
use gs_core::experimental::sheet::{shrink, stacked};
use gs_core::experimental::wblend::{render, Blend, Layer, WblendOptions, LIVE_LAYER_WEIGHT};
use gs_core::pipeline::{scan_frame, scan_frame_count, scan_sensor, Pipeline};
use gs_core::protocol::{ControlMsg, PipelineConfig};
use gs_core::raster::Image;
use gs_core::rig::ScanPath;
use gs_core::scene::{make_scene, SceneKind, SceneSnapshot};
use gs_core::Splat;
use harness::{
    crop_row, evaluate, evaluate_set, house_viewpoints, panel_row, stage_viewpoints, table_header, table_row, EvalView, Kind, Lab,
    Viewpoint,
};

/// The two instants of the dance the stage is judged at (as in the lab).
const STAGE_TIMES: [f32; 2] = [1.3, 4.1];
/// Ticks of a temporal sequence and their spacing in seconds.
const SEQUENCE: usize = 6;
const SEQUENCE_DT: f32 = 1.0 / 30.0;
/// Resolution of the temporal metric.
const TEMPORAL_RES: (u32, u32) = (640, 480);
/// Bytes of one splat on the wire.
const WIRE: usize = 16;

// ---------------------------------------------------------------------------
// Scenarios and their files
// ---------------------------------------------------------------------------

struct Scenario {
    name: String,
    scene: SceneKind,
    config: PipelineConfig,
}

fn scenarios() -> Vec<Scenario> {
    let mut list = Vec::new();
    for scene in [SceneKind::Stage, SceneKind::House] {
        for (preset, base) in [("default", PipelineConfig::for_scene(scene)), ("light", PipelineConfig::light(scene))] {
            for noisy in [false, true] {
                let mut config = base;
                if noisy {
                    config.noise = NoiseModel::realistic();
                }
                // The scan is driven by `ScanFinish`; previews are not judged.
                config.scan.autostart = false;
                config.preview.enabled = false;
                let scene_name = if scene == SceneKind::Stage { "stage" } else { "house" };
                let name = format!("{scene_name}_{preset}_{}", if noisy { "noisy" } else { "clean" });
                list.push(Scenario { name, scene, config });
            }
        }
    }
    list
}

fn write_splats(path: &Path, splats: &[Splat]) {
    std::fs::write(path, bytemuck::cast_slice::<Splat, u8>(splats)).expect("cannot write splats");
}

fn read_splats(path: &Path) -> Vec<Splat> {
    let bytes = std::fs::read(path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    bytes.chunks_exact(std::mem::size_of::<Splat>()).map(bytemuck::pod_read_unaligned).collect()
}

fn read_meta(dir: &Path) -> serde_json::Value {
    let text = std::fs::read_to_string(dir.join("meta.json")).unwrap_or_else(|e| panic!("no dump in {}: {e}", dir.display()));
    serde_json::from_str(&text).expect("meta.json is not JSON")
}

fn times_of(meta: &serde_json::Value, key: &str) -> Vec<f32> {
    meta[key].as_array().map(|a| a.iter().filter_map(|v| v.as_f64()).map(|v| v as f32).collect()).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// dump
// ---------------------------------------------------------------------------

/// A stage pipeline ticked once to scene time `t`, and its client.
fn stage_at(config: PipelineConfig, t: f32) -> (Pipeline, Client) {
    let mut pipeline = Pipeline::new(config);
    let mut client = Client::default();
    client.apply(&pipeline.snapshot());
    client.apply(&pipeline.tick(t));
    (pipeline, client)
}

fn dump_stage(dir: &Path, config: PipelineConfig) {
    let mut meta = serde_json::Map::new();
    let (mut times, mut live_counts) = (Vec::new(), Vec::new());
    for (k, &t) in STAGE_TIMES.iter().enumerate() {
        let (pipeline, client) = stage_at(config, t);
        if k == 0 {
            write_splats(&dir.join("static.splats"), &client.layers[0]);
            meta.insert("config".into(), serde_json::to_value(pipeline.config()).unwrap());
            meta.insert("static".into(), client.layers[0].len().into());
        }
        write_splats(&dir.join(format!("live_{k}.splats")), &client.layers[1]);
        times.push(pipeline.scene_time());
        live_counts.push(client.layers[1].len());
    }
    meta.insert("times".into(), times.into());
    meta.insert("live".into(), live_counts.into());

    // The scene moving: consecutive ticks from the second instant on.
    let (mut pipeline, mut client) = stage_at(config, STAGE_TIMES[1]);
    let mut moving = Vec::new();
    for k in 0..SEQUENCE {
        if k > 0 {
            client.apply(&pipeline.tick(SEQUENCE_DT));
        }
        write_splats(&dir.join(format!("moving_{k}.splats")), &client.layers[1]);
        moving.push(pipeline.scene_time());
    }
    meta.insert("moving".into(), moving.into());

    // The scene frozen while the sensor noise goes on (a clean sensor then
    // repeats itself exactly: nothing to measure).
    if !config.noise.is_none() {
        let (mut pipeline, mut client) = stage_at(config, STAGE_TIMES[1]);
        client.apply(&pipeline.handle_control(ControlMsg::SetTimeScale { time_scale: 0.0 }));
        let mut frozen = Vec::new();
        for k in 0..SEQUENCE {
            client.apply(&pipeline.tick(SEQUENCE_DT));
            write_splats(&dir.join(format!("frozen_{k}.splats")), &client.layers[1]);
            frozen.push(pipeline.scene_time());
        }
        meta.insert("frozen".into(), frozen.into());
    }
    std::fs::write(dir.join("meta.json"), serde_json::to_string_pretty(&meta).unwrap()).expect("cannot write meta.json");
}

fn dump_house(dir: &Path, config: PipelineConfig) {
    let mut pipeline = Pipeline::new(config);
    let mut client = Client::default();
    let ticks = run_full_scan(&mut pipeline, &mut client);
    write_splats(&dir.join("static.splats"), &client.layers[0]);
    let meta = serde_json::json!({
        "config": pipeline.config(),
        "static": client.layers[0].len(),
        "ticks": ticks,
    });
    std::fs::write(dir.join("meta.json"), serde_json::to_string_pretty(&meta).unwrap()).expect("cannot write meta.json");
}

fn dump(lab: &Lab, label: &str, only: Option<&str>) {
    for scenario in scenarios().into_iter().filter(|s| only.is_none_or(|o| s.name.contains(o))) {
        let dir = lab.out.join(label).join(&scenario.name);
        std::fs::create_dir_all(&dir).expect("cannot create the dump directory");
        match scenario.scene {
            SceneKind::Stage => dump_stage(&dir, scenario.config),
            SceneKind::House => dump_house(&dir, scenario.config),
        }
        lab.note(&format!("dumped {label}/{}", scenario.name));
    }
}

// ---------------------------------------------------------------------------
// Renderer variants
// ---------------------------------------------------------------------------

/// One way of drawing a cloud: a blending mode and a splat scale.
#[derive(Clone)]
struct Variant {
    name: String,
    opts: WblendOptions,
}

impl Variant {
    /// Parses `sorted:1.44`, `soft:1` or `hard:0.75`.
    fn parse(text: &str) -> Self {
        let (mode, scale) = text.split_once(':').unwrap_or((text, "1"));
        let scale: f32 = scale.parse().unwrap_or_else(|_| panic!("bad splat scale in {text:?}"));
        let base = match mode {
            "sorted" => WblendOptions::default(),
            "soft" => WblendOptions::normalized_soft(),
            "hard" => WblendOptions::normalized_hard(),
            other => panic!("unknown renderer {other:?} (sorted, soft, hard)"),
        };
        let name = match mode {
            "sorted" => format!("sorted x{scale}"),
            other => format!("normalised {other} x{scale}"),
        };
        Self { name, opts: WblendOptions { kernel_scale: scale, ..base } }
    }

    fn list(text: &str) -> Vec<Self> {
        text.split(',').filter(|t| !t.is_empty()).map(Self::parse).collect()
    }

    /// The two layers of a stage cloud with the weights of this mode.
    fn layers<'a>(&self, fixed: &'a [Splat], live: &'a [Splat]) -> Vec<Layer<'a>> {
        let live_weight = if matches!(self.opts.blend, Blend::Normalized(_)) { LIVE_LAYER_WEIGHT } else { 1.0 };
        vec![Layer { splats: fixed, weight: 1.0 }, Layer { splats: live, weight: live_weight }]
    }
}

// ---------------------------------------------------------------------------
// Evaluation views
// ---------------------------------------------------------------------------

/// An evaluation view whose reference is shared by every label
/// (`reference_key`) while the observation mask belongs to one scan
/// (`observed_key`).
fn eval_view(
    lab: &Lab,
    reference_key: &str,
    observed: Option<(&str, &ScanExposures)>,
    point: &Viewpoint,
    resolution: (u32, u32),
    snap: &SceneSnapshot,
) -> EvalView {
    let cam = point.cam.resized(resolution.0, resolution.1);
    let reference = lab.reference(&format!("{reference_key}_{}", point.name), snap, &cam, true);
    let mask = observed.map(|(key, exposures)| lab.observed(&format!("{key}_{}", point.name), snap, exposures, &cam));
    let params = MaskParams::default();
    EvalView {
        name: point.name.clone(),
        kind: point.kind,
        regions: classify(&reference, mask.as_deref(), &params.scaled_to(cam.width)),
        regions_2px: classify(&reference, mask.as_deref(), &params),
        cam,
        reference,
    }
}

/// Re-takes every sensor frame of the scan described by `config`, without
/// noise: which surface the scan has looked at.
fn scan_exposures(snap: &SceneSnapshot, config: &PipelineConfig) -> ScanExposures {
    let scan = &config.scan;
    let path = ScanPath::house_default().with_speed(scan.speed);
    let opts = CaptureOptions { shadows: config.shadows, ..CaptureOptions::default() };
    let frames = (0..scan_frame_count(&path, scan))
        .map(|i| {
            let shot = scan_frame(&path, scan, i);
            let cam = scan_sensor(&path, shot.time, scan, shot.sensor);
            let depth = render_rgbd(snap, &cam, &opts).depth;
            (cam, depth)
        })
        .collect();
    ScanExposures { frames, max_range: scan.max_range }
}

fn config_of(meta: &serde_json::Value) -> PipelineConfig {
    serde_json::from_value(meta["config"].clone()).expect("meta.json holds no configuration")
}

// ---------------------------------------------------------------------------
// score
// ---------------------------------------------------------------------------

struct ScoreArgs {
    resolution: (u32, u32),
    variants: Vec<Variant>,
    /// Use every n-th held-out house view.
    every: usize,
    temporal: bool,
}

fn luma(image: &Image, i: usize) -> f32 {
    0.299 * image.rgb[i * 3] as f32 + 0.587 * image.rgb[i * 3 + 1] as f32 + 0.114 * image.rgb[i * 3 + 2] as f32
}

/// Flicker of a sequence of pictures against the ground truth of the same
/// instants: `(rms change of the truth, rms change of the pictures, rms of
/// the change the truth does not explain, share of the pixels where that
/// unexplained change exceeds 10 levels)`, in 8-bit levels of luma, over all
/// consecutive pairs.
fn flicker(truth: &[Image], pictures: &[Image]) -> (f32, f32, f32, f32) {
    let (mut truth_sum, mut picture_sum, mut excess_sum, mut jumps, mut count) = (0.0f64, 0.0f64, 0.0f64, 0u64, 0u64);
    for k in 1..pictures.len() {
        for i in 0..(pictures[k].width * pictures[k].height) as usize {
            let truth_change = luma(&truth[k], i) - luma(&truth[k - 1], i);
            let change = luma(&pictures[k], i) - luma(&pictures[k - 1], i);
            let excess = change - truth_change;
            truth_sum += (truth_change * truth_change) as f64;
            picture_sum += (change * change) as f64;
            excess_sum += (excess * excess) as f64;
            jumps += u64::from(excess.abs() > 10.0);
            count += 1;
        }
    }
    let rms = |sum: f64| (sum / count.max(1) as f64).sqrt() as f32;
    (rms(truth_sum), rms(picture_sum), rms(excess_sum), jumps as f32 / count.max(1) as f32)
}

const TEMPORAL_HEADER: &str = "| cloud | renderer | sequence | truth change rms | picture change rms | flicker rms | pixels jumping > 10 levels % |\n|---|---|---|---|---|---|---|";

fn score_temporal(lab: &Lab, row: &str, dir: &Path, meta: &serde_json::Value, fixed: &[Splat], variants: &[Variant], out: &mut Vec<String>) {
    let scene = make_scene(SceneKind::Stage);
    for sequence in ["moving", "frozen"] {
        let times = times_of(meta, sequence);
        if times.is_empty() {
            continue;
        }
        let point = &stage_viewpoints(times[0])[0];
        let cam = point.cam.resized(TEMPORAL_RES.0, TEMPORAL_RES.1);
        let truth: Vec<Image> = times
            .iter()
            .map(|&t| Image::from_frame(&lab.reference(&format!("stage_t{t}_{}", point.name), &scene.snapshot(t), &cam, true)))
            .collect();
        let lives: Vec<Vec<Splat>> = (0..times.len()).map(|k| read_splats(&dir.join(format!("{sequence}_{k}.splats")))).collect();
        for variant in variants {
            let pictures: Vec<Image> =
                lives.iter().map(|live| render(&variant.layers(fixed, live), &cam, &variant.opts).over(BACKGROUND)).collect();
            let (truth_rms, picture_rms, excess, jumps) = flicker(&truth, &pictures);
            out.push(format!(
                "| {row} | {} | {sequence} | {truth_rms:.2} | {picture_rms:.2} | {excess:.2} | {:.2} |",
                variant.name,
                100.0 * jumps
            ));
        }
    }
}

fn score_stage(lab: &Lab, row: &str, dir: &Path, args: &ScoreArgs, temporal: &mut Vec<String>) {
    let meta = read_meta(dir);
    let config = config_of(&meta);
    let scene = make_scene(SceneKind::Stage);
    let fixed = read_splats(&dir.join("static.splats"));
    let times = times_of(&meta, "times");
    let lives: Vec<Vec<Splat>> = (0..times.len()).map(|k| read_splats(&dir.join(format!("live_{k}.splats")))).collect();
    let (mut views, mut instant) = (Vec::new(), Vec::new());
    for (k, &t) in times.iter().enumerate() {
        let snap = scene.snapshot(t);
        for point in stage_viewpoints(t) {
            views.push(eval_view(lab, &format!("stage_t{t}"), None, &point, args.resolution, &snap));
            instant.push(k);
        }
    }
    let live_mean = lives.iter().map(Vec::len).sum::<usize>() / lives.len().max(1);
    let splats = format!("{} + {}", harness::thousands(fixed.len()), harness::thousands(live_mean));
    let wire = format!(
        "{} + {}/tick ({:.1} MB/s at {} Hz)",
        harness::bytes(fixed.len() * WIRE),
        harness::bytes(live_mean * WIRE),
        (live_mean * WIRE) as f64 * config.target_hz as f64 / 1e6,
        config.target_hz
    );
    for variant in &args.variants {
        let layers = |i: usize| variant.layers(&fixed, &lives[instant[i]]);
        let result = evaluate_set(&views, &layers, &variant.opts);
        println!("{}", table_row(&format!("{row}, {}", variant.name), args.resolution, &splats, &wire, &result));
    }
    if args.temporal {
        score_temporal(lab, row, dir, &meta, &fixed, &args.variants, temporal);
    }
}

/// The evaluation views of a stored house scan.
fn house_views(lab: &Lab, observed_key: &str, config: &PipelineConfig, resolution: (u32, u32), every: usize) -> Vec<EvalView> {
    let snap = make_scene(SceneKind::House).snapshot(0.0);
    let exposures = scan_exposures(&snap, config);
    let mut held = 0;
    house_viewpoints()
        .iter()
        .filter(|point| {
            if point.kind != Kind::Held {
                return true;
            }
            held += 1;
            (held - 1) % every.max(1) == 0
        })
        .map(|point| eval_view(lab, "house", Some((observed_key, &exposures)), point, resolution, &snap))
        .collect()
}

fn score_house(lab: &Lab, row: &str, observed_key: &str, dir: &Path, args: &ScoreArgs) {
    let meta = read_meta(dir);
    let config = config_of(&meta);
    let splats = read_splats(&dir.join("static.splats"));
    let views = house_views(lab, observed_key, &config, args.resolution, args.every);
    let count = harness::thousands(splats.len());
    let wire = harness::bytes(splats.len() * WIRE);
    for variant in &args.variants {
        let layers = |_: usize| vec![Layer { splats: &splats, weight: 1.0 }];
        let result = evaluate_set(&views, &layers, &variant.opts);
        println!("{}", table_row(&format!("{row}, {}", variant.name), args.resolution, &count, &wire, &result));
    }
}

fn score(lab: &Lab, label: &str, only: Option<&str>, args: &ScoreArgs) {
    println!("{}", table_header("cloud, renderer"));
    let mut temporal = Vec::new();
    for scenario in scenarios().into_iter().filter(|s| only.is_none_or(|o| s.name.contains(o))) {
        let dir = lab.out.join(label).join(&scenario.name);
        if !dir.join("meta.json").exists() {
            continue;
        }
        let row = format!("{label} {}", scenario.name);
        match scenario.scene {
            SceneKind::Stage => score_stage(lab, &row, &dir, args, &mut temporal),
            SceneKind::House => score_house(lab, &row, &format!("{label}_{}", scenario.name), &dir, args),
        }
        lab.note(&format!("scored {row}"));
    }
    if !temporal.is_empty() {
        println!("\n{TEMPORAL_HEADER}");
        for line in temporal {
            println!("{line}");
        }
    }
}

// ---------------------------------------------------------------------------
// sheets
// ---------------------------------------------------------------------------

/// Crops `(x, y, width, height)` at 1280x960 shown magnified under a view.
type Crops = &'static [(u32, u32, u32, u32)];

fn save_sheet(lab: &Lab, name: &str, panels: &[(String, Image)], crops: Crops) {
    let mut rows = vec![panel_row(panels, 3)];
    let full_width = rows[0].width;
    for &rect in crops {
        // Magnified so that a row of crops is about as wide as the panels.
        let factor = (full_width / (panels.len() as u32 * rect.2)).max(1);
        rows.push(crop_row(panels, rect, factor, 2));
    }
    let sheet = stacked(&rows, 8, [255, 255, 255]);
    lab.save(&format!("sheets/{name}.png"), &sheet);
    lab.save(&format!("sheets/preview/{name}.png"), &shrink(&sheet, 2));
}

fn caption(label: &str, variant: &Variant, score: &gs_core::experimental::metrics::Score) -> String {
    format!("{label}, {}  {:.2} dB  SSIM {:.3}", variant.name, score.psnr, score.ssim)
}

fn sheets(lab: &Lab, before: &str, after: &str, before_variants: &[Variant], after_variants: &[Variant], only: Option<&str>) {
    const FULL: (u32, u32) = (1280, 960);
    std::fs::create_dir_all(lab.out.join("sheets/preview")).expect("cannot create the sheet directory");
    let sides = [(before, "before", before_variants), (after, "after", after_variants)];

    if only.is_none_or(|o| "stage".contains(o)) {
        let scene = make_scene(SceneKind::Stage);
        let scenario = "stage_default_clean";
        let stage_sheets: [(&str, &str, Crops); 3] = [
            ("stage_standard", "ring0", &[(500, 280, 320, 240), (200, 560, 320, 240)]),
            ("stage_closeup", "near", &[(300, 200, 320, 240), (640, 480, 320, 240)]),
            ("stage_grazing", "graze", &[(340, 420, 320, 240), (700, 300, 320, 240)]),
        ];
        for (file, view_name, crops) in stage_sheets {
            let mut panels: Vec<(String, Image)> = Vec::new();
            for (label, side, variants) in sides {
                let dir = lab.out.join(label).join(scenario);
                let meta = read_meta(&dir);
                let t = times_of(&meta, "times")[0];
                let point = stage_viewpoints(t).into_iter().find(|p| p.name == view_name).expect("no such stage view");
                let view = eval_view(lab, &format!("stage_t{t}"), None, &point, FULL, &scene.snapshot(t));
                if panels.is_empty() {
                    panels.push(("ground truth (ray cast)".into(), Image::from_frame(&view.reference)));
                }
                let (fixed, live) = (read_splats(&dir.join("static.splats")), read_splats(&dir.join("live_0.splats")));
                for variant in variants {
                    let result = evaluate(&view, &variant.layers(&fixed, &live), &variant.opts);
                    panels.push((caption(side, variant, &result.score), result.image));
                }
            }
            save_sheet(lab, file, &panels, crops);
        }
    }

    if only.is_none_or(|o| "house".contains(o)) {
        let snap = make_scene(SceneKind::House).snapshot(0.0);
        let scenario = "house_default_clean";
        let house_sheets: [(&str, &str, Crops); 3] = [
            ("house_standard", "living_a0", &[(440, 330, 320, 240), (820, 500, 320, 240)]),
            ("house_closeup", "furniture", &[(440, 330, 320, 240), (200, 200, 320, 240)]),
            ("house_grazing", "graze", &[(440, 330, 320, 240), (760, 420, 320, 240)]),
        ];
        let points = house_viewpoints();
        let clouds: Vec<(Vec<Splat>, ScanExposures)> = sides
            .iter()
            .map(|(label, ..)| {
                let dir = lab.out.join(label).join(scenario);
                (read_splats(&dir.join("static.splats")), scan_exposures(&snap, &config_of(&read_meta(&dir))))
            })
            .collect();
        for (file, view_name, crops) in house_sheets {
            let point = points.iter().find(|p| p.name == view_name).expect("no such house view");
            let mut panels: Vec<(String, Image)> = Vec::new();
            for ((label, side, variants), (splats, exposures)) in sides.iter().zip(&clouds) {
                let key = format!("{label}_{scenario}");
                let view = eval_view(lab, "house", Some((&key, exposures)), point, FULL, &snap);
                if panels.is_empty() {
                    panels.push(("ground truth (ray cast)".into(), Image::from_frame(&view.reference)));
                }
                for variant in variants.iter() {
                    let result = evaluate(&view, &[Layer { splats, weight: 1.0 }], &variant.opts);
                    panels.push((caption(side, variant, &result.score), result.image));
                }
            }
            save_sheet(lab, file, &panels, crops);
        }
    }
}

// ---------------------------------------------------------------------------

fn option<'a>(rest: &'a [String], name: &str) -> Option<&'a str> {
    rest.iter().position(|a| a == name).and_then(|i| rest.get(i + 1)).map(String::as_str)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let usage = "usage: quality_report dump|score|sheets <dir> <label> [<after label>] [options]";
    let (Some(command), Some(dir), Some(label)) = (args.first(), args.get(1), args.get(2)) else {
        eprintln!("{usage}");
        return;
    };
    let rest = &args[3..];
    let only = option(rest, "--only");
    let lab = Lab::new(PathBuf::from(dir), false, None);
    match command.as_str() {
        "dump" => dump(&lab, label, only),
        "score" => {
            let resolution = option(rest, "--res").map_or((1280, 960), |text| {
                let (w, h) = text.split_once('x').expect("--res wants <width>x<height>");
                (w.parse().expect("bad width"), h.parse().expect("bad height"))
            });
            let score_args = ScoreArgs {
                resolution,
                variants: Variant::list(option(rest, "--variants").unwrap_or("sorted:1,soft:1,hard:1")),
                every: option(rest, "--every").map_or(1, |n| n.parse().expect("bad --every")),
                temporal: !rest.iter().any(|a| a == "--no-temporal"),
            };
            score(&lab, label, only, &score_args);
        }
        "sheets" => {
            let after = rest.first().expect("sheets wants a before and an after label");
            let before_variants = Variant::list(option(rest, "--before").unwrap_or("sorted:1"));
            let after_variants = Variant::list(option(rest, "--after").unwrap_or("sorted:1,soft:1"));
            sheets(&lab, label, after, &before_variants, &after_variants, only);
        }
        other => eprintln!("unknown command {other:?}\n{usage}"),
    }
}
