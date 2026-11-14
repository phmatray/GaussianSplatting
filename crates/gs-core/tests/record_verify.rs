//! Second opinion on the recorder and the player: random transport against
//! all three sources, sessions that change under the recorder, recordings
//! read while they are written, long recordings, and the limits.

mod record_support;

use gs_core::pipeline::Pipeline;
use gs_core::protocol::{ControlMsg, PipelineConfig};
use gs_core::record::{
    verify, ByteRange, Compression, KeyframePolicy, Player, PlayerOptions, Pushed, Recorder, RecorderOptions,
    StreamState, Update,
};
use gs_core::scene::SceneKind;
use record_support::*;

/// One random step of a user at the transport controls.
fn random_transport(player: &mut Player, random: &mut Lcg, end: f64) -> Option<f64> {
    match (random.next() * 10.0) as u32 {
        0 => {
            player.seek(random.next() * end * 1.1 - 0.05 * end);
            None
        }
        1 => {
            player.set_speed([0.25, 0.5, 1.0, 1.0, 2.0, 8.0, 40.0][(random.next() * 7.0) as usize]);
            None
        }
        2 => {
            player.set_looping(random.next() < 0.5);
            None
        }
        3 => {
            if player.is_playing() {
                player.pause()
            } else {
                player.play()
            }
            None
        }
        4 => {
            player.play();
            None
        }
        // Most steps are frames of various lengths.
        _ => Some([1.0 / 120.0, 1.0 / 60.0, 1.0 / 30.0, 0.2, 0.0][(random.next() * 5.0) as usize]),
    }
}

fn apply_checked(viewer: &mut Viewer, player: &Player, update: &Update, stream: &Stream, tolerance: f32, what: &str) {
    assert_eq!(update.time, player.time(), "{what}: the update reports the play head");
    viewer.apply_all(update);
    if !update.buffering {
        viewer.assert_shows(&Viewer::live(stream, update.time), tolerance, what);
    }
}

/// Random play, pause, speed, loop and seek against a player that holds
/// the file: after every step the viewer shows the live session at the
/// play head.
fn fuzz_whole(stream: &Stream, file: &[u8], tolerance: f32, steps: usize, seed: u64) {
    let end = stream.last().unwrap().0;
    let mut player = Player::from_bytes(file.to_vec()).unwrap();
    let mut viewer = Viewer::default();
    let mut random = Lcg(seed);
    let mut ends = 0;
    for step in 0..steps {
        let update = match random_transport(&mut player, &mut random, end) {
            Some(dt) => player.advance(dt),
            None => player.update(),
        };
        assert!(!update.buffering);
        assert!((0.0..=end).contains(&update.time), "{}", update.time);
        ends += usize::from(update.ended);
        if update.ended {
            assert!(!player.is_playing() && update.time == end);
        }
        apply_checked(&mut viewer, &player, &update, stream, tolerance, &format!("whole step {step} (seed {seed})"));
    }
    assert!(ends > 0 || steps < 500, "the end was never reached in {steps} steps");
}

/// The same against a `Range` player whose requests are answered late, a
/// few at a time, with a cache of `cache` bytes.
fn fuzz_ranged(stream: &Stream, file: &[u8], tolerance: f32, steps: usize, seed: u64, cache: u64, block: u64) {
    let end = stream.last().unwrap().0;
    let mut player = Player::ranged(file.len() as u64);
    player.set_options(PlayerOptions { fetch_block: block, max_cache_bytes: cache, prefetch_s: 0.5, jump_s: 2.0 });
    let mut viewer = Viewer::default();
    let mut random = Lcg(seed);
    let mut requests: Vec<ByteRange> = Vec::new();
    let what = |step: usize| format!("ranged step {step} (seed {seed}, cache {cache}, block {block})");
    for step in 0..steps {
        // Zero to three answers between two steps of the user.
        for _ in 0..(random.next() * 4.0) as usize {
            let Some(range) = player.wanted() else { break };
            assert!(range.start < range.end && range.end <= file.len() as u64, "{range:?}");
            requests.push(range);
            let used = player.supply(range.start, &file[range.start as usize..range.end as usize]).unwrap();
            assert!(used, "{}: {range:?} was asked for and then refused", what(step));
        }
        let before = player.time();
        let (update, clocked) = match random_transport(&mut player, &mut random, end) {
            Some(dt) => (player.advance(dt), true),
            None => (player.update(), false),
        };
        if update.buffering && clocked && !player.is_looping() {
            // It may still close up to the last record that is on screen.
            assert!(
                update.time <= before.max(player.buffered_until()) + 1e-12,
                "{}: a waiting head does not run on: {before} to {} (buffered until {})",
                what(step),
                update.time,
                player.buffered_until()
            );
        }
        apply_checked(&mut viewer, &player, &update, stream, tolerance, &what(step));
        // Every so often the network catches up: the player must settle on
        // the right picture, however it was pushed around before.
        if step % 37 == 36 {
            player.pause();
            let update = settle(&mut player, file, &mut viewer, &mut requests);
            assert_eq!(update.time, player.time());
            viewer.assert_shows(&Viewer::live(stream, update.time), tolerance, &format!("{} settled", what(step)));
        }
    }
    let fetched: u64 = requests.iter().map(ByteRange::len).sum();
    assert!(fetched > 0);
}

