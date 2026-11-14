//! Recording and replay of real pipeline streams: what a player hands to a
//! viewer at time `t` must leave it with exactly what a viewer of the live
//! session held at time `t`.

mod record_support;

use std::collections::BTreeMap;

use gs_core::pipeline::Pipeline;
use gs_core::protocol::{ControlMsg, PipelineConfig, ScanState};
use gs_core::record::{
    classify, read_info, verify, ByteRange, Compression, IoSink, KeyframePolicy, MessageClass, Player, PlayerOptions,
    Pushed, RecordError, RecordSink, Recorder, RecorderOptions, StreamState, WriteError, HEADER_JSON_SIZE,
};
use gs_core::scene::SceneKind;
use record_support::*;

/// The stream of a pipeline: the snapshot a viewer gets on connecting,
/// then `ticks` ticks.
fn pipeline_stream(
    config: PipelineConfig,
    ticks: usize,
    mut each: impl FnMut(usize, &mut Pipeline) -> Vec<Vec<u8>>,
) -> Stream {
    let mut pipeline = Pipeline::new(config);
    let dt = 1.0 / config.target_hz;
    let mut stream: Stream = pipeline.snapshot().into_iter().map(|o| (0.0, o.bytes)).collect();
    for tick in 1..=ticks {
        let time = tick as f64 * dt as f64;
        stream.extend(each(tick, &mut pipeline).into_iter().map(|bytes| (time, bytes)));
        stream.extend(pipeline.tick(dt).into_iter().map(|o| (time, o.bytes)));
    }
    stream
}

/// Plays `file` forwards, backwards and across seeks, and compares with
/// the live viewer at every stop.
fn check_replay(stream: &Stream, file: &[u8], tolerance: f32, seeks: usize) {
    let report = verify(file).unwrap();
    assert!(report.is_valid(), "{:?}", report.problems);
    let end = stream.last().unwrap().0;
    assert!((report.duration_s - end).abs() < 1e-9);
    assert!((report.info.duration_s - report.duration_s).abs() < 1e-9, "the header was patched");
    assert_eq!(report.messages() as usize + 4, stream.len(), "all but the snapshot burst are message records");

    let mut player = Player::from_bytes(file.to_vec()).unwrap();
    assert_eq!(player.duration(), Some(report.duration_s));
    let mut viewer = Viewer::default();

    // Forward, frame by frame, a little faster than real time.
    player.set_speed(1.5);
    player.play();
    let mut frames = 0;
    loop {
        let update = player.advance(1.0 / 60.0);
        assert!(!update.buffering);
        assert_eq!(update.reset, frames == 0, "forward play never goes through a keyframe again");
        viewer.apply_all(&update);
        frames += 1;
        if frames % 17 == 0 || update.ended {
            viewer.assert_shows(
                &Viewer::live(stream, update.time),
                tolerance,
                &format!("forward at {:.3}", update.time),
            );
        }
        if update.ended {
            assert_eq!(update.time, report.duration_s);
            break;
        }
        assert!(frames < 100_000);
    }
    assert!(!player.is_playing());
    let forward_static = viewer.static_messages;

    // Scrubbing while paused: anywhere, in any order.
    let mut random = Lcg(7);
    for i in 0..seeks {
        let t = match i {
            0 => 0.0,
            1 => end,
            2 => end * 0.5,
            _ => random.next() * end,
        };
        player.seek(t);
        let update = player.update();
        assert!(!update.buffering && !update.ended);
        assert_eq!(update.time, t);
        viewer.apply_all(&update);
        viewer.assert_shows(&Viewer::live(stream, t), tolerance, &format!("seek {i} to {t:.3}"));
    }

    // Slow motion from the middle, then a loop around the end.
    player.seek(end * 0.5);
    player.set_speed(0.25);
    player.play();
    for _ in 0..40 {
        let update = player.advance(1.0 / 30.0);
        viewer.apply_all(&update);
    }
    assert!((player.time() - (end * 0.5 + 40.0 / 30.0 * 0.25)).abs() < 1e-9 || player.time() == end);
    viewer.assert_shows(&Viewer::live(stream, player.time()), tolerance, "slow motion");
    player.set_looping(true);
    player.set_speed(4.0);
    player.seek((end - 0.2).max(0.0));
    let mut wrapped = false;
    for _ in 0..20 {
        let before = player.time();
        let update = player.advance(1.0 / 30.0);
        assert!(!update.ended);
        viewer.apply_all(&update);
        if update.time < before {
            wrapped = true;
            assert!(update.reset);
        }
        viewer.assert_shows(&Viewer::live(stream, update.time), tolerance, &format!("loop at {:.3}", update.time));
    }
    assert!(wrapped || end == 0.0);
    assert!(viewer.static_messages >= forward_static);
}

#[test]
fn stage_replay_matches_the_live_viewer_bit_for_bit() {
    let config = PipelineConfig::light(SceneKind::Stage);
    // Five seconds; halfway through, a setting changes and the pipeline
    // sends a new State and new layers.
    let stream = pipeline_stream(config, 100, |tick, pipeline| {
        if tick == 55 {
            let patch = serde_json::json!({ "shadows": false });
            return pipeline.handle_control(ControlMsg::PatchConfig { patch }).into_iter().map(|o| o.bytes).collect();
        }
        Vec::new()
    });
    let file = record(&stream, options("stage"));
    let report = verify(&file).unwrap();
    assert_eq!(report.info.scene, "stage");
    assert_eq!(report.info.name, "stage");
    assert!(
        report.states == 1 && report.previews >= 100 && report.stats >= 100,
        "{} {} {}",
        report.states,
        report.previews,
        report.stats
    );
    // About one keyframe every 1.2 s at the default rig; the light rig has
    // a smaller live layer, so its keyframes are due later.
    assert!((2..=6).contains(&report.keyframes.len()), "{:?}", report.keyframes);
    assert!(
        report.stored_bytes < report.raw_bytes * 9 / 10,
        "deflate pays: {} of {}",
        report.stored_bytes,
        report.raw_bytes
    );
    check_replay(&stream, &file, 0.0, 60);

    // A seek inside a stage recording does not send the static layer nor
    // the State again: the keyframes hold the very bytes the viewer has.
    let mut player = Player::from_bytes(file.clone()).unwrap();
    let mut viewer = Viewer::default();
    player.seek(1.0);
    viewer.apply_all(&player.update());
    assert_eq!((viewer.state_messages, viewer.static_messages), (1, 1));
    for (from, t) in [(1.0, 2.0), (2.0, 0.3), (0.3, 1.7), (1.7, 0.0)] {
        player.seek(t);
        let update = player.update();
        assert_eq!(update.reset, t < from, "backwards goes through a keyframe");
        viewer.apply_all(&update);
        viewer.assert_shows(&Viewer::live(&stream, t), 0.0, "cheap seek");
    }
    assert_eq!((viewer.state_messages, viewer.static_messages), (1, 1));
    // Across the change of settings both are sent, once.
    player.seek(4.0);
    viewer.apply_all(&player.update());
    assert_eq!((viewer.state_messages, viewer.static_messages), (2, 2));
    // Unless the viewer was used for something else in between.
    player.invalidate_viewer();
    viewer.apply_all(&player.update());
    assert_eq!((viewer.state_messages, viewer.static_messages), (3, 3));
    viewer.assert_shows(&Viewer::live(&stream, 4.0), 0.0, "after invalidation");

    // Without compression the replay is the same and the file larger.
    let plain = record(&stream, RecorderOptions { compression: Compression::None, ..options("stage") });
    assert!(plain.len() > file.len() * 11 / 10);
    check_replay(&stream, &plain, 0.0, 8);
}

