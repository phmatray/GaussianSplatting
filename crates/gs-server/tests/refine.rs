//! Offline refinement and dataset export of a kept capture, against a
//! running server with real sockets. The captures come from the simulated
//! phone of the gs-core tests (`common/phone.rs`); the jobs are cut down to
//! a handful of iterations with `ServerConfig::refine_iterations`.

mod common;
#[path = "common/phone.rs"]
mod phone;

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use common::{get, http, https, loopback, TestServer};
use phone::{lan, start_capture_server, Dirs, Footage};
use serde_json::{json, Value};

/// A sweep through the bedroom, filmed once for all the tests.
fn bedroom() -> &'static Vec<u8> {
    static FILE: OnceLock<Vec<u8>> = OnceLock::new();
    FILE.get_or_init(|| Footage::bedroom(40, 0).gscap("Chambre d'amis"))
}

const CAPTURE: &str = "Chambre-20261114-120000.gscap";

fn refined_dir(dirs: &Dirs) -> std::path::PathBuf {
    dirs.root.path().join("refined")
}

fn datasets_dir(dirs: &Dirs) -> std::path::PathBuf {
    dirs.root.path().join("datasets")
}

/// A server that keeps `CAPTURE` and runs its jobs for `iterations`.
async fn refine_server(iterations: u32, adjust: impl FnOnce(&mut gs_server::ServerConfig)) -> (TestServer, Dirs) {
    let (server, dirs) = start_capture_server(|config| {
        config.refine_iterations = Some(iterations);
        config.refine_threads = 2;
        adjust(config);
    })
    .await;
    std::fs::create_dir_all(dirs.captures()).unwrap();
    std::fs::write(dirs.captures().join(CAPTURE), bedroom()).unwrap();
    (server, dirs)
}

async fn post(port: u16, body: Value) -> common::HttpResponse {
    http(port, "POST", "/api/refine", &[("content-type", "application/json")], body.to_string().as_bytes()).await
}

async fn state(port: u16) -> Value {
    let answer = get(port, "/api/refine").await;
    assert_eq!(answer.status, 200, "{}", answer.text());
    assert_eq!(answer.header("cache-control"), Some("no-store"));
    answer.json()
}

