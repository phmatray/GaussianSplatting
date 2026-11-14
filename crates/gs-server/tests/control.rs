//! Control messages: what is applied, what is ignored, and that abuse from
//! one client leaves the pipeline and the other clients alone.

mod common;

use std::time::{Duration, Instant};

use common::*;
use futures_util::SinkExt;
use gs_core::codec::LayerMode;
use gs_core::pipeline::{LAYER_STATIC, MAX_CONTROL_JSON};
use serde_json::json;
use tokio_tungstenite::tungstenite::Message;

/// Three live viewers on a small stage.
async fn three_viewers(server: &TestServer) -> [Client; 3] {
    let mut clients = [
        Client::connect(server.port()).await,
        Client::connect(server.port()).await,
        Client::connect(server.port()).await,
    ];
    for client in &mut clients {
        client.snapshot().await;
    }
    clients
}

async fn all_until(clients: &mut [Client], what: &str, done: impl Fn(&Viewer) -> bool + Copy) {
    futures_util::future::join_all(clients.iter_mut().map(|client| async move {
        if !done(&client.viewer) {
            client.until(what, |viewer, _| done(viewer)).await;
        }
    }))
    .await;
}

fn config(viewer: &Viewer) -> gs_core::protocol::PipelineConfig {
    viewer.state.as_ref().expect("no State yet").config
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn set_scene_reaches_every_client_with_new_layers() {
    let _serial = serial().await;
    let server = start_server(small_stage(30.0), |_| {}).await;
    let mut clients = three_viewers(&server).await;
    let stage_static = clients[0].viewer.layers[0].len();

    clients[1].send_json(json!({ "type": "set_scene", "scene": "house" })).await;
    all_until(&mut clients, "the house", |v| v.scene() == "house" && v.scan().is_some()).await;
    for client in &clients {
        let viewer = &client.viewer;
        // State first, then both layers as a whole, as in a snapshot.
        let state_at = viewer.log.iter().rposition(|m| m.event == Event::State).unwrap();
        assert!(matches!(
            viewer.log[state_at + 1].event,
            Event::Layer { id: LAYER_STATIC, mode: LayerMode::Replace, .. }
        ));
        assert!(viewer.state.as_ref().unwrap().scan.is_some());
        assert!(viewer.state.as_ref().unwrap().cameras.is_empty());
        assert!(viewer.layers[1].is_empty(), "the live layer of the stage must be cleared");
    }

    clients[2].send_json(json!({ "type": "set_scene", "scene": "stage" })).await;
    all_until(&mut clients, "the stage again", |v| v.scene() == "stage" && !v.layers[1].is_empty()).await;
    for client in &clients {
        assert_eq!(client.viewer.layers[0].len(), stage_static, "the static stage came back different");
    }
    assert_eq!(server.handle.metrics().controls, 2);
    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn patch_config_pause_and_time_scale() {
    let _serial = serial().await;
    let server = start_server(small_stage(30.0), |_| {}).await;
    let mut clients = three_viewers(&server).await;

    // A partial configuration: only the named fields change.
    clients[0]
        .send_json(json!({ "type": "patch_config", "patch": { "target_hz": 12, "preview": { "enabled": false } } }))
        .await;
    all_until(&mut clients, "12 Hz", |v| config(v).target_hz == 12.0).await;
    for client in &clients {
        let config = config(&client.viewer);
        assert!(!config.preview.enabled);
        assert_eq!(config.rig.count, 4, "fields that were not patched must keep their value");
    }
    let start = Instant::now();
    clients[2].pump(Duration::from_secs(2)).await;
    let rate = clients[2].viewer.dynamic_rate_since(start + Duration::from_millis(300));
    assert!((10.5..=13.5).contains(&rate), "live layers at {rate:.1} Hz after asking for 12");
    let previews = clients[2].viewer.log.iter().filter(|m| m.at > start + Duration::from_millis(300));
    assert_eq!(previews.filter(|m| matches!(m.event, Event::Preview { .. })).count(), 0, "previews were switched off");

    // Out-of-range values are clamped by the pipeline, not trusted.
    clients[1].send_json(json!({ "type": "patch_config", "patch": { "target_hz": 1e9, "time_scale": -3 } })).await;
    all_until(&mut clients, "the clamped rate", |v| config(v).target_hz == 120.0).await;
    assert_eq!(config(&clients[0].viewer).time_scale, 0.0);
    clients[1].send_json(json!({ "type": "patch_config", "patch": { "target_hz": 30, "time_scale": 1 } })).await;
    all_until(&mut clients, "30 Hz again", |v| config(v).target_hz == 30.0 && config(v).time_scale == 1.0).await;

    // Pause: the scene stops, Stats keep coming.
    clients[1].send_json(json!({ "type": "set_paused", "paused": true })).await;
    all_until(&mut clients, "the pause", |v| config(v).paused).await;
    clients[0].pump(Duration::from_millis(300)).await;
    let frozen = clients[0].viewer.stats.unwrap();
    let dynamic_before = clients[0].viewer.dynamic_frames();
    clients[0].pump(Duration::from_millis(700)).await;
    let later = clients[0].viewer.stats.unwrap();
    assert_eq!(later.scene_time, frozen.scene_time, "the scene moved while paused");
    assert!(later.tick > frozen.tick + 10, "Stats stopped during the pause");
    assert_eq!(clients[0].viewer.dynamic_frames(), dynamic_before, "live layers were sent while paused");

    // Resume at half speed.
    clients[2].send_json(json!({ "type": "set_time_scale", "time_scale": 0.5 })).await;
    clients[2].send_json(json!({ "type": "set_paused", "paused": false })).await;
    all_until(&mut clients, "the resume", |v| !config(v).paused && config(v).time_scale == 0.5).await;
    clients[0].pump(Duration::from_millis(300)).await;
    let (t0, s0) = (Instant::now(), clients[0].viewer.stats.unwrap().scene_time);
    clients[0].pump(Duration::from_secs(2)).await;
    let (wall, scene) = (t0.elapsed().as_secs_f32(), clients[0].viewer.stats.unwrap().scene_time - s0);
    assert!((scene / wall - 0.5).abs() < 0.08, "scene advanced {scene:.2} s in {wall:.2} s at half speed");

    // Restart puts the dance back to its beginning.
    clients[0].send_json(json!({ "type": "restart" })).await;
    clients[1].until("the restart", |v, m| m.event == Event::Stats && v.stats.unwrap().scene_time < 0.5).await;
    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn garbage_binary_and_absurd_requests_are_ignored() {
    let _serial = serial().await;
    let server = start_server(small_stage(30.0), |_| {}).await;
    let mut clients = three_viewers(&server).await;
    let before = server.handle.metrics();

    let deep = format!("{}{}", "[".repeat(20_000), "]".repeat(20_000));
    let garbage: Vec<String> = vec![
        "".into(),
        "not json".into(),
        "{".into(),
        "{}".into(),
        "null".into(),
        "[1, 2, 3]".into(),
        "\"set_scene\"".into(),
        r#"{"type": "launch_missiles"}"#.into(),
        r#"{"type": "set_scene"}"#.into(),
        r#"{"type": "set_scene", "scene": "moon"}"#.into(),
        r#"{"type": "set_paused", "paused": "yes"}"#.into(),
        r#"{"type": "set_config", "config": 7}"#.into(),
        r#"{"type": "set_time_scale", "time_scale": 1e999}"#.into(),
        r#"{"type": "restart"} trailing"#.into(),
        deep,
        "\u{0}\u{1}\u{2}".into(),
    ];
    let garbage_count = garbage.len() as u64;
    for text in garbage {
        clients[0].send_text(text).await;
    }
    // Binary frames mean nothing in this direction.
    for size in [0usize, 1, 1000, MAX_CONTROL_JSON] {
        clients[0].ws.send(Message::Binary(vec![0xAB; size].into())).await.unwrap();
    }
    // Well-formed messages whose content makes no sense are handed to the
    // pipeline, which ignores or clamps them.
    clients[0].send_json(json!({ "type": "patch_config", "patch": 5 })).await;
    clients[0].send_json(json!({ "type": "patch_config", "patch": { "rig": "none" } })).await;
    clients[0]
        .send_json(json!({ "type": "patch_config", "patch": { "rig": { "count": 4_000_000_000u64, "width": 1e30 } } }))
        .await;
    clients[0].send_json(json!({ "type": "scan_start" })).await;
    // A marker to know when all of the above has been processed.
    clients[0].send_json(json!({ "type": "set_time_scale", "time_scale": 0.75 })).await;
    all_until(&mut clients, "the marker", |v| config(v).time_scale == 0.75).await;

    let after = server.handle.metrics();
    assert_eq!(after.rejected_controls - before.rejected_controls, garbage_count);
    assert_eq!(after.controls - before.controls, 5);
    // The absurd rig was clamped to something the pipeline can serve.
    let rig = config(&clients[1].viewer).rig;
    assert!(rig.count <= 24 && rig.width <= 640 && rig.height <= 480, "rig not sanitized: {rig:?}");
    assert!(u64::from(rig.count) * u64::from(rig.width) * u64::from(rig.height) <= 1_250_000);

    // Everybody is still served, the offender included.
    clients[2].send_json(json!({ "type": "set_config", "config": small_stage(30.0) })).await;
    all_until(&mut clients, "the small rig again", |v| config(v).rig.width == 64 && config(v).time_scale == 1.0).await;
    let start = Instant::now();
    futures_util::future::join_all(clients.iter_mut().map(|c| c.pump(Duration::from_millis(1500)))).await;
    for (i, client) in clients.iter().enumerate() {
        let rate = client.viewer.dynamic_rate_since(start + Duration::from_millis(300));
        assert!((26.0..=33.0).contains(&rate), "viewer {i} gets {rate:.1} Hz after the garbage");
    }
    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn oversized_text_closes_only_the_offender() {
    let _serial = serial().await;
    let server = start_server(small_stage(30.0), |_| {}).await;
    let mut clients = three_viewers(&server).await;

    // Exactly at the limit: still a control message like any other.
    let padded = |json: &str, len: usize| format!("{json}{}", " ".repeat(len - json.len()));
    let exact = padded(r#"{"type":"set_time_scale","time_scale":0.5}"#, MAX_CONTROL_JSON);
    assert_eq!(exact.len(), MAX_CONTROL_JSON);
    clients[0].send_text(exact).await;
    all_until(&mut clients, "the message of maximal size", |v| config(v).time_scale == 0.5).await;

    // One byte more is refused, and so is anything much larger.
    let mut offender = Client::connect(server.port()).await;
    offender.snapshot().await;
    offender.send_text(padded(r#"{"type":"set_time_scale","time_scale":0.25}"#, MAX_CONTROL_JSON + 1)).await;
    let close = offender.closed().await;
    assert_eq!(close.map(|(code, _)| code), Some(1009), "an oversized message is answered with close code 1009");

    let mut flooder = Client::connect(server.port()).await;
    flooder.snapshot().await;
    // The server may hang up while this megabyte is still being written.
    let _ = flooder.ws.send(Message::Text("x".repeat(1 << 20).into())).await;
    flooder.closed().await;
    let mut binary_flooder = Client::connect(server.port()).await;
    binary_flooder.snapshot().await;
    let _ = binary_flooder.ws.send(Message::Binary(vec![0u8; 1 << 20].into())).await;
    binary_flooder.closed().await;

    eventually("the offenders to be gone", || server.handle.metrics().clients == 3).await;
    // The refused message was not applied, and the others are untouched.
    let start = Instant::now();
    futures_util::future::join_all(clients.iter_mut().map(|c| c.pump(Duration::from_millis(1200)))).await;
    for (i, client) in clients.iter().enumerate() {
        assert_eq!(config(&client.viewer).time_scale, 0.5);
        let rate = client.viewer.dynamic_rate_since(start);
        assert!((26.0..=33.0).contains(&rate), "viewer {i} gets {rate:.1} Hz after the oversized messages");
    }
    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn burst_of_500_messages_is_applied_in_order() {
    let _serial = serial().await;
    let server = start_server(small_stage(30.0), |_| {}).await;
    let mut clients = three_viewers(&server).await;
    let before = server.handle.metrics();

    // 499 changes of the time scale, each different from the previous one,
    // then the value everybody must end with.
    let mut burst: Vec<Message> = (0..499)
        .map(|i| json!({ "type": "set_time_scale", "time_scale": 1.0 + (i % 7) as f32 * 0.25 }))
        .map(|value| Message::Text(value.to_string().into()))
        .collect();
    burst.push(Message::Text(json!({ "type": "set_time_scale", "time_scale": 0.125 }).to_string().into()));
    let start = Instant::now();
    {
        let [sender, others @ ..] = &mut clients;
        // Written while the others (and the sender itself) keep reading.
        let (mut sink, mut stream) = futures_util::StreamExt::split(&mut sender.ws);
        let write = async {
            for message in burst {
                sink.feed(message).await.unwrap();
            }
            sink.flush().await.unwrap();
        };
        let mut sender_viewer = Viewer::default();
        let read = async {
            // The sender's own copy of the stream, read until the final value.
            loop {
                match futures_util::StreamExt::next(&mut stream).await {
                    Some(Ok(Message::Binary(bytes))) => {
                        let message = gs_core::codec::decode(&bytes).unwrap();
                        if let gs_core::codec::Body::State(json) = message.body {
                            let state: gs_core::protocol::SessionState = serde_json::from_str(&json).unwrap();
                            let done = state.config.time_scale == 0.125;
                            sender_viewer.state = Some(state);
                            if done {
                                break;
                            }
                        }
                    }
                    Some(Ok(_)) => {}
                    other => panic!("the sender was disconnected during the burst: {other:?}"),
                }
            }
        };
        let rest = async {
            all_until(others, "the last value of the burst", |v| config(v).time_scale == 0.125).await;
        };
        tokio::time::timeout(PATIENCE, async { tokio::join!(write, read, rest) })
            .await
            .expect("the burst was not applied");
    }
    let took = start.elapsed();

    let after = server.handle.metrics();
    assert_eq!(after.controls - before.controls, 500, "not every message of the burst was applied");
    assert_eq!(after.rejected_controls, before.rejected_controls);
    assert_eq!(after.clients, 3, "a viewer was disconnected by the burst");
    // The pipeline is back at its pace, and the last message won for
    // everybody.
    let (ticks, start_rate) = (server.handle.metrics().ticks, Instant::now());
    futures_util::future::join_all(clients[1..].iter_mut().map(|c| c.pump(Duration::from_millis(1200)))).await;
    let rate = (server.handle.metrics().ticks - ticks) as f64 / start_rate.elapsed().as_secs_f64();
    assert!((26.0..=33.0).contains(&rate), "pipeline at {rate:.1} Hz after the burst");
    for client in &clients[1..] {
        assert_eq!(config(&client.viewer).time_scale, 0.125);
    }
    eprintln!("burst: 500 control messages applied in {took:?}, {} resync(s)", after.resyncs - before.resyncs);
    server.handle.shutdown().await;
}