#[test]
fn random_transport_keeps_the_viewer_on_the_live_picture() {
    let stream = tiny_stream();
    let file = record(&stream, tiny_options());
    for seed in 1..=12 {
        fuzz_whole(&stream, &file, TINY_TOLERANCE, 1500, seed);
    }
    for seed in 1..=10 {
        for (cache, block) in [(3_000, 700), (6_000, 500), (20_000, 1500), (1 << 30, 4000), (0, 300)] {
            fuzz_ranged(&stream, &file, TINY_TOLERANCE, 1200, seed, cache, block);
        }
    }
    // Uncompressed, with keyframes as rare as the default policy makes them.
    let options = RecorderOptions { compression: Compression::None, ..options("tiny") };
    let file = record(&stream, options);
    for seed in 20..24 {
        fuzz_whole(&stream, &file, TINY_TOLERANCE, 800, seed);
        fuzz_ranged(&stream, &file, TINY_TOLERANCE, 800, seed, 4_000, 600);
    }
}

/// A session in which everything happens: a stage, a change of scene to
/// the house, a scan that starts, pauses, resumes, is thrown away and
/// started again, a restart, and back to the stage.
fn eventful_stream() -> Stream {
    let config = PipelineConfig::light(SceneKind::Stage);
    let mut pipeline = Pipeline::new(config);
    let dt = 1.0 / config.target_hz;
    let mut stream: Stream = pipeline.snapshot().into_iter().map(|o| (0.0, o.bytes)).collect();
    for tick in 1..=330usize {
        let time = tick as f64 * dt as f64;
        let control = match tick {
            30 => Some(ControlMsg::SetScene { scene: SceneKind::House }),
            40 => Some(ControlMsg::ScanStart),
            100 => Some(ControlMsg::ScanPause),
            110 => Some(ControlMsg::ScanStart),
            150 => Some(ControlMsg::ScanReset),
            160 => Some(ControlMsg::ScanStart),
            200 => Some(ControlMsg::SetPaused { paused: true }),
            210 => Some(ControlMsg::SetPaused { paused: false }),
            240 => Some(ControlMsg::Restart),
            250 => Some(ControlMsg::ScanStart),
            290 => Some(ControlMsg::SetScene { scene: SceneKind::Stage }),
            310 => Some(ControlMsg::PatchConfig { patch: serde_json::json!({ "rig": { "count": 4 } }) }),
            _ => None,
        };
        if let Some(control) = control {
            stream.extend(pipeline.handle_control(control).into_iter().map(|o| (time, o.bytes)));
        }
        stream.extend(pipeline.tick(dt).into_iter().map(|o| (time, o.bytes)));
    }
    stream
}