/// Polls `GET /api/refine` until no job is running; returns the last
/// report and every status seen on the way.
async fn wait_for_the_end(port: u16, patience: Duration) -> (Value, Vec<Value>) {
    let deadline = Instant::now() + patience;
    let mut seen = Vec::new();
    loop {
        let state = state(port).await;
        if state["job"].is_null() {
            return (state["last"].clone(), seen);
        }
        seen.push(state["job"].clone());
        assert!(Instant::now() < deadline, "the job did not end: {}", state["job"]);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_kept_capture_is_refined_listed_served_and_deleted() {
    let dist = tempfile::tempdir().unwrap();
    std::fs::write(dist.path().join("index.html"), "<!doctype html><title>Splat Live</title>").unwrap();
    let (server, dirs) = refine_server(24, |config| config.dist = Some(dist.path().to_path_buf())).await;
    let port = server.port();

    assert_eq!(state(port).await, json!({ "job": null, "last": null }));
    let listed = get(port, "/api/refined").await.json();
    assert_eq!(listed["results"], json!([]));
    assert_eq!(listed["dir"], json!(refined_dir(&dirs).display().to_string()));
    assert_eq!(listed["datasets_dir"], json!(datasets_dir(&dirs).display().to_string()));

    let started = post(port, json!({ "action": "start", "capture": CAPTURE, "preset": "rapide" })).await;
    assert_eq!(started.status, 200, "{}", started.text());
    let job = &started.json()["job"];
    assert_eq!((job["kind"].as_str(), job["preset"].as_str(), job["capture"].as_str()), (Some("refine"), Some("rapide"), Some(CAPTURE)));
    assert_eq!((job["title"].as_str(), job["threads"].as_u64(), job["id"].as_u64()), (Some("Chambre d'amis"), Some(2), Some(1)));
    assert_eq!(job["cancelling"], json!(false));

    // One job at a time: the second is told which one is in the way.
    let second = post(port, json!({ "action": "start", "capture": CAPTURE })).await;
    assert_eq!(second.status, 409, "{}", second.text());
    assert_eq!((second.json()["code"].as_str(), second.json()["job"]["id"].as_u64()), (Some("job_running"), Some(1)));
    let export = post(port, json!({ "action": "export", "capture": CAPTURE, "format": "colmap" })).await;
    assert_eq!((export.status, export.json()["code"].as_str().map(str::to_string)), (409, Some("job_running".to_string())));

    // The server stays responsive while the job runs.
    let asked = Instant::now();
    assert_eq!(get(port, "/api/info").await.status, 200);
    assert!(asked.elapsed() < Duration::from_secs(2), "the server answered after {:?}", asked.elapsed());

    let (last, seen) = wait_for_the_end(port, Duration::from_secs(240)).await;
    assert_eq!(last["outcome"], json!("done"), "{last}");
    assert_eq!(last["error"], Value::Null);
    assert_eq!((last["iterations_done"].as_u64(), last["iterations"].as_u64()), (Some(24), Some(24)));
    // The progress went through the stages of the optimiser, in order.
    let stages: Vec<&str> = seen.iter().filter_map(|job| job["stage"].as_str()).collect();
    assert!(stages.iter().all(|stage| ["prepare", "select", "fuse", "exposure", "poses", "evaluate", "optimise", "write"].contains(stage)), "{stages:?}");
    assert!(seen.windows(2).all(|pair| pair[0]["elapsed_s"].as_f64() <= pair[1]["elapsed_s"].as_f64()));
    assert!(seen.windows(2).all(|pair| pair[0]["iteration"].as_u64() <= pair[1]["iteration"].as_u64()));

    let result = &last["result"];
    let file = "Chambre-20261114-120000-rapide.ply";
    assert_eq!((result["file"].as_str(), result["name"].as_str()), (Some(file), Some("Chambre-20261114-120000-rapide")));
    assert_eq!((result["capture"].as_str(), result["preset"].as_str(), result["mode"].as_str()), (Some(CAPTURE), Some("rapide"), Some("room")));
    assert_eq!((result["title"].as_str(), result["url"].as_str()), (Some("Chambre d'amis"), Some("refined/Chambre-20261114-120000-rapide.ply")));
    assert_eq!((result["beyond_sensor"].as_bool(), result["writing"].as_bool()), (Some(true), Some(false)));
    let gaussians = result["gaussians"].as_u64().unwrap();
    assert!(gaussians > 1000, "{result}");
    // A room comes with the place to look at it from: where the phone
    // started, at eye height, looking into the room.
    let view: Vec<f64> = result["view"].as_array().expect("no start view").iter().filter_map(Value::as_f64).collect();
    assert_eq!(view.len(), 6, "{result}");
    assert!(view[1] > 0.8 && view[1] < 2.0 && (view[0] - view[3]).hypot(view[2] - view[5]) > 0.5, "{view:?}");

    // Listed on both listeners; where the files are is told at the machine only.
    let listed = get(port, "/api/refined").await.json();
    assert_eq!(listed["results"], json!([result]));
    let from_lan = https(lan(&server), &server.tls(), "GET", "/api/refined", &[], &[]).await;
    assert_eq!(from_lan.status, 200);
    assert_eq!(from_lan.json()["results"][0]["file"], json!(file));
    assert!(from_lan.json().get("dir").is_none() && from_lan.json().get("datasets_dir").is_none());

    // Served whole and by ranges, on both listeners; it is the cloud the
    // listing describes.
    let whole = get(port, &format!("/refined/{file}")).await;
    assert_eq!(whole.status, 200);
    assert_eq!(whole.header("accept-ranges"), Some("bytes"));
    assert_eq!(whole.body.len() as u64, result["size"].as_u64().unwrap());
    let splats = gs_core::io::read_ply(&whole.body).expect("the result is not a 3DGS .ply");
    assert_eq!(splats.len() as u64, gaussians);
    let part = http(port, "GET", &format!("/refined/{file}"), &[("range", "bytes=0-9")], &[]).await;
    assert_eq!((part.status, part.body.as_slice()), (206, &whole.body[..10]));
    assert!(part.body.starts_with(b"ply\n"));
    let over_lan = https(lan(&server), &server.tls(), "GET", &format!("/refined/{file}"), &[("range", "bytes=-16")], &[]).await;
    assert_eq!((over_lan.status, over_lan.body.as_slice()), (206, &whole.body[whole.body.len() - 16..]));
    let download = get(port, &format!("/refined/{file}?download")).await;
    assert!(download.header("content-disposition").is_some_and(|value| value.starts_with("attachment")));

    // Its description sits beside it, with the optimiser's own report.
    let described: Value = serde_json::from_slice(&std::fs::read(refined_dir(&dirs).join("Chambre-20261114-120000-rapide.json")).unwrap()).unwrap();
    assert_eq!((described["capture"].as_str(), described["preset"].as_str()), (Some(CAPTURE), Some("rapide")));
    assert_eq!(described["report"]["completed"], json!(true));
    assert_eq!(described["report"]["threads"], json!(2));

    // Shared like a recording: the web build with the result inside.
    let zip = get(port, "/api/share/refined/Chambre-20261114-120000-rapide.zip").await;
    assert_eq!((zip.status, zip.header("content-type")), (200, Some("application/zip")));
    assert_eq!(zip.header("content-length").and_then(|v| v.parse::<usize>().ok()), Some(zip.body.len()));
    let has = |name: &[u8]| zip.body.windows(name.len()).any(|window| window == name);
    assert!(has(b"scene.ply") && has(b"share.json") && has(b"index.html"));
    // (The manifest is deflated; `e2e/server/refine.py` reads it: a room
    // is opened for a walk, from that view.)
    // The result is stored, not deflated: its bytes are in the archive.
    assert!(has(&whole.body[..256]));
    assert_eq!(get(port, "/api/share/refined/nothing.zip").await.status, 404);

    // Deleting is for the machine's own page.
    let foreign = http(port, "DELETE", &format!("/refined/{file}"), &[("origin", "https://evil.example")], &[]).await;
    assert_eq!((foreign.status, foreign.json()["code"].as_str().map(str::to_string)), (403, Some("origin_not_allowed".to_string())));
    let from_lan = https(lan(&server), &server.tls(), "DELETE", &format!("/refined/{file}"), &[], &[]).await;
    assert_eq!((from_lan.status, from_lan.json()["code"].as_str().map(str::to_string)), (403, Some("loopback_only".to_string())));
    let deleted = http(port, "DELETE", &format!("/refined/{file}"), &[], &[]).await;
    assert_eq!((deleted.status, deleted.json()), (200, json!({ "ok": true })));
    assert_eq!(get(port, &format!("/refined/{file}")).await.status, 404);
    assert_eq!(http(port, "DELETE", &format!("/refined/{file}"), &[], &[]).await.status, 404);
    assert_eq!(Dirs::files(&refined_dir(&dirs), ""), Vec::<String>::new(), "the description went with the result");
    assert_eq!(get(port, "/api/refined").await.json()["results"], json!([]));

    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_job_stops_soon_and_keeps_nothing() {
    // More iterations than the test will ever wait for.
    let (server, dirs) = refine_server(1_000_000, |_| {}).await;
    let port = server.port();

    let nothing = post(port, json!({ "action": "cancel" })).await;
    assert_eq!((nothing.status, nothing.json()["code"].as_str().map(str::to_string)), (409, Some("no_job".to_string())));

    // Stopped while it optimises.
    let started = post(port, json!({ "action": "start", "capture": CAPTURE, "preset": "rapide", "beyond_sensor": false })).await;
    assert_eq!(started.status, 200, "{}", started.text());
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let job = state(port).await["job"].clone();
        if job["stage"] == json!("optimise") && job["iteration"].as_u64().is_some_and(|i| i >= 3) {
            assert_eq!(job["iterations"], json!(1_000_000));
            assert!(job["gaussians"].as_u64().unwrap() > 1000 && job["width"].as_u64().unwrap() > 0, "{job}");
            break;
        }
        assert!(!job.is_null(), "the job ended by itself: {}", state(port).await["last"]);
        assert!(Instant::now() < deadline, "the job never reached the optimisation: {job}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let asked = Instant::now();
    let cancelled = post(port, json!({ "action": "cancel" })).await;
    assert_eq!((cancelled.status, cancelled.json()["job"]["cancelling"].as_bool()), (200, Some(true)));
    let (last, _) = wait_for_the_end(port, Duration::from_secs(60)).await;
    assert!(asked.elapsed() < Duration::from_secs(30), "the stop took {:?}", asked.elapsed());
    assert_eq!((last["outcome"].as_str(), last["id"].as_u64()), (Some("cancelled"), Some(1)));
    assert_eq!((&last["result"], &last["error"]), (&Value::Null, &Value::Null));
    assert!(last["iterations_done"].as_u64().unwrap() >= 3);
    // Nothing is left: no result, no half-written file.
    assert_eq!(Dirs::files(&refined_dir(&dirs), ""), Vec::<String>::new());
    assert_eq!(get(port, "/api/refined").await.json()["results"], json!([]));

    // Stopped during the preparation as well, and the slot is free again.
    let again = post(port, json!({ "action": "start", "capture": CAPTURE })).await;
    assert_eq!((again.status, again.json()["job"]["id"].as_u64(), again.json()["job"]["preset"].as_str().map(str::to_string)), (200, Some(2), Some("standard".to_string())));
    assert_eq!(post(port, json!({ "action": "cancel" })).await.status, 200);
    let (last, _) = wait_for_the_end(port, Duration::from_secs(60)).await;
    assert_eq!((last["outcome"].as_str(), last["id"].as_u64()), (Some("cancelled"), Some(2)));
    assert_eq!(Dirs::files(&refined_dir(&dirs), ""), Vec::<String>::new());

    // A server that shuts down takes its job with it.
    assert_eq!(post(port, json!({ "action": "start", "capture": CAPTURE, "preset": "fin" })).await.status, 200);
    let asked = Instant::now();
    server.handle.shutdown().await;
    assert!(asked.elapsed() < Duration::from_secs(30), "the shutdown took {:?}", asked.elapsed());
    assert!(server.handle.refine_state().job.is_none() || server.handle.refine_state().job.is_some_and(|job| job.cancelling));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn requests_that_cannot_start_a_job_are_refused_with_a_code() {
    let (server, dirs) = refine_server(8, |_| {}).await;
    let port = server.port();
    let refused = |answer: &common::HttpResponse| (answer.status, answer.json()["code"].as_str().unwrap_or_default().to_string());

    // No such capture, whatever the name tries.
    for name in ["rien.gscap", "../captures/Chambre-20261114-120000.gscap", "Chambre-20261114-120000", "/etc/passwd", ".gscap", ""] {
        let answer = post(port, json!({ "action": "start", "capture": name })).await;
        assert_eq!(refused(&answer), (404, "not_found".to_string()), "{name:?}");
        let answer = post(port, json!({ "action": "export", "capture": name, "format": "colmap" })).await;
        assert_eq!(refused(&answer), (404, "not_found".to_string()), "{name:?}");
    }
    // Unknown preset, unknown format, unknown action, no body.
    let answer = post(port, json!({ "action": "start", "capture": CAPTURE, "preset": "ultra" })).await;
    assert_eq!(refused(&answer), (400, "bad_request".to_string()));
    let answer = post(port, json!({ "action": "export", "capture": CAPTURE, "format": "blender" })).await;
    assert_eq!(refused(&answer), (400, "bad_request".to_string()));
    for body in [json!({ "action": "restart" }), json!({ "capture": CAPTURE }), json!([]), json!({ "action": "start" })] {
        let answer = post(port, body.clone()).await;
        assert!((400..500).contains(&answer.status) && answer.json()["code"] == json!("bad_request"), "{body}: {}", answer.text());
    }
    // A live capture shows a moving scene: there is no cloud to refine.
    std::fs::write(dirs.captures().join("Danse.gscap"), Footage::dancer("live", 6).gscap("Danse")).unwrap();
    let answer = post(port, json!({ "action": "start", "capture": "Danse.gscap" })).await;
    assert_eq!(refused(&answer), (400, "unsupported_mode".to_string()));
    // A file that is no capture at all starts, fails, and says why.
    std::fs::write(dirs.captures().join("Faux.gscap"), b"GSCAP1\0\0 this is not a capture").unwrap();
    assert_eq!(post(port, json!({ "action": "start", "capture": "Faux.gscap", "preset": "rapide" })).await.status, 200);
    let (last, _) = wait_for_the_end(port, Duration::from_secs(30)).await;
    assert_eq!((last["outcome"].as_str(), last["error"]["code"].as_str()), (Some("failed"), Some("bad_capture")), "{last}");
    assert_eq!(last["result"], Value::Null);

    // The machine's own page only.
    let foreign = http(
        port,
        "POST",
        "/api/refine",
        &[("content-type", "application/json"), ("origin", "https://evil.example")],
        json!({ "action": "start", "capture": CAPTURE }).to_string().as_bytes(),
    )
    .await;
    assert_eq!(refused(&foreign), (403, "origin_not_allowed".to_string()));
    let rebound = http(port, "GET", "/api/refine", &[("host", "evil.example")], &[]).await;
    assert_eq!(rebound.status, 403);
    for (method, body) in [("GET", String::new()), ("POST", json!({ "action": "start", "capture": CAPTURE }).to_string())] {
        let answer = https(lan(&server), &server.tls(), method, "/api/refine", &[("content-type", "application/json")], body.as_bytes()).await;
        assert_eq!(refused(&answer), (403, "loopback_only".to_string()), "{method}");
    }
    assert!(state(port).await["job"].is_null(), "a refused request started a job");

    // Odd names reach nothing outside the directory of the results.
    std::fs::write(dirs.root.path().join("secret.ply"), b"ply\n").unwrap();
    for target in ["/refined/..%2Fsecret.ply", "/refined/%2e%2e%2fsecret.ply", "/refined/.hidden.ply", "/refined/x.json", "/refined/x.gscap", "/refined/"] {
        let (status, _) = common::raw_get(port, target).await;
        assert_eq!(status, 404, "{target}");
    }

    // A server without any directory has nowhere to put a result.
    let bare = common::start_server(common::small_stage(20.0), |_| {}).await;
    let answer = post(bare.port(), json!({ "action": "start", "capture": CAPTURE })).await;
    assert_eq!(refused(&answer), (404, "not_found".to_string()));
    assert_eq!(get(bare.port(), "/api/refined").await.json(), json!({ "results": [], "dir": null, "datasets_dir": null }));
    bare.handle.shutdown().await;
    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_capture_is_exported_as_a_dataset_for_other_trainers() {
    let (server, dirs) = refine_server(8, |_| {}).await;
    let port = server.port();

    for (format, landmark) in [("colmap", "sparse/0/cameras.txt"), ("nerfstudio", "transforms.json")] {
        let started = post(port, json!({ "action": "export", "capture": CAPTURE, "format": format })).await;
        assert_eq!(started.status, 200, "{}", started.text());
        assert_eq!((started.json()["job"]["kind"].as_str(), started.json()["job"]["format"].as_str()), (Some("export"), Some(format)));
        let (last, _) = wait_for_the_end(port, Duration::from_secs(120)).await;
        assert_eq!((last["outcome"].as_str(), last["kind"].as_str()), (Some("done"), Some("export")), "{last}");
        let dataset = &last["dataset"];
        let name = format!("Chambre-20261114-120000-{format}");
        assert_eq!((dataset["name"].as_str(), dataset["format"].as_str()), (Some(name.as_str()), Some(format)));
        let dir = datasets_dir(&dirs).join(&name);
        assert_eq!(dataset["dir"], json!(dir.display().to_string()));
        assert!(dir.join(landmark).is_file(), "{landmark} is missing");
        let images = std::fs::read_dir(dir.join("images")).unwrap().count();
        assert!(images >= 20, "{images} images");
        assert_eq!(dataset["images"].as_u64(), Some(images as u64));
        assert!(dataset["points"].as_u64().unwrap() > 1000);
    }
    // A second export of the same capture does not write into the first.
    assert_eq!(post(port, json!({ "action": "export", "capture": CAPTURE, "format": "colmap" })).await.status, 200);
    let (last, _) = wait_for_the_end(port, Duration::from_secs(120)).await;
    assert_eq!(last["dataset"]["name"], json!("Chambre-20261114-120000-colmap-2"));
    // Nothing of it is a refined result.
    assert_eq!(get(port, "/api/refined").await.json()["results"], json!([]));
    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_state_of_the_disk_is_told_at_the_machine() {
    let (server, dirs) = refine_server(8, |config| config.max_upload_bytes = u64::MAX).await;
    let port = server.port();

    let status = get(port, "/api/record").await.json();
    let storage = &status["storage"];
    assert_eq!((&status["recording"], &status["last"], &storage["problem"]), (&Value::Null, &Value::Null, &Value::Null));
    if cfg!(unix) {
        let free = storage["free_bytes"].as_u64().expect("no free space reported");
        assert!(free > 0);
        assert_eq!(storage["low"], json!(free < 2 << 30));
        assert_eq!(get(port, "/api/captures").await.json()["free_bytes"].as_u64().map(|f| f > 0), Some(true));
    }
    // Not the business of the network.
    let from_lan = https(lan(&server), &server.tls(), "GET", "/api/record", &[], &[]).await;
    assert_eq!(from_lan.status, 200);
    assert!(from_lan.json().get("storage").is_none());

    // A capture that cannot fit is refused as soon as its first bytes have
    // shown that it is one, before the disk is filled with it.
    if cfg!(unix) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut stream = common::tcp(loopback(port), None).await;
        let head = format!("POST /api/captures HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", 1u64 << 60);
        stream.write_all(head.as_bytes()).await.unwrap();
        stream.write_all(&bedroom()[..4096]).await.unwrap();
        // Nothing more will come: the server need not wait for the body.
        stream.shutdown().await.unwrap();
        let mut answer = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(20), stream.read_to_end(&mut answer)).await;
        let text = String::from_utf8_lossy(&answer);
        assert!(text.starts_with("HTTP/1.1 507"), "{text}");
        assert!(text.contains("\"code\":\"disk_full\"") && text.contains("\"free_bytes\":"), "{text}");
        assert_eq!(Dirs::files(&dirs.captures(), ""), vec![CAPTURE.to_string()], "something of the refused upload was kept");
    }
    server.handle.shutdown().await;
}
