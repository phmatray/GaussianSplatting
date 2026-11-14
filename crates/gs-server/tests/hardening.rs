//! Limits that protect the server from its network: the LAN listener
//! holds a bounded number of connections, the loopback listener only
//! answers requests addressed to the machine itself, and a download nobody
//! reads is abandoned.

mod common;

use std::time::Duration;

use common::{eventually, get, loopback, raw_http, small_stage, start_server, tcp, tls_stream, Client};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Whether a new TLS connection to the LAN listener gets an HTTP answer.
async fn lan_answers(server: &common::TestServer) -> bool {
    let addr = loopback(server.lan_port());
    let Ok(mut stream) = tls_stream(addr, &server.tls(), &[b"http/1.1"]).await else { return false };
    let request = format!("GET /api/info HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).await.is_err() {
        return false;
    }
    let mut answer = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut answer)).await;
    answer.starts_with(b"HTTP/1.1 200")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_lan_listener_holds_a_bounded_number_of_connections() {
    let server = start_server(small_stage(20.0), |config| {
        config.lan = true;
        config.max_lan_connections = 6;
        // The idle connections of this test must outlive it.
        config.request_timeout = Duration::from_secs(60);
    })
    .await;
    let addr = loopback(server.lan_port());
    assert!(lan_answers(&server).await);
    // Its slot is free again once the server has seen the connection end.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Five viewers and one connection that says nothing: the listener is full.
    let mut viewers = Vec::new();
    for _ in 0..5 {
        let mut viewer = Client::connect_tls(addr, &server.tls()).await.expect("a viewer within the limit was refused");
        viewer.snapshot().await;
        viewers.push(viewer);
    }
    let idle = tokio::net::TcpStream::connect(addr).await.unwrap();
    eventually("the listener to be full", || server.handle.metrics().clients == 5).await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    // The newcomer is turned away at once (no handshake, no waiting)...
    for _ in 0..20 {
        let started = std::time::Instant::now();
        assert!(!lan_answers(&server).await, "a connection beyond the limit was served");
        assert!(started.elapsed() < Duration::from_secs(5), "the refusal took {:?}", started.elapsed());
    }
    // ...the viewers in place are not disturbed...
    for viewer in &mut viewers {
        viewer.until("a live layer", |_, m| m.is_dynamic()).await;
    }
    // ...and the machine's own listener has nothing to do with it.
    assert_eq!(get(server.port(), "/api/info").await.status, 200);
    let mut local = Client::connect(server.port()).await;
    local.snapshot().await;

    // A slot is free again as soon as its connection ends, whatever it was.
    drop(idle);
    eventually_async("a slot after the idle connection left", || lan_answers(&server)).await;
    let viewer = viewers.pop().unwrap();
    drop(viewer);
    eventually("the viewer to be gone", || server.handle.metrics().clients == 5).await;
    let mut newcomer = None;
    for _ in 0..50 {
        if let Ok(viewer) = Client::connect_tls(addr, &server.tls()).await {
            newcomer = Some(viewer);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    newcomer.expect("no slot after a viewer left").snapshot().await;
    server.handle.shutdown().await;
}

/// Polls an asynchronous condition until it holds.
async fn eventually_async<F, Fut>(what: &str, mut condition: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let start = std::time::Instant::now();
    while !condition().await {
        assert!(start.elapsed() < common::PATIENCE, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_loopback_listener_answers_to_its_own_name_only() {
    let root = tempfile::tempdir().unwrap();
    let recordings = root.path().join("recordings");
    std::fs::create_dir_all(&recordings).unwrap();
    std::fs::write(recordings.join("demo.gsrec"), b"a recording").unwrap();
    let server = start_server(small_stage(20.0), |config| {
        config.lan = true;
        config.recordings_dir = Some(recordings);
    })
    .await;
    let port = server.port();
    let token = server.handle.pairing_token();
    let request = |target: &str, host: &str| format!("GET {target} HTTP/1.1\r\n{host}Connection: close\r\n\r\n");

    // A page that points its own name at 127.0.0.1 (DNS rebinding) is
    // "same origin" with this server for the browser; its requests carry
    // its name, and get nothing: no page, no state, no pairing token, no
    // recording.
    let paths = ["/", "/index.html", "/api/info", "/api/pair", "/api/pair.svg", "/api/recordings", "/api/record",
        "/recordings/demo.gsrec", "/api/share/demo.zip", "/api/captures", "/api/qr.svg"];
    for host in [
        format!("evil.example:{port}"),
        "evil.example".to_string(),
        format!("127.0.0.1.evil.example:{port}"),
        format!("localhost.evil.example:{port}"),
        format!("192.0.2.2:{port}"),
        format!("127.0.0.1:{port}@evil.example"),
        "0.0.0.0".to_string(),
        "".to_string(),
    ] {
        for path in paths {
            let (status, text) = raw_http(port, &request(path, &format!("Host: {host}\r\n"))).await;
            assert!(matches!(status, 400 | 403), "{path} with Host {host:?}: {status}");
            assert!(!text.contains(&token) && !text.contains("a recording") && !text.contains("splat-live"), "{text}");
        }
    }
    // Two names: nobody can tell which one a browser meant.
    let twice = format!("Host: 127.0.0.1:{port}\r\nHost: evil.example\r\n");
    let (status, _) = raw_http(port, &request("/api/info", &twice)).await;
    assert!(matches!(status, 400 | 403), "{status}");

    // The names of the machine itself, with or without the port.
    for host in [format!("127.0.0.1:{port}"), format!("localhost:{port}"), "localhost".into(), format!("[::1]:{port}"), format!("LOCALHOST:{port}")] {
        for path in ["/api/info", "/recordings/demo.gsrec", "/api/recordings"] {
            let (status, _) = raw_http(port, &request(path, &format!("Host: {host}\r\n"))).await;
            assert_eq!(status, 200, "{path} with Host {host}");
        }
    }
    // No Host at all: not a browser (HTTP/1.0 tools), nothing to protect.
    let (status, text) = raw_http(port, "GET /api/info HTTP/1.0\r\n\r\n").await;
    assert_eq!(status, 200, "{text}");

    // The LAN listener is reached under whatever name the network gives
    // the machine.
    let addr = loopback(server.lan_port());
    for host in ["splat.local", "192.168.1.20:7879", "evil.example"] {
        let io: common::BoxIo = Box::new(tls_stream(addr, &server.tls(), &[b"http/1.1"]).await.unwrap());
        let answer = common::request_on(io, "GET", host, "/api/info", &[], &[]).await;
        assert_eq!(answer.status, 200, "Host {host} on the LAN listener");
    }
    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn strangers_on_the_capture_endpoint_are_few() {
    let server = start_server(small_stage(20.0), |config| config.lan = true).await;
    let addr = loopback(server.lan_port());
    let open = || async {
        let io: common::BoxIo = Box::new(tls_stream(addr, &server.tls(), &[b"http/1.1"]).await.unwrap());
        common::handshake(io, &format!("wss://{addr}/ingest"), &[]).await
    };
    // Until it has said a valid hello a connection may send one message of
    // the size of a frame: only so many are let in at a time.
    let mut silent = Vec::new();
    for _ in 0..8 {
        silent.push(open().await.expect("a connection within the limit was refused"));
    }
    match open().await {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => assert_eq!(response.status(), 503),
        Err(other) => panic!("{other}"),
        Ok(_) => panic!("a ninth stranger was let in"),
    }
    // Viewers are not concerned.
    let mut viewer = Client::connect_tls(addr, &server.tls()).await.unwrap();
    viewer.snapshot().await;

    // A stranger that leaves makes room; one that says a valid hello is no
    // longer a stranger and makes room too.
    silent.pop();
    let mut welcomed = None;
    for _ in 0..100 {
        if let Ok(ws) = open().await {
            welcomed = Some(ws);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut phone = welcomed.expect("no room after a stranger left");
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let hello = serde_json::json!({
        "type": "hello", "protocol": 1, "token": server.handle.pairing_token(), "device": "iPhone17,2", "app": "0.1.0",
        "mode": "room", "name": "Salon",
    });
    phone.send(Message::Text(hello.to_string().into())).await.unwrap();
    let Some(Ok(Message::Text(welcome))) = phone.next().await else { panic!("no welcome") };
    assert!(welcome.as_str().contains("welcome"), "{welcome}");
    let mut another = None;
    for _ in 0..100 {
        if let Ok(ws) = open().await {
            another = Some(ws);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(another.is_some(), "a welcomed phone still counted as a stranger");
    server.handle.shutdown().await;
}

/// How many descriptors of this process are open on `path` (Linux; `None`
/// elsewhere).
fn open_descriptors(path: &std::path::Path) -> Option<usize> {
    let entries = std::fs::read_dir("/proc/self/fd").ok()?;
    Some(entries.flatten().filter(|entry| std::fs::read_link(entry.path()).is_ok_and(|target| target == path)).count())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_download_nobody_reads_is_abandoned() {
    let root = tempfile::tempdir().unwrap();
    let recordings = root.path().join("recordings");
    std::fs::create_dir_all(&recordings).unwrap();
    // Far more than the kernel buffers for a client that reads nothing.
    let big = recordings.join("long.gsrec");
    std::fs::File::create(&big).unwrap().set_len(64 << 20).unwrap();
    let big = big.canonicalize().unwrap();
    // A share archive needs a web build to put next to the recording.
    let dist = root.path().join("dist");
    std::fs::create_dir_all(&dist).unwrap();
    std::fs::write(dist.join("index.html"), "<!doctype html><title>viewer</title>").unwrap();
    let dir = recordings.clone();
    let server = start_server(small_stage(20.0), move |config| {
        config.recordings_dir = Some(dir);
        config.dist = Some(dist);
        config.send_timeout = Duration::from_millis(400);
    })
    .await;
    let port = server.port();

    // Readers that read: served whole, however long the file.
    let whole = get(port, "/recordings/long.gsrec").await;
    assert_eq!((whole.status, whole.body.len()), (200, 64 << 20));

    // Readers that do not: the recording itself, and an archive of it.
    let mut stalled = Vec::new();
    for path in ["/recordings/long.gsrec", "/recordings/long.gsrec", "/api/share/long.zip"] {
        let mut stream = tcp(loopback(port), Some(4096)).await;
        let request = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        stalled.push(stream);
    }
    if open_descriptors(&big).is_some() {
        eventually("the downloads to start", || open_descriptors(&big).is_some_and(|open| open >= 2)).await;
        // Nothing is read: the server lets go of the file by itself.
        eventually("the stalled downloads to be abandoned", || open_descriptors(&big) == Some(0)).await;
    } else {
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    // What such a client finds when it finally reads: a download cut
    // short and a closed connection, not a wait without end.
    for mut stream in stalled {
        let mut received = Vec::new();
        let ended = tokio::time::timeout(Duration::from_secs(20), stream.read_to_end(&mut received)).await;
        assert!(ended.is_ok(), "the connection of a stalled download stayed open");
        assert!(received.starts_with(b"HTTP/1.1 200"), "{:?}", String::from_utf8_lossy(&received[..received.len().min(80)]));
        assert!(received.len() < 64 << 20, "the whole file was delivered to a client that did not read");
    }
    // And a reader that takes its time, but does read, still gets it all.
    let mut slow = tcp(loopback(port), None).await;
    let request = format!("GET /recordings/long.gsrec HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\nRange: bytes=0-33554431\r\n\r\n");
    slow.write_all(request.as_bytes()).await.unwrap();
    let mut received = Vec::new();
    let mut block = vec![0u8; 1 << 20];
    loop {
        // A pause before every read, shorter than the limit; together they
        // last several times the limit.
        tokio::time::sleep(Duration::from_millis(60)).await;
        match slow.read(&mut block).await.unwrap() {
            0 => break,
            read => received.extend_from_slice(&block[..read]),
        }
    }
    let head = received.windows(4).position(|window| window == b"\r\n\r\n").expect("no answer") + 4;
    assert!(received.starts_with(b"HTTP/1.1 206"));
    assert_eq!(received.len() - head, 32 << 20, "a slow reader lost part of its range");
    server.handle.shutdown().await;
}
