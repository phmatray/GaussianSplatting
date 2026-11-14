//! Delivery on the live stage: snapshot on connect, pacing, and what slow
//! and stalled viewers get.

mod common;

use std::time::{Duration, Instant};

use common::*;
use gs_core::codec::LayerMode;
use gs_core::pipeline::{LAYER_DYNAMIC, LAYER_STATIC};
use gs_core::protocol::PipelineConfig;
use gs_core::rig::RingRig;
use gs_core::scene::SceneKind;

/// A stage whose live layer is large (every pixel of every camera, about
/// 0.6 MB per tick), so that a client that stops reading is felt by the
/// server within one or two frames instead of disappearing into the
/// kernel's socket buffers.
fn heavy_stage(hz: f32) -> PipelineConfig {
    PipelineConfig {
        rig: RingRig { count: 4, width: 160, height: 120, ..RingRig::default() },
        target_hz: hz,
        background_subtraction: false,
        ..PipelineConfig::for_scene(SceneKind::Stage)
    }
}

/// Receive buffer of the clients that play the slow viewer.
const SMALL_BUFFER: u32 = 8 * 1024;

fn strictly_increasing(seqs: &[u32]) -> bool {
    seqs.windows(2).all(|w| w[0] < w[1])
}

/// The ordering every viewer can rely on, however slow it is: reliable
/// messages in order, and within each latest-wins slot never an older
/// message after a newer one.
fn ordered_per_slot(log: &[Received]) -> bool {
    let mut newest: std::collections::HashMap<String, u32> = std::collections::HashMap::new();
    log.iter().all(|message| {
        let slot = match &message.event {
            Event::State | Event::Layer { id: LAYER_STATIC, .. } => "reliable".to_string(),
            Event::Layer { .. } => "live".to_string(),
            Event::Stats => "stats".to_string(),
            Event::Preview { camera } => format!("preview {camera}"),
        };
        newest.insert(slot, message.seq).is_none_or(|previous| previous < message.seq)
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn snapshot_on_connect_then_live_stream_at_target_rate() {
    let _serial = serial().await;
    let server = start_server(small_stage(30.0), |_| {}).await;
    let mut client = Client::connect(server.port()).await;

    let snapshot = client.snapshot().await;
    let seqs: Vec<u32> = snapshot.iter().map(|m| m.seq).collect();
    assert!(strictly_increasing(&seqs), "snapshot out of order: {seqs:?}");

    // The four messages describe one instant.
    let state = client.viewer.state.clone().unwrap();
    assert_eq!(state.version, gs_core::protocol::PROTOCOL_VERSION);
    assert_eq!(state.scene, "stage");
    assert_eq!(state.cameras.len(), 4);
    assert_eq!(state.config.target_hz, 30.0);
    let stats = client.viewer.stats.unwrap();
    let [layer0, layer1] = &client.viewer.layers;
    assert!(layer0.len() > 1000, "static layer has only {} splats", layer0.len());
    assert!(layer1.len() > 100, "live layer has only {} splats", layer1.len());
    assert_eq!(stats.splats_static as usize, layer0.len());
    assert_eq!(stats.splats_dynamic as usize, layer1.len());
    assert!(snapshot.iter().all(|m| m.time_ms == snapshot[0].time_ms), "snapshot spans several scene times");
    // Decoded splats lie inside the bounds the State announces (with slack
    // for the quantization of the wire format).
    let (min, max) = (state.bounds.min, state.bounds.max);
    let inside = |s: &gs_core::Splat| (0..3).all(|i| s.pos[i] >= min[i] - 0.5 && s.pos[i] <= max[i] + 0.5);
    assert!(layer0.iter().chain(layer1.iter()).all(inside), "splats outside the scene bounds");

    // Then the live stream, at the configured rate.
    let start = Instant::now();
    client.pump(Duration::from_secs(3)).await;
    let live: Vec<&Received> = client.viewer.log.iter().filter(|m| m.at >= start).collect();
    let rate = client.viewer.dynamic_rate_since(start);
    assert!((27.0..=33.0).contains(&rate), "live layers at {rate:.1} Hz instead of 30");
    assert!(live.iter().all(|m| !m.is_reliable()), "nothing reliable is expected while the stage just plays");
    let seqs: Vec<u32> = client.viewer.log.iter().map(|m| m.seq).collect();
    assert!(strictly_increasing(&seqs), "a viewer that keeps up must receive the pipeline order");

    // Stats come with every tick, previews go round the cameras.
    let stats_count = live.iter().filter(|m| m.event == Event::Stats).count();
    assert!((75..=105).contains(&stats_count), "{stats_count} Stats in 3 s");
    for camera in 0..4u8 {
        assert!(live.iter().any(|m| m.event == Event::Preview { camera }), "no preview of camera {camera}");
    }
    let stats = client.viewer.stats.unwrap();
    assert!((25.0..=35.0).contains(&stats.tick_hz), "pipeline reports {} Hz", stats.tick_hz);

    // The scene follows the wall clock: 3 s of stream is 3 s of dance.
    let dynamic: Vec<&&Received> = live.iter().filter(|m| m.is_dynamic()).collect();
    let (first, last) = (dynamic[0], dynamic[dynamic.len() - 1]);
    let wall = last.at.duration_since(first.at).as_secs_f64();
    let period_ms = state.loop_duration.map_or(u32::MAX, |p| (p * 1000.0) as u32);
    let scene = ((last.time_ms + period_ms - first.time_ms) % period_ms) as f64 / 1000.0;
    assert!((scene - wall).abs() < 0.25, "scene advanced {scene:.2} s in {wall:.2} s");

    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_clients_see_the_same_stream() {
    let _serial = serial().await;
    let server = start_server(small_stage(30.0), |_| {}).await;
    let mut a = Client::connect(server.port()).await;
    a.snapshot().await;
    a.pump(Duration::from_millis(300)).await;
    // A second viewer joins while the first one is live.
    let mut b = Client::connect(server.port()).await;
    b.snapshot().await;
    assert_eq!(b.viewer.layers[LAYER_STATIC as usize], a.viewer.layers[LAYER_STATIC as usize]);

    let (a, b) = (Background::spawn(a), Background::spawn(b));
    tokio::time::sleep(Duration::from_secs(1)).await;
    let (a, b) = (a.stop().await, b.stop().await);
    // Every live layer the second viewer got, the first one got too, with
    // the same content: one encoded message is shared by all clients.
    let of = |client: &Client| -> Vec<(u32, usize)> {
        client.viewer.log.iter().filter(|m| m.is_dynamic()).map(|m| (m.seq, m.bytes)).collect()
    };
    let (seen_a, seen_b) = (of(&a), of(&b));
    let common: Vec<&(u32, usize)> = seen_b.iter().filter(|m| seen_a.contains(m)).collect();
    assert!(common.len() >= 20, "only {} live layers in common", common.len());
    let missing =
        seen_b.iter().skip(1).filter(|(seq, _)| *seq <= seen_a.last().unwrap().0).filter(|m| !seen_a.contains(m));
    assert_eq!(missing.count(), 0, "the viewers disagree on what was broadcast");
    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stalled_client_resumes_with_the_latest_layer_and_others_are_unaffected() {
    let _serial = serial().await;
    const HZ: f64 = 20.0;
    const STALL: Duration = Duration::from_secs(4);
    let server = start_server(heavy_stage(HZ as f32), |_| {}).await;
    let port = server.port();

    let mut fast = Vec::new();
    for _ in 0..2 {
        let mut client = Client::connect(port).await;
        client.snapshot().await;
        fast.push(Background::spawn(client));
    }
    let mut stalled = Client::connect_with(port, Some(SMALL_BUFFER), &[]).await.unwrap();
    stalled.snapshot().await;
    stalled.pump(Duration::from_millis(500)).await;
    let frame_bytes = stalled.viewer.log.iter().rev().find(|m| m.is_dynamic()).unwrap().bytes;
    assert!(frame_bytes > 400_000, "live layers of {frame_bytes} bytes are too small for this test");

    let before_stall = stalled.viewer.dynamic_frames();

    // The stalled viewer stops reading altogether. Meanwhile, watch what
    // the server holds for its three viewers.
    let ticks_before = server.handle.metrics().ticks;
    let memory_before = resident_bytes();
    let stall_start = Instant::now();
    let mut most_queued = 0;
    while stall_start.elapsed() < STALL {
        tokio::time::sleep(Duration::from_millis(20)).await;
        most_queued = most_queued.max(server.handle.metrics().queued_bytes as usize);
    }
    let stall_end = Instant::now();
    let memory_after = resident_bytes();
    let ticks_during = server.handle.metrics().ticks - ticks_before;
    let produced = ticks_during as usize * frame_bytes;

    // The pipeline kept its pace: nobody waited for the stalled socket.
    let tick_rate = ticks_during as f64 / STALL.as_secs_f64();
    assert!((HZ * 0.85..=HZ * 1.1).contains(&tick_rate), "pipeline ticked at {tick_rate:.1} Hz during the stall");

    // Nothing piled up for it while some 45 megabytes were produced: every
    // viewer, stalled or not, holds at most the messages of one tick (one
    // live layer and a few small ones).
    let growth = memory_after.saturating_sub(memory_before) as usize;
    eprintln!(
        "stall: {tick_rate:.1} Hz, {produced} bytes produced, at most {most_queued} bytes queued, \
         resident memory grew by {growth} bytes"
    );
    assert!(produced > 30 << 20, "only {produced} bytes produced during the stall");
    assert!(most_queued > 0, "the queue metric does not move");
    assert!(most_queued <= 3 * frame_bytes * 3 / 2, "{most_queued} bytes were queued for three viewers");
    // The memory of the process agrees. It is a coarse witness: it also
    // counts the two fast viewers of this test, whose decoding of a few
    // megabytes of splats per tick moves it by 2 to 17 megabytes from one
    // run to the next. A backlog would be everything that was produced.
    assert!(growth < produced * 3 / 4, "memory grew by {growth} bytes while {produced} bytes were produced");

    // What it gets when it reads again: at most the few frames that were
    // already in the kernel's socket buffers, then the newest one.
    let newest_at_resume = fast[0].dynamic_seq();
    stalled.pump(Duration::from_millis(1500)).await;
    let after: Vec<&Received> = stalled.viewer.log.iter().filter(|m| m.at >= stall_end && m.is_dynamic()).collect();
    let stale = after.iter().filter(|m| m.seq < newest_at_resume).count();
    let skipped = ticks_during as usize - stale;
    assert!(stale <= 6, "{stale} stale live layers were delivered after the stall");
    assert!(skipped >= 60, "only {skipped} live layers were skipped");
    let first_fresh = after.iter().find(|m| m.seq >= newest_at_resume).expect("no fresh live layer after the stall");
    let delay = first_fresh.at.duration_since(stall_end);
    assert!(delay < Duration::from_millis(1000), "the newest layer came {delay:?} after reading resumed");
    // And it is live again at the full rate.
    let rate = stalled.viewer.dynamic_rate_since(stall_end + Duration::from_millis(700));
    assert!(rate > HZ * 0.75, "the resumed viewer only gets {rate:.1} Hz");
    assert!(ordered_per_slot(&stalled.viewer.log), "a stale message was delivered after a newer one");
    assert_eq!(server.handle.metrics().resyncs, 0, "the stage sends nothing reliable: no resync expected");
    assert!(stalled.viewer.dynamic_frames() - before_stall < 60);

    // The other viewers never noticed.
    for (i, background) in fast.into_iter().enumerate() {
        let client = background.stop().await;
        let frames: Vec<Instant> = client
            .viewer
            .log
            .iter()
            .filter(|m| m.is_dynamic() && m.at >= stall_start && m.at <= stall_end)
            .map(|m| m.at)
            .collect();
        let rate = (frames.len() - 1) as f64 / frames.last().unwrap().duration_since(frames[0]).as_secs_f64();
        let worst_gap = frames.windows(2).map(|w| w[1].duration_since(w[0])).max().unwrap();
        assert!((HZ * 0.85..=HZ * 1.1).contains(&rate), "fast viewer {i} got {rate:.1} Hz during the stall");
        assert!(worst_gap < Duration::from_millis(250), "fast viewer {i} waited {worst_gap:?} for a frame");
        // It received every tick: consecutive live layers, none skipped.
        let received = frames.len() as u64;
        assert!(received + 3 >= ticks_during, "fast viewer {i} got {received} of {ticks_during} live layers");
    }
    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn slow_reader_skips_stale_layers_but_sees_every_slot() {
    let _serial = serial().await;
    const HZ: f64 = 20.0;
    let server = start_server(heavy_stage(HZ as f32), |_| {}).await;
    let mut fast = Client::connect(server.port()).await;
    fast.snapshot().await;
    let fast = Background::spawn(fast);
    let mut slow = Client::connect_with(server.port(), Some(SMALL_BUFFER), &[]).await.unwrap();
    slow.snapshot().await;

    // A viewer that takes about fifteen messages per second while the
    // server produces sixty.
    let start = Instant::now();
    let ticks_before = server.handle.metrics().ticks;
    let mut staleness = Vec::new();
    while start.elapsed() < Duration::from_secs(6) {
        let received = slow.message().await;
        if received.is_dynamic() {
            // How far behind the newest broadcast this layer is, in ticks
            // (the three messages of a tick share the sequence counter).
            staleness.push(fast.dynamic_seq().saturating_sub(received.seq) / 3);
        }
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
    let produced = server.handle.metrics().ticks - ticks_before;
    let log: Vec<&Received> = slow.viewer.log.iter().filter(|m| m.at >= start).collect();
    let dynamic = log.iter().filter(|m| m.is_dynamic()).count() as u64;
    let stats = log.iter().filter(|m| m.event == Event::Stats).count() as u64;
    let previews = log.iter().filter(|m| matches!(m.event, Event::Preview { .. })).count() as u64;
    // Every slot gets its turn, and most live layers are skipped.
    assert!(
        dynamic >= 10 && stats >= 10 && previews >= 10,
        "starved slot: {dynamic} layers, {stats} stats, {previews} previews"
    );
    assert!(dynamic * 2 < produced, "{dynamic} of {produced} live layers delivered: nothing was skipped");
    // What it reads is recent. With a queue instead of mailboxes it would
    // be about 60 ticks behind after 6 s, and falling further behind.
    let (early, late) = staleness.split_at(staleness.len() / 2);
    let worst_late = late.iter().max().copied().unwrap_or(0);
    let worst_early = early.iter().max().copied().unwrap_or(0);
    assert!(worst_late <= 20, "a live layer was {worst_late} ticks old when it was read (staleness: {staleness:?})");
    assert!(worst_late <= worst_early + 6, "the viewer is falling behind: {staleness:?}");
    assert!(ordered_per_slot(&slow.viewer.log));

    // Reading at full speed again: live within a moment.
    let resume = Instant::now();
    slow.pump(Duration::from_millis(1500)).await;
    let rate = slow.viewer.dynamic_rate_since(resume + Duration::from_millis(700));
    assert!(rate > HZ * 0.75, "back at full speed the viewer only gets {rate:.1} Hz");
    let last_slow = slow.viewer.log.iter().rev().find(|m| m.is_dynamic()).unwrap().seq;
    let newest = fast.dynamic_seq();
    assert!(newest.saturating_sub(last_slow) <= 3 * 6, "still {} messages behind", newest - last_slow);

    let fast = fast.stop().await;
    let rate = fast.viewer.dynamic_rate_since(start);
    assert!((HZ * 0.85..=HZ * 1.1).contains(&rate), "the fast viewer got {rate:.1} Hz next to the slow one");
    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn layers_are_replace_messages_on_the_right_ids() {
    let _serial = serial().await;
    let server = start_server(small_stage(30.0), |_| {}).await;
    let mut client = Client::connect(server.port()).await;
    client.snapshot().await;
    client.pump(Duration::from_millis(600)).await;
    for message in &client.viewer.log[4..] {
        match message.event {
            Event::Layer { id, mode, .. } => {
                assert_eq!(id, LAYER_DYNAMIC, "only the live layer changes on the stage");
                assert_eq!(mode, LayerMode::Replace);
            }
            Event::State => panic!("unexpected State"),
            Event::Stats | Event::Preview { .. } => {}
        }
    }
    server.handle.shutdown().await;
}
