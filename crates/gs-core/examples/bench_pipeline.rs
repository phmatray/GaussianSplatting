//! Timings of the whole pipeline: capture, reconstruction, encoding.
//!
//! ```text
//! cargo run -p gs-core --release --example bench_pipeline                  # single-threaded
//! cargo run -p gs-core --release --features par --example bench_pipeline   # rayon
//! ```
//!
//! Prints one `RESULT` line per scenario; a first argument keeps the
//! scenarios whose label contains it, a second one is a JSON object merged
//! into their configuration (`bench_pipeline "house light" '{"scan":{"voxel_size":0.032}}'`). The stage numbers are taken over two loops of
//! the dance at the configured tick rate; the house is scanned twice, once in
//! real-time steps (per-tick cost, sensor frames per tick, previews) and once
//! with `ScanFinish` (time to fuse the whole path).
//!
//! Wall-clock times only mean something on an idle machine: every line
//! carries the CPU time of the process per tick (all threads) and the load
//! average, so that a busy machine shows.

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

/// CPU time this process has used so far, all threads, in milliseconds
/// (Linux; `None` elsewhere).
fn cpu_ms() -> Option<f64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // The fields after the command name, which may itself hold spaces.
    let rest = stat.rsplit_once(')')?.1;
    let mut fields = rest.split_whitespace().skip(11);
    let (user, system): (f64, f64) = (fields.next()?.parse().ok()?, fields.next()?.parse().ok()?);
    // Clock ticks; 100 per second on every Linux this runs on.
    Some((user + system) * 10.0)
}