/// A complete scan of the house with the light preset, then a second of
/// idle tail.
fn house_stream() -> Stream {
    let config = PipelineConfig::light(SceneKind::House);
    let mut done_for = 0;
    let mut pipeline = Pipeline::new(config);
    let dt = 1.0 / config.target_hz;
    let mut stream: Stream = pipeline.snapshot().into_iter().map(|o| (0.0, o.bytes)).collect();
    let mut tick = 0;
    while done_for < 20 {
        tick += 1;
        assert!(tick < 5000, "the scan never ends");
        stream.extend(pipeline.tick(dt).into_iter().map(|o| (tick as f64 * dt as f64, o.bytes)));
        done_for += usize::from(pipeline.scan_state() == Some(ScanState::Done));
    }
    stream
}

#[test]
fn house_scan_replay_matches_the_live_viewer() {
    let stream = house_stream();
    let appends =
        stream.iter().filter(|(_, m)| classify(m) == Some(MessageClass::Layer { layer: 0, replace: false })).count();
    assert!(appends > 100, "{appends}");
    let end = stream.last().unwrap().0;
    assert!(end > 60.0, "a whole walk through the house: {end}");

    // Default policy: a scan grows as fast as its stream, so no keyframe
    // is ever worth its size, and the replay is bit for bit the stream.
    let file = record(&stream, options("house"));
    let default_len = file.len();
    let report = verify(&file).unwrap();
    assert_eq!(report.info.scene, "house");
    assert_eq!(report.keyframes.len(), 1, "{:?}", report.keyframes);
    assert!(report.stored_bytes < report.raw_bytes / 2, "{} of {}", report.stored_bytes, report.raw_bytes);
    check_replay(&stream, &file, 0.0, 14);

    // Keyframes forced into the scan: their layer 0 is the merged chunks.
    // Positions are requantized into the box of the whole map, at most half
    // a step of it away; everything else is exact.
    let keyed = RecorderOptions { keyframes: KeyframePolicy { min_interval_s: 4.0, ratio: 0.2 }, ..options("house") };
    let file = record(&stream, keyed);
    let report = verify(&file).unwrap();
    assert!(report.keyframes.len() >= 8, "{:?}", report.keyframes);
    let live = Viewer::live(&stream, end);
    let extent = (0..3)
        .map(|k| {
            let (lo, hi) =
                live.layers[0].iter().fold((f32::MAX, f32::MIN), |(lo, hi), s| (lo.min(s.pos[k]), hi.max(s.pos[k])));
            hi - lo
        })
        .fold(0.0f32, f32::max);
    let step = extent / 65535.0;
    assert!(step < 3.0e-4, "a quantization step of the house is {step} m");
    check_replay(&stream, &file, step * 0.51 + 1.0e-6, 14);

    // Thumbnails left out: a third smaller, same splats.
    let lean = record(&stream, RecorderOptions { previews: false, ..options("house") });
    assert!(lean.len() < default_len * 9 / 10, "{} against {default_len}", lean.len());
    let report = verify(&lean).unwrap();
    assert!(report.is_valid() && report.previews == 0);
}

#[test]
fn tiny_session_replays() {
    let stream = tiny_stream();
    let file = record(&stream, tiny_options());
    let report = verify(&file).unwrap();
    assert!(report.keyframes.len() >= 4, "{:?}", report.keyframes);
    assert!(file.len() < 40_000, "{}", file.len());
    check_replay(&stream, &file, TINY_TOLERANCE, 80);
}

