//! Second opinion on the recorder and the player: random transport against
//! all three sources, sessions that change under the recorder, recordings
//! read while they are written, long recordings, and the limits.

mod record_support;

use gs_core::codec::LayerMode;
use gs_core::pipeline::Pipeline;
use gs_core::protocol::{ControlMsg, PipelineConfig};
use gs_core::record::{
    classify, verify, ByteRange, ChunkedSink, Compression, KeyframePolicy, MessageClass, Player, PlayerOptions, Pushed,
    Recorder, RecorderOptions, StreamState, Update,
};
use gs_core::scene::SceneKind;
use record_support::*;

/// Multiplies the number of random steps: `GSREC_FUZZ=20 cargo test ...`
/// for a long soak, 1 by default.
fn effort() -> usize {
    std::env::var("GSREC_FUZZ").ok().and_then(|v| v.parse().ok()).unwrap_or(1)
}

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
            // And with every request answered, a playing head moves on
            // (or says that it reached the end), whatever the cache.
            player.play();
            let from = player.time();
            let mut moved = false;
            for _ in 0..40 {
                pump(&mut player, file, &mut requests);
                let update = player.advance(1.0 / 60.0);
                apply_checked(&mut viewer, &player, &update, stream, tolerance, &what(step));
                moved |= update.ended || update.time != from;
            }
            assert!(moved, "{}: stuck at {from} of {end} with every request answered", what(step));
        }
    }
    let fetched: u64 = requests.iter().map(ByteRange::len).sum();
    assert!(fetched > 0);
}

#[test]
fn random_transport_keeps_the_viewer_on_the_live_picture() {
    let stream = tiny_stream();
    let file = record(&stream, tiny_options());
    for seed in 1..=12 * effort() as u64 {
        fuzz_whole(&stream, &file, TINY_TOLERANCE, 1500, seed);
    }
    for seed in 1..=10 * effort() as u64 {
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
                    let (lo, hi) = layer
                        .splats
                        .iter()
                        .fold((f32::MAX, f32::MIN), |(lo, hi), s| (lo.min(s.pos[k]), hi.max(s.pos[k])));
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
    let states = stream.iter().filter(|(_, m)| classify(m) == Some(MessageClass::State)).count();
    assert!(states >= 5, "changes of scene and settings announce a new State: {states}");
    if std::env::var_os("GSREC_VERBOSE").is_some() {
        for tick in [30, 40, 100, 110, 150, 160, 240, 250, 290, 310] {
            let time = tick as f64 * (1.0 / 20.0f32) as f64;
            let classes: Vec<_> =
                stream.iter().filter(|(t, _)| *t == time).map(|(_, m)| (classify(m).unwrap(), m.len())).collect();
            eprintln!("tick {tick}: {classes:?}");
        }
    }
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
        // (Each step compares with a live viewer rebuilt from the start
        // of the session, which is what bounds the number of steps.)
        fuzz_whole(&stream, &file, tolerance, 150 * effort(), 1);
        fuzz_ranged(&stream, &file, tolerance, 110 * effort(), 5, 300_000, 40_000);
        fuzz_ranged(&stream, &file, tolerance, 110 * effort(), 6, 1 << 30, 200_000);
    }

    // Recording that starts in the middle of the session, from the state
    // a host kept since the start: in the stage, during the scan, while
    // the scan is paused, right after the map was thrown away.
    let dt = (1.0 / 20.0f32) as f64;
    for start_tick in [33usize, 104, 155, 245, 300] {
        let start_time = start_tick as f64 * dt;
        let first = stream.partition_point(|(time, _)| *time < start_time);
        let mut state = StreamState::new();
        for (_, message) in &stream[..first] {
            state.apply(message).unwrap();
        }
        let mut recorder =
            Recorder::with_state(Vec::new(), RecorderOptions { keyframes: dense, ..options("late") }, state);
        for (time, message) in &stream[first..] {
            let pushed = recorder.push(*time, message).unwrap();
            assert!(matches!(pushed, Pushed::Recorded | Pushed::Keyframe), "{pushed:?}");
        }
        let (file, summary) = recorder.finish().unwrap();
        let report = verify(&file).unwrap();
        assert!(report.is_valid(), "from tick {start_tick}: {:?}", report.problems);
        let end = stream.last().unwrap().0;
        assert!(
            (summary.duration_s - (end - start_time)).abs() < 1e-9,
            "{} from tick {start_tick}",
            summary.duration_s
        );
        // The scene the recording starts in.
        let scene = scene_of(Viewer::live(&stream, start_time).state.as_deref().unwrap());
        assert_eq!(report.info.scene, scene);

        // The same session with the recording's clock: its first instant
        // holds everything up to and including the first recorded tick.
        let rebased: Stream = stream.iter().map(|(time, m)| ((time - start_time).max(0.0), m.clone())).collect();
        fuzz_whole(&rebased, &file, tolerance, 60 * effort(), 1);
        fuzz_ranged(&rebased, &file, tolerance, 75 * effort(), 9, 500_000, 60_000);
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
            let messages = records.iter().filter(|(_, _, keyframe)| !keyframe).count();
            viewer.assert_shows(&Viewer::live_after(&stream, messages + 4), TINY_TOLERANCE, "ranged, growing file");
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
    for seed in 1..=20 * effort() as u64 {
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
            apply_checked(
                &mut viewer,
                &player,
                &update,
                &stream,
                TINY_TOLERANCE,
                &format!("download step {step} seed {seed}"),
            );
        }
        assert_eq!(fed, file.len());
    }
}