/// The one-minute load average (Linux; empty elsewhere).
fn load_average() -> String {
    std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|text| text.split_whitespace().next().map(|load| format!("load {load}")))
        .unwrap_or_default()
}

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
    let (mut dynamic, mut layer_bytes, mut tick_bytes, mut fuse) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut hash = FNV_OFFSET;
    fingerprint(&mut hash, &pipeline.snapshot());
    let cpu_start = cpu_ms();
    for _ in 0..ticks {
        let start = Instant::now();
        let out = pipeline.tick(dt);
        total.push(start.elapsed().as_secs_f64() * 1e3);
        fingerprint(&mut hash, &out);
        let stats = stats_of(&out).expect("every tick emits Stats");
        capture.push(stats.capture_ms as f64);
        recon.push(stats.reconstruct_ms as f64);
        fuse.push(stats.fuse_ms as f64);
        encode.push(stats.encode_ms as f64);
        dynamic.push(stats.splats_dynamic as f64);
        tick_bytes.push(out.iter().map(|m| m.bytes.len()).sum::<usize>() as f64);
        layer_bytes.push(out.iter().filter(|m| m.slot == SLOT_DYNAMIC).map(|m| m.bytes.len()).sum::<usize>() as f64);
    }
    let cpu = cpu_start.zip(cpu_ms()).map_or(String::new(), |(a, b)| format!(", CPU {:.2} ms/tick", (b - a) / ticks as f64));
    let mut sorted = total.clone();
    sorted.sort_by(f64::total_cmp);
    let c = pipeline.config();
    println!(
        "RESULT {label}: {}x{}x{} tick mean {:.2} ms, p95 {:.2}, max {:.2}{cpu} \
         (capture {:.2}, reconstruct {:.2} of which fusion {:.2}, encode {:.2}; {}); \
         setup {:.0} ms; static {} splats, dynamic mean {:.0} max {:.0}; \
         dynamic layer {:.0} kB/tick = {:.2} MB/s at {:.0} Hz, all messages {:.0} kB/tick; \
         fingerprint {hash:016x}",
        c.rig.count,
        c.rig.width,
        c.rig.height,
        mean(&total),
        percentile(&sorted, 0.95),
        percentile(&sorted, 1.0),
        mean(&capture),
        mean(&recon),
        mean(&fuse),
        mean(&encode),
        load_average(),
        setup_ms,
        pipeline.static_splats().len(),
        mean(&dynamic),
        dynamic.iter().copied().fold(0.0, f64::max),
        mean(&layer_bytes) / 1e3,
        mean(&layer_bytes) * c.target_hz as f64 / 1e6,
        c.target_hz,
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
    let load = load_average();
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
    let cpu_start = cpu_ms();
    let start = Instant::now();
    let (mut finish_ticks, mut finish_worst) = (0usize, 0.0f64);
    while pipeline.scan_state() == Some(ScanState::Scanning) {
        let tick = Instant::now();
        pipeline.tick(dt);
        finish_worst = finish_worst.max(tick.elapsed().as_secs_f64() * 1e3);
        finish_ticks += 1;
    }
    let finish_s = start.elapsed().as_secs_f64();
    let finish_cpu = cpu_start.zip(cpu_ms()).map_or(String::new(), |(a, b)| format!(", CPU {:.2} s", (b - a) / 1e3));
    println!(
        "RESULT {label}: {} x {}x{} fov {:.0}, voxel {:.0} mm, {walk_s:.1} s walk: \
         capturing tick mean {:.2} ms, p95 {:.2}, max {:.2} \
         (capture {:.2}, fuse {:.2}; {} of {} ticks capture {frames} frames, at most {most_frames} per tick; \
         {previews} previews); \
         {} splats, {:.1} MB on the wire; ScanFinish {:.2} s{finish_cpu} in {} ticks (worst {:.1} ms); {load}; \
         fingerprint {hash:016x}",
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

    let filter = std::env::args().nth(1);
    let wanted = |label: &str| filter.as_deref().is_none_or(|f| label.contains(f));
    let patch: Option<serde_json::Value> =
        std::env::args().nth(2).map(|text| serde_json::from_str(&text).expect("the second argument is a JSON object"));
    // The patch goes through the pipeline's own `PatchConfig`.
    let patched = |config: PipelineConfig| match &patch {
        Some(patch) => {
            let mut pipeline = Pipeline::new(PipelineConfig { paused: true, ..config });
            pipeline.handle_control(ControlMsg::PatchConfig { patch: patch.clone() });
            PipelineConfig { paused: config.paused, ..*pipeline.config() }
        }
        None => config,
    };
    let default = PipelineConfig::for_scene(SceneKind::Stage);
    let mut stages = vec![
        ("stage default".to_string(), default),
        ("stage default + noise".into(), PipelineConfig { noise: NoiseModel::realistic(), ..default }),
        ("stage default, stacked (no fusion)".into(), {
            let mut config = default;
            config.fusion.enabled = false;
            config
        }),
        ("stage default, no background subtraction".into(), PipelineConfig { background_subtraction: false, ..default }),
        ("stage light".into(), PipelineConfig::light(SceneKind::Stage)),
        ("stage light + noise".into(), PipelineConfig { noise: NoiseModel::realistic(), ..PipelineConfig::light(SceneKind::Stage) }),
    ];
    for (width, height) in [(160, 120), (240, 180), (320, 240)] {
        let mut config = default;
        (config.rig.width, config.rig.height) = (width, height);
        stages.push((format!("stage 8x{width}x{height}"), config));
        stages.push((format!("stage 8x{width}x{height} + noise"), PipelineConfig { noise: NoiseModel::realistic(), ..config }));
    }
    for (label, config) in stages {
        if wanted(&label) {
            stage(&label, patched(config));
        }
    }
    let house_default = PipelineConfig::for_scene(SceneKind::House);
    let mut single = house_default;
    single.scan.head = ScanHead::Single;
    let houses = [
        ("house default", house_default),
        ("house default + noise", PipelineConfig { noise: NoiseModel::realistic(), ..house_default }),
        ("house light", PipelineConfig::light(SceneKind::House)),
        ("house default, single sensor", single),
    ];
    for (label, config) in houses {
        if wanted(label) {
            house(label, patched(config));
        }
    }
}

#[cfg(feature = "par")]
fn rayon_threads() -> usize {
    std::thread::available_parallelism().map_or(1, usize::from)
}

#[cfg(not(feature = "par"))]
fn rayon_threads() -> usize {
    1
}