#[test]
fn a_recording_plays_from_range_requests() {
    let stream = tiny_stream();
    let file = record(&stream, tiny_options());
    let end = stream.last().unwrap().0;
    let whole = verify(&file).unwrap();

    let mut player = Player::ranged(file.len() as u64);
    // Small blocks and a cache much smaller than the file.
    let cache = 6_000;
    player.set_options(PlayerOptions { fetch_block: 700, max_cache_bytes: cache, prefetch_s: 0.5, jump_s: 2.0 });
    let mut viewer = Viewer::default();
    let mut requests = Vec::new();
    assert_eq!(player.wanted(), Some(ByteRange { start: 0, end: 4096 }));
    assert_eq!(player.wanted().unwrap().http_header(), "bytes=0-4095");
    assert!(player.update().buffering);

    // A jump to the end first: the header, the index, the last keyframe.
    player.seek(1.0e9);
    let update = settle(&mut player, &file, &mut viewer, &mut requests);
    assert_eq!(player.info().unwrap().name, "tiny");
    assert_eq!(player.duration(), Some(end));
    assert_eq!(update.time, end);
    assert_eq!(player.keyframe_times().count(), whole.keyframes.len());
    viewer.assert_shows(&Viewer::live(&stream, end), TINY_TOLERANCE, "end");
    let fetched: u64 = requests.iter().map(ByteRange::len).sum();
    let last_keyframe = whole.keyframes.last().unwrap().1;
    assert!(requests.iter().skip(2).all(|r| r.start >= last_keyframe), "{requests:?}");
    assert!(fetched < file.len() as u64 * 3 / 4, "{fetched} of {} bytes fetched to show the end", file.len());
    assert!(!player.stats().index_missing);

    // Scrub around; the cache stays bounded by one keyframe stretch.
    let mut random = Lcg(3);
    for i in 0..60 {
        let t = random.next() * end;
        player.seek(t);
        let update = settle(&mut player, &file, &mut viewer, &mut requests);
        assert_eq!(update.time, t);
        viewer.assert_shows(&Viewer::live(&stream, t), TINY_TOLERANCE, &format!("ranged seek {i} to {t:.3}"));
        assert!(player.stats().cached_bytes <= cache + 12_000, "{:?}", player.stats());
    }

    // Play through from the start with a clock; fetch between frames.
    player.seek(0.0);
    player.play();
    let mut stalls = 0;
    for _ in 0..2000 {
        pump(&mut player, &file, &mut requests);
        let update = player.advance(1.0 / 30.0);
        viewer.apply_all(&update);
        stalls += usize::from(update.buffering);
        if update.ended {
            break;
        }
    }
    assert!(!player.is_playing(), "the play head reached the end");
    assert_eq!(stalls, 0, "data fetched ahead of the play head");
    viewer.assert_shows(&Viewer::live(&stream, end), TINY_TOLERANCE, "ranged play");
    assert!(player.stats().cached_bytes <= cache + 12_000, "{:?}", player.stats());

    // A slow network: one request answered every few frames. The head
    // waits, the picture never runs ahead of the data, the end is reached.
    let mut player = Player::ranged(file.len() as u64);
    player.set_options(PlayerOptions { fetch_block: 500, ..PlayerOptions::default() });
    let mut viewer = Viewer::default();
    player.play();
    let (mut frames, mut stalled) = (0, 0);
    loop {
        if frames % 4 == 0 {
            if let Some(range) = player.wanted() {
                player.supply(range.start, &file[range.start as usize..range.end as usize]).unwrap();
            }
        }
        let before = player.time();
        let update = player.advance(1.0 / 30.0);
        viewer.apply_all(&update);
        if update.buffering {
            stalled += 1;
            assert!(update.time <= before + 1e-12, "a waiting head does not move");
        }
        if !update.buffering {
            viewer.assert_shows(&Viewer::live(&stream, update.time), TINY_TOLERANCE, "slow network");
        }
        frames += 1;
        assert!(frames < 20_000);
        if update.ended {
            break;
        }
    }
    assert!(stalled > 10);
    viewer.assert_shows(&Viewer::live(&stream, end), TINY_TOLERANCE, "slow network end");
}

#[test]
fn a_recording_plays_while_it_downloads() {
    let stream = tiny_stream();
    let file = record(&stream, tiny_options());
    let end = stream.last().unwrap().0;
    for chunk in [1usize, 7, 333, 100_000] {
        let mut player = Player::streaming();
        let mut viewer = Viewer::default();
        player.play();
        assert!(player.advance(0.1).buffering);
        assert_eq!(player.time(), 0.0);
        for piece in file.chunks(chunk) {
            player.push_bytes(piece).unwrap();
            let update = player.advance(0.05);
            viewer.apply_all(&update);
            assert!(update.time <= player.buffered_until() + 1e-12, "the head never runs ahead of the download");
            if !update.buffering {
                // Everything up to the head has arrived and is on screen.
                viewer.assert_shows(
                    &Viewer::live(&stream, update.time),
                    TINY_TOLERANCE,
                    &format!("chunks of {chunk} at {}", update.time),
                );
            }
        }
        player.finish_stream();
        assert_eq!(player.duration(), Some(end));
        for _ in 0..1000 {
            let update = player.advance(0.05);
            viewer.apply_all(&update);
            if update.ended {
                break;
            }
        }
        assert_eq!(player.time(), end);
        viewer.assert_shows(&Viewer::live(&stream, end), TINY_TOLERANCE, "download finished");
        // Once it is all there, it seeks like a file.
        player.seek(1.3);
        viewer.apply_all(&player.update());
        viewer.assert_shows(&Viewer::live(&stream, 1.3), TINY_TOLERANCE, "seek after download");
    }
    assert_eq!(Player::streaming().push_bytes(b"GSCAP1\0\0"), Err(RecordError::BadMagic));
    let mut cut = Player::streaming();
    cut.push_bytes(&file[..10]).unwrap();
    cut.finish_stream();
    assert_eq!(cut.error(), Some(RecordError::TooShort));
    assert!(!cut.update().buffering);
}

/// What is whole in a recording cut at `cut` bytes, by the records of the
/// uncut one: the time of the last complete record and the number of
/// complete message records. `None` when not even a keyframe is whole.
fn playable(records: &[(u64, f64, bool)], cut: usize) -> Option<(f64, usize)> {
    let complete: Vec<_> = records.iter().take_while(|(end, _, _)| *end <= cut as u64).collect();
    let messages = complete.iter().filter(|(_, _, keyframe)| !keyframe).count();
    complete.iter().any(|(_, _, keyframe)| *keyframe).then(|| (complete.last().unwrap().1, messages))
}

