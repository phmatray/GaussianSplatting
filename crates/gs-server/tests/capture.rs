//! Real capture: pairing, the `/ingest` stream of a phone and the import
//! of `.gscap` files, against a running server with real sockets. The
//! phone is the simulated one of the gs-core tests (`common/phone.rs`).

mod common;
#[path = "common/phone.rs"]
mod phone;

use std::sync::OnceLock;
use std::time::Duration;

use common::{eventually, get, http, https, loopback, raw_http, Client, Event};
use futures_util::{SinkExt, StreamExt};
use gs_core::codec::LayerMode;
use gs_core::ingest::file::split_gscap;
use gs_core::ingest::ServerMessage;
use gs_core::pipeline::{LAYER_DYNAMIC, LAYER_STATIC};
use gs_core::protocol::CaptureMode;
use gs_core::record;
use phone::{hello_json, lan, pairing, start_capture_server, Dirs, Footage, PhoneClient, Said};
use serde_json::json;
use tokio_tungstenite::tungstenite::Message;

/// A sweep through the bedroom, filmed once for all the tests.
fn bedroom() -> &'static Footage {
    static FOOTAGE: OnceLock<Footage> = OnceLock::new();
    FOOTAGE.get_or_init(|| Footage::bedroom(24, 0))
}

fn live_dancer() -> &'static Footage {
    static FOOTAGE: OnceLock<Footage> = OnceLock::new();
    FOOTAGE.get_or_init(|| Footage::dancer("live", 12))
}

fn floor_json(y: f32) -> String {
    json!({ "type": "floor", "y": y }).to_string()
}

fn fixture(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ios/SplatCapture/Fixtures").join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn capture_info(viewer: &common::Viewer) -> Option<gs_core::protocol::CaptureInfo> {
    viewer.state.as_ref().and_then(|state| state.capture.clone())
}