/// Half a quantization step of the widest layer 0 box in `stream`, with
/// some slack: how far a merged keyframe may move a position.
fn merge_tolerance(stream: &Stream) -> f32 {
    let mut extent = 0.0f32;
    for (_, message) in stream {
        if let gs_core::codec::Body::Layer(layer) = gs_core::codec::decode(message).unwrap().body {
            if layer.layer_id == 0 {
                for k in 0..3 {
                    let (lo, hi) =
                        layer.splats.iter().fold((f32::MAX, f32::MIN), |(lo, hi), s| (lo.min(s.pos[k]), hi.max(s.pos[k])));
                    extent = extent.max(hi - lo);
                }
            }
        }
    }
    // The union box of several chunks is at most the box of the whole
    // house, some 16 m.
    (extent.max(16.0) / 65535.0) * 0.51 + 1.0e-6
}

#[test]
fn a_session_that_changes_scene_resets_and_scans_replays() {
    let stream = eventful_stream();
    let states = stream.iter().filter(|(_, m)| gs_core::record::classify(m) == Some(gs_core::record::MessageClass::State));
    assert!(states.count() >= 8, "the controls each announce a new State");
    let tolerance = merge_tolerance(&stream);
    assert!(tolerance < 2.0e-4, "{tolerance}");

    // Default policy, and keyframes forced into every phase of the session.
    let dense = KeyframePolicy { min_interval_s: 0.5, ratio: 0.05 };
    for (name, policy, compression) in [
        ("default", KeyframePolicy::default(), Compression::default()),
        ("dense", dense, Compression::default()),
        ("dense raw", dense, Compression::None),
    ] {
        let file = record(&stream, RecorderOptions { keyframes: policy, compression, ..options("eventful") });
        let report = verify(&file).unwrap();
        assert!(report.is_valid(), "{name}: {:?}", report.problems);
        assert_eq!(report.info.scene, "stage", "the scene the recording starts in");
        if name != "default" {
            assert!(report.keyframes.len() > 20, "{name}: {:?}", report.keyframes.len());
        }
        for seed in 1..=3 {
            fuzz_whole(&stream, &file, tolerance, 400, seed);
        }
        fuzz_ranged(&stream, &file, tolerance, 300, 5, 300_000, 40_000);
        fuzz_ranged(&stream, &file, tolerance, 300, 6, 1 << 30, 200_000);
    }

    // Recording that starts in the middle of the session, from the state
    // a host kept since the start: in the stage, during the scan, while
    // the scan is paused, right after the map was thrown away.
    let dt = 1.0 / 20.0;
    for start_tick in [12usize, 33, 77, 104, 155, 245, 300] {
        let start_time = start_tick as f64 * dt;
        let first = stream.partition_point(|(time, _)| *time < start_time);
        let mut state = StreamState::new();
        for (_, message) in &stream[..first] {
            state.apply(message).unwrap();
        }
        let mut recorder = Recorder::with_state(Vec::new(), RecorderOptions { keyframes: dense, ..options("late") }, state);
        for (time, message) in &stream[first..] {
            let pushed = recorder.push(*time, message).unwrap();
            assert!(matches!(pushed, Pushed::Recorded | Pushed::Keyframe), "{pushed:?}");
        }
        let (file, summary) = recorder.finish().unwrap();
        let report = verify(&file).unwrap();
        assert!(report.is_valid(), "from tick {start_tick}: {:?}", report.problems);
        let end = stream.last().unwrap().0;
        assert!((summary.duration_s - (end - start_time)).abs() < 1e-9, "{} from tick {start_tick}", summary.duration_s);
        // The scene the recording starts in.
        let scene = scene_of(Viewer::live(&stream, start_time).state.as_deref().unwrap());
        assert_eq!(report.info.scene, scene);

        // The same session with the recording's clock: its first instant
        // holds everything up to and including the first recorded tick.
        let rebased: Stream = stream.iter().map(|(time, m)| ((time - start_time).max(0.0), m.clone())).collect();
        for seed in 1..=2 {
            fuzz_whole(&rebased, &file, tolerance, 300, seed);
        }
        fuzz_ranged(&rebased, &file, tolerance, 200, 9, 500_000, 60_000);
    }
}

