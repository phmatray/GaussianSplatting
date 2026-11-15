//! Novel-view check of the whole pipeline, with pictures.
//!
//! Runs the [`Pipeline`], decodes its messages like a viewer, draws the
//! reconstructed cloud from held-out cameras with the CPU reference of a
//! viewer's blending mode (normalised by default, `--sorted` for the
//! fallback) and compares it with the ray-cast scene (PSNR by region, SSIM,
//! coverage). Writes `reference | reconstruction | 4x difference` sheets as
//! PNG.
//!
//! ```text
//! cargo run -p gs-core --release --features par --example snap_recon -- <out dir> [options]
//!   --scene stage|house|both     what to run (default both)
//!   --patch '<json>'             partial PipelineConfig merged into the defaults,
//!                                e.g. '{"recon":{"footprint":0.8},"scan":{"voxel_size":0.04}}'
//!   --noise                      realistic sensor noise
//!   --light                      start from PipelineConfig::light
//!   --no-png                     numbers only
//!   --sorted                     draw with sorted "over" compositing and its splat scale
//!   --half-res                   held-out views of the stage at 320x240 instead of 640x480
//!   --previews-from <seconds>    where on the walk the preview strip starts (default 24)
//! ```
//!
//! For the house it also writes `house_previews.png`: the previews of every
//! sensor of the scan head as a viewer receives them while the scan runs in
//! real time, consecutive frames side by side (one row pair for the forward
//! sensor, one row for each of the others).

#[path = "../tests/common/mod.rs"]
mod common;

use std::path::PathBuf;
use std::time::Instant;

use common::{
    evaluate, house_views, mean, observed_mask, run_full_scan, save_png, side_by_side, stack, stage_views, top_view, Client,
    Exposures, Renderer,
};
use gs_core::capture::NoiseModel;
use gs_core::codec::{self, Body};
use gs_core::glam::Vec3;
use gs_core::pipeline::Pipeline;
use gs_core::protocol::{ControlMsg, PipelineConfig, ScanState};
use gs_core::raster::Image;
use gs_core::scene::house::{HouseOptions, HouseScene};
use gs_core::scene::{make_scene, Scene, SceneKind};

struct Options {
    out: PathBuf,
    scenes: Vec<SceneKind>,
    patch: Option<serde_json::Value>,
    noise: bool,
    light: bool,
    png: bool,
    renderer: Renderer,
    /// Divides the resolution of the held-out views.
    view_divisor: u32,
    /// Walking time at which the preview strip starts, seconds.
    previews_from: f32,
}

fn parse_args() -> Options {
    let mut options = Options {
        out: PathBuf::from("snap_recon_out"),
        scenes: vec![SceneKind::Stage, SceneKind::House],
        patch: None,
        noise: false,
        light: false,
        png: true,
        renderer: Renderer::Normalized,
        view_divisor: 1,
        previews_from: 24.0,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--scene" => {
                options.scenes = match args.next().as_deref() {
                    Some("stage") => vec![SceneKind::Stage],
                    Some("house") => vec![SceneKind::House],
                    _ => vec![SceneKind::Stage, SceneKind::House],
                }
            }
            "--patch" => {
                let text = args.next().expect("--patch needs a JSON object");
                options.patch = Some(serde_json::from_str(&text).expect("--patch is not valid JSON"));
            }
            "--noise" => options.noise = true,
            "--light" => options.light = true,
            "--no-png" => options.png = false,
            "--sorted" => options.renderer = Renderer::Sorted,
            "--half-res" => options.view_divisor = 2,
            "--previews-from" => {
                options.previews_from =
                    args.next().and_then(|v| v.parse().ok()).expect("--previews-from needs a time in seconds");
            }
            other => options.out = PathBuf::from(other),
        }
    }
    options
}

fn build(kind: SceneKind, options: &Options) -> Pipeline {
    let mut config = if options.light { PipelineConfig::light(kind) } else { PipelineConfig::for_scene(kind) };
    if options.noise {
        config.noise = NoiseModel::realistic();
    }
    // Scans are driven explicitly below.
    config.scan.autostart = false;
    let mut pipeline = Pipeline::new(config);
    if let Some(patch) = &options.patch {
        let json = serde_json::json!({ "type": "patch_config", "patch": patch }).to_string();
        pipeline.handle_control_json(&json).expect("the patch does not fit PipelineConfig");
    }
    pipeline
}