fn capture_stats(viewer: &common::Viewer) -> Option<gs_core::protocol::CaptureStats> {
    viewer.stats.and_then(|stats| stats.capture)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pairing_is_given_at_the_machine_only() {
    // LAN off: nothing to pair with.
    let (server, _dirs) = start_capture_server(|config| config.lan = false).await;
    let port = server.port();
    for path in ["/api/pair", "/api/pair.svg"] {
        let answer = get(port, path).await;
        assert_eq!((answer.status, answer.json()["code"].as_str()), (404, Some("lan_off")), "{path}");
    }
    assert_eq!(server.handle.pairing_uri(), None);

    server.handle.set_lan(true).await.unwrap();
    let info = server.handle.info();
    let token = server.handle.pairing_token();
    assert!(token.len() == 32 && token.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')), "{token}");
    let answer = get(port, "/api/pair").await;
    if info.lan.urls.is_empty() {
        // A machine without a network address has nothing to offer a phone.
        assert_eq!((answer.status, answer.json()["code"].as_str()), (404, Some("no_address")));
    } else {
        assert_eq!(answer.status, 200, "{}", answer.text());
        assert_eq!(answer.header("cache-control"), Some("no-store"));
        let body = answer.json();
        let uris: Vec<&str> = body["uris"].as_array().unwrap().iter().map(|u| u.as_str().unwrap()).collect();
        assert_eq!(uris.len(), info.lan.urls.len(), "one URI per LAN address");
        assert_eq!(body["uri"].as_str(), Some(uris[0]));
        assert_eq!(server.handle.pairing_uri().as_deref(), Some(uris[0]));
        for (uri, url) in uris.iter().zip(&info.lan.urls) {
            let pairing = gs_core::ingest::parse_pairing_uri(uri).expect("the URI does not parse");
            assert_eq!(format!("https://{}:{}", pairing.host, pairing.port), *url);
            assert_eq!(pairing.port, server.lan_port());
            assert_eq!(pairing.token, token);
            // The SHA-256 of the certificate, as /api/info shows it.
            assert_eq!(pairing.fingerprint, info.lan.fingerprint.replace(':', "").to_lowercase());
        }
        let svg = get(port, "/api/pair.svg").await;
        assert_eq!((svg.status, svg.header("content-type")), (200, Some("image/svg+xml")));
        assert_eq!(svg.header("cache-control"), Some("no-store"));
        assert!(svg.text().contains("<svg"));
        assert_ne!(svg.body, get(port, "/api/qr.svg").await.body, "the pairing code is not the viewer's link");
        assert_eq!(get(port, &format!("/api/pair?i={}", uris.len())).await.status, 404);
        assert_eq!(get(port, &format!("/api/pair.svg?i={}", uris.len())).await.status, 404);
        // The Tauri window may ask; a page of another origin may not.
        assert_eq!(http(port, "GET", "/api/pair", &[("origin", "tauri://localhost")], &[]).await.status, 200);
        for path in ["/api/pair", "/api/pair.svg"] {
            assert_eq!(http(port, "GET", path, &[("origin", "https://evil.example")], &[]).await.status, 403);
        }
    }

    // Never over the network: the token is what lets a phone in.
    for path in ["/api/pair", "/api/pair.svg", "/api/pair?i=0"] {
        let answer = https(lan(&server), &server.tls(), "GET", path, &[], &[]).await;
        assert_eq!(answer.status, 403, "{path} on the LAN listener");
        assert!(!answer.text().contains(&token));
    }
    // Nor to a page that points its own name at this machine.
    let rebound = format!("GET /api/pair HTTP/1.1\r\nHost: evil.example:{port}\r\nConnection: close\r\n\r\n");
    let (status, text) = raw_http(port, &rebound).await;
    assert_eq!(status, 403);
    assert!(!text.contains(&token));
    assert!(!get(port, "/api/info").await.text().contains(&token), "the token is not part of /api/info");

    // Another run, another token.
    let (other, _other_dirs) = start_capture_server(|_| {}).await;
    assert_ne!(other.handle.pairing_token(), token);
    other.handle.shutdown().await;
    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_phone_streams_a_room_and_viewers_follow() {
    let footage = bedroom();
    if std::env::var_os("GS_TEST_LOG").is_some() {
        gs_server::log_to_stderr(true);
    }
    let (server, dirs) = start_capture_server(|_| {}).await;
    let mut viewer = Client::connect(server.port()).await;
    viewer.snapshot().await;
    assert_eq!(viewer.viewer.scene(), "stage");

    let mut phone = PhoneClient::open(&server, "room", "Chambre").await;
    assert_eq!(phone.max_inflight, 4);
    // Every viewer changes source: the capture, empty for now.
    viewer.until("the capture session", |v, _| v.scene() == "capture").await;
    let info = capture_info(&viewer.viewer).expect("State without capture info");
    assert_eq!((info.mode, info.name.as_str(), info.device.as_str()), (CaptureMode::Room, "Chambre", "iPhone17,2"));
    assert!(!info.imported);
    viewer.until("the empty layers", |_, m| m.is_dynamic()).await;
    assert!(viewer.viewer.layers[0].is_empty() && viewer.viewer.layers[1].is_empty());
    let metrics = server.handle.metrics();
    assert!(metrics.capture_shown && metrics.capturing);
    assert_eq!((metrics.phones, metrics.clients), (1, 1), "a phone is not a viewer");
    assert_eq!(server.handle.info().clients, 1);

    phone.text(floor_json(footage.floor_y)).await;
    phone.stream(&footage.frames).await;
    assert_eq!(phone.acks.len(), footage.frames.len());
    assert_eq!(phone.acks.iter().map(|(index, _)| *index).collect::<Vec<_>>(), (0..24).collect::<Vec<u32>>());
    let used = phone.acks.iter().filter(|(_, used)| *used).count();
    assert!(used >= 20, "only {used} of 24 frames were used");

    // The cloud grew on the viewer's side while the phone filmed.
    viewer.until("the last frame", |v, _| capture_stats(v).is_some_and(|c| c.frames_received == 24)).await;
    let appends = viewer
        .viewer
        .log
        .iter()
        .filter(|m| matches!(m.event, Event::Layer { id: LAYER_STATIC, mode: LayerMode::Append, count } if count > 0))
        .count();
    assert!(appends >= 15, "only {appends} appends for 24 frames");
    assert!(viewer.viewer.log.iter().any(|m| m.event == Event::Preview { camera: 0 }), "no preview of the phone");
    let sizes: Vec<usize> = viewer.viewer.static_sizes.iter().map(|(_, size)| *size).collect();
    assert!(sizes.windows(2).filter(|pair| pair[1] > pair[0]).count() >= 15, "layer 0 did not grow: {sizes:?}");
    let before_finish = viewer.viewer.layers[0].len();
    assert!(before_finish > 5000, "only {before_finish} splats from 24 frames");
    assert_eq!(capture_stats(&viewer.viewer).unwrap().frames_used as usize, used);

    // Finish: the last progress for the phone, the final layer for viewers.
    let (frames, splats) = phone.finish().await;
    assert_eq!(frames, 24);
    viewer.until("the finished capture", |v, _| capture_stats(v).is_some_and(|c| c.finished)).await;
    let last_static = viewer.viewer.log.iter().rev().find(|m| m.is_static()).unwrap();
    assert!(matches!(last_static.event, Event::Layer { mode: LayerMode::Replace, .. }), "the final layer is a Replace");
    assert_eq!(viewer.viewer.layers[0].len() as u64, splats, "progress counts what viewers hold");
    assert!(viewer.viewer.state.as_ref().unwrap().capture.as_ref().unwrap().floor_known);
    // Finished: the capture stays on show, but nobody is capturing.
    eventually("the capture to be over", || !server.handle.metrics().capturing).await;
    let metrics = server.handle.metrics();
    assert!(metrics.capture_shown, "{metrics:?}");
    assert_eq!(metrics.capture_frames, 24);
    phone.close().await;
    eventually("the phone to be gone", || server.handle.metrics().phones == 0).await;

    // A viewer that arrives now gets the finished capture.
    let mut late = Client::connect(server.port()).await;
    late.until("the snapshot of the capture", |v, _| v.stats.is_some()).await;
    assert_eq!(late.viewer.scene(), "capture");
    assert_eq!(late.viewer.layers[0].len(), viewer.viewer.layers[0].len());
    assert!(capture_stats(&late.viewer).unwrap().finished);

    // On disk: the raw stream, and the result as a recording of the same name.
    eventually("the recording", || Dirs::files(&dirs.recordings(), ".gsrec").len() == 1).await;
    let captures = Dirs::files(&dirs.captures(), ".gscap");
    let recordings = Dirs::files(&dirs.recordings(), ".gsrec");
    assert_eq!((captures.len(), recordings.len()), (1, 1), "{captures:?} {recordings:?}");
    let stem = captures[0].strip_suffix(".gscap").unwrap();
    assert!(stem.starts_with("Chambre-20"), "{stem}");
    assert_eq!(recordings[0], format!("{stem}.gsrec"));
    let raw = std::fs::read(dirs.captures().join(&captures[0])).unwrap();
    let contents = split_gscap(&raw).expect("the kept capture does not read");
    assert_eq!((contents.frames.len(), contents.truncated), (24, false));
    assert_eq!((contents.header.mode.as_str(), contents.header.name.as_str()), ("room", "Chambre"));
    assert_eq!(contents.header.device, "iPhone17,2");
    assert!((contents.header.floor.expect("no floor in the header") - footage.floor_y).abs() < 1e-4);
    for (kept, sent) in contents.frames.iter().zip(&footage.frames) {
        assert!(*kept == sent.as_slice(), "a frame was not kept byte for byte");
    }
    // Processing the kept file again gives the capture again.
    let (again, summary) = gs_core::ingest::import_gscap(&raw, Default::default()).unwrap();
    assert_eq!((summary.frames, summary.name.as_str()), (24, "Chambre"));
    assert_eq!(again.splat_count() as u64, splats);

    let file = std::fs::read(dirs.recordings().join(&recordings[0])).unwrap();
    let report = record::verify(&file).expect("the recording has no usable header");
    assert!(report.is_valid() && report.complete, "{:?}", report.problems);
    assert_eq!((report.info.name.as_str(), report.info.scene.as_str()), ("Chambre", "capture"));
    assert_eq!(report.keyframes.len(), 1, "a finished scan is a still");
    let mut player = record::Player::from_bytes(file).unwrap();
    let mut replayed = common::Viewer::default();
    for message in player.update().messages {
        replayed.apply(&message);
    }
    assert_eq!(replayed.scene(), "capture");
    assert_eq!(replayed.layers[0].len(), viewer.viewer.layers[0].len(), "the recording holds the final layer");

    let listed = get(server.port(), "/api/recordings").await.json();
    let entry = &listed["recordings"][0];
    assert_eq!((entry["name"].as_str(), entry["title"].as_str()), (Some(stem), Some("Chambre")));
    assert_eq!((entry["scene"].as_str(), entry["recording"].as_bool()), (Some("capture"), Some(false)));

    // Back to the simulation, for everybody.
    viewer.send_json(json!({ "type": "set_scene", "scene": "stage" })).await;
    viewer.until("the stage again", |v, _| v.scene() == "stage").await;
    viewer.until("the stage's live layer", |v, m| m.is_dynamic() && !v.layers[1].is_empty()).await;
    late.until("the stage again", |v, _| v.scene() == "stage").await;
    assert!(late.viewer.state.as_ref().unwrap().capture.is_none());
    assert!(!server.handle.metrics().capture_shown);
    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_scan_the_phone_never_finished_is_saved_all_the_same() {
    let footage = bedroom();
    let (server, dirs) = start_capture_server(|_| {}).await;
    let mut viewer = Client::connect(server.port()).await;
    viewer.snapshot().await;

    // The app is closed in the middle of a scan, or the phone walks out of
    // the network: frames, then nothing, and never a `finish`.
    let mut phone = PhoneClient::open(&server, "room", "Oubli").await;
    phone.text(floor_json(footage.floor_y)).await;
    phone.stream(&footage.frames).await;
    phone.close().await;
    eventually("the phone to be gone", || {
        let metrics = server.handle.metrics();
        metrics.phones == 0 && !metrics.capturing
    })
    .await;
    viewer.until("the last frame", |v, _| capture_stats(v).is_some_and(|c| c.frames_received == 24)).await;
    let shown = viewer.viewer.layers[0].len();
    // The phone may still return and go on: nothing is saved yet.
    assert!(Dirs::files(&dirs.recordings(), ".gsrec").is_empty());

    // The viewers leave the capture: it is finished and saved on its way out.
    viewer.send_json(json!({ "type": "set_scene", "scene": "stage" })).await;
    viewer.until("the stage again", |v, _| v.scene() == "stage").await;
    eventually("the recording", || Dirs::files(&dirs.recordings(), ".gsrec").len() == 1).await;
    let (captures, recordings) = (Dirs::files(&dirs.captures(), ".gscap"), Dirs::files(&dirs.recordings(), ".gsrec"));
    assert_eq!(recordings[0].strip_suffix(".gsrec"), captures[0].strip_suffix(".gscap"));
    let file = std::fs::read(dirs.recordings().join(&recordings[0])).unwrap();
    let report = record::verify(&file).expect("the recording has no usable header");
    assert!(report.is_valid() && report.complete && report.keyframes.len() == 1, "{:?}", report.problems);
    assert_eq!((report.info.name.as_str(), report.info.scene.as_str()), ("Oubli", "capture"));
    let mut replayed = common::Viewer::default();
    for message in record::Player::from_bytes(file).unwrap().update().messages {
        replayed.apply(&message);
    }
    assert!(capture_stats(&replayed).is_some_and(|c| c.finished), "the saved scan is a finished one");
    // The finishing pass only tidies: about what the viewers were shown.
    let saved = replayed.layers[0].len();
    assert!(saved > shown / 2 && saved <= shown, "{saved} splats saved, {shown} shown");
    let raw = std::fs::read(dirs.captures().join(&captures[0])).unwrap();
    assert_eq!(split_gscap(&raw).expect("the kept capture does not read").frames.len(), 24);

    // The same when the server stops under an unfinished scan.
    let mut phone = PhoneClient::open(&server, "room", "Arrêt").await;
    phone.stream(&footage.frames).await;
    server.handle.shutdown().await;
    let recordings = Dirs::files(&dirs.recordings(), ".gsrec");
    assert!(recordings.iter().any(|file| file.starts_with("Arrêt-20")), "{recordings:?}");
    for file in &recordings {
        let report = record::verify(&std::fs::read(dirs.recordings().join(file)).unwrap()).unwrap();
        assert!(report.is_valid() && report.complete, "{file}: {:?}", report.problems);
    }
    drop(phone);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn controls_do_not_disturb_a_capture_in_progress() {
    let footage = bedroom();
    let (server, _dirs) = start_capture_server(|_| {}).await;
    let mut viewer = Client::connect(server.port()).await;
    viewer.snapshot().await;
    let mut phone = PhoneClient::open(&server, "room", "Salon").await;
    phone.stream(&footage.frames[..4]).await;
    viewer.until("the capture", |v, _| capture_stats(v).is_some_and(|c| c.frames_received == 4)).await;

    // While the phone is at it, nothing a viewer sends takes the capture
    // away or reaches the parked simulation.
    let controls = server.handle.metrics().controls;
    for control in [
        json!({ "type": "set_scene", "scene": "house" }),
        json!({ "type": "restart" }),
        json!({ "type": "set_paused", "paused": true }),
        json!({ "type": "scan_start" }),
    ] {
        viewer.send_json(control).await;
    }
    phone.stream(&footage.frames[4..6]).await;
    viewer.until("more of the capture", |v, _| capture_stats(v).is_some_and(|c| c.frames_received == 6)).await;
    assert_eq!(viewer.viewer.scene(), "capture");
    assert_eq!(server.handle.metrics().controls, controls);
    let ticks = server.handle.metrics().ticks;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(server.handle.metrics().ticks, ticks, "the simulation ran behind the capture");

    // The phone leaves without finishing: now a viewer may leave too.
    drop(phone);
    eventually("the phone to be gone", || !server.handle.metrics().capturing).await;
    viewer.send_json(json!({ "type": "restart" })).await;
    viewer.send_json(json!({ "type": "set_scene", "scene": "stage" })).await;
    viewer.until("the stage again", |v, _| v.scene() == "stage").await;
    assert_eq!(server.handle.metrics().controls, controls + 1, "only the scene change counts");
    viewer.until("a live layer", |v, m| m.is_dynamic() && !v.layers[1].is_empty()).await;
    eventually("the simulation to run again", || server.handle.metrics().ticks > ticks).await;
    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_hello_is_checked() {
    let (server, _dirs) = start_capture_server(|_| {}).await;
    let token = pairing(&server).await.token;
    let wrong = "0".repeat(32);
    let hello = |token: &str, mode: &str| hello_json(token, mode, "Salon");
    let cases: Vec<(&str, String, &str)> = vec![
        ("a wrong token", hello(&wrong, "room"), "bad_token"),
        ("an empty token", hello("", "room"), "bad_token"),
        ("a token of another length", hello(&token[..31], "room"), "bad_token"),
        ("the token in upper case", hello(&token.to_uppercase().replace(|c: char| c.is_ascii_digit(), "A"), "room"), "bad_token"),
        ("no token at all", json!({ "type": "hello", "protocol": 1, "mode": "room" }).to_string(), "bad_token"),
        ("a token that is a number", json!({ "type": "hello", "protocol": 1, "token": 12, "mode": "room" }).to_string(), "bad_token"),
        ("another protocol", hello(&token, "room").replace("\"protocol\":1", "\"protocol\":2"), "bad_protocol"),
        ("an unknown mode", hello(&token, "panorama"), "bad_hello"),
        ("another first message", r#"{"type":"finish"}"#.to_string(), "bad_hello"),
        ("no JSON", "bonjour".to_string(), "bad_hello"),
        ("a JSON array", "[1,2,3]".to_string(), "bad_hello"),
        ("deep nesting", "[".repeat(5000), "bad_hello"),
        ("a hello above the size limit", hello(&token, "room").replace("Salon", &"s".repeat(20_000)), "bad_hello"),
    ];
    for (what, text, expected) in cases {
        let mut phone = PhoneClient::connect(&server).await;
        phone.text(text).await;
        let (code, message, close) = phone.refusal().await;
        assert_eq!(code, expected, "{what}");
        assert_eq!(close, Some(1008), "{what}");
        assert!(!message.is_empty() && !message.contains(&token), "{what}: {message}");
        assert!(!message.contains('\u{2014}'), "{what}: {message}");
    }
    // A frame before the hello.
    let mut phone = PhoneClient::connect(&server).await;
    phone.frame(&bedroom().frames[0]).await;
    assert_eq!(phone.refusal().await.0, "bad_hello");
    // Refused phones never became the source.
    assert!(!server.handle.metrics().capture_shown);
    eventually("the refused phones to be gone", || server.handle.metrics().phones == 0).await;

    // A phone that says nothing is told so, well before a connection
    // could pile up.
    let mut silent = PhoneClient::connect(&server).await;
    let started = std::time::Instant::now();
    let (code, _, close) = silent.refusal().await;
    assert_eq!((code.as_str(), close), ("bad_hello", Some(1008)));
    assert!((8..=13).contains(&started.elapsed().as_secs()), "refused after {:?}", started.elapsed());

    // The phone is a native client: it sends no Origin, and a browser's
    // Origin makes no difference. What counts is the token.
    let addr = lan(&server);
    let io: common::BoxIo = Box::new(common::tls_stream(addr, &server.tls(), &[b"http/1.1"]).await.unwrap());
    let url = format!("wss://{addr}/ingest");
    let mut ws = common::handshake(io, &url, &[("origin", "https://evil.example")]).await.expect("refused by Origin");
    ws.send(Message::Text(hello(&token, "room").into())).await.unwrap();
    let Some(Ok(Message::Text(welcome))) = ws.next().await else { panic!("no welcome") };
    assert_eq!(welcome.as_str(), r#"{"type":"welcome","protocol":1,"max_inflight":4}"#);
    drop(ws);

    // Captures come from the network: not on the loopback listener, with
    // or without an upgrade.
    let port = server.port();
    let io: common::BoxIo = Box::new(common::tcp(loopback(port), None).await);
    let refused = common::handshake(io, &format!("ws://127.0.0.1:{port}/ingest"), &[]).await;
    match refused {
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => assert_eq!(response.status(), 403),
        Err(other) => panic!("{other}"),
        Ok(_) => panic!("the loopback listener accepted a phone"),
    }
    assert_eq!(get(port, "/ingest").await.status, 403);
    // Without an upgrade there is nothing to say on the LAN listener either.
    let plain = https(addr, &server.tls(), "GET", "/ingest", &[], &[]).await;
    assert!((400..500).contains(&plain.status), "{}", plain.status);
    assert_eq!(https(addr, &server.tls(), "POST", "/ingest", &[], &[]).await.status, 405);
    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_phone_that_breaks_the_rules_is_disconnected() {
    let footage = bedroom();
    let (server, dirs) = start_capture_server(|_| {}).await;
    let mut viewer = Client::connect(server.port()).await;
    viewer.snapshot().await;

    // More frames than max_inflight without waiting for the acks.
    let mut phone = PhoneClient::open(&server, "room", "Flot").await;
    for frame in footage.frames.iter().cycle().take(40) {
        if phone.ws.send(Message::Binary(frame.clone().into())).await.is_err() {
            break;
        }
    }
    let (code, message, close) = phone.refusal().await;
    assert_eq!((code.as_str(), close), ("flow_control", Some(1008)), "{message}");
    eventually("the flooding phone to be gone", || server.handle.metrics().phones == 0).await;
    let handled = server.handle.metrics().capture_frames;
    assert!(handled <= 4, "{handled} frames of the flood were fused");

    // A frame that does not decode.
    let mut phone = PhoneClient::open(&server, "room", "Casse").await;
    phone.frame(&footage.frames[0]).await;
    let mut broken = footage.frames[1].clone();
    broken[0..4].copy_from_slice(b"JPEG");
    phone.frame(&broken).await;
    let (code, message, close) = phone.refusal().await;
    assert_eq!((code.as_str(), close), ("bad_frame", Some(1007)), "{message}");
    assert_eq!(phone.acks, [(0, true)], "the good frame was acknowledged first");
    eventually("the phone to be detached", || !server.handle.metrics().capturing).await;

    // Text the server does not understand.
    let token = pairing(&server).await.token;
    for (what, text) in [
        ("an unknown type", r#"{"type":"dance"}"#.to_string()),
        ("a second hello", hello_json(&token, "room", "Casse")),
        ("a region without numbers", r#"{"type":"region","center":"ici","radius":1}"#.to_string()),
        ("a floor at infinity", r#"{"type":"floor","y":1e999}"#.to_string()),
        ("no JSON", "pause".to_string()),
        ("a message above the size limit", format!(r#"{{"type":"pause","pad":"{}"}}"#, "x".repeat(20_000))),
    ] {
        let mut phone = PhoneClient::open(&server, "room", "Casse").await;
        phone.text(text).await;
        let (code, _, close) = phone.refusal().await;
        assert_eq!((code.as_str(), close), ("bad_message", Some(1008)), "{what}");
    }

    // A message above the cap is not even read to its end.
    let mut phone = PhoneClient::open(&server, "room", "Casse").await;
    let _ = phone.ws.send(Message::Binary(vec![0u8; (8 << 20) + 1].into())).await;
    let ended = loop {
        match phone.said().await {
            Said::Closed(frame) => break frame,
            Said::Message(_) => {}
        }
    };
    assert!(matches!(ended, None | Some((1009, _))), "{ended:?}");

    // None of this stopped the server from serving its viewer, and the
    // viewer still holds a decodable stream (every message was applied).
    eventually("the phones to be gone", || server.handle.metrics().phones == 0).await;
    viewer.pump(Duration::from_millis(200)).await;
    assert_eq!(viewer.viewer.scene(), "capture");
    // Only readable frames reached the disk.
    for file in Dirs::files(&dirs.captures(), ".gscap") {
        let raw = std::fs::read(dirs.captures().join(&file)).unwrap();
        gs_core::ingest::import_gscap(&raw, Default::default()).unwrap_or_else(|e| panic!("{file}: {e}"));
    }
    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_phone_at_a_time_and_a_returning_phone_continues() {
    let footage = bedroom();
    let (server, dirs) = start_capture_server(|_| {}).await;
    let token = pairing(&server).await.token;
    let mut viewer = Client::connect(server.port()).await;
    viewer.snapshot().await;

    let mut first = PhoneClient::open(&server, "room", "Salon").await;
    first.text(floor_json(footage.floor_y)).await;
    first.stream(&footage.frames[..8]).await;

    // Another device while the first is capturing: refused, in French,
    // and the first one does not notice.
    let other_device = hello_json(&token, "room", "Salon").replace("iPhone17,2", "iPad14,3");
    let mut second = PhoneClient::connect(&server).await;
    second.text(other_device.clone()).await;
    let (code, message, close) = second.refusal().await;
    assert_eq!((code.as_str(), close), ("busy", Some(1008)));
    assert!(message.contains("appareil"), "{message}");
    // Neither does another capture name from the same model get in.
    let mut second = PhoneClient::connect(&server).await;
    second.text(hello_json(&token, "object", "Vase")).await;
    assert_eq!(second.refusal().await.0, "busy");
    first.stream(&footage.frames[8..10]).await;
    assert_eq!(first.acks.len(), 10);
    viewer.until("ten frames", |v, _| capture_stats(v).is_some_and(|c| c.frames_received == 10)).await;
    let before = viewer.viewer.layers[0].len();

    // The connection drops in the middle of the capture; the phone comes
    // back with the same hello, region and floor, and goes on.
    drop(first);
    eventually("the lost phone to be noticed", || !server.handle.metrics().capturing).await;
    let mut back = PhoneClient::open(&server, "room", "Salon").await;
    back.text(floor_json(footage.floor_y)).await;
    back.stream(&footage.frames[10..16]).await;
    viewer.until("sixteen frames", |v, _| capture_stats(v).is_some_and(|c| c.frames_received == 16)).await;
    assert!(viewer.viewer.layers[0].len() > before, "the scan did not go on");
    assert!(
        viewer.viewer.replaced.iter().all(|(_, previous)| previous.is_empty() || !viewer.viewer.layers[0].is_empty()),
        "the scan was thrown away on reconnection"
    );
    let empties = viewer.viewer.static_sizes.iter().skip(2).filter(|(_, size)| *size == 0).count();
    assert_eq!(empties, 0, "layer 0 was emptied on reconnection");

    // The phone reconnects although the server still holds its old
    // connection (a Wi-Fi drop the server has not noticed): the newcomer
    // wins, the stale connection is told and closed.
    let mut again = PhoneClient::open(&server, "room", "Salon").await;
    let (code, _, close) = back.refusal().await;
    assert_eq!((code.as_str(), close), ("busy", Some(1008)));
    again.stream(&footage.frames[16..20]).await;
    let (frames, splats) = again.finish().await;
    assert_eq!(frames, 20);
    assert!(splats > 0);
    viewer.until("the end", |v, _| capture_stats(v).is_some_and(|c| c.finished)).await;

    // One capture, one file, with every frame of the three connections.
    let captures = Dirs::files(&dirs.captures(), ".gscap");
    assert_eq!(captures.len(), 1, "{captures:?}");
    eventually("the file to be complete", || {
        let raw = std::fs::read(dirs.captures().join(&captures[0])).unwrap();
        split_gscap(&raw).is_ok_and(|contents| contents.frames.len() == 20 && !contents.truncated)
    })
    .await;
    eventually("the recording", || Dirs::files(&dirs.recordings(), ".gsrec").len() == 1).await;

    // Once the capture is finished, another device may start its own.
    let mut second = PhoneClient::connect(&server).await;
    second.text(other_device).await;
    assert!(matches!(second.said().await, Said::Message(ServerMessage::Welcome { .. })));
    viewer.until("the new capture", |v, _| capture_info(v).is_some_and(|c| c.device == "iPad14,3")).await;
    viewer.until("its empty layer", |v, m| m.is_static() && v.layers[0].is_empty()).await;
    // The phone that finished was let go without an error.
    let ended = loop {
        match again.said().await {
            Said::Closed(frame) => break frame,
            Said::Message(message) => assert!(!matches!(message, ServerMessage::Error { .. }), "{message:?}"),
        }
    };
    assert!(matches!(ended, Some((1000, _))), "{ended:?}");
    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reset_and_a_relaunched_app_start_from_nothing() {
    let footage = bedroom();
    let (server, dirs) = start_capture_server(|_| {}).await;
    let mut viewer = Client::connect(server.port()).await;
    viewer.snapshot().await;
    let mut phone = PhoneClient::open(&server, "room", "Essai").await;
    phone.stream(&footage.frames[..6]).await;
    viewer.until("six frames", |v, _| capture_stats(v).is_some_and(|c| c.frames_received == 6)).await;
    assert!(!viewer.viewer.layers[0].is_empty());

    // Reset: progress from zero, an empty scene, a new file.
    let seen = phone.progress.len();
    phone.text(r#"{"type":"reset"}"#).await;
    while phone.progress.len() == seen {
        assert!(matches!(phone.said().await, Said::Message(_)));
    }
    assert_eq!(phone.progress.last(), Some(&(0, 0)));
    viewer.until("the empty scene", |v, m| m.is_static() && v.layers[0].is_empty()).await;
    phone.stream(&footage.frames[6..9]).await;
    viewer.until("three frames", |v, _| capture_stats(v).is_some_and(|c| c.frames_received == 3)).await;
    // Pause: frames are acknowledged but not used.
    phone.text(r#"{"type":"pause"}"#).await;
    phone.stream(&footage.frames[9..11]).await;
    assert_eq!(phone.acks[9..], [(9, false), (10, false)]);
    phone.text(r#"{"type":"resume"}"#).await;
    phone.stream(&footage.frames[11..12]).await;
    assert_eq!(phone.acks[11], (11, true));
    drop(phone);
    eventually("the phone to be gone", || server.handle.metrics().phones == 0).await;
    let captures = Dirs::files(&dirs.captures(), ".gscap");
    assert_eq!(captures.len(), 1, "the file of before the reset is gone: {captures:?}");
    // Complete on the disk once the server has noticed that the phone left.
    eventually("the file to be complete", || {
        let raw = std::fs::read(dirs.captures().join(&captures[0])).unwrap();
        split_gscap(&raw).is_ok_and(|contents| contents.frames.len() == 6)
    })
    .await;
    let raw = std::fs::read(dirs.captures().join(&captures[0])).unwrap();
    let contents = split_gscap(&raw).unwrap();
    assert_eq!(contents.frames.len(), 6, "what follows the reset, and nothing of before");
    assert!(contents.frames[0] == footage.frames[6].as_slice());

    // The app is relaunched: same hello, but its frame index starts again
    // and so does ARKit's world. Nothing of the old scan may remain.
    let before = viewer.viewer.layers[0].len();
    assert!(before > 0);
    let mut relaunched = PhoneClient::open(&server, "room", "Essai").await;
    relaunched.stream(&footage.frames[..2]).await;
    viewer.until("the new world", |v, _| capture_stats(v).is_some_and(|c| c.frames_received == 2)).await;
    assert!(
        viewer.viewer.static_sizes.iter().rev().take(6).any(|(_, size)| *size == 0),
        "the old scan was not cleared: {:?}",
        viewer.viewer.static_sizes.iter().rev().take(6).collect::<Vec<_>>()
    );
    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn progress_comes_every_second_and_silence_is_noticed() {
    let footage = bedroom();
    let (server, _dirs) = start_capture_server(|config| {
        config.ping_interval = Duration::from_millis(300);
        config.idle_timeout = Duration::from_millis(900);
    })
    .await;
    let mut phone = PhoneClient::open(&server, "room", "Rythme").await;
    phone.stream(&footage.frames[..3]).await;

    // About once a second, whether frames arrive or not.
    let started = std::time::Instant::now();
    while phone.progress.len() < 3 {
        assert!(matches!(phone.said().await, Said::Message(_)));
    }
    let elapsed = started.elapsed().as_secs_f64();
    assert!((1.5..=4.5).contains(&elapsed), "three reports took {elapsed:.1} s");
    assert!(phone.progress.iter().all(|(frames, splats)| *frames == 3 && *splats > 0), "{:?}", phone.progress);

    // The phone pings every five seconds: answered.
    phone.ws.send(Message::Ping(b"abc".to_vec().into())).await.unwrap();
    let pong = loop {
        match tokio::time::timeout(common::PATIENCE, phone.ws.next()).await.expect("no pong") {
            Some(Ok(Message::Pong(payload))) => break payload,
            Some(Ok(_)) => {}
            other => panic!("{other:?}"),
        }
    };
    assert_eq!(&pong[..], b"abc");

    // A phone that went away without a word (it does not even answer the
    // server's pings, because nothing reads its socket here) is let go, so
    // that it does not hold the capture against the next one.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let ended = loop {
        match phone.said().await {
            Said::Closed(frame) => break frame,
            Said::Message(_) => {}
        }
    };
    assert!(matches!(ended, Some((1008, _)) | None), "{ended:?}");
    eventually("the silent phone to be gone", || server.handle.metrics().phones == 0).await;
    assert!(!server.handle.metrics().capturing);
    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_capture_is_recorded_as_a_stream_in_front_of_the_scan() {
    let (room, live) = (bedroom(), live_dancer());
    let (server, dirs) = start_capture_server(|_| {}).await;
    let mut viewer = Client::connect(server.port()).await;
    viewer.snapshot().await;

    // First a scan...
    let mut phone = PhoneClient::open(&server, "room", "Fond").await;
    phone.stream(&room.frames[..8]).await;
    phone.finish().await;
    phone.close().await;
    viewer.until("the finished scan", |v, _| capture_stats(v).is_some_and(|c| c.finished)).await;
    let scan = viewer.viewer.layers[0].len();
    assert!(scan > 0);

    // ...then, by the same phone, a live capture: it plays in front of it.
    let mut phone = PhoneClient::open(&server, "live", "Danse").await;
    viewer.until("the live capture", |v, _| capture_info(v).is_some_and(|c| c.mode == CaptureMode::Live)).await;
    viewer.until("its layers", |_, m| m.is_dynamic()).await;
    assert!(viewer.viewer.layers[0].len() > scan / 2, "the scan is not behind the live capture");
    // The indices of the app go on after the scan's.
    let frames: Vec<Vec<u8>> = live
        .frames
        .iter()
        .enumerate()
        .map(|(i, bytes)| {
            let mut frame = gs_core::ingest::decode_frame(bytes).unwrap();
            frame.index = 100 + i as u32;
            gs_core::ingest::encode_frame(&frame).unwrap()
        })
        .collect();
    let dynamic_before = viewer.viewer.dynamic_frames();
    for frame in &frames {
        phone.stream(std::slice::from_ref(frame)).await;
        // Paced like a phone, so that the recording has a time line.
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    assert!(phone.acks.iter().all(|(_, used)| *used), "live mode uses every frame: {:?}", phone.acks);
    viewer.until("the last live frame", |v, _| capture_stats(v).is_some_and(|c| c.frames_received == 12)).await;
    assert!(viewer.viewer.dynamic_frames() - dynamic_before >= 6, "the live layer was not replaced frame by frame");
    assert!(!viewer.viewer.layers[1].is_empty(), "nothing on the live layer");
    phone.finish().await;
    phone.close().await;

    eventually("the recordings", || Dirs::files(&dirs.recordings(), ".gsrec").len() == 2).await;
    let recordings = Dirs::files(&dirs.recordings(), ".gsrec");
    let stream = recordings.iter().find(|name| name.starts_with("Danse-")).expect("no recording of the live capture");
    eventually("the recording to be closed", || {
        record::verify(&std::fs::read(dirs.recordings().join(stream)).unwrap()).is_ok_and(|report| report.complete)
    })
    .await;
    let report = record::verify(&std::fs::read(dirs.recordings().join(stream)).unwrap()).unwrap();
    assert!(report.is_valid(), "{:?}", report.problems);
    assert!(report.layers >= 12, "{} layer messages for 12 live frames", report.layers);
    assert!(report.duration_s > 0.3, "a stream has a duration: {}", report.duration_s);
    // The still of the scan is the other one.
    let still = recordings.iter().find(|name| name.starts_with("Fond-")).unwrap();
    let report = record::verify(&std::fs::read(dirs.recordings().join(still)).unwrap()).unwrap();
    assert!(report.is_valid() && report.complete && report.keyframes.len() == 1);
    assert_eq!(Dirs::files(&dirs.captures(), ".gscap").len(), 2);
    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_capture_file_is_imported_over_both_listeners() {
    let footage = Footage::bedroom(12, 0);
    let file = footage.gscap("Chambre importée");
    let (server, dirs) = start_capture_server(|_| {}).await;
    let (port, addr, tls) = (server.port(), lan(&server), server.tls());
    let token = pairing(&server).await.token;
    let mut viewer = Client::connect(port).await;
    viewer.snapshot().await;

    // From the network: only with the pairing token.
    let bearer = format!("Bearer {token}");
    let wrong = format!("Bearer {}", "f".repeat(32));
    let basic = format!("Basic {token}");
    for (what, headers) in [
        ("no token", vec![]),
        ("a wrong token", vec![("authorization", wrong.as_str())]),
        ("another scheme", vec![("authorization", basic.as_str())]),
        ("the bare token", vec![("authorization", token.as_str())]),
        ("an Origin instead of a token", vec![("origin", "https://192.0.2.2")]),
    ] {
        let answer = https(addr, &tls, "POST", "/api/captures", &headers, &file).await;
        assert_eq!(answer.status, 401, "{what}: {}", answer.text());
        assert_eq!(answer.json()["ok"], json!(false));
        assert!(!answer.text().contains(&token));
    }
    assert!(Dirs::files(&dirs.captures(), ".gscap").is_empty(), "a refused upload left a file");
    assert_eq!(viewer.viewer.scene(), "stage");

    let answer = https(addr, &tls, "POST", "/api/captures", &[("authorization", &bearer)], &file).await;
    assert_eq!(answer.status, 200, "{}", answer.text());
    let body = answer.json();
    assert_eq!(body["ok"], json!(true));
    assert_eq!((body["frames"].as_u64(), body["truncated"].as_bool()), (Some(12), Some(false)));
    assert_eq!(body["name"].as_str(), Some("Chambre importée"));
    assert_eq!(body["shown"], json!(true));
    let kept = body["file"].as_str().expect("the capture was not kept").to_string();
    let recording = body["recording"].as_str().expect("the capture was not saved as a recording").to_string();
    assert!(kept.starts_with("Chambre-importée-20") && kept.ends_with(".gscap"), "{kept}");
    assert_eq!(kept, format!("{recording}.gscap"), "the two files of one capture share their name");
    assert_eq!(std::fs::read(dirs.captures().join(&kept)).unwrap(), file, "the kept file is the upload");
    assert_eq!(Dirs::files(&dirs.captures(), "").len(), 1, "nothing else was left in the directory");

    // Viewers are shown the result.
    viewer.until("the imported capture", |v, _| capture_stats(v).is_some_and(|c| c.finished)).await;
    let info = capture_info(&viewer.viewer).unwrap();
    assert!(info.imported && info.name == "Chambre importée" && info.device == "simulateur");
    assert!(viewer.viewer.layers[0].len() > 3000);
    let saved = std::fs::read(dirs.recordings().join(format!("{recording}.gsrec"))).unwrap();
    let report = record::verify(&saved).unwrap();
    assert!(report.is_valid() && report.complete && report.keyframes.len() == 1);
    assert_eq!(report.info.name, "Chambre importée");

    // On the machine itself: the Origin rules of the other POST routes.
    let own = format!("http://127.0.0.1:{port}");
    for origin in ["https://evil.example", "null", "http://localhost:3000"] {
        let answer = http(port, "POST", "/api/captures", &[("origin", origin)], &file).await;
        assert_eq!(answer.status, 403, "origin {origin}");
    }
    for headers in [vec![], vec![("origin", own.as_str())], vec![("origin", "tauri://localhost")]] {
        let answer = http(port, "POST", "/api/captures", &headers, &file).await;
        assert_eq!(answer.status, 200, "{headers:?}: {}", answer.text());
    }
    assert_eq!(Dirs::files(&dirs.captures(), ".gscap").len(), 4);
    // Also when the name had to be numbered (three imports in one second).
    let stems = |dir: &std::path::Path, ext: &str| -> Vec<String> {
        Dirs::files(dir, ext).iter().map(|file| file.strip_suffix(ext).unwrap().to_string()).collect()
    };
    assert_eq!(stems(&dirs.captures(), ".gscap"), stems(&dirs.recordings(), ".gsrec"));

    // What is not a capture is refused with the code of the fault.
    for (name, code) in [
        ("bad_file_magic.gscap", "bad_file_magic"),
        ("bad_file_short.gscap", "bad_file_magic"),
        ("bad_header_json.gscap", "bad_header"),
        ("bad_header_array.gscap", "bad_header"),
        ("bad_header_length_huge.gscap", "bad_header"),
        ("bad_header_utf8.gscap", "bad_header"),
        ("bad_frame_record.gscap", "bad_length"),
        ("bad_frame_record_huge.gscap", "bad_length"),
    ] {
        let answer = http(port, "POST", "/api/captures", &[], &fixture(name)).await;
        assert_eq!((answer.status, answer.json()["code"].as_str()), (400, Some(code)), "{name}: {}", answer.text());
        assert_eq!(answer.json()["ok"], json!(false));
    }
    let answer = http(port, "POST", "/api/captures", &[], b"").await;
    assert_eq!(answer.status, 400, "an empty body: {}", answer.text());
    let mut garbage = file.clone();
    garbage.truncate(5000);
    garbage.extend_from_slice(&[0xFF; 3000]);
    let answer = http(port, "POST", "/api/captures", &[], &garbage).await;
    assert_eq!(answer.status, 400, "a damaged frame: {}", answer.text());
    // A recording cut short keeps its complete frames.
    let answer = http(port, "POST", "/api/captures", &[], &fixture("truncated.gscap")).await;
    assert_eq!(answer.status, 200, "{}", answer.text());
    assert_eq!((answer.json()["frames"].as_u64(), answer.json()["truncated"].as_bool()), (Some(2), Some(true)));
    assert_eq!(Dirs::files(&dirs.captures(), ".gscap").len(), 5, "refused files are not kept");
    assert_eq!(Dirs::files(&dirs.captures(), "").len(), 5, "nor are their leftovers");

    for method in ["PUT", "PATCH", "DELETE"] {
        assert_eq!(http(port, method, "/api/captures", &[], &file).await.status, 405, "{method}");
    }

    // The kept captures: listed, fetched and deleted at the machine only.
    let listed = get(port, "/api/captures").await;
    assert_eq!(listed.status, 200);
    let listed = listed.json();
    let entries = listed["captures"].as_array().unwrap();
    assert_eq!(entries.len(), 5);
    let entry = entries.iter().find(|e| e["file"] == json!(kept)).expect("the upload is not listed");
    assert_eq!((entry["title"].as_str(), entry["mode"].as_str()), (Some("Chambre importée"), Some("room")));
    assert_eq!((entry["size"].as_u64(), entry["capturing"].as_bool()), (Some(file.len() as u64), Some(false)));
    assert_eq!(listed["dir"].as_str(), Some(dirs.captures().to_str().unwrap()));
    let url = format!("/{}", entry["url"].as_str().unwrap());
    let fetched = get(port, &url).await;
    assert_eq!((fetched.status, fetched.body == file), (200, true));
    for path in ["/api/captures", url.as_str()] {
        assert_eq!(https(addr, &tls, "GET", path, &[], &[]).await.status, 403, "GET {path} on the LAN listener");
    }
    assert_eq!(https(addr, &tls, "DELETE", &url, &[("authorization", &bearer)], &[]).await.status, 403);
    assert_eq!(http(port, "DELETE", &url, &[("origin", "https://evil.example")], &[]).await.status, 403);

    // A kept capture is processed again on request.
    let open = |body: serde_json::Value| async move {
        http(port, "POST", "/api/captures/open", &[("content-type", "application/json")], body.to_string().as_bytes()).await
    };
    viewer.send_json(json!({ "type": "set_scene", "scene": "stage" })).await;
    viewer.until("the stage", |v, _| v.scene() == "stage").await;
    let answer = open(json!({ "file": kept })).await;
    assert_eq!(answer.status, 200, "{}", answer.text());
    assert_eq!((answer.json()["frames"].as_u64(), answer.json()["file"].as_str()), (Some(12), Some(kept.as_str())));
    viewer.until("the capture again", |v, _| v.scene() == "capture").await;
    assert_eq!(open(json!({ "file": "../recordings/x.gscap" })).await.status, 404);
    assert_eq!(open(json!({ "file": "absent.gscap" })).await.status, 404);
    assert_eq!(open(json!({ "name": kept })).await.status, 422);
    let lan_open =
        https(addr, &tls, "POST", "/api/captures/open", &[("content-type", "application/json")], b"{\"file\":\"x.gscap\"}");
    assert_eq!(lan_open.await.status, 403);

    assert_eq!(http(port, "DELETE", &url, &[], &[]).await.status, 200);
    assert_eq!(http(port, "DELETE", &url, &[], &[]).await.status, 404);
    assert_eq!(get(port, &url).await.status, 404);
    assert!(!dirs.captures().join(&kept).exists());
    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn imports_are_bounded_and_wait_for_a_capturing_phone() {
    let footage = bedroom();
    let file = Footage { frames: footage.frames[..6].to_vec(), ..footage.clone() }.gscap("Petit");
    let limit = file.len() as u64 + 10;
    let (server, dirs) = start_capture_server(|config| {
        config.max_upload_bytes = limit;
        config.request_timeout = Duration::from_millis(1500);
    })
    .await;
    let port = server.port();

    // Within the limit.
    assert_eq!(http(port, "POST", "/api/captures", &[], &file).await.status, 200);
    // Announced too large: refused before a byte of it is read.
    let mut large = file.clone();
    large.extend_from_slice(&[0u8; 64]);
    let answer = http(port, "POST", "/api/captures", &[], &large).await;
    assert_eq!((answer.status, answer.json()["code"].as_str()), (413, Some("too_large")));
    // Not announced (chunked): refused once the limit is passed.
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = common::tcp(loopback(port), None).await;
    let head = format!("POST /api/captures HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nTransfer-Encoding: chunked\r\n\r\n");
    stream.write_all(head.as_bytes()).await.unwrap();
    for piece in large.chunks(4096) {
        let _ = stream.write_all(format!("{:x}\r\n", piece.len()).as_bytes()).await;
        let _ = stream.write_all(piece).await;
        let _ = stream.write_all(b"\r\n").await;
    }
    let _ = stream.write_all(b"0\r\n\r\n").await;
    let mut answer = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut answer)).await;
    assert!(String::from_utf8_lossy(&answer).starts_with("HTTP/1.1 413"), "{}", String::from_utf8_lossy(&answer));

    // A body that stops arriving does not hold its connection (or its
    // half-written file) for ever.
    let mut stalled = common::tcp(loopback(port), None).await;
    let head = format!("POST /api/captures HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: {}\r\n\r\n", file.len());
    stalled.write_all(head.as_bytes()).await.unwrap();
    stalled.write_all(&file[..2000]).await.unwrap();
    let mut answer = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(20), stalled.read_to_end(&mut answer)).await;
    assert!(String::from_utf8_lossy(&answer).starts_with("HTTP/1.1 408"), "{}", String::from_utf8_lossy(&answer));
    eventually("the leftovers to be removed", || Dirs::files(&dirs.captures(), "").len() == 1).await;

    // While a phone is capturing, its capture is not replaced by a file.
    let mut phone = PhoneClient::open(&server, "room", "En cours").await;
    phone.stream(&footage.frames[..2]).await;
    let answer = http(port, "POST", "/api/captures", &[], &file).await;
    assert_eq!((answer.status, answer.json()["code"].as_str()), (409, Some("busy")));
    phone.finish().await;
    assert_eq!(http(port, "POST", "/api/captures", &[], &file).await.status, 200);
    server.handle.shutdown().await;

    // A server that keeps no captures still imports them.
    let (server, dirs) = start_capture_server(|config| config.keep_captures = false).await;
    let answer = http(server.port(), "POST", "/api/captures", &[], &file).await;
    assert_eq!(answer.status, 200, "{}", answer.text());
    assert_eq!(answer.json()["file"], json!(null));
    assert!(answer.json()["recording"].is_string());
    assert!(Dirs::files(&dirs.captures(), "").is_empty());
    let mut phone = PhoneClient::open(&server, "room", "Sans fichier").await;
    phone.stream(&footage.frames[..2]).await;
    phone.finish().await;
    eventually("the recording", || Dirs::files(&dirs.recordings(), ".gsrec").len() == 2).await;
    assert!(Dirs::files(&dirs.captures(), "").is_empty());
    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_imported_live_capture_becomes_a_recording_with_its_own_time_line() {
    let footage = live_dancer();
    let (server, dirs) = start_capture_server(|_| {}).await;
    let answer = http(server.port(), "POST", "/api/captures", &[], &footage.gscap("Danse")).await;
    assert_eq!(answer.status, 200, "{}", answer.text());
    let recording = answer.json()["recording"].as_str().unwrap().to_string();
    let file = std::fs::read(dirs.recordings().join(format!("{recording}.gsrec"))).unwrap();
    let report = record::verify(&file).unwrap();
    assert!(report.is_valid() && report.complete, "{:?}", report.problems);
    // Twelve frames at fifteen per second, whatever the import took.
    assert!((report.duration_s - 11.0 / 15.0).abs() < 0.05, "duration {}", report.duration_s);
    assert!(report.layers >= 12, "{} layer messages", report.layers);
    // Half-way through, the replay shows the dancer.
    let mut player = record::Player::from_bytes(file).unwrap();
    player.seek(0.4);
    let mut replayed = common::Viewer::default();
    for message in player.update().messages {
        replayed.apply(&message);
    }
    assert!(replayed.layers[LAYER_DYNAMIC as usize].len() > 500, "{} splats", replayed.layers[1].len());
    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_relaunched_app_does_not_film_in_front_of_the_old_scan() {
    let (room, live) = (bedroom(), live_dancer());
    let (server, _dirs) = start_capture_server(|_| {}).await;
    let mut viewer = Client::connect(server.port()).await;
    viewer.snapshot().await;
    let mut phone = PhoneClient::open(&server, "room", "Fond").await;
    phone.stream(&room.frames[..8]).await;
    phone.finish().await;
    phone.close().await;
    viewer.until("the finished scan", |v, _| capture_stats(v).is_some_and(|c| c.finished)).await;

    // A live capture follows, and until its first frame the scan is kept
    // behind it. That frame carries index 0: the app was relaunched, its
    // AR world is a new one, and the old scan would stand anywhere in it.
    let mut phone = PhoneClient::open(&server, "live", "Danse").await;
    phone.text(floor_json(live.floor_y)).await;
    viewer.until("the live capture", |v, _| capture_info(v).is_some_and(|c| c.mode == CaptureMode::Live)).await;
    viewer.until("the scan behind it", |v, m| m.is_static() && !v.layers[0].is_empty()).await;
    phone.stream(&live.frames[..3]).await;
    viewer.until("the live frames", |v, _| capture_stats(v).is_some_and(|c| c.frames_received == 3)).await;
    assert!(viewer.viewer.layers[0].is_empty(), "the scan of another world is still shown");
    assert!(!viewer.viewer.layers[1].is_empty());
    // The floor the phone told for the new world was kept.
    assert!(viewer.viewer.state.as_ref().unwrap().capture.as_ref().unwrap().floor_known);
    server.handle.shutdown().await;
}
