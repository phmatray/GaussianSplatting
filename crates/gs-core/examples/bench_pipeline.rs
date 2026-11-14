//! Timings of the whole pipeline: capture, reconstruction, encoding.
//!
//! ```text
//! cargo run -p gs-core --release --example bench_pipeline                  # single-threaded
//! cargo run -p gs-core --release --features par --example bench_pipeline   # rayon
//! ```
//!
//! Prints one line per scenario and a `RESULT` summary. The stage numbers are
//! taken over two loops of the dance at the configured tick rate; the house
//! is scanned twice, once in real-time steps (per-tick cost, sensor frames
//! per tick, previews) and once with `ScanFinish` (time to fuse the whole
//! path).

use std::time::Instant;

use gs_core::capture::NoiseModel;
use gs_core::codec::{self, Kind};
use gs_core::pipeline::{Outgoing, Pipeline, SLOT_DYNAMIC};
use gs_core::protocol::{ControlMsg, PipelineConfig, ScanHead, ScanState, Stats};
use gs_core::scene::SceneKind;

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() { 0.0 } else { values.iter().sum::<f64>() / values.len() as f64 }
}

/// FNV-1a over the bodies of all layer messages: equal fingerprints mean
/// equal splats on the wire (compare the single-threaded and the rayon run).
fn fingerprint(hash: &mut u64, messages: &[Outgoing]) {
    for message in messages.iter().filter(|m| codec::peek_kind(&m.bytes) == Ok(Kind::Layer)) {
        for &byte in &message.bytes[codec::HEADER_SIZE..] {
            *hash = (*hash ^ byte as u64).wrapping_mul(0x0000_0100_0000_01B3);
        }
    }
}

const FNV_OFFSET: u64 = 0xCBF2_9CE4_8422_2325;

fn stats_of(messages: &[Outgoing]) -> Option<Stats> {
    messages.iter().rev().find(|m| codec::peek_kind(&m.bytes) == Ok(Kind::Stats)).and_then(|m| {
        match codec::decode(&m.bytes).ok()?.body {
            codec::Body::Stats(json) => serde_json::from_str(&json).ok(),
            _ => None,
        }
    })
}

/// Ticks the live stage through two loops of the dance.
fn stage(label: &str, config: PipelineConfig) {
    let start = Instant::now();
    let mut pipeline = Pipeline::new(config);
    let setup_ms = start.elapsed().as_secs_f64() * 1e3;
    let dt = 1.0 / pipeline.config().target_hz;
    let ticks = (2.0 * 9.6 / dt) as usize;
    let (mut total, mut capture, mut recon, mut encode) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let (mut dynamic, mut layer_bytes, mut tick_bytes) = (Vec::new(), Vec::new(), Vec::new());
    let mut hash = FNV_OFFSET;
    fingerprint(&mut hash, &pipeline.snapshot());
    for _ in 0..ticks {
        let start = Instant::now();
        let out = pipeline.tick(dt);
        total.push(start.elapsed().as_secs_f64() * 1e3);
        fingerprint(&mut hash, &out);
        let stats = stats_of(&out).expect("every tick emits Stats");
        capture.push(stats.capture_ms as f64);
        recon.push(stats.reconstruct_ms as f64);
        encode.push(stats.encode_ms as f64);
        dynamic.push(stats.splats_dynamic as f64);
        tick_bytes.push(out.iter().map(|m| m.bytes.len()).sum::<usize>() as f64);
        layer_bytes.push(out.iter().filter(|m| m.slot == SLOT_DYNAMIC).map(|m| m.bytes.len()).sum::<usize>() as f64);
    }
    let mut sorted = total.clone();
    sorted.sort_by(f64::total_cmp);
    let c = pipeline.config();
    println!(
        "RESULT {label}: {}x{}x{} tick mean {:.2} ms, p95 {:.2}, max {:.2} (capture {:.2}, reconstruct {:.2}, encode {:.2}); \
         setup {:.0} ms; static {} splats, dynamic mean {:.0} max {:.0}; \
         dynamic layer {:.0} kB/tick, all messages {:.0} kB/tick; \
         fingerprint {hash:016x}",
        c.rig.count,
        c.rig.width,
        c.rig.height,
        mean(&total),
        percentile(&sorted, 0.95),
        percentile(&sorted, 1.0),
        mean(&capture),
        mean(&recon),
        mean(&encode),
        setup_ms,
        pipeline.static_splats().len(),
        mean(&dynamic),
        dynamic.iter().copied().fold(0.0, f64::max),
        mean(&layer_bytes) / 1e3,
        mean(&tick_bytes) / 1e3,
    );
}

