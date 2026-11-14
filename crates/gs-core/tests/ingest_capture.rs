//! A capture session end to end, fed by the simulated phone of
//! `ingest_sim`: what viewers receive, how the stream rules are followed,
//! and how well the fused cloud shows the scene from viewpoints the phone
//! never took (the instrument of `novel_view.rs`).
//!
//! Everything here is synthetic: the "phone" is a ray caster with the
//! defects of a real one modelled after its documentation, not a device.
//! Set `GS_RECON_OUT=<dir>` for `reference | reconstruction | 4x difference`
//! sheets of the quality tests.

mod common;
mod ingest_sim;

use common::{evaluate, house_views, mean, observed_mask, output_dir, save_png, side_by_side, stack, Client, Exposures, Renderer, Score};
use gs_core::codec::{self, Body, LayerMode};
use gs_core::glam::{Quat, Vec3};
use gs_core::ingest::frame::{encode_frame, CaptureFrame, Orientation, Tracking};
use gs_core::ingest::message::{ClientMessage, ServerMessage};
use gs_core::ingest::prepare::SkipReason;
use gs_core::ingest::{CaptureRegion, CaptureSession, GscapHeader, GscapImporter, GscapWriter, Hello, IngestParams, Reply};
use gs_core::protocol::{CaptureMode, ScanHead, ScanState};
use gs_core::scene::stage::StageScene;
use gs_core::scene::{house, make_scene, Scene, SceneKind, SceneSnapshot};
use gs_core::{Camera, Splat};
use ingest_sim::{orbit_pose, Phone, PhoneSim, Shot};

/// A small, fast phone for the behaviour tests: 64x48 depth, 160x120 colour.
fn small_sim() -> PhoneSim {
    PhoneSim { color_width: 160, color_height: 120, depth_width: 64, depth_height: 48, ..PhoneSim::default() }
}

fn house() -> SceneSnapshot {
    make_scene(SceneKind::House).snapshot(0.0)
}

fn session(mode: CaptureMode) -> CaptureSession {
    CaptureSession::new(&Hello::offline(mode, "Essai", "iPhone17,2", "0.1.0"), IngestParams::default())
}

/// The phone standing in the living room, turning on the spot: frame `i` of
/// `n` over `degrees`.
fn living_room_turn(sim: &PhoneSim, i: usize, n: usize, degrees: f32) -> Camera {
    let eye = Vec3::new(-2.6, 1.4, -1.8);
    let yaw = (degrees * i as f32 / n.max(1) as f32).to_radians();
    let dir = Vec3::new(-yaw.sin(), -0.25, -yaw.cos());
    sim.look_at(eye, eye + dir)
}

/// Films `n` frames of a turn in the living room.
fn living_room_shots(sim: PhoneSim, n: usize, degrees: f32) -> (Phone, Vec<Shot>) {
    let snap = house();
    let mut phone = Phone::new(sim, &living_room_turn(&sim, 0, n, degrees));
    let shots = (0..n).map(|i| phone.shoot(&snap, None, &living_room_turn(&sim, i, n, degrees), i as f64 * 0.1)).collect();
    (phone, shots)
}

fn ack_of(reply: &Reply) -> (u32, bool) {
    match reply.to_phone.as_slice() {
        [ServerMessage::Ack { frame, used }] => (*frame, *used),
        other => panic!("expected one ack, got {other:?}"),
    }
}

fn normal_of(splat: &Splat) -> Vec3 {
    Quat::from_array(splat.rot) * Vec3::Z
}