#[test]
fn a_recording_cut_at_any_byte_plays_what_is_whole() {
    let stream = tiny_stream();
    let file = record(&stream, tiny_options());
    let records = records_of(&file);
    assert!(records.len() > 80);
    let first_record = 12 + HEADER_JSON_SIZE;
    // The live viewer after the snapshot burst (the first keyframe) and
    // `n` more messages.
    let mut lives: BTreeMap<usize, Viewer> = BTreeMap::new();
    let mut live_after = |n: usize| lives.entry(n).or_insert_with(|| Viewer::live_after(&stream, 4 + n)).clone();

    for cut in 0..file.len() {
        let part = &file[..cut];
        let expected = playable(&records, cut);

        // The checker: an unusable header, or a report that says "cut".
        match verify(part) {
            Err(error) => {
                assert!(cut < first_record, "{cut}: {error}");
                assert_eq!(error, RecordError::TooShort, "{cut}");
            }
            Ok(report) => {
                assert!(cut >= first_record);
                assert!(report.truncated && !report.complete && !report.is_valid(), "{cut}");
                let only_empty = report.problems.iter().all(|p| p.error == RecordError::Empty);
                assert!(only_empty && report.problems.is_empty() == expected.is_some(), "{cut}: {:?}", report.problems);
                assert_eq!(expected.map_or(0.0, |e| e.0), report.duration_s, "{cut}");
            }
        }

        // From memory.
        match (Player::from_bytes(part.to_vec()), expected) {
            (Err(error), None) => {
                assert_eq!(error, if cut < first_record { RecordError::TooShort } else { RecordError::Empty }, "{cut}");
            }
            (Ok(mut player), Some((end, messages))) => {
                assert_eq!(player.duration(), Some(end), "{cut}");
                player.seek(f64::MAX);
                let mut viewer = Viewer::default();
                viewer.apply_all(&player.update());
                viewer.assert_shows(&live_after(messages), TINY_TOLERANCE, &format!("cut at {cut}"));
            }
            (got, expected) => panic!("cut at {cut}: {:?} but expected {expected:?}", got.err()),
        }

        // Thinned out, the two other ways in (they cost more per cut).
        if cut % 7 != 0 && cut + 40 < file.len() {
            continue;
        }
        let mut streamed = Player::streaming();
        let pushed = part.chunks(97).try_for_each(|piece| streamed.push_bytes(piece));
        assert_eq!(pushed, Ok(()));
        streamed.finish_stream();
        let mut ranged = Player::ranged(cut as u64);
        ranged.set_options(PlayerOptions { fetch_block: 1500, ..PlayerOptions::default() });
        for (name, player) in [("streamed", &mut streamed), ("ranged", &mut ranged)] {
            let mut viewer = Viewer::default();
            player.seek(f64::MAX);
            let update = settle(player, part, &mut viewer, &mut Vec::new());
            match expected {
                Some((end, messages)) => {
                    assert_eq!((player.error(), player.duration()), (None, Some(end)), "{name} cut at {cut}");
                    assert_eq!(update.time, end);
                    viewer.assert_shows(&live_after(messages), TINY_TOLERANCE, &format!("{name} cut at {cut}"));
                    assert!(player.stats().index_missing || name == "streamed");
                }
                None => {
                    assert!(player.error().is_some(), "{name} cut at {cut}");
                    assert!(update.messages.is_empty() && viewer.state.is_none());
                }
            }
        }
    }
}

/// The index entries of a finished recording, and where they are.
fn index_of(file: &[u8]) -> (usize, Vec<(f64, u64)>) {
    let at = u64::from_le_bytes(file[file.len() - 16..file.len() - 8].try_into().unwrap()) as usize;
    let entries = file[at + 16..file.len() - 16]
        .chunks_exact(16)
        .map(|e| (f64::from_le_bytes(e[..8].try_into().unwrap()), u64::from_le_bytes(e[8..].try_into().unwrap())))
        .collect();
    (at, entries)
}

#[test]
fn a_wrong_index_costs_speed_not_correctness() {
    let stream = tiny_stream();
    let file = record(&stream, tiny_options());
    let end = stream.last().unwrap().0;
    let (index_at, entries) = index_of(&file);
    assert!(entries.len() >= 4);
    assert_eq!(entries, verify(&file).unwrap().keyframes);
    let entry_at = |i: usize| index_at + 16 + i * 16;

    let mut cases: Vec<(&str, Vec<u8>)> = Vec::new();
    let mut patched = |name: &'static str, offset: usize, bytes: &[u8]| {
        let mut f = file.clone();
        f[offset..offset + bytes.len()].copy_from_slice(bytes);
        cases.push((name, f));
    };
    // Offsets into the middle of records, behind the file, before the header.
    patched("offset inside a record", entry_at(2) + 8, &(entries[2].1 + 5).to_le_bytes());
    let records = records_of(&file);
    let after_keyframe = records.iter().find(|(end, _, _)| *end > entries[1].1).unwrap().0;
    patched("offset of a message record", entry_at(1) + 8, &after_keyframe.to_le_bytes());
    patched("offset beyond the file", entry_at(3) + 8, &u64::MAX.to_le_bytes());
    patched("offset in the header", entry_at(0) + 8, &4u64.to_le_bytes());
    patched("wrong time", entry_at(2), &(entries[2].0 + 0.125).to_le_bytes());
    patched("time not a number", entry_at(1), &f64::NAN.to_le_bytes());
    patched("trailer points elsewhere", file.len() - 16, &(index_at as u64 + 3).to_le_bytes());
    patched("trailer points at a keyframe", file.len() - 16, &entries[1].1.to_le_bytes());
    patched("no end marker", file.len() - 8, b"GSRECENX");
    patched("index of another kind", index_at + 4, &[0]);
    patched("index length", index_at, &40u32.to_le_bytes());
    // An index whose every entry points at the same real keyframe.
    let mut same = file.clone();
    for i in 0..entries.len() {
        same[entry_at(i) + 8..entry_at(i) + 16].copy_from_slice(&entries[1].1.to_le_bytes());
    }
    cases.push(("all entries the same", same));

    for (name, damaged) in &cases {
        let report = verify(damaged).unwrap();
        assert!(!report.is_valid(), "{name}");
        // A player that holds the file reads the records themselves.
        let mut whole = Player::from_bytes(damaged.clone()).unwrap();
        // A player that fetches ranges notices and falls back to reading on.
        let mut ranged = Player::ranged(damaged.len() as u64);
        ranged.set_options(PlayerOptions { fetch_block: 900, ..PlayerOptions::default() });
        let mut random = Lcg(11);
        let mut views = [Viewer::default(), Viewer::default()];
        for i in 0..25 + entries.len() {
            // Every stretch between two keyframes is visited.
            let t = entries.get(i).map_or_else(|| random.next() * end, |(time, _)| (time + 0.3).min(end));
            whole.seek(t);
            views[0].apply_all(&whole.update());
            ranged.seek(t);
            settle(&mut ranged, damaged, &mut views[1], &mut Vec::new());
            let live = Viewer::live(&stream, t.min(whole.duration().unwrap()));
            // "index of another kind" turns the index into a message record
            // that no viewer gets: both players drop it.
            views[0].assert_shows(&live, TINY_TOLERANCE, &format!("{name}: whole, seek {i} to {t}"));
            views[1].assert_shows(&live, TINY_TOLERANCE, &format!("{name}: ranged, seek {i} to {t}"));
        }
        assert!(ranged.stats().index_missing, "{name}");
    }
}