#[test]
fn a_recording_plays_while_it_is_being_written() {
    let stream = tiny_stream();
    let end = stream.last().unwrap().0;
    let mut recorder = Recorder::new(Vec::new(), tiny_options());
    // A viewer that follows the file as it grows, as a download would.
    let mut follower = Player::streaming();
    let mut follower_viewer = Viewer::default();
    follower.play();
    let mut fed = 0;
    let mut random = Lcg(11);
    for (i, (time, message)) in stream.iter().enumerate() {
        recorder.push(*time, message).unwrap();
        let written = recorder.sink().clone();
        assert_eq!(written.len() as u64, recorder.bytes_written());
        follower.push_bytes(&written[fed..]).unwrap();
        fed = written.len();
        let update = follower.advance(0.3);
        assert!(!update.ended, "the file is not finished: the follower waits at its end");
        apply_checked(&mut follower_viewer, &follower, &update, &stream, TINY_TOLERANCE, &format!("follower {i}"));
        assert!(update.time <= recorder.duration_s() + 1e-12);

        if written.is_empty() {
            continue;
        }
        // What is on disk right now, opened as a file: it is a recording
        // cut short and plays to the last message written.
        let report = verify(&written).unwrap();
        assert!(report.truncated && !report.complete && report.problems.is_empty(), "{report:?}");
        assert_eq!(report.duration_s, recorder.duration_s());
        assert_eq!(report.info.duration_s, 0.0, "the header does not know the length yet");
        // The recorder's first instant swallows the opening burst.
        let shown = Viewer::live_after(&stream, (i + 1).max(4));
        let mut player = Player::from_bytes(written.clone()).unwrap();
        assert_eq!(player.duration(), Some(recorder.duration_s()));
        let mut viewer = Viewer::default();
        player.seek(f64::MAX);
        viewer.apply_all(&player.update());
        viewer.assert_shows(&shown, TINY_TOLERANCE, &format!("file after {i} messages"));

        if random.next() < 0.3 {
            // And through Range requests against the length it has now,
            // also when the writer's buffer was flushed mid-record.
            let cut = written.len() - (random.next() * 40.0) as usize;
            let records = records_of(&written[..cut]);
            let Some(&(_, last_time, _)) = records.last() else { continue };
            let mut player = Player::ranged(cut as u64);
            player.set_options(PlayerOptions { fetch_block: 900, ..PlayerOptions::default() });
            let mut viewer = Viewer::default();
            player.seek(f64::MAX);
            let update = settle(&mut player, &written[..cut], &mut viewer, &mut Vec::new());
            assert_eq!(update.time, last_time);
            assert!(player.stats().index_missing && player.error().is_none() && player.damage().is_none());
            viewer.assert_shows(&Viewer::live_after(&stream, records.len() + 3), TINY_TOLERANCE, "ranged, growing file");
        }
    }
    let (file, _) = recorder.finish().unwrap();
    // The end of the file arrives; the header the follower read at the
    // start was rewritten since, which changes nothing for it.
    follower.push_bytes(&file[fed..]).unwrap();
    follower.finish_stream();
    assert_eq!(follower.duration(), Some(end));
    let mut ended = false;
    for _ in 0..100 {
        let update = follower.advance(0.3);
        apply_checked(&mut follower_viewer, &follower, &update, &stream, TINY_TOLERANCE, "follower, end");
        ended |= update.ended;
    }
    assert!(ended && follower.time() == end);
    assert!(verify(&file).unwrap().is_valid());
}

/// Random transport against a download in progress.
#[test]
fn random_transport_during_a_download() {
    let stream = tiny_stream();
    let file = record(&stream, tiny_options());
    let end = stream.last().unwrap().0;
    for seed in 1..=20u64 {
        let mut random = Lcg(seed * 977);
        let mut player = Player::streaming();
        let mut viewer = Viewer::default();
        let mut fed = 0;
        for step in 0..2500 {
            if fed < file.len() && random.next() < 0.4 {
                let piece = ((random.next() * 300.0) as usize + 1).min(file.len() - fed);
                player.push_bytes(&file[fed..fed + piece]).unwrap();
                fed += piece;
                if fed == file.len() {
                    player.finish_stream();
                }
            }
            let update = match random_transport(&mut player, &mut random, end) {
                Some(dt) => player.advance(dt),
                None => player.update(),
            };
            assert!(!update.buffering || fed < file.len(), "step {step}: nothing left to wait for");
            apply_checked(&mut viewer, &player, &update, &stream, TINY_TOLERANCE, &format!("download step {step} seed {seed}"));
        }
        assert_eq!(fed, file.len());
    }
}