fn stage(options: &Options) {
    let start = Instant::now();
    let mut pipeline = build(SceneKind::Stage, options);
    let scene = make_scene(SceneKind::Stage);
    let mut client = Client::default();
    client.apply(&pipeline.snapshot());
    println!("stage: pipeline ready in {:.0} ms", start.elapsed().as_secs_f64() * 1e3);

    let mut views = stage_views();
    for view in &mut views {
        view.cam = view.cam.resized(view.cam.width / options.view_divisor, view.cam.height / options.view_divisor);
    }
    let mut all = Vec::new();
    let mut dynamic = Vec::new();
    // Three instants of the dance; each tick jumps straight to the next one.
    for (round, dt) in [1.3f32, 2.8, 3.6].into_iter().enumerate() {
        client.apply(&pipeline.tick(dt));
        let time = pipeline.scene_time();
        let snap = scene.snapshot(time);
        dynamic.push(client.layers[1].len() as f32);
        let mut sheets = Vec::new();
        for view in &views {
            let score = evaluate(&snap, &client.layers, &view.cam, pipeline.config().shadows, None, options.renderer);
            println!(
                "  t={time:4.1} {:8} psnr {:5.2} dB (interior {:4.1}, texture {:4.1}, silhouette {:4.1})  ssim {:.3}  coverage {:5.1} %",
                view.name,
                score.psnr,
                score.interior,
                score.texture,
                score.silhouette,
                score.ssim,
                100.0 * score.coverage
            );
            all.push(score.psnr);
            sheets.push(side_by_side(&score));
        }
        if options.png {
            save_png(&options.out.join(format!("stage_t{round}.png")), &stack(&sheets));
        }
    }
    let stats = *client.stats();
    println!(
        "RESULT stage renderer={} psnr_mean={:.2} psnr_min={:.2} static={} dynamic={:.0} tick_bytes={}",
        options.renderer.name(),
        mean(&all),
        all.iter().copied().fold(f32::INFINITY, f32::min),
        client.layers[0].len(),
        mean(&dynamic),
        stats.bytes,
    );
}

fn house(options: &Options) {
    let mut pipeline = build(SceneKind::House, options);
    let scene = make_scene(SceneKind::House);
    let snap = scene.snapshot(0.0);
    let mut client = Client::default();
    let start = Instant::now();
    let ticks = run_full_scan(&mut pipeline, &mut client);
    let scan_s = start.elapsed().as_secs_f64();
    let cloud = &client.layers[0];
    println!("house: {} splats after {ticks} ticks, {scan_s:.1} s, {} bytes on the wire", cloud.len(), client.bytes);

    let exposures = Exposures::of_scan(&snap, pipeline.config());
    let (mut psnrs, mut coverages, mut sheets) = (Vec::new(), Vec::new(), Vec::new());
    let (mut psnrs_observed, mut coverages_observed, mut observed) = (Vec::new(), Vec::new(), Vec::new());
    for view in house_views() {
        let mask = observed_mask(&snap, &exposures, &view.cam);
        let score = evaluate(&snap, &client.layers, &view.cam, pipeline.config().shadows, Some(&mask), options.renderer);
        println!(
            "  {:12} psnr {:5.2} dB  coverage {:5.1} %   where observed ({:5.1} %): psnr {:5.2} dB  coverage {:5.1} %",
            view.name,
            score.psnr,
            100.0 * score.coverage,
            100.0 * score.observed_fraction,
            score.psnr_observed,
            100.0 * score.coverage_observed
        );
        psnrs.push(score.psnr);
        coverages.push(score.coverage);
        psnrs_observed.push(score.psnr_observed);
        coverages_observed.push(score.coverage_observed);
        observed.push(score.observed_fraction);
        sheets.push(side_by_side(&score));
    }
    if options.png {
        for (i, chunk) in sheets.chunks(6).enumerate() {
            save_png(&options.out.join(format!("house_{i}.png")), &stack(chunk));
        }
        // Dollhouse view: ceiling clipped the way the viewer does in
        // miniature mode (15 cm below it, which also removes the beams that
        // hang 13 cm down), with the scan path drawn on the reconstruction.
        let state = client.state.as_ref().expect("no State received");
        let ceiling = state.ceiling_height.expect("the house reports its ceiling height");
        let open = HouseScene::with_options(HouseOptions { ceiling: false }).snapshot(0.0);
        let clipped: Vec<_> = cloud.iter().copied().filter(|s| s.pos[1] < ceiling - 0.15).collect();
        let top = top_view();
        let mut score = evaluate(&open, &[clipped, Vec::new()], &top.cam, pipeline.config().shadows, None, options.renderer);
        for p in &state.scan.as_ref().expect("the house reports its scan path").path {
            let Some((x, y, _)) = top.cam.project(Vec3::from_array(*p)) else { continue };
            let image = &mut score.reconstruction;
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let (x, y) = (x as i64 + dx, y as i64 + dy);
                if x >= 0 && y >= 0 && x < image.width as i64 && y < image.height as i64 {
                    let i = (y as usize * image.width as usize + x as usize) * 3;
                    image.rgb[i..i + 3].copy_from_slice(&[255, 0, 255]);
                }
            }
        }
        println!("  {:12} psnr {:5.2} dB  coverage {:5.1} %", top.name, score.psnr, 100.0 * score.coverage);
        save_png(&options.out.join("house_top.png"), &side_by_side(&score));
    }
    let min = |v: &[f32]| v.iter().copied().fold(f32::INFINITY, f32::min);
    println!(
        "RESULT house renderer={} psnr_mean={:.2} psnr_min={:.2} coverage_mean={:.2} coverage_min={:.2} observed={:.1} \
         psnr_obs_mean={:.2} psnr_obs_min={:.2} coverage_obs_mean={:.2} coverage_obs_min={:.2} splats={} scan_s={:.1}",
        options.renderer.name(),
        mean(&psnrs),
        min(&psnrs),
        100.0 * mean(&coverages),
        100.0 * min(&coverages),
        100.0 * mean(&observed),
        mean(&psnrs_observed),
        min(&psnrs_observed),
        100.0 * mean(&coverages_observed),
        100.0 * min(&coverages_observed),
        cloud.len(),
        scan_s,
    );
    // Leave the pipeline in a defined state for anyone extending this.
    pipeline.handle_control(ControlMsg::ScanPause);
}