#[test]
fn damaged_records_end_or_thin_the_playable_part() {
    let stream = tiny_stream();
    let file = record(&stream, RecorderOptions { compression: Compression::None, ..tiny_options() });
    let records = records_of(&file);
    // Framing damage in a message record a third into the file: everything
    // before it plays.
    let at = (29..records.len()).find(|&i| !records[i].2 && !records[i - 1].2).unwrap();
    let start = records[at - 1].0 as usize;
    let before = records[at - 1].1;
    // The stream up to the damage: the snapshot burst is the first keyframe.
    let whole_before = Viewer::live_after(&stream, 4 + records[..at].iter().filter(|r| !r.2).count());
    for (name, offset, bytes, error) in [
        ("length too small", 0, &3u32.to_le_bytes()[..], RecordError::BadLength),
        ("length huge", 0, &u32::MAX.to_le_bytes()[..], RecordError::BadLength),
        ("kind", 4, &[7][..], RecordError::BadKind),
        ("flags", 5, &[0x80][..], RecordError::BadFlags),
        ("time", 8, &f64::NAN.to_le_bytes()[..], RecordError::BadTime),
        ("time backwards", 8, &0.0f64.to_le_bytes()[..], RecordError::BadTime),
    ] {
        let mut damaged = file.clone();
        damaged[start + offset..start + offset + bytes.len()].copy_from_slice(bytes);
        let report = verify(&damaged).unwrap();
        assert_eq!(report.problems.len(), 1, "{name}");
        assert_eq!((report.problems[0].offset, report.problems[0].error), (start as u64, error), "{name}");
        assert_eq!(
            (report.records, report.duration_s, report.complete, report.truncated),
            (at as u64, before, false, false),
            "{name}"
        );

        let mut streamed = Player::streaming();
        streamed.push_bytes(&damaged).unwrap();
        streamed.finish_stream();
        // A player that fetches ranges finds the damage when it reads it;
        // until then the index lets it play what lies behind.
        let mut ranged = Player::ranged(damaged.len() as u64);
        let mut viewer = Viewer::default();
        ranged.seek(f64::MAX);
        let update = settle(&mut ranged, &damaged, &mut viewer, &mut Vec::new());
        assert_eq!((update.time, ranged.damage()), (stream.last().unwrap().0, None), "{name}");
        ranged.seek(before - 0.5);
        settle(&mut ranged, &damaged, &mut viewer, &mut Vec::new());
        ranged.play();
        for _ in 0..200 {
            pump(&mut ranged, &damaged, &mut Vec::new());
            if ranged.advance(0.05).ended {
                break;
            }
        }
        assert!(ranged.damage().is_some() && !ranged.is_playing(), "{name}");
        let mut whole = Player::from_bytes(damaged.clone()).unwrap();
        for player in [&mut whole, &mut streamed, &mut ranged] {
            let mut viewer = Viewer::default();
            player.invalidate_viewer();
            player.seek(f64::MAX);
            let update = settle(player, &damaged, &mut viewer, &mut Vec::new());
            assert_eq!(update.time, before, "{name}");
            assert_eq!(player.damage().map(|d| (d.offset, d.error)), Some((start as u64, error)), "{name}");
            viewer.assert_shows(&whole_before, TINY_TOLERANCE, name);
            player.seek(1.0);
            settle(player, &damaged, &mut viewer, &mut Vec::new());
            viewer.assert_shows(&Viewer::live(&stream, 1.0), TINY_TOLERANCE, name);
        }
    }

    // A message that is not one: reported, skipped, the rest plays.
    let mut damaged = file.clone();
    damaged[start + 16..start + 20].copy_from_slice(b"XXXX");
    let report = verify(&damaged).unwrap();
    assert_eq!(
        report.problems.iter().map(|p| (p.offset, p.error)).collect::<Vec<_>>(),
        [(start as u64, RecordError::BadMessage)]
    );
    assert!(report.complete && !report.is_valid());
    // Played through, the viewer gets every message but that one.
    let missing = 4 + records[..at].iter().filter(|r| !r.2).count();
    let mut thinned = stream.clone();
    thinned.remove(missing);
    let mut player = Player::from_bytes(damaged).unwrap();
    let mut viewer = Viewer::default();
    player.play();
    while player.time() < before + 0.2 {
        viewer.apply_all(&player.advance(0.05));
    }
    viewer.assert_shows(&Viewer::live(&thinned, player.time()), TINY_TOLERANCE, "skipped message");
    assert_eq!(player.stats().dropped, 1);

    // A keyframe that is not a snapshot: seeks use the one before it.
    let keyframes = verify(&file).unwrap().keyframes;
    let (time, offset) = keyframes[2];
    let mut damaged = file.clone();
    let message_len = offset as usize + 16;
    damaged[message_len..message_len + 4].copy_from_slice(&7u32.to_le_bytes());
    let report = verify(&damaged).unwrap();
    assert_eq!(
        report.problems.iter().map(|p| (p.offset, p.error)).collect::<Vec<_>>(),
        [(offset, RecordError::BadKeyframe)]
    );
    assert!(report.complete, "the index lists the keyframe records, usable or not");
    assert_eq!(report.keyframes.len(), keyframes.len() - 1);
    let mut player = Player::from_bytes(damaged).unwrap();
    let mut viewer = Viewer::default();
    player.seek(time + 0.3);
    viewer.apply_all(&player.update());
    viewer.assert_shows(&Viewer::live(&stream, time + 0.3), TINY_TOLERANCE, "bad keyframe");
    assert_eq!(player.keyframe_times().count(), keyframes.len() - 1);
}

