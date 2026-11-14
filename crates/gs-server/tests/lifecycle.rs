//! Start, idle, churn and stop: nothing ticks without viewers, nothing is
//! left behind by viewers that come and go, and shutdown is clean.

mod common;

use std::time::{Duration, Instant};

use common::*;
use futures_util::SinkExt;
use gs_core::protocol::PipelineConfig;
use gs_core::rig::RingRig;
use gs_core::scene::SceneKind;
use gs_server::ServerConfig;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message;

fn alive_tasks() -> usize {
    tokio::runtime::Handle::current().metrics().num_alive_tasks()
}

fn open_files() -> usize {
    std::fs::read_dir("/proc/self/fd").map_or(0, |dir| dir.count())
}

/// A WebSocket handshake written by hand, on a socket the test then
/// controls byte by byte.
async fn raw_websocket(port: u16, recv_buffer: Option<u32>) -> tokio::net::TcpStream {
    let mut stream = tcp(loopback(port), recv_buffer).await;
    let request = format!(
        "GET /ws HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut head = [0u8; 12];
    stream.read_exact(&mut head).await.unwrap();
    assert_eq!(&head, b"HTTP/1.1 101", "handshake refused");
    stream
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn no_ticking_without_clients() {
    let _serial = serial().await;
    let server = start_server(small_stage(30.0), |_| {}).await;
    // Nobody has connected yet: the pipeline is built but does not run.
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert_eq!(server.handle.metrics().ticks, 0, "ticked before the first viewer");
    // HTTP requests are not viewers.
    for _ in 0..5 {
        assert_eq!(get(server.port(), "/api/info").await.status, 200);
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(server.handle.metrics().ticks, 0, "an HTTP request started the pipeline");

    let mut viewer = Client::connect(server.port()).await;
    viewer.snapshot().await;
    viewer.pump(Duration::from_millis(700)).await;
    let seen = viewer.viewer.stats.unwrap();
    assert!(seen.tick >= 15 && seen.scene_time > 0.4, "the stream did not run: {seen:?}");
    drop(viewer);
    eventually("the viewer to be gone", || server.handle.metrics().clients == 0).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // With the last viewer gone, time stands still.
    let idle = server.handle.metrics();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let later = server.handle.metrics();
    assert_eq!(later.ticks, idle.ticks, "the pipeline ticked with no viewer");
    assert_eq!(later.scene_time, idle.scene_time, "scene time advanced with no viewer");
    assert!(idle.ticks < seen.tick + 10, "the pipeline went on long after the viewer left");

    // The next viewer finds the scene exactly where it stopped, and it does
    // not jump ahead by the idle time when it starts again.
    let mut viewer = Client::connect(server.port()).await;
    viewer.snapshot().await;
    let stats = viewer.viewer.stats.unwrap();
    assert_eq!((stats.tick, stats.scene_time), (idle.ticks, idle.scene_time));
    let first_tick =
        viewer.until("the first tick", |v, m| m.event == Event::Stats && v.stats.unwrap().tick > idle.ticks).await;
    let advanced = viewer.viewer.stats.unwrap().scene_time - idle.scene_time;
    assert!((0.0..0.2).contains(&advanced), "the scene jumped by {advanced} s after 2 s of idling ({first_tick:?})");
    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn churning_clients_leave_nothing_behind() {
    let _serial = serial().await;
    let server = start_server(small_stage(30.0), |config| config.lan = true).await;
    let (port, lan_port, tls) = (server.port(), server.lan_port(), server.tls());

    // Warm up once so that lazily created state is part of the baseline.
    let mut warm = Client::connect(port).await;
    warm.snapshot().await;
    drop(warm);
    eventually("the warm-up viewer to be gone", || server.handle.metrics().clients == 0).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (tasks_before, files_before) = (alive_tasks(), open_files());

    // Twenty clients that connect, behave in different ways, and leave in
    // different ways, for a few seconds.
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut workers = Vec::new();
    for worker in 0..20u32 {
        let tls = tls.clone();
        workers.push(tokio::spawn(async move {
            let mut sessions = 0u32;
            while Instant::now() < deadline {
                let style = (worker + sessions) % 8;
                match style {
                    // A viewer that watches for a moment and closes politely.
                    0 | 1 => {
                        let mut client = Client::connect(port).await;
                        client.pump(Duration::from_millis(40 + 20 * u64::from(worker))).await;
                        let _ = client.ws.close(None).await;
                    }
                    // One that vanishes without a close frame.
                    2 | 3 => {
                        let mut client = Client::connect(port).await;
                        client.pump(Duration::from_millis(10 * u64::from(worker))).await;
                        drop(client);
                    }
                    // One that leaves before even reading its snapshot.
                    4 => drop(Client::connect(port).await),
                    // One over TLS that sends a control message and goes.
                    5 => {
                        let mut client = Client::connect_tls(loopback(lan_port), &tls).await.unwrap();
                        client.send_json(serde_json::json!({ "type": "set_time_scale", "time_scale": 1.0 })).await;
                        client.pump(Duration::from_millis(30)).await;
                    }
                    // One that never reads and never answers.
                    6 => {
                        let stream = raw_websocket(port, Some(4096)).await;
                        tokio::time::sleep(Duration::from_millis(150)).await;
                        drop(stream);
                    }
                    // And connections that never become a WebSocket.
                    _ => {
                        let mut stream = tcp(loopback(port), None).await;
                        let _ = stream.write_all(b"GET /ws HTTP/1.1\r\nHost: x\r\n").await;
                        drop(stream);
                        drop(tcp(loopback(lan_port), None).await);
                        let _ = get(port, "/api/info").await;
                    }
                }
                sessions += 1;
            }
            sessions
        }));
    }
    let mut sessions = 0;
    for worker in workers {
        sessions += worker.await.expect("a churn worker failed");
    }
    assert!(sessions >= 200, "only {sessions} sessions in the churn");

    // Everything winds down: no session, no task, no socket left.
    eventually("every session to end", || server.handle.metrics().clients == 0).await;
    eventually("the session tasks to end", || alive_tasks() <= tasks_before).await;
    eventually("the sockets to be closed", || open_files() <= files_before).await;
    assert_eq!(server.handle.info().clients, 0);
    // The pipeline noticed too: it is idle again.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let ticks = server.handle.metrics().ticks;
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(server.handle.metrics().ticks, ticks, "the pipeline still ticks for clients that are gone");

    // And the server works as on the first day.
    let mut viewer = Client::connect(port).await;
    viewer.snapshot().await;
    let start = Instant::now();
    viewer.pump(Duration::from_millis(1500)).await;
    let rate = viewer.viewer.dynamic_rate_since(start);
    assert!((26.0..=33.0).contains(&rate), "{rate:.1} Hz after the churn");
    eprintln!("churn: {sessions} sessions, {tasks_before} tasks and {files_before} files before and after");
    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shutdown_with_connected_clients_is_clean() {
    let _serial = serial().await;
    let tasks_at_start = alive_tasks();
    let server = start_server(small_stage(30.0), |config| config.lan = true).await;
    let (port, lan_port) = (server.port(), server.lan_port());

    let mut viewers = Vec::new();
    for _ in 0..3 {
        let mut client = Client::connect(port).await;
        client.snapshot().await;
        viewers.push(Background::spawn(client));
    }
    let mut secure = Client::connect_tls(loopback(lan_port), &server.tls()).await.unwrap();
    secure.snapshot().await;
    viewers.push(Background::spawn(secure));
    // One viewer that reads nothing at all: it must not hold up the exit.
    let mut silent = raw_websocket(port, Some(4096)).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(server.handle.info().clients, 5);

    let start = Instant::now();
    server.handle.shutdown().await;
    let took = start.elapsed();
    assert!(took < Duration::from_secs(5), "shutdown took {took:?}");

    // Every viewer that listens was told why (1001, going away).
    for (i, background) in viewers.into_iter().enumerate() {
        let mut client = background.stop().await;
        assert!(client.viewer.dynamic_frames() > 3, "viewer {i} was not live before the shutdown");
        let close = match tokio::time::timeout(Duration::from_secs(5), client.next()).await {
            // The background reader may already have consumed the close frame.
            Ok(Incoming::Closed(frame)) => frame,
            Ok(Incoming::Message(_)) => client.closed().await,
            Err(_) => panic!("viewer {i} was left hanging"),
        };
        assert!(matches!(close, None | Some((1001, _))), "viewer {i} got {close:?}");
    }
    // The silent one sees its connection end.
    let mut rest = Vec::new();
    let end = tokio::time::timeout(Duration::from_secs(5), silent.read_to_end(&mut rest)).await;
    assert!(end.is_ok(), "the connection of the silent viewer was left open");

    // Both ports are closed, the pipeline thread is gone, and the handle
    // says so instead of hanging or panicking.
    assert!(!accepts(loopback(port)).await, "the HTTP port still accepts connections");
    assert!(!accepts(loopback(lan_port)).await, "the LAN port still accepts connections");
    let metrics = server.handle.metrics();
    assert_eq!(metrics.clients, 0);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(server.handle.metrics().ticks, metrics.ticks);
    assert!(matches!(server.handle.set_lan(true).await, Err(gs_server::Error::Closed)));
    assert!(!server.handle.info().lan.enabled);
    server.handle.shutdown().await;
    eventually("the server tasks to end", || alive_tasks() <= tasks_at_start).await;
    eprintln!("shutdown with 5 viewers took {took:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn close_frame_on_shutdown_is_1001() {
    let _serial = serial().await;
    let server = start_server(small_stage(30.0), |_| {}).await;
    let mut viewer = Client::connect(server.port()).await;
    viewer.snapshot().await;
    let handle = server.handle.clone();
    let stopping = tokio::spawn(async move { handle.shutdown().await });
    let close = viewer.closed().await;
    assert_eq!(close.map(|(code, _)| code), Some(1001));
    stopping.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropping_the_handle_stops_the_server() {
    let _serial = serial().await;
    let config =
        ServerConfig { pipeline: small_stage(30.0), port: 0, lan_port: 0, lan: true, ..ServerConfig::default() };
    let handle = gs_server::start(config).await.unwrap();
    let (port, lan_port) = (handle.http_addr().port(), handle.lan_addr().unwrap().port());
    let mut viewer = Client::connect(port).await;
    viewer.snapshot().await;
    let clone = handle.clone();
    drop(handle);
    // A clone keeps it alive.
    viewer.pump(Duration::from_millis(300)).await;
    assert!(accepts(loopback(port)).await);
    drop(clone);
    assert_eq!(viewer.closed().await.map(|(code, _)| code), Some(1001));
    let start = Instant::now();
    while accepts(loopback(port)).await || accepts(loopback(lan_port)).await {
        assert!(start.elapsed() < Duration::from_secs(10), "the listeners outlived the handle");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keepalive_drops_dead_clients_and_keeps_quiet_ones() {
    let _serial = serial().await;
    // A paused pipeline sends only small Stats: without pings, nothing
    // would reveal a viewer that silently went away.
    let mut pipeline = small_stage(10.0);
    pipeline.paused = true;
    let server = start_server(pipeline, |config| {
        config.ping_interval = Duration::from_millis(200);
        config.idle_timeout = Duration::from_millis(800);
    })
    .await;
    let port = server.port();

    // A normal viewer answers pings (its library does) and stays.
    let mut quiet = Client::connect(port).await;
    quiet.snapshot().await;
    let quiet = Background::spawn(quiet);

    // This one reads everything but never answers a ping.
    let mut mute = raw_websocket(port, None).await;
    let start = Instant::now();
    let mut sink = vec![0u8; 16 * 1024];
    let ended = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match mute.read(&mut sink).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    })
    .await;
    let after = start.elapsed();
    assert!(ended.is_ok(), "a viewer that answers no ping was kept for 10 s");
    assert!(after > Duration::from_millis(700), "dropped after only {after:?}: before the idle timeout");
    eventually("the mute viewer to be counted out", || server.handle.metrics().clients == 1).await;

    // The quiet one is still there well past the timeout, and still served.
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(server.handle.metrics().clients, 1);
    let quiet = quiet.stop().await;
    let stats = quiet.viewer.log.iter().filter(|m| m.event == Event::Stats).count();
    assert!(stats > 15, "the quiet viewer received only {stats} Stats");
    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_client_that_never_reads_is_dropped_after_the_send_timeout() {
    let _serial = serial().await;
    // Large live layers, a client with a tiny window that reads nothing:
    // its socket blocks within a frame or two.
    let pipeline = PipelineConfig {
        rig: RingRig { count: 4, width: 160, height: 120, ..RingRig::default() },
        target_hz: 20.0,
        background_subtraction: false,
        ..PipelineConfig::for_scene(SceneKind::Stage)
    };
    let server = start_server(pipeline, |config| config.send_timeout = Duration::from_millis(1200)).await;
    let mut healthy = Client::connect(server.port()).await;
    healthy.snapshot().await;
    let healthy = Background::spawn(healthy);

    let start = Instant::now();
    let mut stuck = raw_websocket(server.port(), Some(4096)).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(server.handle.metrics().clients, 2, "dropped before the timeout");
    eventually("the stuck viewer to be dropped", || server.handle.metrics().clients == 1).await;
    let after = start.elapsed();
    assert!(after >= Duration::from_millis(1200) && after < Duration::from_secs(6), "dropped after {after:?}");
    // Its connection is really closed, not just forgotten.
    let mut rest = Vec::new();
    assert!(tokio::time::timeout(Duration::from_secs(10), stuck.read_to_end(&mut rest)).await.is_ok());

    let healthy = healthy.stop().await;
    let rate = healthy.viewer.dynamic_rate_since(start);
    assert!((17.0..=22.0).contains(&rate), "the healthy viewer got {rate:.1} Hz meanwhile");
    // A viewer may also send while it is not being read; keep `SinkExt` honest.
    let mut late = Client::connect(server.port()).await;
    late.ws.send(Message::Ping(Vec::new().into())).await.unwrap();
    late.snapshot().await;
    server.handle.shutdown().await;
}