/// Frames per row of the preview strip.
const STRIP_COLUMNS: usize = 8;

/// Images of equal height side by side, padded with black to `columns`.
fn row_of(images: &[Image], columns: usize) -> Image {
    let (w, h) = images.first().map_or((0, 0), |i| (i.width as usize, i.height as usize));
    let mut out = Image::new((w * columns) as u32, h as u32, [0, 0, 0]);
    for (k, image) in images.iter().take(columns).enumerate() {
        for y in 0..h {
            let dst = (y * w * columns + k * w) * 3;
            out.rgb[dst..dst + w * 3].copy_from_slice(&image.rgb[y * w * 3..(y + 1) * w * 3]);
        }
    }
    out
}

/// Walks the scan in real time, like a host does, and collects the previews
/// each sensor sends from `previews_from` on.
fn house_previews(options: &Options) {
    let mut pipeline = build(SceneKind::House, options);
    let mut client = Client::default();
    client.apply(&pipeline.snapshot());
    client.apply(&pipeline.handle_control(ControlMsg::ScanStart));
    let dt = 1.0 / pipeline.config().target_hz;
    let sensors = pipeline.config().scan.head.sensor_count() as usize;
    // Two rows for the forward sensor, which the viewer shows large.
    let wanted = |sensor: usize| if sensor == 0 { 2 * STRIP_COLUMNS } else { STRIP_COLUMNS };
    let mut strips: Vec<Vec<Image>> = vec![Vec::new(); sensors];
    let mut spans = vec![(f32::NAN, f32::NAN); sensors];
    while pipeline.scan_state() == Some(ScanState::Scanning) && (0..sensors).any(|k| strips[k].len() < wanted(k)) {
        let out = pipeline.tick(dt);
        let time = pipeline.scene_time();
        for message in &out {
            let Ok(codec::Message { body: Body::Preview(preview), .. }) = codec::decode(&message.bytes) else { continue };
            let k = preview.camera_id as usize;
            if time < options.previews_from || k >= sensors || strips[k].len() >= wanted(k) {
                continue;
            }
            if strips[k].is_empty() {
                spans[k].0 = time;
            }
            spans[k].1 = time;
            strips[k].push(Image { width: preview.width as u32, height: preview.height as u32, rgb: preview.rgb });
        }
        client.apply(&out);
    }
    let mut rows = Vec::new();
    for (k, strip) in strips.iter().enumerate().filter(|(_, strip)| !strip.is_empty()) {
        println!(
            "  previews of sensor {k}: {} consecutive frames, walking time {:.2} s to {:.2} s",
            strip.len(),
            spans[k].0,
            spans[k].1
        );
        rows.extend(strip.chunks(STRIP_COLUMNS).map(|chunk| row_of(chunk, STRIP_COLUMNS)));
    }
    if rows.is_empty() {
        // Previews switched off by a patch, or a start beyond the walk.
        println!("  no previews after {:.1} s of walking: house_previews.png not written", options.previews_from);
        return;
    }
    save_png(&options.out.join("house_previews.png"), &stack(&rows));
}

fn main() {
    let options = parse_args();
    for kind in options.scenes.clone() {
        match kind {
            SceneKind::Stage => stage(&options),
            SceneKind::House => {
                house(&options);
                if options.png {
                    house_previews(&options);
                }
            }
        }
    }
    if options.png {
        println!("pictures in {}", options.out.display());
    }
}