/// A long session of small messages: `seconds` at 4 ticks a second.
fn long_stream(seconds: usize) -> Stream {
    let mut stream: Stream = vec![
        (0.0, text(0, true, r#"{"scene":"long"}"#.into())),
        (0.0, layer(1, 0, LayerMode::Replace, &splats(30, 1))),
        (0.0, layer(2, 1, LayerMode::Replace, &splats(6, 2))),
        (0.0, text(3, false, r#"{"tick":0}"#.into())),
    ];
    for tick in 1..=(seconds * 4) as u32 {
        let time = tick as f64 * 0.25;
        let seq = stream.len() as u32;
        stream.push((time, layer(seq, 1, LayerMode::Replace, &splats(4 + (tick % 5) as usize, tick))));
        if tick % 40 == 0 {
            stream.push((time, preview(seq + 1, (tick % 3) as u8)));
        }
        stream.push((time, text(seq + 2, false, format!(r#"{{"tick":{tick}}}"#))));
    }
    stream
}

/// The live viewer of [`long_stream`] at `t`, without replaying the whole
/// session: layer 1 and the statistics are the latest, the thumbnails the
/// latest of each camera.
fn long_live(stream: &Stream, t: f64) -> Viewer {
    let upto = stream.partition_point(|(time, _)| *time <= t);
    let mut viewer = Viewer::default();
    for (_, message) in &stream[..4] {
        viewer.apply(message);
    }
    let mut cameras = Vec::new();
    let mut live = false;
    let mut statistics = false;
    let mut late = Vec::new();
    for (_, message) in stream[4..upto].iter().rev() {
        let wanted = match classify(message).unwrap() {
            MessageClass::Layer { layer: 1, replace: true } => !std::mem::replace(&mut live, true),
            MessageClass::Stats => !std::mem::replace(&mut statistics, true),
            MessageClass::Preview(camera) => {
                let new = !cameras.contains(&camera);
                cameras.push(camera);
                new
            }
            other => panic!("{other:?} in a long stream"),
        };
        if wanted {
            late.push(message);
        }
        if live && statistics && cameras.len() == 3 {
            break;
        }
    }
    for message in late.into_iter().rev() {
        viewer.apply(message);
    }
    viewer
}

#[test]
fn a_recording_of_hours_opens_and_seeks_in_a_few_requests() {
    let seconds = 5400;
    let stream = long_stream(seconds);
    let end = stream.last().unwrap().0;
    let policy = KeyframePolicy { min_interval_s: 1.0, ratio: 0.5 };
    let started = std::time::Instant::now();
    let file = record(&stream, RecorderOptions { keyframes: policy, ..options("long") });
    let recording = started.elapsed();
    let report = verify(&file).unwrap();
    assert!(report.is_valid(), "{:?}", report.problems);
    // More keyframes than the first request for the end of the file holds
    // index entries for (64 KiB / 16 bytes).
    let keyframes = report.keyframes.len();
    assert!(keyframes > 4500, "{keyframes}");
    let index_bytes = keyframes * 16 + 16;
    assert!(index_bytes > 64 << 10);

    // From memory.
    let started = std::time::Instant::now();
    let mut player = Player::from_bytes(file.clone()).unwrap();
    let opening = started.elapsed();
    assert_eq!(player.duration(), Some(end));
    assert_eq!(player.keyframe_times().count(), keyframes);
    assert_eq!(player.stats().records, report.records - 1, "every record but the index");
    let mut viewer = Viewer::default();
    let mut random = Lcg(4);
    let started = std::time::Instant::now();
    let mut handed = 0;
    for i in 0..200 {
        let t = random.next() * end;
        player.seek(t);
        let update = player.update();
        assert!(update.reset || i > 0);
        handed += update.messages.len();
        viewer.apply_all(&update);
        if i % 8 == 0 {
            viewer.assert_shows(&long_live(&stream, t), TINY_TOLERANCE, &format!("long seek to {t:.2}"));
        }
    }
    let seeking = started.elapsed() / 200;
    // A seek hands over a keyframe and what little follows it, not the
    // session from its start.
    assert!(handed / 200 <= 12, "{} messages per seek", handed / 200);

    // Through Range requests, with requests as small as the records.
    let mut player = Player::ranged(file.len() as u64);
    player.set_options(PlayerOptions { fetch_block: 4096, max_cache_bytes: 64 << 10, ..PlayerOptions::default() });
    let mut viewer = Viewer::default();
    let mut requests = Vec::new();
    player.seek(end * 0.75);
    let update = settle(&mut player, &file, &mut viewer, &mut requests);
    assert_eq!(update.time, end * 0.75);
    viewer.assert_shows(&long_live(&stream, end * 0.75), TINY_TOLERANCE, "ranged, three quarters in");
    assert!(!player.stats().index_missing);
    assert_eq!(player.duration(), Some(end));
    assert_eq!(player.keyframe_times().count(), keyframes, "the whole index was read");
    // Header, end of file, the rest of the index, then the keyframe.
    assert!(requests.len() <= 5, "{requests:?}");
    assert_eq!(requests[1].len(), 64 << 10);
    assert_eq!(requests[2].end, file.len() as u64);
    let opening_bytes: u64 = requests.iter().map(ByteRange::len).sum();
    assert!(opening_bytes < index_bytes as u64 + (64 << 10) + 3 * 4096, "{opening_bytes}");

    let mut worst = 0;
    for i in 0..120 {
        let t = random.next() * end;
        let before = requests.len();
        player.seek(t);
        let update = settle(&mut player, &file, &mut viewer, &mut requests);
        assert_eq!(update.time, t);
        worst = worst.max(requests.len() - before);
        if i % 8 == 0 {
            viewer.assert_shows(&long_live(&stream, t), TINY_TOLERANCE, &format!("long ranged seek to {t:.2}"));
        }
        let stats = player.stats();
        assert!(stats.cached_bytes <= (64 << 10) + 8192, "{stats:?}");
        assert!(stats.records < 2000, "only the records of what is cached are known: {stats:?}");
    }
    assert!(worst <= 2, "{worst} requests for one seek");

    eprintln!(
        "long recording: {seconds} s, {} records, {keyframes} keyframes, {:.1} MB (index {:.0} KB); written in {recording:.2?}, \
         opened from memory in {opening:.2?} ({:.1} MB of record positions), {seeking:.1?} per seek; \
         over ranges {opening_bytes} bytes to the first picture, at most {worst} requests per seek",
        report.records,
        file.len() as f64 / 1e6,
        index_bytes as f64 / 1e3,
        report.records as f64 * 24.0 / 1e6,
    );
}

#[test]
fn an_index_that_understates_the_duration_does_not_hide_the_rest() {
    let stream = tiny_stream();
    let policy = KeyframePolicy { min_interval_s: 1.0, ratio: 2.5 };
    let file = record(&stream, RecorderOptions { keyframes: policy, ..options("tiny") });
    let end = stream.last().unwrap().0;
    let index_at = u64::from_le_bytes(file[file.len() - 16..file.len() - 8].try_into().unwrap()) as usize;
    let mut lying = file.clone();
    // Not so low that the index contradicts itself: the time of its own
    // last keyframe.
    let short = verify(&file).unwrap().keyframes.last().unwrap().0;
    assert!(short > 0.0 && short < end - 0.2, "{short} of {end}");
    lying[index_at + 8..index_at + 16].copy_from_slice(&short.to_le_bytes());
    // The checker says so.
    assert!(!verify(&lying).unwrap().is_valid());

    // A player that reads the file plays all of it.
    let player = Player::from_bytes(lying.clone()).unwrap();
    assert_eq!(player.duration(), Some(end));

    // A Range player believes the index until it has seen better; it
    // must not sit at the false end for ever without saying "ended".
    let mut player = Player::ranged(lying.len() as u64);
    player.set_options(PlayerOptions { fetch_block: 800, ..PlayerOptions::default() });
    let mut viewer = Viewer::default();
    player.play();
    let mut ended = false;
    for _ in 0..3000 {
        pump(&mut player, &lying, &mut Vec::new());
        let update = player.advance(1.0 / 30.0);
        viewer.apply_all(&update);
        if !update.buffering {
            viewer.assert_shows(&Viewer::live(&stream, update.time), TINY_TOLERANCE, "lying duration");
        }
        if update.ended {
            ended = true;
            break;
        }
    }
    assert!(ended, "the player never reports the end (head at {} of {end})", player.time());
    assert_eq!(player.time(), end, "everything that was recorded plays");
    assert_eq!(player.duration(), Some(end));
}

/// A layer message of `count` splats that are all zero bytes: valid, and
/// it deflates to almost nothing.
fn blank_layer(count: u32) -> Vec<u8> {
    let mut message = layer(1, 0, LayerMode::Replace, &[]);
    message[16 + 4..16 + 8].copy_from_slice(&count.to_le_bytes());
    message.resize(48 + count as usize * 16, 0);
    assert_eq!(classify(&message), Some(MessageClass::Layer { layer: 0, replace: true }));
    message
}

/// Needs about 1 GB of memory for a moment, hence not part of the default
/// run: `cargo test -p gs-core --test record_verify -- --ignored`.
#[test]
#[ignore = "allocates about 1 GB"]
fn a_snapshot_beyond_what_readers_inflate_is_refused_not_written() {
    // 129 MiB of layer 0: more than a reader inflates (`MAX_INFLATED`),
    // although deflate shrinks it to a few hundred kilobytes.
    let big = blank_layer((129 << 20) / 16);
    let state = text(0, true, r#"{"scene":"big"}"#.into());
    let stats = text(2, false, "{}".into());

    let mut recorder = Recorder::new(Vec::new(), options("big"));
    recorder.push(0.0, &state).unwrap();
    recorder.push(0.0, &big).unwrap();
    let pushed = recorder.push(0.0, &stats);
    let file = recorder.sink().clone();
    if !file.is_empty() {
        // Whatever was written must be readable.
        let report = verify(&file).unwrap();
        assert!(report.problems.is_empty(), "the recorder wrote a file that does not read: {:?}", report.problems);
    }
    assert!(
        matches!(pushed, Err(gs_core::record::WriteError::Record(gs_core::record::RecordError::TooLarge))),
        "{pushed:?}"
    );
    assert_eq!(recorder.bytes_written(), 0);
    let (file, summary) = recorder.finish().unwrap();
    assert!(file.is_empty() && summary.bytes == 0);

    // Just below the limit it is recorded, and it plays.
    let fits = blank_layer((127 << 20) / 16);
    let mut recorder = Recorder::new(Vec::new(), options("big"));
    recorder.push(0.0, &state).unwrap();
    recorder.push(0.0, &fits).unwrap();
    assert_eq!(recorder.push(0.0, &stats).unwrap(), Pushed::Keyframe);
    // A message record of that size, too.
    assert_eq!(recorder.push(1.0, &fits).unwrap(), Pushed::Recorded);
    let (file, _) = recorder.finish().unwrap();
    assert!(file.len() < 4 << 20, "{}", file.len());
    let report = verify(&file).unwrap();
    assert!(report.is_valid(), "{:?}", report.problems);
    let mut player = Player::from_bytes(file).unwrap();
    player.seek(1.0);
    let update = player.update();
    assert_eq!(update.messages.len(), 4, "State, layer 1, Stats and the later of the two layer 0 messages");
    assert_eq!(update.messages.iter().map(Vec::len).max(), Some(fits.len()));
    assert_eq!(player.stats().dropped, 0);
}

#[test]
fn a_clock_that_is_not_a_number_at_first_still_gives_a_time_line() {
    let stream = tiny_stream();
    let end = stream.last().unwrap().0;
    let mut recorder = Recorder::new(Vec::new(), tiny_options());
    for (i, (time, message)) in stream.iter().enumerate() {
        // The opening burst and most of the first tick without a clock.
        let clock = if i < 6 { f64::NAN } else { 100.0 + time };
        recorder.push(clock, message).unwrap();
    }
    let (file, summary) = recorder.finish().unwrap();
    let report = verify(&file).unwrap();
    assert!(report.is_valid(), "{:?}", report.problems);
    // Time starts with the first message that has one (the end of tick 1).
    assert!((summary.duration_s - (end - 0.25)).abs() < 1e-9, "{}", summary.duration_s);
    let mut player = Player::from_bytes(file).unwrap();
    let mut viewer = Viewer::default();
    for t in [0.0, 3.3, 1.1, end - 0.25] {
        player.seek(t);
        viewer.apply_all(&player.update());
        viewer.assert_shows(&Viewer::live(&stream, t + 0.25), TINY_TOLERANCE, &format!("late clock, {t}"));
    }
    // Never a clock at all: a still recording of the last state.
    let mut recorder = Recorder::new(Vec::new(), tiny_options());
    for (_, message) in &stream {
        recorder.push(f64::NAN, message).unwrap();
    }
    let (file, summary) = recorder.finish().unwrap();
    assert_eq!(summary.duration_s, 0.0);
    assert!(verify(&file).unwrap().is_valid());
    let mut player = Player::from_bytes(file).unwrap();
    let mut viewer = Viewer::default();
    viewer.apply_all(&player.update());
    viewer.assert_shows(&Viewer::live(&stream, end), TINY_TOLERANCE, "no clock");
}

/// Random damage to the index and the trailer only: the records are whole,
/// so a `Range` player must still show the right picture wherever it can
/// get to, and say when it reached the end.
#[test]
fn random_damage_to_the_index_never_changes_the_picture() {
    let stream = tiny_stream();
    let file = record(&stream, tiny_options());
    let end = stream.last().unwrap().0;
    let index_at = u64::from_le_bytes(file[file.len() - 16..file.len() - 8].try_into().unwrap()) as usize;
    let mut random = Lcg(42);
    let (mut detours, mut short) = (0, 0);
    for round in 0..1500 * effort() {
        let mut bytes = file.clone();
        for _ in 0..1 + round % 3 {
            let at = index_at + (random.next() * (bytes.len() - index_at) as f64) as usize;
            bytes[at] = if round % 2 == 0 { (random.next() * 256.0) as u8 } else { bytes[at] ^ (1 << (round % 8)) };
        }
        let mut player = Player::ranged(bytes.len() as u64);
        player.set_options(PlayerOptions {
            fetch_block: 300 + (random.next() * 2000.0) as u64,
            max_cache_bytes: 5000,
            prefetch_s: 0.5,
            jump_s: 2.0,
        });
        let mut viewer = Viewer::default();
        let mut requests = Vec::new();
        for _ in 0..4 {
            player.seek(random.next() * end);
            let update = settle(&mut player, &bytes, &mut viewer, &mut requests);
            let changed =
                || (0..bytes.len()).filter(|&i| bytes[i] != file[i]).map(|i| (i, bytes[i])).collect::<Vec<_>>();
            let what = format!(
                "index damage {round}: bytes {:?} (index at {index_at}), head {}, keyframes {:?}",
                changed(),
                update.time,
                player.keyframe_times().collect::<Vec<_>>()
            );
            viewer.assert_shows(&Viewer::live(&stream, update.time), TINY_TOLERANCE, &what);
        }
        detours += usize::from(player.stats().index_missing);
        // Play to the end from wherever that left the head.
        player.play();
        let mut ended = false;
        for _ in 0..2000 {
            pump(&mut player, &bytes, &mut requests);
            let update = player.advance(1.0 / 20.0);
            viewer.apply_all(&update);
            if !update.buffering {
                viewer.assert_shows(&Viewer::live(&stream, update.time), TINY_TOLERANCE, "index damage, playing");
            }
            if update.ended {
                ended = true;
                break;
            }
        }
        let duration = player.duration().unwrap();
        // A duration the index overstates is believed (trailing silence is
        // legal); one it understates is corrected by the records.
        if duration <= end {
            assert!(
                ended,
                "round {round}: no end at {} of {duration}; wanted {:?}, buffered until {}, damage {:?}, {:?}, \
                 changed bytes at {:?} (index at {index_at}, {} bytes)",
                player.time(),
                player.wanted(),
                player.buffered_until(),
                player.damage(),
                player.stats(),
                (0..bytes.len()).filter(|&i| bytes[i] != file[i]).map(|i| (i, bytes[i], file[i])).collect::<Vec<_>>(),
                bytes.len(),
            );
            // The whole recording was shown, unless the damage made the
            // index record itself unreadable as a record: then the player
            // cannot tell it from damage to the last records.
            short += usize::from(player.time() < end);
            assert!(player.time() == end || player.damage().is_some(), "round {round}: stopped at {}", player.time());
        }
    }
    assert!(detours > 100 * effort(), "{detours}");
    assert!(short < 400 * effort(), "{short} of the damaged files stopped short");
}

/// Plays `file` through Range requests from start to end, answering every
/// request between two frames, and returns the number of frames that had
/// to wait. Panics when the end is not reached.
fn play_through(stream: &Stream, file: &[u8], options: PlayerOptions, speed: f64, tolerance: f32) -> usize {
    let end = stream.last().unwrap().0;
    let mut player = Player::ranged(file.len() as u64);
    player.set_options(options);
    player.set_speed(speed);
    let mut viewer = Viewer::default();
    let mut requests = Vec::new();
    player.play();
    let mut waits = 0;
    let frames = (end / speed * 60.0) as usize + 1;
    for frame in 0..frames * 4 + 200 {
        pump(&mut player, file, &mut requests);
        let update = player.advance(1.0 / 60.0);
        viewer.apply_all(&update);
        waits += usize::from(update.buffering);
        if !update.buffering {
            viewer.assert_shows(&Viewer::live(stream, update.time), tolerance, &format!("frame {frame}"));
        }
        if update.ended {
            assert_eq!(update.time, end);
            return waits;
        }
    }
    panic!(
        "{options:?} at speed {speed}: stuck at {} of {end} after {waits} waiting frames; wanted {:?}, {:?}",
        player.time(),
        player.wanted(),
        player.stats()
    );
}

/// A cache smaller than the stretch between two keyframes leaves no room
/// to read ahead. Playback may then wait at block boundaries, but it must
/// get through: the player has to ask for what the play head needs next,
/// not for what it needed where it waits.
#[test]
fn playback_gets_through_whatever_the_cache_and_the_block_size() {
    let stream = tiny_stream();
    for (name, file) in [
        ("keyframes every second", record(&stream, tiny_options())),
        ("one keyframe", record(&stream, RecorderOptions { compression: Compression::None, ..options("tiny") })),
    ] {
        for cache in [0u64, 1000, 3000, 5000, 6000, 20_000, 1 << 30] {
            for block in [1u64, 150, 300, 700, 1024, 2500, 1 << 20] {
                for speed in [0.5, 1.0, 3.0] {
                    let options =
                        PlayerOptions { fetch_block: block, max_cache_bytes: cache, prefetch_s: 0.5, jump_s: 2.0 };
                    let waits = play_through(&stream, &file, options, speed, TINY_TOLERANCE);
                    if cache >= 20_000 && block >= 700 {
                        // Room to read ahead: only the start waits.
                        assert!(waits <= 3, "{name}, cache {cache}, block {block}, speed {speed}: {waits} waits");
                    }
                }
            }
        }
    }
}

#[test]
fn a_chunked_sink_holds_the_same_recording_without_ever_moving_it() {
    let stream = tiny_stream();
    let whole = record(&stream, tiny_options());
    for chunk in [1usize, 100, 1000, 1024, 4096, 1 << 20] {
        let mut recorder = Recorder::new(ChunkedSink::with_chunk_size(chunk), tiny_options().with_max_bytes(1 << 20));
        for (time, message) in &stream {
            recorder.push(*time, message).unwrap();
        }
        let (sink, summary) = recorder.finish().unwrap();
        assert_eq!(sink.len(), summary.bytes);
        // The header, rewritten at the end, lies across several pieces.
        assert_eq!(sink.to_vec(), whole, "pieces of {chunk}");
        let chunks = sink.into_chunks();
        assert_eq!(chunks.len(), whole.len().div_ceil(chunk));
        assert!(chunks.iter().all(|piece| piece.capacity() == chunk));
    }
    // What a `Vec<u8>` sink costs instead: up to twice the recording once
    // it is done, and old and new block side by side whenever it grows.
    let mut recorder = Recorder::new(Vec::new(), tiny_options());
    let mut peak = 0;
    for (time, message) in &stream {
        recorder.push(*time, message).unwrap();
        peak = peak.max(recorder.sink().capacity());
    }
    assert!(peak >= whole.len() && peak <= 2 * whole.len(), "{peak} for {}", whole.len());
}

/// `file` with another index: the records as they are, then an index
/// record with `entries` and the trailer.
fn with_index(file: &[u8], entries: &[(f64, u64)], duration: f64) -> Vec<u8> {
    let index_at = u64::from_le_bytes(file[file.len() - 16..file.len() - 8].try_into().unwrap()) as usize;
    let mut out = file[..index_at].to_vec();
    out.extend_from_slice(&(entries.len() as u32 * 16 + 12).to_le_bytes());
    out.extend_from_slice(&[2, 0, 0, 0]);
    out.extend_from_slice(&duration.to_le_bytes());
    for (time, offset) in entries {
        out.extend_from_slice(&time.to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
    }
    out.extend_from_slice(&(index_at as u64).to_le_bytes());
    out.extend_from_slice(b"GSRECEND");
    out
}

/// An index entry that points into the middle of a record, a network that
/// answers with less than was asked for, and a cache that is always full:
/// the player must find the lie, not trip over it while dropping bytes.
#[test]
fn a_false_index_entry_at_the_edge_of_what_is_cached_is_found_out() {
    let stream = tiny_stream();
    let file = record(&stream, tiny_options());
    let report = verify(&file).unwrap();
    let mut entries = report.keyframes.clone();
    let records = records_of(&file);
    // A place 20 bytes into the first record that starts after 2.4 s.
    let at = records.iter().position(|(_, time, _)| *time > 2.4).unwrap();
    let lie = records[at - 1].0 + 20;
    assert!(records[at].0 > lie + 40, "the record is longer than that");
    let place = entries.partition_point(|(_, offset)| *offset < lie);
    entries.insert(place, (2.5, lie));
    let lying = with_index(&file, &entries, report.duration_s);
    assert!(!verify(&lying).unwrap().is_valid());

    for cache in [0u64, 2000, 1 << 30] {
        let mut player = Player::ranged(lying.len() as u64);
        player.set_options(PlayerOptions { fetch_block: 64, max_cache_bytes: cache, prefetch_s: 0.0, jump_s: 2.0 });
        let mut viewer = Viewer::default();
        // Answers that stop at the false offset, wherever a request crosses it.
        let short = |player: &mut Player, viewer: &mut Viewer, rounds: usize| {
            let mut update = player.update();
            for _ in 0..rounds {
                let Some(range) = player.wanted() else { break };
                let end = if range.start == 0 || range.end == lying.len() as u64 {
                    // The header and the index, whole.
                    range.end
                } else if range.start < lie && range.end > lie {
                    lie
                } else {
                    range.end.min(range.start + 8)
                };
                player.supply(range.start, &lying[range.start as usize..end as usize]).unwrap();
                update = player.update();
                viewer.apply_all(&update);
            }
            update
        };
        player.seek(2.2);
        let update = short(&mut player, &mut viewer, 100_000);
        assert!(!update.buffering && update.time == 2.2);
        viewer.assert_shows(&Viewer::live(&stream, 2.2), TINY_TOLERANCE, "before the false entry");
        // Read on until the cached bytes end right at the false offset.
        player.seek(2.45);
        let mut guard = 0;
        while player.wanted().is_some_and(|range| range.start != lie) {
            short(&mut player, &mut viewer, 1);
            guard += 1;
            assert!(guard < 10_000);
        }
        // Something else used the viewer; then a seek that the index
        // sends to the false entry, with the network still trickling.
        player.invalidate_viewer();
        player.seek(2.6);
        let update = short(&mut player, &mut viewer, 100_000);
        assert!(!update.buffering && update.time == 2.6, "{update:?}");
        viewer.assert_shows(&Viewer::live(&stream, 2.6), TINY_TOLERANCE, "past the false entry");
        assert!(player.stats().index_missing);
        assert!(player.keyframe_times().all(|time| time != 2.5), "the false entry is gone");
    }
}

/// Damaged files through a `Range` player on a bad day: short answers,
/// any cache, any block size, the viewer taken away in between. Nothing
/// may panic, and whatever reaches the viewer decodes.
#[test]
fn damaged_files_over_a_bad_network_never_panic() {
    let stream = tiny_stream();
    let files = [
        record(&stream, tiny_options()),
        record(&stream, RecorderOptions { compression: Compression::None, ..tiny_options() }),
    ];
    let mut random = Lcg(2026);
    let mut shown = 0;
    let rounds = 4000 * effort();
    for round in 0..rounds {
        let mut bytes = files[round % 2].clone();
        let index_at = u64::from_le_bytes(bytes[bytes.len() - 16..bytes.len() - 8].try_into().unwrap()) as usize;
        for _ in 0..1 + round % 4 {
            // Half of the damage goes to the index, where a player that
            // fetches by offset is most easily led astray.
            let lo = if round % 2 == 0 { index_at } else { 1024 };
            let at = lo + (random.next() * (bytes.len() - lo) as f64) as usize;
            bytes[at] = if round % 3 == 0 { (random.next() * 256.0) as u8 } else { bytes[at] ^ (1 << (round % 8)) };
        }
        if round % 11 == 0 {
            bytes.truncate(1024 + (random.next() * (bytes.len() - 1024) as f64) as usize);
        }
        let mut player = Player::ranged(bytes.len() as u64);
        player.set_options(PlayerOptions {
            fetch_block: 1 + (random.next() * random.next() * 3000.0) as u64,
            max_cache_bytes: [0, 500, 3000, 8000, 1 << 30][(random.next() * 5.0) as usize],
            prefetch_s: random.next(),
            jump_s: random.next() * 3.0,
        });
        let mut viewer = Viewer::default();
        let mut answers = 0;
        for _ in 0..60 {
            match (random.next() * 8.0) as u32 {
                0 => player.invalidate_viewer(),
                1 => player.play(),
                2 => player.pause(),
                3 => player.set_looping(random.next() < 0.5),
                _ => player.seek(random.next() * 7.0),
            }
            for _ in 0..(random.next() * 12.0) as usize {
                let Some(range) = player.wanted() else { break };
                assert!(range.start < range.end, "{range:?}");
                let start = (range.start as usize).min(bytes.len());
                let mut end = (range.end as usize).min(bytes.len());
                if random.next() < 0.4 {
                    // A short answer (never an empty one).
                    end = start + 1 + (random.next() * (end - start) as f64) as usize;
                    end = end.min(bytes.len());
                }
                answers += 1;
                if player.supply(range.start, &bytes[start..end]).is_err() {
                    break;
                }
                viewer.apply_all(&player.update());
            }
            viewer.apply_all(&player.advance(random.next() * 0.3));
        }
        assert!(answers > 0);
        shown += usize::from(viewer.state.is_some());
    }
    assert!(shown > rounds / 2, "most damaged files still show something: {shown} of {rounds}");
}

/// A file built to inflate: six records of a hundred kilobytes that are
/// six `State` messages of a hundred megabytes. One update hands out no
/// more than `Player::MAX_UPDATE_BYTES`.
#[test]
#[ignore = "allocates about 1 GB"]
fn a_file_built_to_inflate_does_not_fill_the_memory() {
    let good = record(&tiny_stream(), tiny_options());
    let records = records_of(&good);
    // The header and the first keyframe of a real recording.
    let mut file = good[..records[0].0 as usize].to_vec();
    for i in 0..6 {
        // (Each one different: a State the viewer holds is not sent again.)
        let mut state = text(i, true, String::new());
        state.resize(100 << 20, b' ');
        let packed = miniz_oxide::deflate::compress_to_vec(&state, 1);
        assert!(packed.len() < 1 << 20);
        file.extend_from_slice(&(packed.len() as u32 + 12).to_le_bytes());
        file.extend_from_slice(&[0, 1, 0, 0]);
        file.extend_from_slice(&(i as f64).to_le_bytes());
        file.extend_from_slice(&packed);
    }
    assert!(file.len() < 4 << 20);
    // Every record is a valid message: the checker has nothing to say
    // but that the file was cut short.
    let report = verify(&file).unwrap();
    assert!(report.problems.is_empty() && report.states == 6, "{:?}", report.problems);

    let mut player = Player::from_bytes(file).unwrap();
    player.seek(f64::MAX);
    let update = player.update();
    let handed: usize = update.messages.iter().map(Vec::len).sum();
    assert!(handed <= Player::MAX_UPDATE_BYTES, "{handed}");
    assert!(handed > 400 << 20, "what fits is handed out: {handed}");
    assert!(player.stats().dropped >= 1);
}
