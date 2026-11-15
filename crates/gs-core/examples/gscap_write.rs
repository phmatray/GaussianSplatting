//! Writes a `.gscap` as the phone companion would record it, filmed by the
//! simulated phone of `tests/ingest_sim` in the procedural scenes: for
//! tests, demos and benchmarks without a device.
//!
//! ```text
//! cargo run --release -p gs-core --features par --example gscap_write -- bedroom out/bedroom.gscap
//! cargo run --release -p gs-core --features par --example gscap_write -- house out/house.gscap --frames 600 --realistic
//! cargo run --release -p gs-core --features par --example gscap_write -- person out/dancer.gscap --color-width 1920
//! ```
//!
//! Scenarios: `house` (the whole walk of the home scan, the phone swept in
//! a spiral; 1670 frames unless `--frames` cuts it), `bedroom` (one room,
//! 220 frames), `person` and `object` (one orbit around the dancer of the
//! stage, holding still; with and without a person mask), `live` (the
//! dancer dancing, the phone swaying in front; 15 frames per second).
//!
//! Options: `--frames N`, `--color-width W` (default 960; the height is
//! three quarters of it), `--realistic` (depth noise, pose jitter and
//! drift, wandering exposure), `--no-floor` (leave the floor height out of
//! the header), `--no-confidence`, `--plain` (planes not deflated),
//! `--grain G` (sensor grain of G levels on the colour image: the JPEGs get
//! as large as a real phone's, for decoding benchmarks).

#[path = "../tests/ingest_sim/mod.rs"]
mod ingest_sim;

use std::io::BufWriter;

use gs_core::glam::Vec3;
use gs_core::ingest::frame::encode_frame;
use gs_core::ingest::{CaptureRegion, GscapHeader, GscapWriter};
use gs_core::scene::stage::StageScene;
use gs_core::scene::{make_scene, Scene, SceneKind, SceneSnapshot};
use gs_core::Camera;
use ingest_sim::{orbit_pose, room_sweep, HouseWalk, Phone, PhoneSim};

/// The loop of the house tour through the bedroom.
const BEDROOM_LOOP: [[f32; 2]; 7] =
    [[-1.3, 2.0], [-1.7, 1.78], [-2.35, 1.36], [-2.92, 1.78], [-2.8, 2.38], [-2.25, 2.6], [-1.68, 2.28]];

fn usage() -> ! {
    eprintln!(
        "usage: gscap_write <house|bedroom|person|object|live> <out.gscap> [--frames N] [--color-width W] \
         [--realistic] [--no-floor] [--no-confidence] [--plain] [--grain G]"
    );
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(scenario), Some(out)) = (args.first(), args.get(1)) else { usage() };
    let flag = |name: &str| args.iter().any(|a| a == name);
    let value = |name: &str| args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).and_then(|v| v.parse::<usize>().ok());

    let mut sim = if flag("--realistic") { PhoneSim::realistic() } else { PhoneSim::default() };
    if let Some(width) = value("--color-width") {
        sim.color_width = (width as u32).clamp(64, 4096);
        sim.color_height = sim.color_width * 3 / 4;
    }
    sim.with_confidence = !flag("--no-confidence");
    if let Some(grain) = value("--grain") {
        sim.color_grain = grain as f32;
    }
    sim.compress = !flag("--plain");

    let stage = StageScene::new();
    let house = make_scene(SceneKind::House).snapshot(0.0);
    let dancer = Vec3::new(0.0, 1.0, 0.0);
    let walk = HouseWalk::new(10.0);
    // Per scenario: mode, default frame count, frames per second, the
    // camera and the scene of frame `i`, the scene without the person.
    type Shot<'a> = Box<dyn Fn(usize, usize) -> (Camera, SceneSnapshot, Option<SceneSnapshot>) + 'a>;
    let (mode, default_frames, fps, region, shot): (&str, usize, f64, Option<f32>, Shot) = match scenario.as_str() {
        "house" => ("room", walk.frame_count(), 10.0, None, Box::new(|i, _| (sim.camera(walk.pose(walk.time(i))), house.clone(), None))),
        "bedroom" => ("room", 220, 10.0, None, Box::new(|i, n| (sim.camera(room_sweep(&BEDROOM_LOOP, i, n, 55.0)), house.clone(), None))),
        "person" | "object" => {
            let masked = scenario == "person";
            let radius = if masked { 1.2 } else { 1.1 };
            (
                if masked { "person" } else { "object" },
                120,
                10.0,
                Some(radius),
                Box::new(move |i, n| {
                    let (eye, target) = orbit_pose(dancer, 2.3, 1.0, 1.75, i as f32 / n as f32, 1.0);
                    (sim.look_at(eye, target), stage.snapshot(1.0), masked.then(|| stage.snapshot_static()))
                }),
            )
        }
        "live" => (
            "live",
            150,
            15.0,
            None,
            Box::new(|i, n| {
                // A quarter turn back and forth in front of the dancer.
                let sway = 0.125 * (std::f32::consts::TAU * i as f32 / n as f32).sin();
                let (eye, target) = orbit_pose(dancer, 2.6, 1.4, 1.4, sway, 1.0);
                (sim.look_at(eye, target), stage.snapshot(i as f32 / 15.0), Some(stage.snapshot_static()))
            }),
        ),
        _ => usage(),
    };
    let frames = value("--frames").unwrap_or(default_frames).clamp(1, default_frames.max(1));

    let (first, ..) = shot(0, frames);
    let mut phone = Phone::new(sim, &first);
    let header = GscapHeader {
        protocol: Some(1),
        device: "simulateur".into(),
        app: env!("CARGO_PKG_VERSION").into(),
        mode: mode.into(),
        name: format!("Simulation {scenario}"),
        created: "2026-11-14T12:00:00Z".into(),
        region: region.map(|radius| CaptureRegion { center: phone.world.to_capture(dancer).to_array(), radius }),
        floor: (!flag("--no-floor")).then(|| phone.world.floor_y()),
    };
    let file = std::fs::File::create(out).unwrap_or_else(|e| panic!("{out}: {e}"));
    let mut writer = GscapWriter::new(BufWriter::new(file), &header, gs_core::ingest::file::HEADER_PAD).expect("cannot write the header");
    let start = std::time::Instant::now();
    for i in 0..frames {
        let (cam, snap, empty) = shot(i, frames);
        let taken = phone.shoot(&snap, empty.as_ref(), &cam, i as f64 / fps);
        writer.append(&encode_frame(&taken.frame).expect("frame encoding failed")).expect("write failed");
        if (i + 1) % 50 == 0 {
            eprintln!("{} / {frames} frames, {:.1} MB, {:.0} s", i + 1, writer.bytes() as f64 / 1e6, start.elapsed().as_secs_f64());
        }
    }
    println!(
        "{out}: mode {mode}, {frames} frames of {}x{} colour and {}x{} depth, {:.1} MB ({:.0} kB per frame)",
        sim.color_width,
        sim.color_height,
        sim.depth_width,
        sim.depth_height,
        writer.bytes() as f64 / 1e6,
        writer.bytes() as f64 / 1e3 / frames as f64
    );
}
