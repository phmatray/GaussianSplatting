//! A long capture without a device and without a file: the simulated phone
//! of `tests/ingest_sim` walks the house for as long as asked and every
//! frame goes straight into a [`CaptureSession`], with a progress tick
//! every second of capture, as the server drives it. Reports, minute by
//! minute, what a frame and a tick cost and how much memory the process
//! holds: the bounds a host can rely on for a capture of any length.
//!
//! ```text
//! cargo run --release -p gs-core --features par --example gscap_soak -- --minutes 10 --realistic
//! ```
//!
//! Options: `--minutes M` (default 10, at 10 frames per second),
//! `--color-width W` (default 960), `--realistic`, `--grain G`,
//! `--mode room|object|person|live`, `--laps L` (how many times the walk
//! goes through the house in that time; default 1: more laps revisit more),
//! and the ingest parameters `--voxel METERS`, `--far METERS`,
//! `--min-confidence 0|1|2`, `--max-voxels N`, `--layer-window CELLS`.

#[path = "../tests/ingest_sim/mod.rs"]
mod ingest_sim;

use std::time::Instant;

use gs_core::ingest::frame::encode_frame;
use gs_core::ingest::message::ClientMessage;
use gs_core::ingest::{CaptureSession, Hello, IngestParams};
use gs_core::protocol::CaptureMode;
use gs_core::scene::{make_scene, SceneKind};
use ingest_sim::{HouseWalk, Phone, PhoneSim};

/// Resident memory of this process in MB (Linux; 0 elsewhere).
fn resident_mb() -> f64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
            line.split_whitespace().nth(1)?.parse::<f64>().ok()
        })
        .map_or(0.0, |kb| kb / 1024.0)
}

fn percentile(values: &mut [f64], p: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    values[((values.len() - 1) as f64 * p).round() as usize]
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let flag = |name: &str| args.iter().any(|a| a == name);
    let text = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1));
    let number = |name: &str| text(name).and_then(|v| v.parse::<f64>().ok());

    let minutes = number("--minutes").unwrap_or(10.0).clamp(0.1, 600.0);
    let laps = number("--laps").unwrap_or(1.0).clamp(0.1, 100.0);
    let mode = text("--mode").and_then(|m| CaptureMode::from_name(m)).unwrap_or(CaptureMode::Room);
    let mut sim = if flag("--realistic") { PhoneSim::realistic() } else { PhoneSim::default() };
    if let Some(width) = number("--color-width") {
        sim.color_width = (width as u32).clamp(64, 4096);
        sim.color_height = sim.color_width * 3 / 4;
    }
    sim.color_grain = number("--grain").unwrap_or(0.0) as f32;

    let house = make_scene(SceneKind::House).snapshot(0.0);
    let walk = HouseWalk::new(10.0);
    let frames = (minutes * 600.0) as usize;
    // Walking time of frame `i`: the whole path `laps` times over.
    let walk_time = |i: usize| (i as f32 / frames as f32 * laps as f32).fract() * walk.path.duration();
    let mut phone = Phone::new(sim, &sim.camera(walk.pose(walk_time(0))));
    let mut params = IngestParams::default();
    let of_mode = match mode {
        CaptureMode::Room => &mut params.room,
        CaptureMode::Object => &mut params.object,
        CaptureMode::Person => &mut params.person,
        CaptureMode::Live => &mut params.live,
    };
    of_mode.voxel_size = number("--voxel").map_or(of_mode.voxel_size, |v| v as f32);
    of_mode.far = number("--far").map_or(of_mode.far, |v| v as f32);
    params.min_confidence = number("--min-confidence").map_or(params.min_confidence, |v| v as u8);
    params.max_voxels = number("--max-voxels").map_or(params.max_voxels, |v| v as usize);
    params.layer_window = number("--layer-window").map_or(params.layer_window, |v| v as f32);
    let mut session = CaptureSession::new(&Hello::offline(mode, "Endurance", "simulateur", "0"), params);
    session.handle_message(&ClientMessage::Floor { y: phone.world.floor_y() });

    println!("{frames} frames ({minutes} min at 10 per second), mode {}, {laps} lap(s), colour {}x{}", mode.name(), sim.color_width, sim.color_height);
    println!("minute  frames  used   splats  frame ms (median / 95 % / worst)  tick ms (median / worst)  viewers MB  frame kB  memory MB");
    let (mut frame_ms, mut tick_ms) = (Vec::new(), Vec::new());
    let (mut viewer_bytes, mut wire_bytes, mut worst_frame, mut worst_tick) = (0usize, 0usize, 0.0f64, 0.0f64);
    let start = Instant::now();
    for i in 0..frames {
        let cam = sim.camera(walk.pose(walk_time(i)));
        let shot = phone.shoot(&house, None, &cam, i as f64 / 10.0);
        let bytes = encode_frame(&shot.frame).expect("frame encoding failed");
        wire_bytes += bytes.len();
        let begin = Instant::now();
        let reply = session.handle_frame(&bytes).expect("the simulated phone sends valid frames");
        frame_ms.push(begin.elapsed().as_secs_f64() * 1e3);
        viewer_bytes += reply.to_viewers.iter().map(|m| m.bytes.len()).sum::<usize>();
        if (i + 1) % 10 == 0 {
            let begin = Instant::now();
            let reply = session.progress();
            tick_ms.push(begin.elapsed().as_secs_f64() * 1e3);
            viewer_bytes += reply.to_viewers.iter().map(|m| m.bytes.len()).sum::<usize>();
        }
        if (i + 1) % 600 == 0 || i + 1 == frames {
            let (f95, fmax) = (percentile(&mut frame_ms, 0.95), percentile(&mut frame_ms, 1.0));
            let tmax = percentile(&mut tick_ms, 1.0);
            worst_frame = worst_frame.max(fmax);
            worst_tick = worst_tick.max(tmax);
            println!(
                "{:6.1}  {:6}  {:5}  {:7}  {:8.1} / {:5.1} / {:6.1}        {:8.1} / {:6.1}        {:10.1}  {:8.0}  {:9.0}",
                (i + 1) as f64 / 600.0,
                session.frames_received(),
                session.frames_used(),
                session.splat_count(),
                percentile(&mut frame_ms, 0.5),
                f95,
                fmax,
                percentile(&mut tick_ms, 0.5),
                tmax,
                viewer_bytes as f64 / 1e6,
                wire_bytes as f64 / 1e3 / (i + 1) as f64,
                resident_mb()
            );
            frame_ms.clear();
            tick_ms.clear();
        }
    }
    let begin = Instant::now();
    let reply = session.finish();
    let finish_ms = begin.elapsed().as_secs_f64() * 1e3;
    viewer_bytes += reply.to_viewers.iter().map(|m| m.bytes.len()).sum::<usize>();
    let stats = session.stats();
    println!(
        "finish {finish_ms:.0} ms; {} splats, full: {}; worst frame {worst_frame:.1} ms, worst tick {worst_tick:.1} ms; \
         viewers got {:.1} MB; memory {:.0} MB; whole run {:.0} s",
        session.splat_count(),
        stats.capture.is_some_and(|c| c.full),
        viewer_bytes as f64 / 1e6,
        resident_mb(),
        start.elapsed().as_secs_f64()
    );
}