#[test]
fn hostile_bytes_never_panic_and_never_reach_the_viewer() {
    let stream = tiny_stream();
    let files = [
        record(&stream, tiny_options()),
        record(&stream, RecorderOptions { compression: Compression::None, ..tiny_options() }),
    ];
    let mut random = Lcg(99);
    let mut played = 0;
    for round in 0..6000 {
        let mut bytes = files[round % 2].clone();
        // One to three bytes changed, in the records or (sometimes) the header.
        for _ in 0..1 + round % 3 {
            let lo = if round % 10 == 0 { 0 } else { 1024 };
            let at = lo + (random.next() * (bytes.len() - lo) as f64) as usize;
            bytes[at] = if round % 4 == 0 { (random.next() * 256.0) as u8 } else { bytes[at] ^ (1 << (round % 8)) };
        }
        if round % 13 == 0 {
            bytes.truncate((random.next() * bytes.len() as f64) as usize);
        }
        let _ = verify(&bytes);
        let mut players = Vec::new();
        players.extend(Player::from_bytes(bytes.clone()).ok());
        let mut streamed = Player::streaming();
        if bytes.chunks(211).try_for_each(|piece| streamed.push_bytes(piece)).is_ok() {
            streamed.finish_stream();
            players.push(streamed);
        }
        let mut ranged = Player::ranged(bytes.len() as u64);
        ranged.set_options(PlayerOptions {
            fetch_block: 1 + (random.next() * 3000.0) as u64,
            max_cache_bytes: 8000,
            ..PlayerOptions::default()
        });
        players.push(ranged);
        for player in &mut players {
            let mut viewer = Viewer::default();
            let duration = player.duration().unwrap_or(6.0);
            for step in 0..6 {
                player.seek(random.next() * duration * 1.2);
                for _ in 0..400 {
                    while let Some(range) = player.wanted() {
                        let end = (range.end as usize).min(bytes.len());
                        let start = (range.start as usize).min(end);
                        if player.supply(range.start, &bytes[start..end]) != Ok(true) {
                            break;
                        }
                    }
                    // Whatever comes out decodes: `Viewer::apply` insists.
                    let update = if step % 2 == 0 { player.update() } else { player.advance(0.1) };
                    viewer.apply_all(&update);
                    if !update.buffering {
                        break;
                    }
                }
            }
            played += usize::from(viewer.state.is_some());
        }
    }
    assert!(played > 6000, "most damaged files still show something: {played}");

    // Pure noise, and structure with hostile sizes.
    for seed in 0..200u64 {
        let mut random = Lcg(seed);
        let mut noise: Vec<u8> =
            (0..(random.next() * 3000.0) as usize).map(|_| (random.next() * 256.0) as u8).collect();
        if seed % 2 == 0 && noise.len() > 12 {
            noise[..8].copy_from_slice(b"GSREC1\0\0");
            noise[8..12].copy_from_slice(&((seed as u32) % 64).to_le_bytes());
        }
        let _ = verify(&noise);
        let _ = Player::from_bytes(noise.clone());
        let mut streamed = Player::streaming();
        let _ = streamed.push_bytes(&noise);
        streamed.finish_stream();
        let _ = streamed.update();
        let mut ranged = Player::ranged(noise.len() as u64);
        for _ in 0..50 {
            let Some(range) = ranged.wanted() else { break };
            if ranged
                .supply(
                    range.start,
                    &noise[(range.start as usize).min(noise.len())..(range.end as usize).min(noise.len())],
                )
                .is_err()
            {
                break;
            }
            let _ = ranged.update();
        }
    }
    // A deflate bomb: a megabyte of zeros per kilobyte, far beyond what a
    // record may inflate to, is refused instead of allocated.
    let bomb = miniz_oxide::deflate::compress_to_vec(&vec![0u8; (128 << 20) + 1], 1);
    assert!(bomb.len() < 1 << 20);
    let mut file = files[1][..12 + HEADER_JSON_SIZE].to_vec();
    file.extend_from_slice(&(bomb.len() as u32 + 12).to_le_bytes());
    file.extend_from_slice(&[1, 1, 0, 0]);
    file.extend_from_slice(&0f64.to_le_bytes());
    file.extend_from_slice(&bomb);
    let report = verify(&file).unwrap();
    assert_eq!(report.problems[0].error, RecordError::BadDeflate);
    // The framing is whole; the keyframe is found unusable when needed.
    let mut player = Player::from_bytes(file).unwrap();
    let update = player.update();
    assert!(update.messages.is_empty() && !update.buffering);
    assert_eq!(player.error(), Some(RecordError::Empty));
}

#[test]
fn the_recorder_starts_anywhere_and_stops_at_its_bound() {
    let stream = tiny_stream();
    let end = stream.last().unwrap().0;

    // Started in the middle of the session from a state kept on the side,
    // on a clock that does not start at zero.
    let split = stream.iter().position(|(t, _)| *t >= 2.0).unwrap();
    let mut state = StreamState::new();
    for (_, message) in &stream[..split] {
        state.apply(message).unwrap();
    }
    let mut recorder = Recorder::with_state(Vec::new(), tiny_options(), state);
    for (time, message) in &stream[split..] {
        recorder.push(1000.0 + time, message).unwrap();
    }
    assert!((recorder.duration_s() - (end - 2.0)).abs() < 1e-9);
    let (file, summary) = recorder.finish().unwrap();
    assert!((summary.duration_s - (end - 2.0)).abs() < 1e-9 && summary.keyframes >= 3 && !summary.full);
    assert!(verify(&file).unwrap().is_valid());
    let mut player = Player::from_bytes(file).unwrap();
    let mut viewer = Viewer::default();
    for t in [0.0, 1.0, 3.9, 0.4] {
        player.seek(t);
        viewer.apply_all(&player.update());
        viewer.assert_shows(&Viewer::live(&stream, 2.0 + t), TINY_TOLERANCE, &format!("late start, {t}"));
    }

    // A bound: the file never exceeds it, is complete, and plays up to
    // where the recorder stopped.
    for bound in [0u64, 500, 1500, 3000, 7000, 12_000, 1 << 20] {
        let mut recorder = Recorder::new(Vec::new(), tiny_options().with_max_bytes(bound));
        let mut stopped_at = None;
        for (i, (time, message)) in stream.iter().enumerate() {
            match recorder.push(*time, message).unwrap() {
                Pushed::Full => {
                    stopped_at.get_or_insert(i);
                    assert!(recorder.is_full());
                }
                _ => assert!(stopped_at.is_none(), "nothing is recorded after the bound was hit"),
            }
            assert!(recorder.bytes_written() <= bound);
        }
        let (file, summary) = recorder.finish().unwrap();
        assert!(file.len() as u64 <= bound, "{} bytes with a bound of {bound}", file.len());
        assert_eq!(summary.full, stopped_at.is_some());
        if file.is_empty() {
            assert!(bound < 2000);
            continue;
        }
        let report = verify(&file).unwrap();
        assert!(report.is_valid(), "bound {bound}: {:?}", report.problems);
        let mut player = Player::from_bytes(file).unwrap();
        player.seek(f64::MAX);
        let mut viewer = Viewer::default();
        viewer.apply_all(&player.update());
        viewer.assert_shows(
            &Viewer::live_after(&stream, stopped_at.unwrap_or(stream.len())),
            TINY_TOLERANCE,
            &format!("bound {bound}"),
        );
        assert!(report.duration_s <= end && (stopped_at.is_some() || report.duration_s == end));
    }

    // To a file through `IoSink`: the same bytes as in memory.
    let path = std::env::temp_dir().join(format!("gsrec-test-{}.gsrec", std::process::id()));
    let sink = IoSink(std::io::BufWriter::new(std::fs::File::create(&path).unwrap()));
    let mut recorder = Recorder::new(sink, tiny_options());
    for (time, message) in &stream {
        recorder.push(*time, message).unwrap();
    }
    let (sink, _) = recorder.finish().unwrap();
    drop(sink);
    let on_disk = std::fs::read(&path).unwrap();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(on_disk, record(&stream, tiny_options()));
    // A recorder that is dropped leaves a file that plays.
    let mut recorder = Recorder::new(Vec::new(), tiny_options());
    for (time, message) in &stream[..60] {
        recorder.push(*time, message).unwrap();
    }
    let unfinished = recorder.sink().clone();
    let report = verify(&unfinished).unwrap();
    assert!(report.truncated && report.problems.is_empty() && report.info.duration_s == 0.0);
    assert_eq!(Player::from_bytes(unfinished).unwrap().duration(), Some(stream[59].0));
}

