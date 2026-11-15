//! Small simulated captures for the integration tests, filmed by the
//! simulated phone of gs-core (`tests/ingest_sim`).
#![allow(dead_code)]

#[path = "../../../gs-core/tests/ingest_sim/mod.rs"]
pub mod ingest_sim;

use std::io::BufWriter;
use std::path::{Path, PathBuf};

use gs_core::glam::Vec3;
use gs_core::ingest::frame::{encode_frame, CaptureFrame};
use gs_core::ingest::{CaptureRegion, GscapHeader, GscapWriter};
use gs_core::scene::stage::StageScene;
use gs_core::scene::{make_scene, Scene, SceneKind};
use gs_core::Camera;
use ingest_sim::{orbit_pose, room_sweep, CaptureWorld, Phone, PhoneSim};

pub const DANCER: Vec3 = Vec3::new(0.0, 1.0, 0.0);
const BEDROOM_LOOP: [[f32; 2]; 7] = [[-1.3, 2.0], [-1.7, 1.78], [-2.35, 1.36], [-2.92, 1.78], [-2.8, 2.38], [-2.25, 2.6], [-1.68, 2.28]];

/// A phone with small pictures: 160x120 colour, 64x48 depth.
pub fn small_sim() -> PhoneSim {
    PhoneSim { color_width: 160, color_height: 120, depth_width: 64, depth_height: 48, ..PhoneSim::default() }
}

/// A fresh directory under the system's temporary directory.
pub fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gs-refine-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("cannot create a scratch directory");
    dir
}

/// What a test may know about a capture it wrote.
pub struct Written {
    pub path: PathBuf,
    pub world: CaptureWorld,
    /// The true cameras, scene coordinates.
    pub cameras: Vec<Camera>,
    pub frames: Vec<CaptureFrame>,
}

/// Films `frames` frames of a scenario (`bedroom`, `object` or `person`)
/// and writes the `.gscap`. `edit` may change each frame before it is
/// written.
pub fn write_capture(path: &Path, scenario: &str, frames: usize, sim: PhoneSim, mut edit: impl FnMut(usize, &mut CaptureFrame)) -> Written {
    let stage = StageScene::new();
    let (mode, snap, empty, region) = match scenario {
        "bedroom" => ("room", make_scene(SceneKind::House).snapshot(0.0), None, None),
        "object" => ("object", stage.snapshot(1.0), None, Some(1.1)),
        "person" => ("person", stage.snapshot(1.0), Some(stage.snapshot_static()), Some(1.2)),
        other => panic!("unknown scenario {other}"),
    };
    let cameras: Vec<Camera> = (0..frames)
        .map(|i| match scenario {
            "bedroom" => sim.camera(room_sweep(&BEDROOM_LOOP, i, frames, 55.0)),
            _ => {
                let (eye, target) = orbit_pose(DANCER, 2.3, 1.0, 1.75, i as f32 / frames as f32, 1.0);
                sim.look_at(eye, target)
            }
        })
        .collect();
    let mut phone = Phone::new(sim, &cameras[0]);
    let world = phone.world;
    let header = GscapHeader {
        protocol: Some(1),
        device: "simulateur".into(),
        app: "test".into(),
        mode: mode.into(),
        name: format!("Essai {scenario}"),
        created: "2026-11-14T12:00:00Z".into(),
        region: region.map(|radius| CaptureRegion { center: world.to_capture(DANCER).to_array(), radius }),
        floor: Some(world.floor_y()),
    };
    let file = std::fs::File::create(path).expect("cannot create the capture");
    let mut writer = GscapWriter::new(BufWriter::new(file), &header, gs_core::ingest::file::HEADER_PAD).expect("header");
    let mut written = Vec::new();
    for (i, cam) in cameras.iter().enumerate() {
        let mut frame = phone.shoot(&snap, empty.as_ref(), cam, i as f64 / 10.0).frame;
        edit(i, &mut frame);
        writer.append(&encode_frame(&frame).expect("frame")).expect("write");
        written.push(frame);
    }
    drop(writer);
    Written { path: path.to_path_buf(), world, cameras, frames: written }
}