/// Kinds of the messages of a reply, in order: `S`tate, layer `0`/`1` with
/// `a`ppend or `r`eplace, `P`review, s`T`ats.
fn kinds(messages: &[gs_core::pipeline::Outgoing]) -> String {
    messages
        .iter()
        .map(|m| match codec::decode(&m.bytes).unwrap().body {
            Body::State(_) => "S".to_string(),
            Body::Layer(layer) => format!("{}{}", layer.layer_id, if layer.mode == LayerMode::Append { 'a' } else { 'r' }),
            Body::Preview(_) => "P".to_string(),
            Body::Stats(_) => "T".to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn a_capture_is_an_ordinary_session_for_a_viewer() {
    let (phone, shots) = living_room_shots(small_sim(), 12, 120.0);
    let mut session = session(CaptureMode::Room);
    let mut client = Client::default();

    // Before any frame: an empty, well-formed session.
    let first = session.snapshot();
    assert_eq!(kinds(&first), "S 0r 1r T");
    client.apply(&first);
    let state = client.state.clone().unwrap();
    assert_eq!((state.scene.as_str(), state.config.scene, state.config.scan.head), ("capture", SceneKind::House, ScanHead::Single));
    let capture = state.capture.unwrap();
    assert_eq!((capture.mode, capture.name.as_str(), capture.device.as_str()), (CaptureMode::Room, "Essai", "iPhone17,2"));
    assert!(!capture.floor_known && !capture.imported && capture.region.is_none());
    assert_eq!(client.scan_state(), Some(ScanState::Idle));
    assert!(state.cameras.is_empty() && !state.bounds.is_empty());

    for (i, shot) in shots.iter().enumerate() {
        let bytes = encode_frame(&shot.frame).unwrap();
        let reply = session.handle_frame(&bytes).unwrap();
        assert_eq!(ack_of(&reply), (shot.frame.index, true));
        let report = reply.frame.unwrap();
        assert!(report.used && report.skipped.is_none() && report.splats > 0, "{report:?}");
        // New surface is appended, the phone's picture follows, Stats close
        // the reply; the first frame also announces where to look from.
        let expected = if i == 0 { "S 0a P T" } else { "0a P T" };
        assert_eq!(kinds(&reply.to_viewers), expected, "frame {i}");
        client.apply(&reply.to_viewers);
        assert_eq!(client.layers[0].len(), session.static_splats().len(), "appends keep the viewer in step");
        let stats = *client.stats();
        let counters = stats.capture.unwrap();
        assert_eq!((counters.frames_received, counters.frames_used), (i as u32 + 1, i as u32 + 1));
        assert_eq!((counters.paused, counters.finished, counters.full, counters.tracking), (false, false, false, 2));
        assert_eq!(client.scan_state(), Some(ScanState::Scanning));
        assert_eq!(stats.splats_static as usize, client.layers[0].len());
        assert!(stats.pixels > 2000 && stats.total_ms > 0.0 && stats.bytes > 0);
    }
    assert!(client.layers[1].is_empty());
    assert!(client.layers[0].len() > 10_000, "{} splats", client.layers[0].len());

    // The phone shows up like the handheld sensor of the home scan: its
    // pose in Stats (viewer coordinates), its picture as preview 0.
    let (floor, known) = session.floor();
    assert!(!known, "nobody said where the floor is yet");
    let last = shots.last().unwrap();
    let camera = client.stats().scan.unwrap().camera;
    let expected = phone.world.to_capture(last.cam.position()) - Vec3::new(0.0, floor, 0.0);
    assert!((Vec3::from_slice(&camera.world_from_cam[12..15]) - expected).length() < 1e-4);
    assert_eq!((camera.id, camera.width, camera.height), (0, 64, 48));
    assert!((camera.fov_y.to_degrees() - 46.8).abs() < 0.5, "{}", camera.fov_y.to_degrees());
    let preview = &client.previews[&0];
    assert_eq!((preview.width, preview.height), (64, 48));
    assert!(preview.depth_mm.iter().filter(|&&d| d > 0).count() > 2000);
    assert!(preview.rgb.iter().any(|&v| v > 100));

    // About once a second: progress for the phone; the floor is found in
    // the data (the phone never reported it), so everything moves to it.
    let reply = session.progress();
    assert_eq!(reply.to_phone, [ServerMessage::Progress { frames: 12, splats: session.static_splats().len() as u64 }]);
    assert_eq!(kinds(&reply.to_viewers), "S 0r T");
    client.apply(&reply.to_viewers);
    let (floor, known) = session.floor();
    assert!(known && (floor - phone.world.floor_y()).abs() < 0.015, "floor at {floor}, truly {}", phone.world.floor_y());
    assert!(client.state.as_ref().unwrap().capture.as_ref().unwrap().floor_known);
    // Nothing changed since: only Stats.
    assert_eq!(kinds(&session.progress().to_viewers), "T");

    // The end: final State, final layer 0, an empty layer 1, Stats.
    let reply = session.finish();
    assert_eq!(kinds(&reply.to_viewers), "S 0r 1r T");
    assert!(matches!(reply.to_phone.as_slice(), [ServerMessage::Progress { frames: 12, .. }]));
    client.apply(&reply.to_viewers);
    assert_eq!(client.scan_state(), Some(ScanState::Done));
    assert!(client.stats().capture.unwrap().finished);
    assert_eq!(client.layers[0].len(), session.static_splats().len());
    // A second finish changes nothing.
    assert_eq!(kinds(&session.finish().to_viewers), "T");

    // In the scene's own coordinates the cloud is the living room: the
    // floor at 0, nothing outside the house.
    let cloud = phone.world.to_scene(&client.layers[0], floor);
    let on_floor: Vec<&Splat> = cloud.iter().filter(|s| normal_of(s).y > 0.9 && s.pos[1] < 0.3).collect();
    assert!(on_floor.len() > 1000, "{} floor splats", on_floor.len());
    assert!(mean(&on_floor.iter().map(|s| s.pos[1]).collect::<Vec<_>>()).abs() < 0.015);
    for s in &cloud {
        assert!(s.pos[0].abs() < house::HALF_X + 0.3 && s.pos[2].abs() < house::HALF_Z + 0.3 && (-0.1..2.7).contains(&s.pos[1]), "{:?}", s.pos);
    }
    // A late joiner gets the same picture.
    let mut late = Client::default();
    late.apply(&session.snapshot());
    assert_eq!(late.layers[0], client.layers[0]);
    assert_eq!(late.state.as_ref().unwrap().capture, client.state.as_ref().unwrap().capture);
    assert!(late.previews.contains_key(&0));
    let path = &late.state.as_ref().unwrap().scan.as_ref().unwrap().path;
    assert!(!path.is_empty() && path.iter().all(|p| (p[1] - 1.4).abs() < 0.02), "the phone was held 1.4 m above the floor");
}

#[test]
fn the_stream_rules_are_followed() {
    let (_, shots) = living_room_shots(small_sim(), 6, 40.0);
    let frame = |i: usize, tracking: Tracking| CaptureFrame { tracking, ..shots[i].frame.clone() };
    let mut session = session(CaptureMode::Room);

    // Limited tracking is acknowledged and not fused.
    let reply = session.handle_decoded(&frame(0, Tracking::Limited));
    assert_eq!(ack_of(&reply), (0, false));
    assert_eq!(reply.frame.unwrap().skipped, Some(SkipReason::Tracking));
    assert_eq!(kinds(&reply.to_viewers), "T");
    assert_eq!(session.handle_decoded(&frame(0, Tracking::NotAvailable)).frame.unwrap().skipped, Some(SkipReason::Tracking));
    assert!(session.static_splats().is_empty());
    assert!(ack_of(&session.handle_decoded(&frame(1, Tracking::Normal))).1);

    // Pause: frames are acknowledged, not used; resume takes them again.
    let reply = session.handle_text(r#"{"type":"pause"}"#).unwrap();
    assert!(reply.to_phone.is_empty());
    assert_eq!(kinds(&reply.to_viewers), "T");
    assert!(session.is_paused());
    let before = session.static_splats().len();
    let reply = session.handle_decoded(&frame(2, Tracking::Normal));
    assert_eq!((ack_of(&reply), reply.frame.unwrap().skipped), ((2, false), Some(SkipReason::Paused)));
    assert_eq!(session.static_splats().len(), before);
    session.handle_text(r#"{"type":"resume"}"#).unwrap();
    assert!(ack_of(&session.handle_decoded(&frame(2, Tracking::Normal))).1);
    assert!(session.static_splats().len() > before);

    // Unknown or broken messages end the connection with `bad_message`.
    for bad in [r#"{"type":"hello","protocol":1}"#, "{}", "pause", r#"{"type":"region","center":[0,0],"radius":1}"#] {
        let error = session.handle_text(bad).unwrap_err();
        assert_eq!(error.code.as_str(), "bad_message", "{bad}");
        assert_eq!(error.code.close_code(), 1008);
        assert!(error.to_message().to_json().starts_with(r#"{"type":"error","code":"bad_message","message":""#));
    }
    // A frame that is not a frame: `bad_frame`, close 1007, nothing counted.
    let received = session.frames_received();
    let error = session.handle_frame(b"GSCF but not really").unwrap_err();
    assert_eq!((error.code.as_str(), error.code.close_code()), ("bad_frame", 1007));
    assert_eq!(session.frames_received(), received);

    // Finish answers with a last progress; later frames are not used.
    let reply = session.handle_text(r#"{"type":"finish"}"#).unwrap();
    assert!(matches!(reply.to_phone.as_slice(), [ServerMessage::Progress { frames: 5, .. }]), "{:?}", reply.to_phone);
    assert!(session.is_finished());
    let reply = session.handle_decoded(&frame(3, Tracking::Normal));
    assert_eq!((ack_of(&reply), reply.frame.unwrap().skipped), ((3, false), Some(SkipReason::Finished)));

    // The phone reconnects with the same hello: the capture goes on.
    assert!(session.matches(&Hello::offline(CaptureMode::Room, "Essai", "iPad", "9")));
    assert!(!session.matches(&Hello::offline(CaptureMode::Object, "Essai", "iPhone17,2", "0.1.0")));
    assert!(!session.matches(&Hello::offline(CaptureMode::Room, "Autre", "iPhone17,2", "0.1.0")));
    session.resume();
    assert!(!session.is_finished() && ack_of(&session.handle_decoded(&frame(4, Tracking::Normal))).1);

    // Reset: the scan starts again from nothing, counters included.
    let reply = session.handle_text(r#"{"type":"reset"}"#).unwrap();
    assert_eq!(reply.to_phone, [ServerMessage::Progress { frames: 0, splats: 0 }]);
    assert_eq!(kinds(&reply.to_viewers), "S 0r 1r T");
    let mut client = Client::default();
    client.apply(&reply.to_viewers);
    assert!(client.layers[0].is_empty() && client.stats().capture.unwrap().frames_received == 0);
    assert_eq!((session.frames_received(), session.floor()), (0, (0.0, false)));
    assert!(ack_of(&session.handle_decoded(&frame(5, Tracking::Normal))).1, "and it takes frames again");
    assert_eq!(session.name(), "Essai");
}

#[test]
fn frames_a_real_device_could_send_never_poison_the_map() {
    let (_, shots) = living_room_shots(small_sim(), 2, 20.0);
    let good = &shots[0].frame;
    let mut session = session(CaptureMode::Room);
    let skipped = |session: &mut CaptureSession, frame: CaptureFrame| session.handle_decoded(&frame).frame.unwrap().skipped;

    let mut pose = good.world_from_cam;
    pose[12] = f32::NAN;
    assert_eq!(skipped(&mut session, CaptureFrame { world_from_cam: pose, ..good.clone() }), Some(SkipReason::Pose));
    assert_eq!(skipped(&mut session, CaptureFrame { world_from_cam: [0.0; 16], ..good.clone() }), Some(SkipReason::Pose));
    assert_eq!(skipped(&mut session, CaptureFrame { intrinsics: [0.0, 0.0, 0.0, 0.0], ..good.clone() }), Some(SkipReason::Intrinsics));
    assert_eq!(skipped(&mut session, CaptureFrame { color: Vec::new(), ..good.clone() }), Some(SkipReason::NoColor));
    let mut broken = good.color.clone();
    broken.truncate(broken.len() / 3);
    assert_eq!(skipped(&mut session, CaptureFrame { color: broken, ..good.clone() }), Some(SkipReason::Color));
    assert_eq!(skipped(&mut session, CaptureFrame { color: b"not a jpeg".to_vec(), ..good.clone() }), Some(SkipReason::Color));
    assert_eq!(skipped(&mut session, CaptureFrame { depth_mm: vec![0; good.depth_mm.len()], ..good.clone() }), Some(SkipReason::NoDepth));
    // Everything beyond the range of the mode, or of no confidence at all.
    assert_eq!(skipped(&mut session, CaptureFrame { depth_mm: vec![9000; good.depth_mm.len()], ..good.clone() }), Some(SkipReason::NoDepth));
    let distrust = Some(vec![0u8; good.depth_mm.len()]);
    assert_eq!(skipped(&mut session, CaptureFrame { confidence: distrust, ..good.clone() }), Some(SkipReason::NoDepth));
    assert!(session.static_splats().is_empty() && session.frames_used() == 0 && session.frames_received() == 9);

    // A depth map and a colour image of unrelated sizes and aspect ratios
    // (a phone app that crops differently) still fuse.
    let odd = CaptureFrame { depth_width: 48, depth_height: 64, ..good.clone() };
    assert_eq!(skipped(&mut session, odd), None);
    // Without a confidence plane (older devices) every depth is trusted.
    assert_eq!(skipped(&mut session, CaptureFrame { confidence: None, ..good.clone() }), None);
    assert!(session.static_splats().iter().all(|s| s.pos.iter().chain(&s.scale).all(|v| v.is_finite())));
}

#[test]
fn the_floor_follows_the_phone_then_the_data() {
    let (phone, shots) = living_room_shots(small_sim(), 10, 100.0);
    let truth = phone.world.floor_y();
    assert!((truth + 1.4).abs() < 1e-5, "the phone starts 1.4 m above the floor: {truth}");

    // Told by the phone before the first frame: exact from the start.
    let mut told = session(CaptureMode::Room);
    let reply = told.handle_message(&ClientMessage::Floor { y: truth });
    assert_eq!(kinds(&reply.to_viewers), "S T");
    let mut client = Client::default();
    client.apply(&told.snapshot());
    for shot in &shots {
        client.apply(&told.handle_decoded(&shot.frame).to_viewers);
    }
    assert_eq!(told.floor(), (truth, true));
    // Mean height of the level surface near `expected`.
    let floor_of = |layer: &[Splat], expected: f32| {
        let ys: Vec<f32> =
            layer.iter().filter(|s| normal_of(s).y > 0.9 && (s.pos[1] - expected).abs() < 0.2).map(|s| s.pos[1]).collect();
        assert!(ys.len() > 500, "{} floor splats", ys.len());
        mean(&ys) - expected
    };
    assert!(floor_of(&client.layers[0], 0.0).abs() < 0.01);
    assert_eq!(client.counts[1], 2 + shots.len(), "no layer was sent twice");

    // Never told: a guess first (the phone 1.3 m up), then the data. The
    // viewer's cloud is consistent after every reply.
    let mut alone = session(CaptureMode::Room);
    let mut client = Client::default();
    client.apply(&alone.snapshot());
    client.apply(&alone.handle_decoded(&shots[0].frame).to_viewers);
    assert_eq!(alone.floor(), (-1.3, false));
    for shot in &shots[1..5] {
        client.apply(&alone.handle_decoded(&shot.frame).to_viewers);
    }
    assert!(floor_of(&client.layers[0], -0.1).abs() < 0.01, "the guess is 10 cm off, as expected");
    client.apply(&alone.progress().to_viewers);
    let (found, known) = alone.floor();
    assert!(known && (found - truth).abs() < 0.015, "found {found}");
    assert!(floor_of(&client.layers[0], 0.0).abs() < 0.015);
    assert_eq!(client.layers[0].len(), alone.static_splats().len());

    // Told late, after frames were fused: the phone's word wins, and the
    // very next frame brings the whole layer in the new coordinates.
    let late = truth + 0.25;
    let reply = alone.handle_message(&ClientMessage::Floor { y: late });
    assert_eq!(kinds(&reply.to_viewers), "S T");
    client.apply(&reply.to_viewers);
    let reply = alone.handle_decoded(&shots[5].frame);
    assert_eq!(kinds(&reply.to_viewers), "0r P T");
    client.apply(&reply.to_viewers);
    assert_eq!(alone.floor(), (late, true));
    assert!(floor_of(&client.layers[0], -0.25).abs() < 0.015);
    // A floor that moves by millimeters is not worth a new layer.
    assert_eq!(kinds(&alone.handle_message(&ClientMessage::Floor { y: late + 0.004 }).to_viewers), "T");
    // The data no longer overrides what the phone said.
    alone.progress();
    alone.finish();
    assert_eq!(alone.floor(), (late, true));
}

/// The stage with its dancer frozen at `t`, and without the dancer.
fn stage_at(t: f32) -> (SceneSnapshot, SceneSnapshot) {
    let stage = StageScene::new();
    (stage.snapshot(t), stage.snapshot_static())
}

/// Orbit of a phone around the dancer: frame `i` of `n`.
fn dancer_orbit(sim: &PhoneSim, i: usize, n: usize, turns: f32) -> Camera {
    let (eye, target) = orbit_pose(Vec3::new(0.0, 1.0, 0.0), 2.3, 1.1, 1.7, i as f32 / n as f32, turns);
    sim.look_at(eye, target)
}

#[test]
fn object_mode_is_cropped_to_its_region_and_follows_it() {
    let sim = small_sim();
    let (snap, _) = stage_at(1.0);
    let n = 24;
    let mut phone = Phone::new(sim, &dancer_orbit(&sim, 0, n, 1.0));
    let shots: Vec<Shot> = (0..n).map(|i| phone.shoot(&snap, None, &dancer_orbit(&sim, i, n, 1.0), i as f64 * 0.1)).collect();
    let dancer = phone.world.to_capture(Vec3::new(0.0, 1.0, 0.0));
    let inside = |splats: &[Splat], center: Vec3, radius: f32, floor: f32| {
        splats.iter().all(|s| (s.position() + Vec3::new(0.0, floor, 0.0)).distance(center) <= radius + 1e-3)
    };

    let mut session = session(CaptureMode::Object);
    assert_eq!(session.params().object.voxel_size, 0.008, "finer cells than a room");
    let mut client = Client::default();
    client.apply(&session.snapshot());
    // Before the phone names a region: 0.6 m around what the first frame
    // looks at, which is the dancer.
    for shot in &shots[..6] {
        client.apply(&session.handle_decoded(&shot.frame).to_viewers);
    }
    let auto = session.region().unwrap();
    assert_eq!(auto.radius, 0.6);
    assert!(Vec3::from_array(auto.center).distance(dancer) < 0.45, "{:?} vs {dancer:?}", auto.center);
    let floor = session.floor().0;
    assert!(!client.layers[0].is_empty() && inside(&client.layers[0], Vec3::from_array(auto.center), 0.6, floor));
    let announced = client.state.as_ref().unwrap().capture.as_ref().unwrap().region.unwrap();
    assert_eq!(announced.radius, 0.6);

    // The phone places the region on the dancer's head: the State follows
    // at once, the cloud within a second; nothing outside is left.
    let head = dancer + Vec3::new(0.0, 0.6, 0.0);
    let region = ClientMessage::Region(CaptureRegion { center: head.to_array(), radius: 0.35 });
    let reply = session.handle_text(&region.to_json()).unwrap();
    assert_eq!(kinds(&reply.to_viewers), "S T");
    client.apply(&reply.to_viewers);
    let reply = session.progress();
    assert!(kinds(&reply.to_viewers).contains("0r"), "{}", kinds(&reply.to_viewers));
    client.apply(&reply.to_viewers);
    let floor = session.floor().0;
    assert!(!client.layers[0].is_empty() && inside(&client.layers[0], head, 0.35, floor));
    let small = client.layers[0].len();

    // Enlarged a little: what was fused around the old region shows up
    // without a single new frame.
    let wider = ClientMessage::Region(CaptureRegion { center: head.to_array(), radius: 0.5 });
    client.apply(&session.handle_message(&wider).to_viewers);
    client.apply(&session.progress().to_viewers);
    assert!(client.layers[0].len() > small * 3 / 2, "{} then {}", small, client.layers[0].len());
    assert!(inside(&client.layers[0], head, 0.5, session.floor().0));

    // The rest of the orbit with the region on the whole dancer.
    let whole = ClientMessage::Region(CaptureRegion { center: dancer.to_array(), radius: 1.1 });
    client.apply(&session.handle_message(&whole).to_viewers);
    for shot in &shots[6..] {
        client.apply(&session.handle_decoded(&shot.frame).to_viewers);
    }
    client.apply(&session.finish().to_viewers);
    let floor = session.floor().0;
    assert!(inside(&client.layers[0], dancer, 1.1, floor));
    assert!(client.layers[0].len() > 5_000, "{} splats", client.layers[0].len());
    // Seen from all around, at 8 mm.
    let cloud = phone.world.to_scene(&client.layers[0], floor);
    for side in [Vec3::X, Vec3::NEG_X, Vec3::Z, Vec3::NEG_Z] {
        assert!(cloud.iter().filter(|s| normal_of(s).dot(side) > 0.7 && s.pos[1] > 0.5).count() > 300, "nothing faces {side}");
    }
    // A region in room mode means nothing and changes nothing.
    let mut room = self::session(CaptureMode::Room);
    assert_eq!(kinds(&room.handle_message(&whole).to_viewers), "T");
    assert_eq!(room.region(), None);
}

#[test]
fn person_mode_keeps_the_person_and_live_mode_shows_one_view() {
    let sim = small_sim();
    let (snap, empty) = stage_at(1.0);
    let n = 16;
    let mut phone = Phone::new(sim, &dancer_orbit(&sim, 0, n, 1.0));
    let shots: Vec<Shot> = (0..n).map(|i| phone.shoot(&snap, Some(&empty), &dancer_orbit(&sim, i, n, 1.0), i as f64 * 0.1)).collect();
    assert!(shots.iter().all(|s| s.frame.mask.as_ref().unwrap().iter().filter(|&&m| m == 255).count() > 100));

    // Person: only what the mask marks is fused. The stage floor inside the
    // region (radius 1.2 m by default) stays out.
    let mut person = session(CaptureMode::Person);
    person.handle_message(&ClientMessage::Floor { y: phone.world.floor_y() });
    for shot in &shots {
        assert!(ack_of(&person.handle_decoded(&shot.frame)).1);
    }
    person.finish();
    let cloud = phone.world.to_scene(&person.static_splats(), person.floor().0);
    assert!(cloud.len() > 3_000, "{} splats", cloud.len());
    let on_stage_floor = cloud.iter().filter(|s| s.pos[1] < 0.02).count();
    assert!(on_stage_floor * 20 < cloud.len(), "{on_stage_floor} of {} splats are stage floor", cloud.len());
    assert!(cloud.iter().all(|s| Vec3::new(s.pos[0], 0.0, s.pos[2]).length() < 1.3), "the dancer stands in the middle");
    // The same frames without their masks: the floor comes along.
    let mut unmasked = session(CaptureMode::Person);
    unmasked.handle_message(&ClientMessage::Floor { y: phone.world.floor_y() });
    for shot in &shots {
        unmasked.handle_decoded(&CaptureFrame { mask: None, ..shot.frame.clone() });
    }
    let with_floor = phone.world.to_scene(&unmasked.static_splats(), unmasked.floor().0);
    assert!(with_floor.iter().filter(|s| s.pos[1] < 0.02).count() > 2_000);
    // Once masks were seen, a frame that shows nobody adds nothing.
    let nobody = CaptureFrame { mask: Some(vec![0; shots[0].frame.depth_mm.len()]), ..shots[0].frame.clone() };
    assert_eq!(person.handle_message(&ClientMessage::Resume).to_viewers.len(), 1);
    assert_eq!(person.handle_decoded(&nobody).frame.unwrap().skipped, Some(SkipReason::NoPerson));
    assert_eq!(person.handle_decoded(&CaptureFrame { mask: None, ..shots[0].frame.clone() }).frame.unwrap().skipped, Some(SkipReason::NoPerson));

    // Live: every frame replaces layer 1 with that one view; layer 0 is
    // never touched; limited tracking is no reason to drop a frame.
    let mut live = session(CaptureMode::Live);
    let mut client = Client::default();
    client.apply(&live.snapshot());
    let mut sizes = Vec::new();
    for (i, shot) in shots.iter().take(6).enumerate() {
        let tracking = if i == 3 { Tracking::Limited } else { Tracking::Normal };
        // The phone is held upright: the picture is turned, the geometry not.
        let frame = CaptureFrame { tracking, orientation: Orientation::Portrait, ..shot.frame.clone() };
        let reply = live.handle_decoded(&frame);
        assert_eq!(ack_of(&reply), (frame.index, true));
        // (A State comes along when the view shows enough floor to find it.)
        let kinds = kinds(&reply.to_viewers);
        assert!(kinds == "S 1r P T" || (i > 0 && kinds == "1r P T"), "frame {i}: {kinds}");
        client.apply(&reply.to_viewers);
        assert!(client.layers[0].is_empty());
        assert_eq!(client.layers[1].len(), live.dynamic_splats().len());
        sizes.push(client.layers[1].len());
        assert_eq!(client.stats().splats_dynamic as usize, client.layers[1].len());
    }
    // (The stage has no walls: a third of every picture shows the scene.)
    assert!(sizes.iter().all(|&n| (700..=3072).contains(&n)), "{sizes:?}");
    let preview = &client.previews[&0];
    assert_eq!((preview.width, preview.height), (48, 64), "the preview is upright");
    // The single view lies where the scene is: the dancer's splats are
    // within reach of the dancer, the stage floor at y = 0 once the floor
    // was found in the view itself.
    let (floor, known) = live.floor();
    assert!(known && (floor - phone.world.floor_y()).abs() < 0.03, "floor {floor}");
    let cloud = phone.world.to_scene(&client.layers[1], floor);
    assert!(cloud.iter().filter(|s| normal_of(s).y > 0.9 && s.pos[1].abs() < 0.03).count() > 300);
    // With the switch, live shows the person only.
    let params = IngestParams { live_person_only: true, ..IngestParams::default() };
    let mut person_only = CaptureSession::new(&Hello::offline(CaptureMode::Live, "x", "d", "a"), params);
    person_only.handle_decoded(&shots[0].frame);
    let count = person_only.dynamic_splats().len();
    assert!(count > 50 && count * 2 < sizes[0], "{count} of {}", sizes[0]);
    // Finishing a live capture leaves its last view in place.
    let reply = live.finish();
    assert_eq!(kinds(&reply.to_viewers), "S 0r T");
    assert!(!live.dynamic_splats().is_empty());

    // Live over a finished scan of the same AR session: the scan stays as
    // layer 0 (cropped as it was shown), at its floor, and takes no frame.
    let backdrop = person.static_splats();
    let floor = person.floor();
    let mut over = person.into_live(&Hello::offline(CaptureMode::Live, "Direct", "d", "a"));
    assert_eq!((over.mode(), over.name(), over.frames_received(), over.floor()), (CaptureMode::Live, "Direct", 0, floor));
    let mut client = Client::default();
    client.apply(&over.snapshot());
    assert_eq!(client.layers[0].len(), backdrop.len());
    let reply = over.handle_decoded(&shots[2].frame);
    assert!(kinds(&reply.to_viewers).ends_with("1r P T") && !kinds(&reply.to_viewers).contains('0'));
    client.apply(&reply.to_viewers);
    assert_eq!(over.static_splats(), backdrop);
    assert!(!client.layers[1].is_empty() && over.floor() == floor);
}

/// Mean colour of a cloud.
fn mean_rgb(splats: &[Splat]) -> [f32; 3] {
    let mut sum = [0.0f64; 3];
    for s in splats {
        for (total, &value) in sum.iter_mut().zip(&s.rgba) {
            *total += value as f64;
        }
    }
    sum.map(|v| (v / splats.len().max(1) as f64) as f32)
}

#[test]
fn exposure_drift_and_single_frames_do_not_repaint_the_map() {
    // The same wall-to-wall view of the living room, filmed from two spots
    // 5 cm apart, exposure jumping between frames.
    let sim = small_sim();
    let snap = house();
    let cam = |i: usize| {
        let eye = Vec3::new(-2.6 + 0.05 * (i % 2) as f32, 1.4, -1.8);
        sim.look_at(eye, eye + Vec3::new(-1.0, -0.2, -0.6))
    };
    let mut phone = Phone::new(sim, &cam(0));
    let shots: Vec<Shot> = (0..12).map(|i| phone.shoot(&snap, None, &cam(i), i as f64 * 0.1)).collect();
    // Re-expose a frame: decode its JPEG, scale, encode again.
    let exposed = |frame: &CaptureFrame, gain: [f32; 3]| {
        let image = gs_core::ingest::image::decode_jpeg(&frame.color, 160, 120).unwrap();
        let rgb: Vec<u8> = image.rgb.chunks(3).flat_map(|p| [0, 1, 2].map(|c| (p[c] as f32 * gain[c]).min(255.0) as u8)).collect();
        let mut color = Vec::new();
        jpeg_encoder::Encoder::new(&mut color, 90).encode(&rgb, 160, 120, jpeg_encoder::ColorType::Rgb).unwrap();
        CaptureFrame { color, ..frame.clone() }
    };
    let run = |params: IngestParams, gains: &dyn Fn(usize) -> [f32; 3]| {
        let mut session = CaptureSession::new(&Hello::offline(CaptureMode::Room, "x", "d", "a"), params);
        let mut after_first = [0.0; 3];
        for (i, shot) in shots.iter().enumerate() {
            session.handle_decoded(&exposed(&shot.frame, gains(i)));
            if i == 0 {
                after_first = mean_rgb(&session.static_splats());
            }
        }
        (after_first, mean_rgb(&session.static_splats()))
    };
    let distance = |a: [f32; 3], b: [f32; 3]| (0..3).map(|c| (a[c] - b[c]).abs()).fold(0.0f32, f32::max);
    let defaults = IngestParams::default();
    assert!(defaults.exposure_compensation && defaults.blend_frames >= 8);

    // Steady exposure: the reference. Compensation finds nothing to do.
    let (_, steady) = run(defaults, &|_| [1.0; 3]);
    let (_, untouched) = run(IngestParams { exposure_compensation: false, ..defaults }, &|_| [1.0; 3]);
    assert!(distance(steady, untouched) < 1.5, "{steady:?} compensated, {untouched:?} not");
    // Every frame after the first 30 % darker and bluish: compensated, the
    // map keeps the look of its first frame; uncompensated it drifts.
    let dark = |i: usize| if i == 0 { [1.0; 3] } else { [0.6, 0.7, 0.8] };
    let (_, compensated) = run(defaults, &dark);
    let (_, raw) = run(IngestParams { exposure_compensation: false, ..defaults }, &dark);
    assert!(distance(steady, compensated) < 4.0, "steady {steady:?}, compensated {compensated:?}");
    assert!(distance(steady, raw) > 20.0, "steady {steady:?}, uncompensated {raw:?}");
    // One wild frame among steady ones (a flash, the sun through a window):
    // even uncompensated it is one vote among twelve.
    let flash = |i: usize| if i == 8 { [1.6; 3] } else { [1.0; 3] };
    let (_, flashed) = run(IngestParams { exposure_compensation: false, ..defaults }, &flash);
    assert!(distance(steady, flashed) < 8.0, "steady {steady:?}, flashed {flashed:?}");
    // Compensated, what remains is what the flash clipped to white.
    let (_, flashed) = run(defaults, &flash);
    assert!(distance(steady, flashed) < 6.0, "steady {steady:?}, flashed and compensated {flashed:?}");
}

#[test]
fn noise_jitter_and_the_cell_cap_keep_the_map_single_layered_and_bounded() {
    // Two turns on the spot, a frame every 10 degrees: every surface is
    // seen half a dozen times, as when a person pans a phone.
    let ideal = living_room_shots(small_sim(), 72, 720.0);
    let rough_sim = PhoneSim { color_width: 160, color_height: 120, depth_width: 64, depth_height: 48, seed: 7, ..PhoneSim::realistic() };
    let noisy = living_room_shots(rough_sim, 72, 720.0);
    let fuse = |shots: &[Shot], params: IngestParams| {
        let mut session = CaptureSession::new(&Hello::offline(CaptureMode::Room, "x", "d", "a"), params);
        for shot in shots {
            session.handle_decoded(&shot.frame);
        }
        session
    };
    let clean = fuse(&ideal.1, IngestParams::default());
    let rough = fuse(&noisy.1, IngestParams::default());
    let (a, b) = (clean.static_splats().len(), rough.static_splats().len());
    // Noise of 1.5 mm at 1 m (1 cm at 2.6 m, 2.4 cm on the far wall) and
    // poses off by 3 mm plus drift: were surfaces doubled, there would be
    // twice the splats.
    assert!(a > 15_000 && (b as f32) < a as f32 * 1.45 && (b as f32) > a as f32 * 0.85, "{a} clean, {b} noisy");
    // The final pass drops what a single frame put somewhere: the rim of
    // what the turn saw (cells only the first or the last frame reached),
    // and under noise the specks that no second frame confirmed.
    let (mut clean, mut rough) = (clean, rough);
    clean.finish();
    rough.finish();
    let (a2, b2) = (clean.static_splats().len(), rough.static_splats().len());
    println!("clean {a} -> {a2}, noisy {b} -> {b2}");
    assert!(a2 <= a && a2 as f32 > a as f32 * 0.99, "{a} before the final pass, {a2} after");
    assert!(b2 < b && b2 as f32 > b as f32 * 0.9, "{b} before the final pass, {b2} after");

    // A cap far below what the room needs: reached, reported, respected.
    let capped = IngestParams { max_voxels: 5_000, ..IngestParams::default() };
    let mut small = fuse(&ideal.1, capped);
    assert_eq!(small.static_splats().len(), 5_000);
    let stats = small.stats();
    assert!(stats.capture.unwrap().full && stats.splats_static == 5_000);
    assert!(!rough.stats().capture.unwrap().full);
    // Frames still count as used: they refine what is there.
    assert!(small.handle_decoded(&ideal.1[0].frame).frame.unwrap().used);
    assert_eq!(small.finish().to_phone, [ServerMessage::Progress { frames: 73, splats: small.static_splats().len() as u64 }]);
}

#[test]
fn a_file_imports_to_the_same_cloud_as_its_stream() {
    let (phone, shots) = living_room_shots(small_sim(), 10, 90.0);
    let header = GscapHeader {
        protocol: Some(1),
        device: "simulateur".into(),
        app: "0.1.0".into(),
        mode: "room".into(),
        name: "Salon simulé".into(),
        created: "2026-11-14T12:00:00Z".into(),
        region: None,
        floor: Some(phone.world.floor_y()),
    };
    let mut writer = GscapWriter::new(Vec::new(), &header, 1024).unwrap();
    for shot in &shots {
        writer.append(&encode_frame(&shot.frame).unwrap()).unwrap();
    }
    let file = writer.into_inner();

    // Streamed.
    let mut streamed = CaptureSession::from_header(&header, IngestParams::default()).unwrap();
    for shot in &shots {
        streamed.handle_decoded(&shot.frame);
    }
    streamed.finish();

    // Imported in pieces of every size: never more than one frame buffered.
    for chunk in [1000, 64 * 1024, file.len()] {
        let mut importer = GscapImporter::new(IngestParams::default());
        for piece in file.chunks(chunk) {
            importer.push(piece).unwrap();
        }
        assert_eq!(importer.frames(), 10);
        assert_eq!(importer.session().unwrap().frames_used(), 10);
        let (mut session, summary) = importer.finish().unwrap();
        assert_eq!((summary.frames, summary.frames_used, summary.truncated, summary.name.as_str()), (10, 10, false, "Salon simulé"));
        assert_eq!(summary.bytes, file.len() as u64);
        assert_eq!(session.static_splats(), streamed.static_splats(), "pieces of {chunk}");
        let state = session.state();
        assert!(state.capture.as_ref().unwrap().imported && session.is_finished());
        let mut client = Client::default();
        client.apply(&session.snapshot());
        assert_eq!(client.layers[0].len(), streamed.static_splats().len());
        assert_eq!(client.scan_state(), Some(ScanState::Done));
    }
    // From a reader, and cut short in the middle of the last frame.
    let (session, summary) = gs_core::ingest::import_gscap_reader(std::io::Cursor::new(&file), IngestParams::default()).unwrap();
    assert_eq!((summary.frames, session.static_splats().len()), (10, streamed.static_splats().len()));
    let (session, summary) = gs_core::ingest::import_gscap(&file[..file.len() - 500], IngestParams::default()).unwrap();
    assert_eq!((summary.frames, summary.truncated), (9, true));
    assert!(session.is_finished() && !session.static_splats().is_empty());
}

// ---------------------------------------------------------------------------
// Novel-view quality
// ---------------------------------------------------------------------------

fn save_sheet(name: &str, scores: &[Score]) {
    if let Some(dir) = output_dir() {
        let sheets: Vec<_> = scores.iter().map(side_by_side).collect();
        save_png(&dir.join(name), &stack(&sheets));
    }
}

/// Where a person filming the bedroom walks: the loop of the house tour
/// between bed and wardrobe, in and out through the door.
const BEDROOM_LOOP: [[f32; 2]; 7] =
    [[-1.3, 2.0], [-1.7, 1.78], [-2.35, 1.36], [-2.92, 1.78], [-2.8, 2.38], [-2.25, 2.6], [-1.68, 2.28]];

/// Films the bedroom: `n` frames along [`BEDROOM_LOOP`], the phone swept
/// in a spiral (a full turn every 55 frames, 6.5 degrees per frame).
/// Returns the session, the phone and what it truly saw.
fn film_bedroom(sim: PhoneSim, params: IngestParams, n: usize, tell_floor: bool) -> (CaptureSession, Phone, Exposures) {
    let snap = house();
    let camera = |i: usize| sim.camera(ingest_sim::room_sweep(&BEDROOM_LOOP, i, n, 55.0));
    let mut phone = Phone::new(sim, &camera(0));
    let mut session = CaptureSession::new(&Hello::offline(CaptureMode::Room, "Chambre", "simulateur", "0.1.0"), params);
    if tell_floor {
        session.handle_message(&ClientMessage::Floor { y: phone.world.floor_y() });
    }
    let mut frames = Vec::new();
    for i in 0..n {
        let shot = phone.shoot(&snap, None, &camera(i), i as f64 * 0.1);
        let reply = session.handle_decoded(&shot.frame);
        assert!(reply.frame.unwrap().used, "frame {i}: {:?}", reply.frame);
        frames.push((shot.cam, shot.truth));
        if i % 10 == 9 {
            session.progress();
        }
    }
    session.finish();
    (session, phone, Exposures { frames, max_range: params.room.far })
}

/// Scores the views of `room` (those the phone has seen at least 60 % of).
fn score_room(session: &CaptureSession, phone: &Phone, exposures: &Exposures, room: &str, label: &str) -> (f32, f32, f32, usize) {
    let snap = house();
    let cloud = phone.world.to_scene(&session.static_splats(), session.floor().0);
    let layers = [cloud, Vec::new()];
    let mut scores = Vec::new();
    for view in house_views().iter().filter(|v| v.name.starts_with(room)) {
        let mask = observed_mask(&snap, exposures, &view.cam);
        let score = evaluate(&snap, &layers, &view.cam, true, Some(&mask), Renderer::Normalized);
        println!(
            "{label} {}: PSNR {:.2} dB ({:.2} where seen), SSIM {:.3}, coverage {:.2} % ({:.2} where seen), seen {:.1} %",
            view.name,
            score.psnr,
            score.psnr_observed,
            score.ssim,
            100.0 * score.coverage,
            100.0 * score.coverage_observed,
            100.0 * score.observed_fraction
        );
        if score.observed_fraction >= 0.6 {
            scores.push(score);
        }
    }
    save_sheet(&format!("capture_{}.png", label.replace(' ', "_")), &scores);
    let psnr = mean(&scores.iter().map(|s| s.psnr_observed).collect::<Vec<_>>());
    let ssim = mean(&scores.iter().map(|s| s.ssim).collect::<Vec<_>>());
    let coverage = mean(&scores.iter().map(|s| s.coverage_observed).collect::<Vec<_>>());
    println!("{label}: {} splats, {} views, PSNR where seen {psnr:.2} dB, SSIM {ssim:.3}, coverage where seen {:.2} %", layers[0].len(), scores.len(), 100.0 * coverage);
    (psnr, ssim, coverage, layers[0].len())
}

#[test]
fn a_filmed_bedroom_is_reconstructed_like_the_simulated_scan() {
    // An ideal phone: 256x192 LiDAR depth, colour 1.5 times finer (the
    // ray caster's cost is the colour image; the examples use 960x720).
    let sim = PhoneSim { color_width: 384, color_height: 288, ..PhoneSim::default() };
    let (session, phone, exposures) = film_bedroom(sim, IngestParams::default(), 220, true);
    let (psnr, ssim, coverage, splats) = score_room(&session, &phone, &exposures, "bedroom", "ideal phone");
    // Measured: 141 898 splats of 2.4 cm; where the phone has looked, PSNR
    // 25.79 dB, SSIM 0.833, coverage 99.79 %. The simulated scan head (five
    // exact sensors with co-registered colour, the same cells) scores
    // 25.50 dB, SSIM 0.868 and 99.83 % on these four views
    // (`the_simulated_scan_scores_on_the_same_views`): the phone's finer
    // depth map makes up for its averaged depth and its JPEG. Its SSIM is
    // lower because it counts the whole picture, and 3 to 12 % of these
    // views the phone never looked at (the floor under the person).
    assert!(psnr >= 25.3 && ssim >= 0.82 && coverage >= 0.995, "PSNR {psnr:.2} dB, SSIM {ssim:.3}, coverage {coverage:.4}");
    assert!((120_000..170_000).contains(&splats), "{splats} splats");

    // A realistic one: noise, jitter, drift, wandering exposure, and no
    // floor message.
    let sim = PhoneSim { color_width: 384, color_height: 288, ..PhoneSim::realistic() };
    let (session, phone, exposures) = film_bedroom(sim, IngestParams::default(), 220, false);
    let (floor, known) = session.floor();
    assert!(known && (floor - phone.world.floor_y()).abs() < 0.02, "floor found at {floor}, truly {}", phone.world.floor_y());
    let ceiling = session.state().ceiling_height.unwrap();
    assert!((ceiling - house::CEILING_HEIGHT).abs() < 0.04, "ceiling at {ceiling}");
    let (noisy_psnr, noisy_ssim, noisy_coverage, _) = score_room(&session, &phone, &exposures, "bedroom", "realistic phone");
    // Measured: 176 086 splats, PSNR 23.49 dB, SSIM 0.797, coverage 99.85 %.
    // The 2.3 dB are the colour cast of the first frame, which the exposure
    // compensation spreads over the whole map (there is no absolute
    // reference), and doubled fringes at 3 to 4 m, where the simulated
    // depth noise (1.4 to 2.4 cm) reaches the size of a cell: a quarter
    // more splats than the ideal phone leaves.
    assert!(noisy_psnr >= 23.0 && noisy_ssim >= 0.78 && noisy_coverage >= 0.995, "PSNR {noisy_psnr:.2} dB, SSIM {noisy_ssim:.3}, coverage {noisy_coverage:.4}");
}

/// The yardstick for the test above: the simulated home scan (five exact
/// sensors, the default 2.4 cm cells), scored on the same bedroom views.
/// Run it with `--ignored` to refresh the numbers quoted there.
#[test]
#[ignore = "reference numbers only: the whole simulated house scan"]
fn the_simulated_scan_scores_on_the_same_views() {
    use gs_core::pipeline::Pipeline;
    use gs_core::protocol::PipelineConfig;
    let mut config = PipelineConfig::for_scene(SceneKind::House);
    config.scan.autostart = false;
    config.preview.enabled = false;
    let mut pipeline = Pipeline::new(config);
    let mut client = Client::default();
    common::run_full_scan(&mut pipeline, &mut client);
    let snap = house();
    let exposures = Exposures::of_scan(&snap, pipeline.config());
    let mut scores = Vec::new();
    for view in house_views().iter().filter(|v| v.name.starts_with("bedroom")) {
        let mask = observed_mask(&snap, &exposures, &view.cam);
        let score = evaluate(&snap, &client.layers, &view.cam, true, Some(&mask), Renderer::Normalized);
        println!("scan head {}: PSNR {:.2} dB ({:.2} where seen), SSIM {:.3}, coverage where seen {:.2} %", view.name, score.psnr, score.psnr_observed, score.ssim, 100.0 * score.coverage_observed);
        scores.push(score);
    }
    println!(
        "scan head, bedroom: PSNR where seen {:.2} dB, SSIM {:.3}, coverage where seen {:.2} %",
        mean(&scores.iter().map(|s| s.psnr_observed).collect::<Vec<_>>()),
        mean(&scores.iter().map(|s| s.ssim).collect::<Vec<_>>()),
        100.0 * mean(&scores.iter().map(|s| s.coverage_observed).collect::<Vec<_>>())
    );
}
