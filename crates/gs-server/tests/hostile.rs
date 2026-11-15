//! Clients that misbehave on purpose: connections that ask for nothing,
//! requests dressed up to pass the origin check, and a storm of LAN
//! switches while viewers come and go over TLS.

mod common;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use common::*;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

fn alive_tasks() -> usize {
    tokio::runtime::Handle::current().metrics().num_alive_tasks()
}

fn open_files() -> usize {
    std::fs::read_dir("/proc/self/fd").map_or(0, |dir| dir.count())
}

/// Reads until the server ends the connection (cleanly or not) and returns
/// what it sent and when it ended; `None` if the connection is still open
/// after `limit`.
async fn until_closed<S: AsyncRead + Unpin>(stream: &mut S, limit: Duration) -> Option<(String, Instant)> {
    let mut answer = Vec::new();
    // An abrupt end (no TLS close_notify, a reset) is an end too.
    let _ended = tokio::time::timeout(limit, stream.read_to_end(&mut answer)).await.ok()?;
    Some((String::from_utf8_lossy(&answer).into_owned(), Instant::now()))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn connections_that_ask_for_nothing_are_closed() {
    let _serial = serial().await;
    const TIMEOUT: Duration = Duration::from_millis(700);
    // A paused stage: the viewer below is as quiet as a viewer gets.
    let mut pipeline = small_stage(10.0);
    pipeline.paused = true;
    let server = start_server(pipeline, |config| {
        config.lan = true;
        config.request_timeout = TIMEOUT;
    })
    .await;
    let (port, lan_port, tls) = (server.port(), server.lan_port(), server.tls());
    let files_before = open_files();

    // A real viewer first: an upgraded connection is not an idle one.
    let mut viewer = Client::connect(port).await;
    viewer.snapshot().await;
    let viewer = Background::spawn(viewer);

    // 1. Connected, says nothing.
    let start = Instant::now();
    let mut silent = tcp(loopback(port), None).await;
    // 2. Slowloris: a request head that never ends, one byte at a time.
    let trickle = tcp(loopback(port), None).await;
    let (mut trickle, mut trickle_out) = trickle.into_split();
    trickle_out
        .write_all(format!("GET /api/info HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-Slow: ").as_bytes())
        .await
        .unwrap();
    let dripping = tokio::spawn(async move {
        while trickle_out.write_all(b"a").await.is_ok() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });
    // 3. On the TLS port without a TLS handshake.
    let mut no_handshake = tcp(loopback(lan_port), None).await;
    // 4. TLS handshake done, then nothing.
    let mut tls_silent = tls_stream(loopback(lan_port), &tls, &[b"h2", b"http/1.1"]).await.unwrap();
    // 5. One request answered, then the kept-alive connection stays idle.
    let mut idle = tcp(loopback(port), None).await;
    idle.write_all(format!("GET /api/info HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").as_bytes()).await.unwrap();
    // 6. A POST that announces a body and sends only the start of it.
    let mut slow_body = tcp(loopback(port), None).await;
    slow_body
        .write_all(
            format!(
                "POST /api/lan HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\n\
                 Content-Length: 17\r\n\r\n{{\"en"
            )
            .as_bytes(),
        )
        .await
        .unwrap();

    let limit = Duration::from_secs(8);
    let outcomes = tokio::join!(
        until_closed(&mut silent, limit),
        until_closed(&mut trickle, limit),
        until_closed(&mut no_handshake, limit),
        until_closed(&mut tls_silent, limit),
        until_closed(&mut idle, limit),
        until_closed(&mut slow_body, limit),
    );
    // This side of the connections goes too (the sockets of the test count
    // as open files of the process, like those of the server).
    dripping.abort();
    let _ = dripping.await;
    drop((silent, trickle, no_handshake, tls_silent, idle, slow_body));
    let (silent, trickle, no_handshake, tls_silent, idle, slow_body) = outcomes;
    let cases = [
        ("a silent connection", &silent),
        ("a trickled request head", &trickle),
        ("a connection without TLS handshake", &no_handshake),
        ("a silent TLS connection", &tls_silent),
        ("an idle kept-alive connection", &idle),
        ("a POST without its body", &slow_body),
    ];
    for (what, outcome) in cases {
        let Some((_, closed_at)) = outcome else { panic!("{what} was still open after {limit:?}") };
        // Closed because of the timeout, not refused outright: none of them
        // existed before `start`.
        let after = closed_at.duration_since(start);
        assert!(after >= TIMEOUT, "{what} was closed after only {after:?}");
        eprintln!("{what}: closed after {after:?}");
    }
    // The kept-alive one got its answer before it went idle, and the POST
    // was told why it failed; LAN was not touched by half a request.
    assert!(idle.unwrap().0.starts_with("HTTP/1.1 200"));
    assert!(slow_body.unwrap().0.starts_with("HTTP/1.1 408"), "a POST without its body must end with 408");
    assert!(server.handle.info().lan.enabled);

    // The viewer was left alone all along, and the server answers as before.
    tokio::time::sleep(2 * TIMEOUT).await;
    assert_eq!(server.handle.metrics().clients, 1, "the timeout hit an upgraded connection");
    let viewer = viewer.stop().await;
    let stats = viewer.viewer.log.iter().filter(|m| m.event == Event::Stats).count();
    assert!(stats > 20, "the viewer received only {stats} Stats meanwhile");
    assert_eq!(get(port, "/api/info").await.status, 200);
    assert_eq!(https(loopback(lan_port), &tls, "GET", "/api/info", &[], &[]).await.status, 200);
    drop(viewer);
    eventually("every socket to be closed", || open_files() <= files_before).await;
    server.handle.shutdown().await;
}

/// Sends `request` byte for byte and returns the status code of the answer
/// without waiting for the connection to end (a refused upgrade leaves it
/// open for the next request).
async fn status_of(port: u16, request: &str) -> u16 {
    let mut stream = tcp(loopback(port), None).await;
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut head = [0u8; 12];
    tokio::time::timeout(PATIENCE, stream.read_exact(&mut head)).await.expect("no answer").expect("no answer");
    std::str::from_utf8(&head[9..]).ok().and_then(|code| code.parse().ok()).unwrap_or(0)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn origin_and_host_cannot_be_dressed_up() {
    let server = start_server(small_stage(30.0), |_| {}).await;
    let port = server.port();
    let own = format!("127.0.0.1:{port}");
    let post = |host: &str, origins: &[&str]| {
        let origins: String = origins.iter().map(|origin| format!("Origin: {origin}\r\n")).collect();
        format!(
            "POST /api/lan HTTP/1.1\r\nHost: {host}\r\n{origins}Content-Type: application/json\r\n\
             Content-Length: 16\r\nConnection: close\r\n\r\n{{\"enabled\":true}}"
        )
    };
    let upgrade = |host: &str, origins: &[&str]| {
        let origins: String = origins.iter().map(|origin| format!("Origin: {origin}\r\n")).collect();
        format!(
            "GET /ws HTTP/1.1\r\nHost: {host}\r\n{origins}Upgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )
    };
    let good = format!("http://{own}");
    // A host that starts like the loopback address but is another one, with
    // the origin a page served from "there" would have.
    let userinfo = format!("{own}@evil.example");
    let userinfo_origin = format!("http://{userinfo}");
    let cases: [(&str, &str, Vec<&str>); 5] = [
        // Two Origin headers: whichever one is "the" origin, it is not ours.
        ("two origins, ours first", &own, vec![&good, "http://evil.example"]),
        ("two origins, ours last", &own, vec!["http://evil.example", &good]),
        ("the same origin twice", &own, vec!["tauri://localhost", "tauri://localhost"]),
        ("userinfo in the host", &userinfo, vec![&userinfo_origin]),
        ("a name instead of a port", "localhost:evil.example", vec!["http://localhost:evil.example"]),
    ];
    for (what, host, origins) in cases {
        assert_eq!(status_of(port, &post(host, &origins)).await, 403, "POST /api/lan with {what}");
        assert_eq!(status_of(port, &upgrade(host, &origins)).await, 403, "WebSocket with {what}");
    }
    assert!(!server.handle.info().lan.enabled, "one of the dressed-up requests switched LAN on");
    assert_eq!(server.handle.metrics().clients, 0);
    // The honest versions of the same requests pass.
    assert_eq!(status_of(port, &post(&own, &[&good])).await, 200);
    assert!(server.handle.info().lan.enabled);
    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lan_switch_storm_with_viewers_over_tls() {
    let _serial = serial().await;
    let server = start_server(small_stage(30.0), |config| config.lan = true).await;
    let (port, lan_port, tls) = (server.port(), server.lan_port(), server.tls());
    let fingerprint = server.handle.info().lan.fingerprint;

    let mut warm = Client::connect(port).await;
    warm.snapshot().await;
    drop(warm);
    eventually("the warm-up viewer to be gone", || server.handle.metrics().clients == 0).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let (tasks_before, files_before) = (alive_tasks(), open_files());

    // A viewer on the loopback listener: none of this is its business.
    let mut local = Client::connect(port).await;
    local.snapshot().await;
    let local = Background::spawn(local);
    let (storm_start, ticks_before) = (Instant::now(), server.handle.metrics().ticks);

    // Viewers over TLS that reconnect as soon as they are thrown out.
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (sessions, clean, abrupt) =
        (Arc::new(AtomicU32::new(0)), Arc::new(AtomicU32::new(0)), Arc::new(AtomicU32::new(0)));
    let mut viewers = Vec::new();
    for _ in 0..5 {
        let (tls, stop) = (tls.clone(), stop.clone());
        let (sessions, clean, abrupt) = (sessions.clone(), clean.clone(), abrupt.clone());
        viewers.push(tokio::spawn(async move {
            while !stop.load(Ordering::Relaxed) {
                // Refused while the listener is closed, or cut during the
                // handshake when it closes right then.
                let Ok(mut client) = Client::connect_tls(loopback(lan_port), &tls).await else {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                    continue;
                };
                sessions.fetch_add(1, Ordering::Relaxed);
                loop {
                    match tokio::time::timeout(Duration::from_secs(20), client.next()).await {
                        Ok(Incoming::Message(_)) => {}
                        Ok(Incoming::Closed(Some((1001, _)))) => {
                            clean.fetch_add(1, Ordering::Relaxed);
                            break;
                        }
                        Ok(Incoming::Closed(None)) => {
                            abrupt.fetch_add(1, Ordering::Relaxed);
                            break;
                        }
                        Ok(Incoming::Closed(other)) => panic!("a viewer was closed with {other:?}"),
                        Err(_) => panic!("a viewer over TLS was left hanging"),
                    }
                }
            }
        }));
    }

    // Three switchers that contradict each other as fast as they can.
    let mut switchers = Vec::new();
    for switcher in 0..3u32 {
        let fingerprint = fingerprint.clone();
        switchers.push(tokio::spawn(async move {
            for round in 0..60u32 {
                let enabled = (round + switcher) % 2 == 0;
                let body = format!("{{\"enabled\":{enabled}}}");
                let response =
                    http(port, "POST", "/api/lan", &[("content-type", "application/json")], body.as_bytes()).await;
                assert_eq!(response.status, 200, "{}", response.text());
                let lan = &response.json()["lan"];
                // Another switcher may have had the last word, but an
                // answer is always a coherent state, and an open listener
                // is always the same one: same port, same certificate.
                if lan["enabled"] == true {
                    assert_eq!(lan["fingerprint"], fingerprint.as_str());
                    for url in lan["urls"].as_array().unwrap() {
                        assert!(url.as_str().unwrap().ends_with(&format!(":{lan_port}")), "the LAN port moved: {url}");
                    }
                } else {
                    assert_eq!(lan["urls"].as_array().unwrap().len(), 0);
                    assert_eq!(lan["fingerprint"], "");
                }
                // A short breath, and now and then a longer one, so that
                // viewers do get in between two switches.
                let pause = if round % 10 == 9 { 150 } else { 4 };
                tokio::time::sleep(Duration::from_millis(pause)).await;
            }
        }));
    }
    for switcher in switchers {
        switcher.await.expect("a switcher failed");
    }
    let (storm_end, ticks_during) = (Instant::now(), server.handle.metrics().ticks - ticks_before);
    let storm = storm_end.duration_since(storm_start);

    // Settle on "on": viewers come back; then "off": every one of them is
    // told to go, and the port is closed.
    assert!(server.handle.set_lan(true).await.unwrap().lan.enabled);
    eventually("the TLS viewers to be back", || server.handle.metrics().clients == 6).await;
    stop.store(true, Ordering::Relaxed);
    assert!(!server.handle.set_lan(false).await.unwrap().lan.enabled);
    for viewer in viewers {
        viewer.await.expect("a TLS viewer failed");
    }
    assert!(!accepts(loopback(lan_port)).await, "the LAN port is still open");
    eventually("only the local viewer to be left", || server.handle.metrics().clients == 1).await;
    let (sessions, clean, abrupt) =
        (sessions.load(Ordering::Relaxed), clean.load(Ordering::Relaxed), abrupt.load(Ordering::Relaxed));
    assert!(sessions >= 5, "only {sessions} TLS sessions during the storm");
    assert_eq!(clean + abrupt, sessions);
    // A viewer that was streaming is told (1001); only one caught in its
    // very first instants may see the connection drop instead.
    assert!(clean >= 5 && clean * 2 >= sessions, "{clean} clean closes out of {sessions} sessions");

    // The local viewer never noticed: it received the live layer of every
    // tick of the storm (give or take the ones at its edges), one State,
    // and was never resynchronised.
    let local = local.stop().await;
    let received =
        local.viewer.log.iter().filter(|m| m.is_dynamic() && m.at >= storm_start && m.at <= storm_end).count() as u64;
    assert!(ticks_during >= 15, "the pipeline only ticked {ticks_during} times during the storm");
    assert!(received + 3 >= ticks_during, "the local viewer got {received} of {ticks_during} live layers");
    assert_eq!(local.viewer.log.iter().filter(|m| m.event == Event::State).count(), 1);
    assert_eq!(server.handle.metrics().resyncs, 0);
    drop(local);

    eventually("every session to end", || server.handle.metrics().clients == 0).await;
    // Fewer than before the storm, when the LAN listener was open.
    eventually("the tasks to end", || alive_tasks() < tasks_before).await;
    eventually("the sockets to be closed", || open_files() < files_before).await;
    eprintln!("storm: 180 switches in {storm:?}, {sessions} TLS sessions ({clean} closed with 1001, {abrupt} cut)");
    server.handle.shutdown().await;
}
