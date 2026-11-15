//! The home scan: reliable, ordered delivery of layer 0, late joiners and
//! resynchronisation after a backlog overflow.

mod common;

use std::time::{Duration, Instant};

use common::*;
use gs_core::codec::{self, LayerMode};
use gs_core::pipeline::LAYER_STATIC;
use gs_core::protocol::{PipelineConfig, ScanState};
use serde_json::json;

const VOXEL: f32 = 0.08;

/// A scan of a few seconds that is started by hand.
fn manual_scan() -> PipelineConfig {
    let mut config = small_house(96, 72, VOXEL, 2.5, 4.0);
    config.scan.autostart = false;
    config
}

fn scan_state(viewer: &Viewer) -> Option<ScanState> {
    viewer.scan().map(|scan| scan.state)
}

fn progress(viewer: &Viewer) -> f32 {
    viewer.scan().map_or(0.0, |scan| scan.progress)
}

fn appends(viewer: &Viewer) -> usize {
    viewer
        .log
        .iter()
        .filter(|m| matches!(m.event, Event::Layer { id: LAYER_STATIC, mode: LayerMode::Append, .. }))
        .count()
}

/// Reads all clients until each has seen `done`.
async fn all_until(clients: &mut [&mut Client], what: &str, done: impl Fn(&Viewer) -> bool + Copy) {
    futures_util::future::join_all(clients.iter_mut().map(|client| async move {
        if !done(&client.viewer) {
            client.until(what, |viewer, _| done(viewer)).await;
        }
    }))
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn appends_reach_everyone_in_order_and_late_joiners_hold_the_same_layer() {
    let _serial = serial().await;
    let server = start_server(manual_scan(), |_| {}).await;
    let port = server.port();

    // Two viewers from the start. Nothing is scanned yet.
    let mut a = Client::connect(port).await;
    let mut b = Client::connect(port).await;
    for client in [&mut a, &mut b] {
        client.snapshot().await;
        assert_eq!(client.viewer.scene(), "house");
        assert!(client.viewer.layers[0].is_empty() && client.viewer.layers[1].is_empty());
        assert_eq!(scan_state(&client.viewer), Some(ScanState::Idle));
        assert!(client.viewer.state.as_ref().unwrap().scan.is_some(), "the State of the house carries the scan path");
    }

    // Phase 1: scanning. A third viewer joins in the middle of it.
    a.send_json(json!({ "type": "scan_start" })).await;
    all_until(&mut [&mut a, &mut b], "a third of the scan", |v| progress(v) > 0.3).await;
    let mut c = Client::connect(port).await;
    let c_snapshot = c.snapshot().await;
    let c_joined_with = c.viewer.layers[0].len();
    assert!(c_joined_with > 500, "the mid-scan snapshot holds only {c_joined_with} splats");
    assert_eq!(c.viewer.stats.unwrap().splats_static as usize, c_joined_with);
    assert_eq!(scan_state(&c.viewer), Some(ScanState::Scanning));

    // Phase 2: pause. Layer 0 is sent again as a whole (splats were refined
    // in place since they were appended). A fourth viewer joins now.
    all_until(&mut [&mut a, &mut b, &mut c], "half of the scan", |v| progress(v) > 0.5).await;
    b.send_json(json!({ "type": "scan_pause" })).await;
    all_until(&mut [&mut a, &mut b, &mut c], "the pause", |v| scan_state(v) == Some(ScanState::Paused)).await;
    let pause_replace = a.viewer.replaced.last().expect("no Replace at the pause").0;
    let mut d = Client::connect(port).await;
    d.snapshot().await;
    assert_eq!(scan_state(&d.viewer), Some(ScanState::Paused));
    assert_eq!(d.viewer.layers[0], a.viewer.layers[0], "joining during the pause must give the layer the others hold");

    // Phase 3: resume to the end. The scan closes with one more Replace.
    d.send_json(json!({ "type": "scan_start" })).await;
    all_until(&mut [&mut a, &mut b, &mut c, &mut d], "the end of the scan", |v| scan_state(v) == Some(ScanState::Done))
        .await;

    // Every reliable message reached the two early viewers, in order.
    let (log_a, log_b) = (a.viewer.reliable_log(), b.viewer.reliable_log());
    assert_eq!(log_a, log_b, "the early viewers received different reliable streams");
    assert!(log_a.windows(2).all(|w| w[0].0 < w[1].0), "reliable messages out of order");
    let total_appends = appends(&a.viewer);
    assert!(total_appends >= 20, "only {total_appends} Appends in the whole scan");
    let replaces: Vec<u32> = a.viewer.replaced.iter().map(|(seq, _)| *seq).collect();
    assert_eq!(replaces.len(), 3, "expected the snapshot, the pause and the final Replace, got {replaces:?}");
    let final_replace = *replaces.last().unwrap();
    assert!(
        matches!(log_a.last(), Some((seq, Event::Layer { mode: LayerMode::Replace, .. })) if *seq == final_replace)
    );

    // The mid-scan joiner: after its snapshot it received exactly what the
    // early viewers received after that point, nothing lost or repeated.
    let after_join = c_snapshot[1].seq;
    let tail_a: Vec<_> = log_a.iter().filter(|(seq, _)| *seq > after_join).cloned().collect();
    let tail_c: Vec<_> = c.viewer.reliable_log().into_iter().filter(|(seq, _)| *seq > after_join).collect();
    assert_eq!(tail_c, tail_a);
    assert!(appends(&c.viewer) >= 5 && appends(&c.viewer) < total_appends);
    // Its layer 0 had the same size as theirs after every message.
    for (seq, size) in &c.viewer.static_sizes[1..] {
        let early = a.viewer.static_sizes.iter().find(|(s, _)| s == seq).expect("message missing on the early viewer");
        assert_eq!(*size, early.1, "layer 0 sizes differ after message {seq}");
    }
    // Right before the pause Replace: what it appended is bit-identical to
    // what they appended; the part that came with its snapshot holds the
    // same cells, already refined (a refined splat stays in its cell).
    let before = |viewer: &Viewer, seq: u32| viewer.replaced.iter().find(|(s, _)| *s == seq).unwrap().1.clone();
    let (early, late) = (before(&a.viewer, pause_replace), before(&c.viewer, pause_replace));
    assert_eq!(late.len(), early.len());
    assert!(late.len() > c_joined_with, "nothing was appended between the join and the pause");
    assert_eq!(late[c_joined_with..], early[c_joined_with..]);
    let distance =
        |l: &gs_core::Splat, e: &gs_core::Splat| (0..3).map(|i| (l.pos[i] - e.pos[i]).abs()).fold(0.0, f32::max);
    let moved = late[..c_joined_with].iter().zip(&early[..c_joined_with]).filter(|(l, e)| distance(l, e) > 1.5 * VOXEL);
    let (moved, farthest) = moved.fold((0, 0.0f32), |(n, far), (l, e)| (n + 1, far.max(distance(l, e))));
    assert!(
        moved * 50 <= c_joined_with && farthest < 6.0 * VOXEL,
        "{moved} of {c_joined_with} snapshot splats are away from where the early viewers have them (up to {farthest} m)"
    );

    // The viewer that joined during the pause: snapshot plus Appends is
    // exactly the layer of the early viewers, splat for splat, right up to
    // the final Replace.
    let (early, late) = (before(&a.viewer, final_replace), before(&d.viewer, final_replace));
    assert!(early.len() > a.viewer.replaced[1].1.len(), "nothing was appended after the pause");
    assert_eq!(late, early, "snapshot + Appends differs from the early viewer's layer 0");

    // And after the final Replace everybody holds the finished house.
    let finished = &a.viewer.layers[0];
    assert!(finished.len() > 2000);
    for (name, client) in [("b", &b), ("c", &c), ("d", &d)] {
        assert_eq!(&client.viewer.layers[0], finished, "viewer {name} ends with another house");
        assert_eq!(client.viewer.stats.unwrap().splats_static as usize, finished.len());
        assert!(client.viewer.layers[1].is_empty());
    }

    // A viewer that arrives after the scan gets the finished house at once.
    let start = Instant::now();
    let mut e = Client::connect(port).await;
    e.snapshot().await;
    let took = start.elapsed();
    assert_eq!(&e.viewer.layers[0], finished);
    assert_eq!(scan_state(&e.viewer), Some(ScanState::Done));
    // Not merely equal once decoded: the layer of its snapshot is, byte for
    // byte, the `Replace` that closed the scan for the early viewers.
    let wire = a.viewer.static_replace_payload();
    assert!(wire.len() > finished.len() * codec::SPLAT_WIRE_SIZE);
    for (name, client) in [("b", &b), ("c", &c), ("d", &d), ("e (late)", &e)] {
        assert!(client.viewer.static_replace_payload() == wire, "viewer {name} holds other layer-0 bytes");
    }
    eprintln!(
        "house: {} splats, {total_appends} Appends, late joiner synchronised in {took:?} ({} bytes)",
        finished.len(),
        e.viewer.log[1].bytes
    );
    assert_eq!(server.handle.metrics().resyncs, 0, "no viewer was slow: no resync expected");
    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn backlog_overflow_resynchronises_and_converges() {
    let _serial = serial().await;
    // A denser scan (more and larger Appends), a tight backlog bound, and
    // small socket buffers so that a viewer that stops reading is felt at
    // once.
    let mut config = small_house(128, 96, 0.04, 2.0, 10.0);
    config.scan.autostart = false;
    let server = start_server(config, |server| {
        server.queue.max_messages = 12;
        server.send_buffer = Some(16 * 1024);
    })
    .await;
    let port = server.port();

    let mut healthy = Client::connect(port).await;
    healthy.snapshot().await;
    let mut stalled = Client::connect_with(port, Some(8 * 1024), &[]).await.unwrap();
    stalled.snapshot().await;
    healthy.send_json(json!({ "type": "scan_start" })).await;
    stalled.until("the scan to start", |v, _| progress(v) > 0.05).await;
    let before_stall = appends(&stalled.viewer);

    // The healthy viewer keeps reading; the other one reads nothing while
    // sixty more reliable messages go out, ten times the bound.
    let healthy = Background::spawn(healthy);
    let reliable_before = healthy.reliable();
    eventually("sixty Appends during the stall", || healthy.reliable() >= reliable_before + 60).await;
    // At most one snapshot was made meanwhile: the next one waits until
    // the viewer can take it.
    let during_stall = server.handle.metrics().resyncs;
    assert!(during_stall <= 1, "{during_stall} snapshots were made for a viewer that reads nothing");

    // Reading again: the stale backlog is gone, a fresh snapshot arrives
    // (State, then layer 0 as a whole) and the stream continues from it.
    // The rest of the walk is then fused as fast as the pipeline can.
    stalled.send_json(json!({ "type": "scan_finish" })).await;
    stalled.until("the end of the scan", |v, _| scan_state(v) == Some(ScanState::Done)).await;
    let mut healthy = healthy.stop().await;
    if scan_state(&healthy.viewer) != Some(ScanState::Done) {
        healthy.until("the end of the scan", |v, _| scan_state(v) == Some(ScanState::Done)).await;
    }
    let metrics = server.handle.metrics();
    assert!(metrics.resyncs >= 1, "the stalled viewer was never resynchronised");

    let log = stalled.viewer.reliable_log();
    assert!(log.windows(2).all(|w| w[0].0 < w[1].0), "reliable messages out of order after the resync");
    let states: Vec<usize> = log.iter().enumerate().filter(|(_, (_, e))| *e == Event::State).map(|(i, _)| i).collect();
    assert!(states.len() >= 2, "no second State: the viewer was not resynchronised");
    for &i in &states {
        assert!(
            matches!(log.get(i + 1), Some((_, Event::Layer { id: LAYER_STATIC, mode: LayerMode::Replace, .. }))),
            "a State of a snapshot must be followed by layer 0 as a whole"
        );
    }
    // It skipped Appends (they were dropped with the backlog)...
    let (got, all) = (appends(&stalled.viewer), appends(&healthy.viewer));
    let healthy_resynced = healthy.viewer.log.iter().filter(|m| m.event == Event::State).count() > 1;
    assert!(got >= before_stall);
    assert!(
        got + 20 < all || healthy_resynced,
        "the stalled viewer received {got} of {all} Appends: nothing overflowed"
    );
    // ...and still ends with exactly the same house.
    assert!(healthy.viewer.layers[0].len() > 5000);
    assert_eq!(stalled.viewer.layers[0], healthy.viewer.layers[0], "the resynchronised viewer ends with another house");
    assert!(
        stalled.viewer.static_replace_payload() == healthy.viewer.static_replace_payload(),
        "the resynchronised viewer holds other layer-0 bytes"
    );
    // When its last Replace is the one that closed the scan (and not a
    // snapshot that came after it), what it held right before was already
    // complete: snapshot plus Appends had the size the healthy viewer had.
    let (final_seq, held) = stalled.viewer.replaced.last().unwrap();
    if let Some((_, expected)) = healthy.viewer.replaced.iter().find(|(seq, _)| seq == final_seq) {
        assert_eq!(held.len(), expected.len(), "snapshot + Appends after the resync has the wrong size");
    }
    eprintln!("resync: {} resync(s), stalled viewer got {got} of {all} Appends", metrics.resyncs);
    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn scan_does_not_progress_without_viewers() {
    let _serial = serial().await;
    let server = start_server(small_house(64, 48, 0.1, 1.0, 2.0), |_| {}).await;
    let mut first = Client::connect(server.port()).await;
    first.snapshot().await;
    first.until("some progress", |v, _| progress(v) > 0.02).await;
    drop(first);
    eventually("the viewer to be gone", || server.handle.metrics().clients == 0).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let (ticks, time) = (server.handle.metrics().ticks, server.handle.metrics().scene_time);

    tokio::time::sleep(Duration::from_millis(1500)).await;
    let metrics = server.handle.metrics();
    assert_eq!((metrics.ticks, metrics.scene_time), (ticks, time), "the scan went on with nobody watching");

    // The next viewer finds the scan where it was left.
    let mut second = Client::connect(server.port()).await;
    second.snapshot().await;
    let stats = second.viewer.stats.unwrap();
    assert_eq!(stats.scene_time, time);
    assert_eq!(stats.scan.unwrap().state, ScanState::Scanning);
    server.handle.shutdown().await;
}