#[test]
fn the_recorder_takes_the_stream_as_it_comes() {
    let stream = tiny_stream();
    // Messages before the State are part of the first keyframe; nothing is
    // written until the State is known; garbage is left out; time never
    // runs backwards.
    let mut recorder = Recorder::new(Vec::new(), tiny_options());
    assert_eq!(recorder.push(5.0, &stream[1].1).unwrap(), Pushed::Recorded);
    assert_eq!(recorder.push(5.0, b"not a message").unwrap(), Pushed::Rejected);
    assert_eq!(recorder.bytes_written(), 0);
    assert_eq!(recorder.push(5.0, &stream[0].1).unwrap(), Pushed::Recorded);
    assert_eq!(recorder.push(5.0, &stream[2].1).unwrap(), Pushed::Recorded);
    assert_eq!(recorder.bytes_written(), 0, "the burst that describes the session is one keyframe");
    assert_eq!(recorder.push(5.5, &stream[3].1).unwrap(), Pushed::Keyframe);
    assert_eq!(recorder.push(f64::NAN, &stream[4].1).unwrap(), Pushed::Recorded);
    assert_eq!(recorder.push(4.0, &stream[5].1).unwrap(), Pushed::Recorded);
    assert_eq!(recorder.duration_s(), 0.0);
    assert_eq!(recorder.push(6.5, &stream[6].1).unwrap(), Pushed::Recorded);
    assert_eq!(recorder.duration_s(), 1.0);
    let (file, summary) = recorder.finish().unwrap();
    assert_eq!((summary.messages, summary.keyframes, summary.rejected, summary.scene.as_str()), (3, 1, 1, "tiny"));
    let report = verify(&file).unwrap();
    assert!(report.is_valid(), "{:?}", report.problems);
    let mut player = Player::from_bytes(file).unwrap();
    let mut viewer = Viewer::default();
    viewer.apply_all(&player.update());
    let mut live = Viewer::default();
    for (_, message) in &stream[..6] {
        live.apply(message);
    }
    viewer.assert_shows(&live, 0.0, "the first instant");

    // No State, no file.
    let mut recorder = Recorder::new(Vec::new(), tiny_options());
    recorder.push(0.0, &stream[1].1).unwrap();
    recorder.push(1.0, &stream[3].1).unwrap();
    let (file, summary) = recorder.finish().unwrap();
    assert!(file.is_empty() && summary.bytes == 0);
    assert_eq!(Player::from_bytes(file).err(), Some(RecordError::TooShort));
    // A State alone is a still recording of nothing.
    let mut recorder = Recorder::new(Vec::new(), tiny_options());
    recorder.push(0.0, &stream[0].1).unwrap();
    let (file, _) = recorder.finish().unwrap();
    let report = verify(&file).unwrap();
    assert!(report.is_valid() && report.keyframes.len() == 1 && report.duration_s == 0.0);

    // Without statistics and thumbnails: none in the file, none in the
    // keyframes, the splats as ever.
    let lean = RecorderOptions { stats: false, previews: false, ..tiny_options() };
    let mut recorder = Recorder::new(Vec::new(), lean);
    let mut skipped = 0;
    for (time, message) in &stream {
        skipped += usize::from(recorder.push(*time, message).unwrap() == Pushed::Skipped);
    }
    assert!(skipped > 40);
    let (file, _) = recorder.finish().unwrap();
    let report = verify(&file).unwrap();
    assert!(report.is_valid() && report.stats == 0 && report.previews == 0 && report.keyframes.len() >= 3);
    let mut player = Player::from_bytes(file.clone()).unwrap();
    let mut viewer = Viewer::default();
    for t in [5.9, 2.2, 4.1] {
        player.seek(t);
        viewer.apply_all(&player.update());
        let live = Viewer::live(&stream, t);
        assert!(viewer.stats.is_none() && viewer.previews.is_empty());
        assert_eq!(viewer.layers[1], live.layers[1]);
        assert_eq!(viewer.layers[0].len(), live.layers[0].len());
    }
    assert!(file.len() < record(&stream, tiny_options()).len() * 3 / 4);

    // The header alone tells what a file is.
    let file = record(&stream, tiny_options());
    let info = read_info(&file[..12 + HEADER_JSON_SIZE]).unwrap();
    assert_eq!((info.name.as_str(), info.scene.as_str(), info.protocol, info.duration_s), ("tiny", "tiny", 1, 6.0));
    assert_eq!(info.created, "2026-11-14T10:00:00Z");
    assert_eq!(read_info(&file[..500]), Err(RecordError::TooShort));
    assert_eq!(read_info(b"PK\x03\x04"), Err(RecordError::BadMagic));

    // A sink that fails (a full disk): the recorder stops, and what
    // reached the sink still plays.
    struct Disk {
        bytes: Vec<u8>,
        room: usize,
    }
    impl RecordSink for Disk {
        type Error = &'static str;
        fn append_bytes(&mut self, bytes: &[u8]) -> Result<(), Self::Error> {
            let fits = bytes.len().min(self.room - self.bytes.len());
            self.bytes.extend_from_slice(&bytes[..fits]);
            if fits < bytes.len() {
                Err("disk full")
            } else {
                Ok(())
            }
        }
        fn patch_bytes(&mut self, offset: u64, bytes: &[u8]) -> Result<(), Self::Error> {
            self.bytes.patch_bytes(offset, bytes).map_err(|_| "unreachable")
        }
    }
    let mut recorder = Recorder::new(Disk { bytes: Vec::new(), room: 9000 }, tiny_options());
    let mut failed_at = None;
    for (i, (time, message)) in stream.iter().enumerate() {
        match recorder.push(*time, message) {
            Ok(_) => assert!(failed_at.is_none()),
            Err(WriteError::Sink(error)) => {
                assert_eq!((error, failed_at.replace(i)), ("disk full", None));
            }
            Err(error) => assert_eq!((error, failed_at.is_some()), (WriteError::Stopped, true)),
        }
    }
    let failed_at = failed_at.expect("9000 bytes do not hold the session");
    let disk = recorder.into_sink();
    assert_eq!(disk.bytes.len(), 9000);
    let report = verify(&disk.bytes).unwrap();
    assert!(report.truncated && report.problems.is_empty());
    let mut player = Player::from_bytes(disk.bytes).unwrap();
    player.seek(f64::MAX);
    let mut viewer = Viewer::default();
    viewer.apply_all(&player.update());
    assert!(
        player.duration().unwrap() <= stream[failed_at].0 && player.duration().unwrap() >= stream[failed_at].0 - 0.5
    );
    assert!(viewer.state.is_some());
    let mut recorder = Recorder::new(Disk { bytes: Vec::new(), room: 3000 }, tiny_options());
    let _ = stream.iter().try_for_each(|(time, message)| recorder.push(*time, message).map(|_| ()));
    assert_eq!(recorder.finish().err(), Some(WriteError::Stopped));

    // A name too long for the fixed header is cut, not refused.
    let long = RecorderOptions { name: "é".repeat(5000), ..tiny_options() };
    let file = record(&stream, long);
    let report = verify(&file).unwrap();
    assert!(report.is_valid());
    assert!(
        report.info.name.chars().all(|c| c == 'é')
            && (100..=HEADER_JSON_SIZE / 2).contains(&report.info.name.chars().count())
    );
}

