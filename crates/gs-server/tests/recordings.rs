//! 4D recording and sharing: the record button, the recordings directory
//! served with `Range`, deletion and the share archive, against a running
//! server with real sockets.

mod common;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{eventually, get, http, https, loopback, raw_get, small_stage, start_server, Client, TestServer};
use gs_core::pipeline::Pipeline;
use gs_core::record::{self, Player, Recorder, RecorderOptions};
use serde_json::{json, Value};

/// A server with a recordings directory of its own.
async fn recording_server(adjust: impl FnOnce(&mut gs_server::ServerConfig)) -> (TestServer, tempfile::TempDir) {
    let root = tempfile::tempdir().unwrap();
    let recordings = root.path().join("recordings");
    let server = start_server(small_stage(30.0), |config| {
        config.recordings_dir = Some(recordings);
        adjust(config);
    })
    .await;
    (server, root)
}

fn recordings_dir(root: &tempfile::TempDir) -> PathBuf {
    root.path().join("recordings")
}

/// A recording of `seconds` of the small stage, made with the library
/// alone: what the server would write, without waiting for it.
fn demo_recording(title: &str, seconds: f32) -> Vec<u8> {
    let mut pipeline = Pipeline::new(small_stage(20.0));
    let mut recorder = Recorder::new(Vec::new(), RecorderOptions::named(title));
    for message in pipeline.snapshot() {
        recorder.push(0.0, &message.bytes).unwrap();
    }
    let ticks = (seconds * 20.0) as u32;
    for tick in 1..=ticks {
        for message in pipeline.tick(0.05) {
            recorder.push(f64::from(tick) * 0.05, &message.bytes).unwrap();
        }
    }
    recorder.finish().unwrap().0
}

fn put(root: &tempfile::TempDir, file: &str, bytes: &[u8]) {
    std::fs::create_dir_all(recordings_dir(root)).unwrap();
    std::fs::write(recordings_dir(root).join(file), bytes).unwrap();
}

async fn post_json(port: u16, path: &str, body: Value) -> common::HttpResponse {
    http(port, "POST", path, &[("content-type", "application/json")], body.to_string().as_bytes()).await
}

fn files_of(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .map(|entries| entries.flatten().filter_map(|e| e.file_name().into_string().ok()).collect())
        .unwrap_or_default();
    names.sort();
    names
}

fn le(bytes: &[u8], at: usize, size: usize) -> u64 {
    bytes[at..at + size].iter().rev().fold(0, |value, byte| (value << 8) | u64::from(*byte))
}