/// Scans the house in real-time steps, then once more with `ScanFinish`.
fn house(label: &str, mut config: PipelineConfig) {
    config.scan.autostart = true;
    let mut pipeline = Pipeline::new(config);
    let dt = 1.0 / pipeline.config().target_hz;
    let (mut total, mut busy, mut capture, mut fuse) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let (mut bytes, mut frames, mut most_frames, mut previews) = (0usize, 0u64, 0u64, 0usize);
    let mut hash = FNV_OFFSET;
    let c = *pipeline.config();
    let frame_pixels = (c.scan.width * c.scan.height) as u64;
    while pipeline.scan_state() == Some(ScanState::Scanning) {
        let start = Instant::now();
        let out = pipeline.tick(dt);
        let ms = start.elapsed().as_secs_f64() * 1e3;
        fingerprint(&mut hash, &out);
        total.push(ms);
        // Ticks between two sensor frames only emit Stats.
        if let Some(stats) = stats_of(&out).filter(|s| s.pixels > 0) {
            busy.push(ms);
            capture.push(stats.capture_ms as f64);
            fuse.push(stats.reconstruct_ms as f64);
            let fired = stats.pixels as u64 / frame_pixels;
            frames += fired;
            most_frames = most_frames.max(fired);
        }
        previews += out.iter().filter(|m| codec::peek_kind(&m.bytes) == Ok(Kind::Preview)).count();
        bytes += out.iter().map(|m| m.bytes.len()).sum::<usize>();
    }
    let mut sorted = busy.clone();
    sorted.sort_by(f64::total_cmp);
    let splats = pipeline.static_splats().len();
    let walk_s = pipeline.scene_time();

    pipeline.handle_control(ControlMsg::ScanStart);
    pipeline.handle_control(ControlMsg::ScanFinish);
    let start = Instant::now();
    let (mut finish_ticks, mut finish_worst) = (0usize, 0.0f64);
    while pipeline.scan_state() == Some(ScanState::Scanning) {
        let tick = Instant::now();
        pipeline.tick(dt);
        finish_worst = finish_worst.max(tick.elapsed().as_secs_f64() * 1e3);
        finish_ticks += 1;
    }
    let finish_s = start.elapsed().as_secs_f64();
    println!(
        "RESULT {label}: {} x {}x{} fov {:.0}, voxel {:.0} mm, {walk_s:.1} s walk: \
         capturing tick mean {:.2} ms, p95 {:.2}, max {:.2} \
         (capture {:.2}, fuse {:.2}; {} of {} ticks capture {frames} frames, at most {most_frames} per tick; \
         {previews} previews); \
         {} splats, {:.1} MB on the wire; ScanFinish {:.2} s in {} ticks (worst {:.1} ms); fingerprint {hash:016x}",
        c.scan.head.sensor_count(),
        c.scan.width,
        c.scan.height,
        c.scan.fov_y_deg,
        c.scan.voxel_size * 1e3,
        mean(&busy),
        percentile(&sorted, 0.95),
        percentile(&sorted, 1.0),
        mean(&capture),
        mean(&fuse),
        busy.len(),
        total.len(),
        splats,
        bytes as f64 / 1e6,
        finish_s,
        finish_ticks,
        finish_worst,
    );
}

fn main() {
    let threads = if cfg!(feature = "par") { format!("rayon, {} threads", rayon_threads()) } else { "single-threaded".into() };
    println!("pipeline benchmark ({threads})");

    stage("stage default", PipelineConfig::for_scene(SceneKind::Stage));
    let noisy = PipelineConfig { noise: NoiseModel::realistic(), ..PipelineConfig::for_scene(SceneKind::Stage) };
    stage("stage default + noise", noisy);
    stage(
        "stage default, no background subtraction",
        PipelineConfig { background_subtraction: false, ..PipelineConfig::for_scene(SceneKind::Stage) },
    );
    stage("stage light", PipelineConfig::light(SceneKind::Stage));
    for (width, height) in [(160, 120), (240, 180), (320, 240)] {
        let mut config = PipelineConfig::for_scene(SceneKind::Stage);
        (config.rig.width, config.rig.height) = (width, height);
        stage(&format!("stage 8x{width}x{height}"), config);
    }
    house("house default", PipelineConfig::for_scene(SceneKind::House));
    house("house light", PipelineConfig::light(SceneKind::House));
    let mut single = PipelineConfig::for_scene(SceneKind::House);
    single.scan.head = ScanHead::Single;
    house("house default, single sensor", single);
}

#[cfg(feature = "par")]
fn rayon_threads() -> usize {
    std::thread::available_parallelism().map_or(1, usize::from)
}

#[cfg(not(feature = "par"))]
fn rayon_threads() -> usize {
    1
}