#[test]
fn the_play_head_obeys_the_transport() {
    let stream = tiny_stream();
    let file = record(&stream, tiny_options());
    let end = stream.last().unwrap().0;
    let mut player = Player::from_bytes(file).unwrap();
    assert_eq!((player.time(), player.is_playing(), player.speed(), player.is_looping()), (0.0, false, 1.0, false));

    // Paused: the clock does not move the head, but the viewer is brought
    // to it.
    let update = player.advance(1.0);
    assert!(update.reset && !update.messages.is_empty() && update.time == 0.0);
    assert!(player.advance(1.0).messages.is_empty());
    player.play();
    assert_eq!(player.advance(0.5).time, 0.5);
    player.set_speed(2.0);
    assert_eq!(player.advance(0.5).time, 1.5);
    player.set_speed(0.5);
    assert_eq!(player.advance(0.5).time, 1.75);
    // A clock that misbehaves is no clock.
    for dt in [f64::NAN, -3.0, f64::NEG_INFINITY] {
        assert_eq!(player.advance(dt).time, 1.75);
    }
    for (asked, got) in [(0.0, 1.0 / 64.0), (1.0e9, 64.0), (f64::NAN, 1.0), (-2.0, 1.0 / 64.0)] {
        player.set_speed(asked);
        assert_eq!(player.speed(), got);
    }
    player.set_speed(1.0);
    player.pause();
    assert_eq!(player.advance(0.5).time, 1.75);

    // Seeks are clamped to the recording.
    for (asked, got) in [(-1.0, 0.0), (f64::NAN, 0.0), (1.0e12, end), (f64::INFINITY, 0.0), (2.5, 2.5)] {
        player.seek(asked);
        assert_eq!(player.update().time, got, "{asked}");
    }

    // The end: reported once, the head stays, play starts over.
    player.seek(end - 0.1);
    player.play();
    let update = player.advance(0.5);
    assert!(update.ended && update.time == end && !player.is_playing());
    let update = player.advance(0.5);
    assert!(!update.ended && update.messages.is_empty() && update.time == end);
    player.play();
    assert_eq!(player.time(), 0.0);
    assert!(player.advance(0.25).reset);

    // Looping: the head wraps and keeps the remainder.
    player.set_looping(true);
    player.seek(end - 0.1);
    player.update();
    let update = player.advance(0.35);
    assert!((update.time - 0.25).abs() < 1e-9 && update.reset && !update.ended && player.is_playing());
    // A step of several lengths still lands inside.
    let update = player.advance(end * 3.0 + 0.5);
    assert!((update.time - 0.75).abs() < 1e-9, "{}", update.time);

    // A big step forward goes through a keyframe; a small one does not.
    player.set_looping(false);
    player.pause();
    player.seek(0.3);
    player.update();
    player.seek(1.2);
    assert!(!player.update().reset);
    player.seek(5.2);
    assert!(player.update().reset);
    let resyncs = player.stats().resyncs;
    player.seek(5.3);
    assert!(!player.update().reset);
    assert_eq!(player.stats().resyncs, resyncs);
    assert!(player.keyframe_times().all(|t| (0.0..=end).contains(&t)));
    assert_eq!(player.keyframe_times().next(), Some(0.0));
}