/// The files of a ZIP archive, read through its directory, with the
/// checksums verified.
fn unzip(zip: &[u8]) -> Vec<(String, Vec<u8>)> {
    let end = zip.len() - 22;
    assert_eq!(le(zip, end, 4), 0x0605_4b50, "no end record");
    let count = le(zip, end + 10, 2);
    let mut at = le(zip, end + 16, 4) as usize;
    let mut files = Vec::new();
    for _ in 0..count {
        assert_eq!(le(zip, at, 4), 0x0201_4b50, "not a directory entry");
        let (method, crc) = (le(zip, at + 10, 2), le(zip, at + 16, 4) as u32);
        let (stored, size) = (le(zip, at + 20, 4) as usize, le(zip, at + 24, 4) as usize);
        let (name_len, extra_len) = (le(zip, at + 28, 2) as usize, le(zip, at + 30, 2) as usize);
        let local = le(zip, at + 42, 4) as usize;
        let name = String::from_utf8(zip[at + 46..at + 46 + name_len].to_vec()).unwrap();
        at += 46 + name_len + extra_len;
        assert_eq!(le(zip, local, 4), 0x0403_4b50, "not a local header");
        let data = local + 30 + le(zip, local + 26, 2) as usize + le(zip, local + 28, 2) as usize;
        let body = &zip[data..data + stored];
        let content = match method {
            0 => body.to_vec(),
            8 => {
                let mut content = Vec::new();
                flate2::read::DeflateDecoder::new(body).read_to_end(&mut content).unwrap();
                content
            }
            other => panic!("compression method {other}"),
        };
        assert_eq!(content.len(), size, "{name}");
        let mut check = flate2::Crc::new();
        check.update(&content);
        assert_eq!(check.sum(), crc, "checksum of {name}");
        files.push((name, content));
    }
    files
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_record_button_records_what_viewers_receive() {
    let (server, root) = recording_server(|_| {}).await;
    let port = server.port();
    let dir = recordings_dir(&root);

    // Nothing runs yet.
    let status = get(port, "/api/record").await;
    assert_eq!((status.status, status.header("cache-control")), (200, Some("no-store")));
    assert_eq!(status.json(), json!({ "recording": null, "last": null }));
    let answer = post_json(port, "/api/record", json!({ "action": "stop" })).await;
    assert_eq!((answer.status, answer.json()["code"].as_str()), (409, Some("not_recording")));

    // Start, with a name that is not a file name.
    let mut viewer = Client::connect(port).await;
    viewer.snapshot().await;
    let answer = post_json(port, "/api/record", json!({ "action": "start", "name": "Danse du salon !" })).await;
    assert_eq!(answer.status, 200, "{}", answer.text());
    let started = answer.json();
    assert_eq!(started["recording"]["file"], json!("Danse-du-salon.gsrec"));
    assert_eq!(started["recording"]["name"], json!("Danse-du-salon"));
    assert_eq!(started["recording"]["title"], json!("Danse du salon !"));
    let answer = post_json(port, "/api/record", json!({ "action": "start", "name": "autre" })).await;
    assert_eq!((answer.status, answer.json()["code"].as_str()), (409, Some("already_recording")));
    assert_eq!(answer.json()["recording"]["file"], json!("Danse-du-salon.gsrec"));

    viewer.pump(Duration::from_millis(1300)).await;
    let status = get(port, "/api/record").await.json();
    assert!(status["recording"]["duration_s"].as_f64().unwrap() >= 1.0, "{status}");
    assert!(status["recording"]["size"].as_u64().unwrap() > 10_000, "{status}");
    // While it is written, the file is listed as such and stays.
    let listed = get(port, "/api/recordings").await.json();
    assert_eq!(listed["recordings"][0]["recording"], json!(true));
    assert_eq!(listed["recording"]["file"], json!("Danse-du-salon.gsrec"));
    assert_eq!(http(port, "DELETE", "/recordings/Danse-du-salon.gsrec", &[], &[]).await.status, 409);
    assert_eq!(get(port, "/api/share/Danse-du-salon.zip").await.status, 409);
    // Nor is half of it exported by the desktop shell.
    let half = tempfile::tempdir().unwrap();
    for destination in [half.path().join("half.gsrec"), half.path().join("half.zip")] {
        let export = if destination.extension().is_some_and(|ext| ext == "zip") {
            server.handle.export_share("Danse-du-salon", &destination)
        } else {
            server.handle.export_recording("Danse-du-salon", &destination)
        };
        assert_eq!(export.unwrap_err().kind(), std::io::ErrorKind::ResourceBusy, "{}", destination.display());
        assert!(!destination.exists(), "a refused export left {}", destination.display());
    }

    let answer = post_json(port, "/api/record", json!({ "action": "stop" })).await;
    assert_eq!(answer.status, 200, "{}", answer.text());
    let stopped = answer.json();
    assert_eq!(stopped["recording"], json!(null));
    let saved = &stopped["saved"];
    assert_eq!((saved["file"].as_str(), saved["reason"].as_str()), (Some("Danse-du-salon.gsrec"), Some("stopped")));
    assert_eq!((saved["scene"].as_str(), saved["url"].as_str()), (Some("stage"), Some("recordings/Danse-du-salon.gsrec")));
    let duration = saved["duration_s"].as_f64().unwrap();
    assert!((1.2..4.0).contains(&duration), "duration {duration}");
    assert_eq!(get(port, "/api/record").await.json()["last"]["file"], json!("Danse-du-salon.gsrec"));

    // The file is a complete recording of what the viewer saw.
    let file = std::fs::read(dir.join("Danse-du-salon.gsrec")).unwrap();
    assert_eq!(file.len() as u64, saved["size"].as_u64().unwrap());
    let report = record::verify(&file).unwrap();
    assert!(report.is_valid() && report.complete, "{:?}", report.problems);
    assert_eq!((report.info.name.as_str(), report.info.scene.as_str()), ("Danse du salon !", "stage"));
    assert!((report.duration_s - duration).abs() < 1e-6);
    assert!(report.layers >= 20, "{} layers in {duration:.1} s at 30 Hz", report.layers);
    let mut player = Player::from_bytes(file).unwrap();
    player.seek(1.0);
    let mut replayed = common::Viewer::default();
    for message in player.update().messages {
        replayed.apply(&message);
    }
    assert_eq!(replayed.scene(), "stage");
    assert_eq!(replayed.layers[0].len(), viewer.viewer.layers[0].len(), "the static layer of the session");
    assert!(!replayed.layers[1].is_empty(), "no dancer one second into the recording");
    drop(viewer);
    eventually("the viewer to leave", || server.handle.metrics().clients == 0).await;

    // The recorder counts as a spectator: the scene runs for it alone.
    let ticks = server.handle.metrics().ticks;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(server.handle.metrics().ticks, ticks, "ticking for nobody");
    let answer = post_json(port, "/api/record", json!({ "action": "start", "name": "Danse du salon !" })).await;
    assert_eq!(answer.json()["recording"]["file"], json!("Danse-du-salon-2.gsrec"), "a name that is taken");
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let saved = post_json(port, "/api/record", json!({ "action": "stop" })).await.json()["saved"].clone();
    assert!(saved["duration_s"].as_f64().unwrap() >= 1.0, "{saved}");
    assert!(server.handle.metrics().ticks >= ticks + 20);
    let report = record::verify(&std::fs::read(dir.join("Danse-du-salon-2.gsrec")).unwrap()).unwrap();
    assert!(report.is_valid() && report.complete && report.layers >= 20, "{} layers", report.layers);
    let ticks = server.handle.metrics().ticks;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(server.handle.metrics().ticks, ticks, "still ticking after the recording");

    // Names: none, hostile, endless.
    let unnamed = post_json(port, "/api/record", json!({ "action": "start" })).await.json();
    let file = unnamed["recording"]["file"].as_str().unwrap().to_string();
    assert!(file.starts_with("enregistrement-20") && file.ends_with(".gsrec"), "{file}");
    post_json(port, "/api/record", json!({ "action": "stop" })).await;
    for (name, expected) in [
        ("../../etc/passwd", "etc-passwd.gsrec"),
        ("..", "enregistrement.gsrec"),
        ("a/b\\c:d", "a-b-c-d.gsrec"),
        ("  été   au\tsalon\u{0}\u{7} ", "été-au-salon.gsrec"),
        ("CON", "enregistrement-2.gsrec"),
    ] {
        let answer = post_json(port, "/api/record", json!({ "action": "start", "name": name })).await;
        assert_eq!(answer.json()["recording"]["file"].as_str(), Some(expected), "{name:?}: {}", answer.text());
        assert_eq!(post_json(port, "/api/record", json!({ "action": "stop" })).await.status, 200);
    }
    let long = post_json(port, "/api/record", json!({ "action": "start", "name": "x".repeat(5000) })).await.json();
    assert_eq!(long["recording"]["title"].as_str().unwrap().len(), 120);
    assert!(long["recording"]["file"].as_str().unwrap().len() <= 70);
    post_json(port, "/api/record", json!({ "action": "stop" })).await;
    // Everything landed in the directory, and nowhere else.
    assert_eq!(files_of(root.path()), ["recordings"]);
    assert!(files_of(&dir).iter().all(|name| name.ends_with(".gsrec")), "{:?}", files_of(&dir));

    // Requests that are not the button's.
    for (what, body) in [
        ("an unknown action", r#"{"action":"pause"}"#),
        ("no action", r#"{"name":"x"}"#),
        ("a name that is not text", r#"{"action":"start","name":5}"#),
        ("no JSON", "start"),
        ("an empty body", ""),
    ] {
        let answer = http(port, "POST", "/api/record", &[("content-type", "application/json")], body.as_bytes()).await;
        assert!((400..500).contains(&answer.status), "{what}: {}", answer.status);
    }
    assert_eq!(http(port, "POST", "/api/record", &[], br#"{"action":"start"}"#).await.status, 415);
    assert_eq!(get(port, "/api/record").await.json()["recording"], json!(null));
    for method in ["PUT", "DELETE", "PATCH"] {
        assert_eq!(http(port, method, "/api/record", &[], &[]).await.status, 405, "{method}");
    }
    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recording_is_decided_at_the_machine() {
    let (server, root) = recording_server(|config| config.lan = true).await;
    let (port, addr, tls) = (server.port(), loopback(server.lan_port()), server.tls());
    let start = json!({ "action": "start", "name": "x" }).to_string();
    let json_type = ("content-type", "application/json");

    // Not from the network.
    let answer = https(addr, &tls, "POST", "/api/record", &[json_type], start.as_bytes()).await;
    assert_eq!((answer.status, answer.json()["code"].as_str()), (403, Some("loopback_only")));
    // Not from a page of another origin in the user's browser.
    for origin in ["https://evil.example", "null", "http://localhost:3000", &format!("http://evil.example:{port}")] {
        let answer = http(port, "POST", "/api/record", &[json_type, ("origin", origin)], start.as_bytes()).await;
        assert_eq!(answer.status, 403, "origin {origin}");
    }
    assert!(files_of(&recordings_dir(&root)).is_empty());
    // From the application's own pages.
    let own = format!("http://127.0.0.1:{port}");
    for origin in [own.as_str(), "tauri://localhost"] {
        let answer = http(port, "POST", "/api/record", &[json_type, ("origin", origin)], start.as_bytes()).await;
        assert_eq!(answer.status, 200, "origin {origin}: {}", answer.text());
        let stop = json!({ "action": "stop" }).to_string();
        assert_eq!(http(port, "POST", "/api/record", &[json_type, ("origin", origin)], stop.as_bytes()).await.status, 200);
    }
    // Everybody may know that a recording runs, and which ones exist.
    assert_eq!(https(addr, &tls, "GET", "/api/record", &[], &[]).await.status, 200);
    let listed = https(addr, &tls, "GET", "/api/recordings", &[], &[]).await.json();
    assert_eq!(listed["recordings"].as_array().unwrap().len(), 2);
    assert!(listed.get("dir").is_none(), "the path of the directory is nobody's business on the network");
    let local = get(port, "/api/recordings").await.json();
    assert_eq!(local["dir"].as_str(), Some(recordings_dir(&root).to_str().unwrap()));
    server.handle.shutdown().await;

    // A server without a recordings directory says so.
    let server = start_server(small_stage(20.0), |_| {}).await;
    let answer = post_json(server.port(), "/api/record", json!({ "action": "start" })).await;
    assert_eq!((answer.status, answer.json()["code"].as_str()), (503, Some("no_recordings_dir")));
    assert_eq!(get(server.port(), "/api/recordings").await.json(), json!({ "recordings": [], "recording": null, "dir": null }));
    assert_eq!(get(server.port(), "/recordings/x.gsrec").await.status, 404);
    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_recording_ends_by_itself_at_the_size_limit_and_at_shutdown() {
    let limit = 400_000;
    let (server, root) = recording_server(|config| config.max_recording_bytes = limit).await;
    let port = server.port();
    let started = post_json(port, "/api/record", json!({ "action": "start", "name": "plein" })).await;
    assert_eq!(started.status, 200, "{}", started.text());
    eventually("the recording to fill up", || server.handle.record_state().recording.is_none()).await;
    let state = get(port, "/api/record").await.json();
    assert_eq!((state["last"]["file"].as_str(), state["last"]["reason"].as_str()), (Some("plein.gsrec"), Some("full")));
    let file = std::fs::read(recordings_dir(&root).join("plein.gsrec")).unwrap();
    assert!(file.len() as u64 <= limit && file.len() as u64 > limit / 2, "{} bytes", file.len());
    let report = record::verify(&file).unwrap();
    assert!(report.is_valid() && report.complete, "a full recording is a whole one: {:?}", report.problems);
    assert_eq!(post_json(port, "/api/record", json!({ "action": "stop" })).await.status, 409);
    server.handle.shutdown().await;

    // A server that stops closes the recording it was writing.
    let (server, root) = recording_server(|_| {}).await;
    post_json(server.port(), "/api/record", json!({ "action": "start", "name": "arrêt" })).await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    server.handle.shutdown().await;
    let report = record::verify(&std::fs::read(recordings_dir(&root).join("arrêt.gsrec")).unwrap()).unwrap();
    assert!(report.is_valid() && report.complete, "{:?}", report.problems);
    assert!(report.duration_s > 0.3);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recordings_are_listed_served_by_ranges_and_deleted() {
    let (server, root) = recording_server(|config| config.lan = true).await;
    let (port, addr, tls) = (server.port(), loopback(server.lan_port()), server.tls());
    let demo = demo_recording("Démo", 2.0);
    put(&root, "demo.gsrec", &demo);
    put(&root, "été au salon.gsrec", &demo[..3000]);
    put(&root, "notes.txt", b"not a recording");
    put(&root, ".hidden.gsrec", &demo);
    put(&root, "broken.gsrec", b"GSREC1\0\0 but not really");
    std::fs::write(root.path().join("secret.gsrec"), b"outside").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(root.path().join("secret.gsrec"), recordings_dir(&root).join("link.gsrec")).unwrap();
    std::fs::create_dir(recordings_dir(&root).join("dir.gsrec")).unwrap();

    // The listing: files that are recordings, by their headers.
    let listed = get(port, "/api/recordings").await.json();
    let entries = listed["recordings"].as_array().unwrap();
    let names: Vec<&str> = entries.iter().map(|e| e["file"].as_str().unwrap()).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(sorted, ["broken.gsrec", "demo.gsrec", "été au salon.gsrec"]);
    let entry = entries.iter().find(|e| e["file"] == json!("demo.gsrec")).unwrap();
    assert_eq!((entry["name"].as_str(), entry["title"].as_str()), (Some("demo"), Some("Démo")));
    assert_eq!((entry["scene"].as_str(), entry["size"].as_u64()), (Some("stage"), Some(demo.len() as u64)));
    assert!((entry["duration_s"].as_f64().unwrap() - 2.0).abs() < 0.01);
    assert!(entry["created"].as_str().unwrap().ends_with('Z'));
    assert_eq!((entry["url"].as_str(), entry["recording"].as_bool()), (Some("recordings/demo.gsrec"), Some(false)));
    let odd = entries.iter().find(|e| e["file"] == json!("été au salon.gsrec")).unwrap();
    assert_eq!(odd["url"].as_str(), Some("recordings/%C3%A9t%C3%A9%20au%20salon.gsrec"));
    // A file without a readable header is listed by its file name.
    let broken = entries.iter().find(|e| e["file"] == json!("broken.gsrec")).unwrap();
    assert_eq!((broken["title"].as_str(), broken["duration_s"].as_f64()), (Some("broken"), Some(0.0)));

    // The whole file, on both listeners.
    let whole = get(port, "/recordings/demo.gsrec").await;
    assert_eq!(whole.status, 200);
    assert!(whole.body == demo);
    assert_eq!(whole.header("content-length"), Some(demo.len().to_string().as_str()));
    assert_eq!(whole.header("accept-ranges"), Some("bytes"));
    assert_eq!(whole.header("content-type"), Some("application/octet-stream"));
    assert_eq!(whole.header("content-encoding"), None, "a recording is not compressed on the way");
    assert!(whole.header("content-disposition").is_none());
    let etag = whole.header("etag").expect("no validator").to_string();
    assert!(etag.starts_with('"') && etag.ends_with('"'), "a strong validator: {etag}");
    assert!(whole.header("last-modified").is_some_and(|date| date.ends_with(" GMT")));
    let over_lan = https(addr, &tls, "GET", "/recordings/demo.gsrec", &[("accept-encoding", "gzip, br")], &[]).await;
    assert!(over_lan.status == 200 && over_lan.body == demo && over_lan.header("content-encoding").is_none());
    let by_url = get(port, &format!("/{}", odd["url"].as_str().unwrap())).await;
    assert!(by_url.status == 200 && by_url.body == demo[..3000]);
    let download = get(port, "/recordings/demo.gsrec?download=1").await;
    assert!(download.header("content-disposition").is_some_and(|value| value.starts_with("attachment; filename=\"demo.gsrec\"")));

    let head = http(port, "HEAD", "/recordings/demo.gsrec", &[], &[]).await;
    assert_eq!((head.status, head.body.len()), (200, 0));
    assert_eq!(head.header("content-length"), Some(demo.len().to_string().as_str()));
    assert_eq!((head.header("accept-ranges"), head.header("etag")), (Some("bytes"), Some(etag.as_str())));

    // Single ranges.
    let len = demo.len();
    let last = len - 1;
    let cases: Vec<(String, usize, usize)> = vec![
        ("bytes=0-0".into(), 0, 0),
        ("bytes=0-1023".into(), 0, 1023),
        ("bytes=1024-4095".into(), 1024, 4095),
        (format!("bytes={}-", len - 100), len - 100, last),
        ("bytes=-16".into(), len - 16, last),
        (format!("bytes=-{}", len * 2), 0, last),
        (format!("bytes=5-{}", len * 2), 5, last),
        (format!("bytes={last}-{last}"), last, last),
        ("bytes=0-".into(), 0, last),
    ];
    for (range, start, end) in &cases {
        for lan in [false, true] {
            let answer = if lan {
                https(addr, &tls, "GET", "/recordings/demo.gsrec", &[("range", range)], &[]).await
            } else {
                http(port, "GET", "/recordings/demo.gsrec", &[("range", range)], &[]).await
            };
            assert_eq!(answer.status, 206, "{range}");
            assert_eq!(answer.header("content-range"), Some(format!("bytes {start}-{end}/{len}").as_str()), "{range}");
            assert_eq!(answer.header("content-length"), Some((end - start + 1).to_string().as_str()), "{range}");
            assert!(answer.body == demo[*start..=*end], "{range}: wrong bytes");
            assert_eq!(answer.header("etag"), Some(etag.as_str()));
        }
    }
    let head = http(port, "HEAD", "/recordings/demo.gsrec", &[("range", "bytes=10-19")], &[]).await;
    assert_eq!((head.status, head.body.len(), head.header("content-length")), (206, 0, Some("10")));
    assert_eq!(head.header("content-range"), Some(format!("bytes 10-19/{len}").as_str()));

    // Ranges that lie beyond the file.
    for range in [format!("bytes={len}-"), format!("bytes={}-{}", len + 5, len + 9), "bytes=-0".to_string()] {
        let answer = http(port, "GET", "/recordings/demo.gsrec", &[("range", &range)], &[]).await;
        assert_eq!(answer.status, 416, "{range}");
        assert_eq!(answer.header("content-range"), Some(format!("bytes */{len}").as_str()), "{range}");
        assert!(answer.body.is_empty());
    }
    // What is not a single byte range gets the whole file.
    for range in ["bytes=0-9,20-29", "bytes=9-0", "bytes=abc", "bytes=", "lines=0-5", "bytes=0-9, 5-", "bytes=-"] {
        let answer = http(port, "GET", "/recordings/demo.gsrec", &[("range", range)], &[]).await;
        assert_eq!((answer.status, answer.body.len()), (200, len), "{range}");
        assert!(answer.header("content-range").is_none());
    }

    // Conditional requests against the validator.
    let fresh = http(port, "GET", "/recordings/demo.gsrec", &[("if-none-match", &etag)], &[]).await;
    assert_eq!((fresh.status, fresh.body.len()), (304, 0));
    let partial = [("range", "bytes=0-9"), ("if-range", etag.as_str())];
    assert_eq!(http(port, "GET", "/recordings/demo.gsrec", &partial, &[]).await.status, 206);
    for other in ["\"0-0\"", "W/\"0-0\"", &format!("W/{etag}"), "Sat, 14 Nov 2026 10:00:00 GMT"] {
        let stale = [("range", "bytes=0-9"), ("if-range", other)];
        let answer = http(port, "GET", "/recordings/demo.gsrec", &stale, &[]).await;
        assert_eq!((answer.status, answer.body.len()), (200, len), "If-Range: {other}");
    }
    // The file is replaced: another validator, and an old range request
    // gets the new file whole.
    tokio::time::sleep(Duration::from_millis(20)).await;
    let replaced = demo_recording("Démo 2", 1.0);
    put(&root, "demo.gsrec", &replaced);
    let answer = http(port, "GET", "/recordings/demo.gsrec", &partial, &[]).await;
    assert_eq!(answer.status, 200);
    assert!(answer.body == replaced);
    assert_ne!(answer.header("etag"), Some(etag.as_str()));
    put(&root, "demo.gsrec", &demo);

    // The player of gs-core, fed through Range requests alone.
    let mut player = Player::ranged(demo.len() as u64);
    player.seek(1.5);
    let mut replayed = common::Viewer::default();
    let mut requests = 0;
    loop {
        let update = player.update();
        for message in &update.messages {
            replayed.apply(message);
        }
        if !update.buffering {
            break;
        }
        let wanted = player.wanted().expect("buffering without asking for anything");
        let range = wanted.http_header();
        let answer = https(addr, &tls, "GET", "/recordings/demo.gsrec", &[("range", &range)], &[]).await;
        assert_eq!(answer.status, 206, "{range}");
        player.supply(wanted.start, &answer.body).unwrap();
        requests += 1;
        assert!(requests < 200, "the player never got what it asked for");
    }
    assert_eq!(replayed.scene(), "stage");
    assert!(!replayed.layers[0].is_empty() && !replayed.layers[1].is_empty(), "after {requests} requests");
    assert!((player.time() - 1.5).abs() < 0.06);

    // Names that are not recordings of the directory.
    for path in [
        "/recordings/absent.gsrec",
        "/recordings/notes.txt",
        "/recordings/.hidden.gsrec",
        "/recordings/link.gsrec",
        "/recordings/dir.gsrec",
        "/recordings/demo",
        "/recordings/demo.gsrec.txt",
        "/recordings/demo.GSREC",
        "/recordings/..%2Fsecret.gsrec",
        "/recordings/%2e%2e%2fsecret.gsrec",
        "/recordings/..%5Csecret.gsrec",
        "/recordings/%2Fetc%2Fpasswd.gsrec",
        "/recordings/demo.gsrec%00.txt",
        "/recordings/demo.gsrec%00",
        "/recordings/a%0Ab.gsrec",
        "/recordings/c%3Ademo.gsrec",
        "/recordings/",
        "/recordings/demo.gsrec/",
        "/recordings/demo.gsrec/more",
    ] {
        for lan in [false, true] {
            let answer = if lan { https(addr, &tls, "GET", path, &[], &[]).await } else { get(port, path).await };
            assert!(matches!(answer.status, 400 | 404), "{path}: {}", answer.status);
            assert!(!answer.text().contains("outside"));
        }
    }
    for target in ["/recordings/../secret.gsrec", "/recordings/./demo.gsrec/../../secret.gsrec", "/recordings/%ff.gsrec"] {
        let (status, text) = raw_get(port, target).await;
        assert!(matches!(status, 400 | 404), "{target}: {status}");
        assert!(!text.contains("outside"));
    }
    for method in ["POST", "PUT", "PATCH"] {
        assert_eq!(http(port, method, "/recordings/demo.gsrec", &[], b"x").await.status, 405, "{method}");
    }

    // Deleting: at the machine, by the application.
    assert_eq!(https(addr, &tls, "DELETE", "/recordings/demo.gsrec", &[], &[]).await.status, 403);
    assert_eq!(http(port, "DELETE", "/recordings/demo.gsrec", &[("origin", "https://evil.example")], &[]).await.status, 403);
    for path in ["/recordings/link.gsrec", "/recordings/..%2Fsecret.gsrec", "/recordings/notes.txt", "/recordings/dir.gsrec"] {
        assert_eq!(http(port, "DELETE", path, &[], &[]).await.status, 404, "{path}");
    }
    assert!(root.path().join("secret.gsrec").exists() && recordings_dir(&root).join("notes.txt").exists());
    let deleted = http(port, "DELETE", "/recordings/demo.gsrec", &[("origin", "tauri://localhost")], &[]).await;
    assert_eq!((deleted.status, deleted.json()), (200, json!({ "ok": true })));
    assert_eq!(http(port, "DELETE", "/recordings/demo.gsrec", &[], &[]).await.status, 404);
    assert_eq!(get(port, "/recordings/demo.gsrec").await.status, 404);
    assert!(!recordings_dir(&root).join("demo.gsrec").exists());
    assert_eq!(http(port, "DELETE", "/recordings/%C3%A9t%C3%A9%20au%20salon.gsrec", &[], &[]).await.status, 200);
    assert_eq!(get(port, "/api/recordings").await.json()["recordings"].as_array().unwrap().len(), 1);

    // The Tauri window reads recordings across origins: ranges included.
    let preflight = http(
        port,
        "OPTIONS",
        "/recordings/broken.gsrec",
        &[
            ("origin", "tauri://localhost"),
            ("access-control-request-method", "DELETE"),
            ("access-control-request-headers", "range"),
        ],
        &[],
    )
    .await;
    assert_eq!(preflight.header("access-control-allow-origin"), Some("tauri://localhost"));
    assert!(preflight.header("access-control-allow-methods").is_some_and(|m| m.contains("DELETE")));
    assert!(preflight.header("access-control-allow-headers").is_some_and(|h| h.to_lowercase().contains("range")));
    let ranged = [("origin", "tauri://localhost"), ("range", "bytes=0-3")];
    let answer = http(port, "GET", "/recordings/broken.gsrec", &ranged, &[]).await;
    assert_eq!(answer.status, 206);
    assert!(answer.header("access-control-expose-headers").is_some_and(|h| h.to_lowercase().contains("content-range")));
    let foreign = [("origin", "https://evil.example"), ("range", "bytes=0-3")];
    let answer = http(port, "GET", "/recordings/broken.gsrec", &foreign, &[]).await;
    assert!(answer.header("access-control-allow-origin").is_none());
    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_share_archive_holds_the_viewer_and_the_recording() {
    // A web build like Trunk's, with a file that is not part of it.
    let dist = tempfile::tempdir().unwrap();
    let index = "<!doctype html><title>Splat Live</title><script type=module src=./app-0123456789abcdef.js></script>";
    std::fs::write(dist.path().join("index.html"), index).unwrap();
    std::fs::write(dist.path().join("app-0123456789abcdef.js"), "export default 1;\n".repeat(400)).unwrap();
    let wasm: Vec<u8> = (0u32..50_000).map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8).collect();
    std::fs::write(dist.path().join("app-0123456789abcdef_bg.wasm"), &wasm).unwrap();
    std::fs::create_dir(dist.path().join("assets")).unwrap();
    std::fs::write(dist.path().join("assets/style.css"), "body{margin:0}").unwrap();
    std::fs::write(dist.path().join(".trunk-lock"), "").unwrap();
    let dist_dir = dist.path().to_path_buf();
    let (server, root) = recording_server(|config| {
        config.lan = true;
        config.dist = Some(dist_dir);
    })
    .await;
    let (port, addr, tls) = (server.port(), loopback(server.lan_port()), server.tls());
    let demo = demo_recording("Démo \"du\" salon", 1.5);
    put(&root, "demo.gsrec", &demo);
    put(&root, "été.gsrec", &demo);

    let answer = get(port, "/api/share/demo.zip").await;
    assert_eq!(answer.status, 200, "{}", answer.text());
    assert_eq!(answer.header("content-type"), Some("application/zip"));
    assert_eq!(answer.header("content-length"), Some(answer.body.len().to_string().as_str()), "the size is announced");
    assert!(answer.header("content-disposition").is_some_and(|v| v.starts_with("attachment; filename=\"demo.zip\"")));
    assert!(answer.header("content-encoding").is_none());
    let files = unzip(&answer.body);
    let names: Vec<&str> = files.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(
        names,
        [
            "app-0123456789abcdef.js",
            "app-0123456789abcdef_bg.wasm",
            "assets/style.css",
            "index.html",
            "share.json",
            "recording.gsrec"
        ]
    );
    let content = |name: &str| &files.iter().find(|(n, _)| n == name).unwrap().1;
    assert_eq!(content("index.html"), index.as_bytes());
    assert_eq!(content("app-0123456789abcdef_bg.wasm"), &wasm);
    assert!(content("recording.gsrec") == &demo, "the recording is in the archive byte for byte");
    let manifest: Value = serde_json::from_slice(content("share.json")).unwrap();
    assert_eq!(manifest["file"], json!("recording.gsrec"));
    assert_eq!((manifest["autoplay"].as_bool(), manifest["loop"].as_bool()), (Some(true), Some(true)));
    assert_eq!((manifest["name"].as_str(), manifest["scene"].as_str()), (Some("Démo \"du\" salon"), Some("stage")));
    assert_eq!(manifest["size"].as_u64(), Some(demo.len() as u64));
    assert!((manifest["duration_s"].as_f64().unwrap() - 1.5).abs() < 0.01);
    // The text of the build shrank; the archive is about the recording.
    assert!(answer.body.len() < demo.len() + wasm.len() + 3000, "{} bytes", answer.body.len());

    // Served by this server, the viewer is not an unpacked archive: its
    // question gets an answer that is neither a manifest nor an error.
    let probe = get(port, "/share.json").await;
    assert_eq!((probe.status, probe.body.len(), probe.header("cache-control")), (204, 0, Some("no-store")));
    assert_eq!(get(port, "/assets/share.json").await.status, 404);
    // A web build that carries a manifest is served it like any file.
    std::fs::write(dist.path().join("share.json"), "{\"file\":\"x.gsrec\"}").unwrap();
    let own = get(port, "/share.json").await;
    assert_eq!((own.status, own.text().as_str()), (200, "{\"file\":\"x.gsrec\"}"));
    std::fs::remove_file(dist.path().join("share.json")).unwrap();

    // Over the network too, and under a name with accents.
    let over_lan = https(addr, &tls, "GET", "/api/share/%C3%A9t%C3%A9.zip", &[], &[]).await;
    assert_eq!(over_lan.status, 200);
    assert_eq!(unzip(&over_lan.body).len(), 6);
    let head = http(port, "HEAD", "/api/share/demo.zip", &[], &[]).await;
    assert_eq!((head.status, head.body.len(), head.header("content-type")), (200, 0, Some("application/zip")));
    // A HEAD announces the length a GET delivers, not an empty archive.
    let whole = get(port, "/api/share/demo.zip").await;
    assert_eq!(head.header("content-length"), Some(whole.body.len().to_string().as_str()));

    // The same archive written to a file, for a shell that cannot download.
    let exported = root.path().join("export.zip");
    let (handle, destination) = (server.handle.clone(), exported.clone());
    let written = tokio::task::spawn_blocking(move || handle.export_share("demo", &destination)).await.unwrap().unwrap();
    let on_disk = std::fs::read(&exported).unwrap();
    assert_eq!(written, on_disk.len() as u64);
    assert_eq!(unzip(&on_disk), files);
    let copy = root.path().join("copy.gsrec");
    let (handle, destination) = (server.handle.clone(), copy.clone());
    tokio::task::spawn_blocking(move || handle.export_recording("demo", &destination)).await.unwrap().unwrap();
    assert!(std::fs::read(&copy).unwrap() == demo);
    assert!(server.handle.export_recording("../secret", &copy).is_err());
    assert!(server.handle.export_share("absent", &exported).is_err());

    // What cannot be shared.
    for path in [
        "/api/share/absent.zip",
        "/api/share/demo",
        "/api/share/demo.gsrec",
        "/api/share/demo.zip.zip",
        "/api/share/..%2Fsecret.zip",
        "/api/share/.zip",
        "/api/share/",
    ] {
        assert_eq!(get(port, path).await.status, 404, "{path}");
    }
    for method in ["POST", "DELETE", "PUT"] {
        assert_eq!(http(port, method, "/api/share/demo.zip", &[], &[]).await.status, 405, "{method}");
    }
    server.handle.shutdown().await;

    // Without a web build there is nothing to unpack onto a web host.
    let (server, root) = recording_server(|_| {}).await;
    put(&root, "demo.gsrec", &demo);
    let answer = get(server.port(), "/api/share/demo.zip").await;
    if gs_server::has_embedded_frontend() {
        assert_eq!(answer.status, 200);
    } else {
        assert_eq!((answer.status, answer.json()["code"].as_str()), (503, Some("no_frontend")));
    }
    server.handle.shutdown().await;
}
