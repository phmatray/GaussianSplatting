//! Novel-view check of the whole pipeline, with pictures.
//!
//! Runs the [`Pipeline`], decodes its messages like a viewer, renders the
//! reconstructed cloud from held-out cameras with the CPU rasterizer and
//! compares it with the ray-cast scene (PSNR, coverage). Writes
//! `reference | reconstruction | 4x difference` sheets as PNG.
//!
//! ```text
//! cargo run -p gs-core --release --features par --example snap_recon -- <out dir> [options]
//!   --scene stage|house|both     what to run (default both)
//!   --patch '<json>'             partial PipelineConfig merged into the defaults,
//!                                e.g. '{"recon":{"footprint":0.8},"scan":{"voxel_size":0.04}}'
//!   --noise                      realistic sensor noise
//!   --light                      start from PipelineConfig::light
//!   --no-png                     numbers only
//! ```

#[path = "../tests/common/mod.rs"]
mod common;

use std::path::PathBuf;
use std::time::Instant;

use common::{evaluate, house_views, mean, run_full_scan, save_png, side_by_side, stack, stage_views, Client};
use gs_core::capture::NoiseModel;
use gs_core::pipeline::Pipeline;
use gs_core::protocol::{ControlMsg, PipelineConfig};
use gs_core::scene::{make_scene, SceneKind};

struct Options {
    out: PathBuf,
    scenes: Vec<SceneKind>,
    patch: Option<serde_json::Value>,
    noise: bool,
    light: bool,
    png: bool,
}

fn parse_args() -> Options {
    let mut options = Options {
        out: PathBuf::from("snap_recon_out"),
        scenes: vec![SceneKind::Stage, SceneKind::House],
        patch: None,
        noise: false,
        light: false,
        png: true,
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

    let views = stage_views();
    let mut all = Vec::new();
    let mut dynamic = Vec::new();
    // Three instants of the dance; each tick jumps straight to the next one.
    for (round, dt) in [1.3f32, 2.8, 3.6].into_iter().enumerate() {
        client.apply(&pipeline.tick(dt));
        let time = pipeline.scene_time();
        let snap = scene.snapshot(time);
        let cloud = client.cloud();
        dynamic.push(client.layers[1].len() as f32);
        let mut sheets = Vec::new();
        for view in &views {
            let score = evaluate(&snap, &cloud, &view.cam, pipeline.config().shadows);
            println!("  t={time:4.1} {:8} psnr {:5.2} dB  coverage {:5.1} %", view.name, score.psnr, 100.0 * score.coverage);
            all.push(score.psnr);
            sheets.push(side_by_side(&score));
        }
        if options.png {
            save_png(&options.out.join(format!("stage_t{round}.png")), &stack(&sheets));
        }
    }
    let stats = *client.stats();
    println!(
        "RESULT stage psnr_mean={:.2} psnr_min={:.2} static={} dynamic={:.0} tick_bytes={}",
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
    let cloud = client.cloud();
    println!("house: {} splats after {ticks} ticks, {scan_s:.1} s, {} bytes on the wire", cloud.len(), client.bytes);

    let (mut psnrs, mut coverages, mut sheets) = (Vec::new(), Vec::new(), Vec::new());
    for view in house_views() {
        let score = evaluate(&snap, &cloud, &view.cam, pipeline.config().shadows);
        println!("  {:12} psnr {:5.2} dB  coverage {:5.1} %", view.name, score.psnr, 100.0 * score.coverage);
        psnrs.push(score.psnr);
        coverages.push(score.coverage);
        sheets.push(side_by_side(&score));
    }
    if options.png {
        for (i, chunk) in sheets.chunks(6).enumerate() {
            save_png(&options.out.join(format!("house_{i}.png")), &stack(chunk));
        }
    }
    println!(
        "RESULT house psnr_mean={:.2} psnr_min={:.2} coverage_mean={:.2} coverage_min={:.2} splats={} scan_s={:.1}",
        mean(&psnrs),
        psnrs.iter().copied().fold(f32::INFINITY, f32::min),
        100.0 * mean(&coverages),
        100.0 * coverages.iter().copied().fold(f32::INFINITY, f32::min),
        cloud.len(),
        scan_s,
    );
    // Leave the pipeline in a defined state for anyone extending this.
    pipeline.handle_control(ControlMsg::ScanPause);
}

fn main() {
    let options = parse_args();
    for kind in options.scenes.clone() {
        match kind {
            SceneKind::Stage => stage(&options),
            SceneKind::House => house(&options),
        }
    }
    if options.png {
        println!("pictures in {}", options.out.display());
    }
}
