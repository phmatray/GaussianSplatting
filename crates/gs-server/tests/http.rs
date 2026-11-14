//! The HTTP side: info JSON, LAN switch, QR code, static files, origin
//! rules and CORS.

mod common;

use std::collections::BTreeSet;
use std::io::Read;
use std::net::{IpAddr, SocketAddr};
use std::path::Path;

use common::*;
use gs_server::ServerConfig;
use http_body_util::{BodyExt, Empty};
use hyper::body::Bytes;
use hyper_util::rt::{TokioExecutor, TokioIo};
use serde_json::{json, Value};

const JSON: (&str, &str) = ("content-type", "application/json");
const SECRET: &str = "TOP-SECRET-OUTSIDE-THE-DIST";

fn keys(value: &Value) -> BTreeSet<String> {
    value.as_object().expect("not an object").keys().cloned().collect()
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|n| n.to_string()).collect()
}

async fn post_lan(port: u16, enabled: bool, headers: &[(&str, &str)]) -> HttpResponse {
    let mut all = vec![JSON];
    all.extend_from_slice(headers);
    http(port, "POST", "/api/lan", &all, json!({ "enabled": enabled }).to_string().as_bytes()).await
}

fn is_fingerprint(text: &str) -> bool {
    let parts: Vec<&str> = text.split(':').collect();
    parts.len() == 32
        && parts.iter().all(|p| p.len() == 2 && p.bytes().all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b)))
}

/// A web build like Trunk's, with a file that must never be served next to it.
fn fake_dist() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let dist = root.path().join("dist");
    std::fs::create_dir_all(dist.join("sub")).unwrap();
    std::fs::write(root.path().join("secret.txt"), SECRET).unwrap();
    std::fs::write(dist.join("index.html"), "<!doctype html><title>Fake app</title><p>INDEX-PAGE</p>").unwrap();
    std::fs::write(dist.join("app-0123456789abcdef.js"), format!("// {}\nexport default 1;\n", "x".repeat(4000)))
        .unwrap();
    std::fs::write(dist.join("app-0123456789abcdef_bg.wasm"), b"\0asm\x01\0\0\0").unwrap();
    std::fs::write(dist.join("style-fedcba9876543210.css"), "body { margin: 0 }").unwrap();
    std::fs::write(dist.join("worker.js"), "self.onmessage = () => {};").unwrap();
    std::fs::write(dist.join("sub/data.json"), "{\"a\": 1}").unwrap();
    std::fs::write(dist.join(".hidden"), "HIDDEN-FILE").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(root.path().join("secret.txt"), dist.join("leak.txt")).unwrap();
    root
}

async fn server_with_dist(dist: &Path) -> TestServer {
    let dist = dist.to_path_buf();
    start_server(small_stage(30.0), move |config| config.dist = Some(dist)).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn info_json_has_exactly_the_specified_shape() {
    let server = start_server(small_stage(30.0), |_| {}).await;
    let port = server.port();
    let response = get(port, "/api/info").await;
    assert_eq!(response.status, 200);
    assert_eq!(response.header("content-type"), Some("application/json"));
    assert_eq!(response.header("cache-control"), Some("no-store"));
    let info = response.json();
    assert_eq!(keys(&info), set(&["name", "version", "protocol", "http_url", "ws_path", "lan", "clients"]));
    assert_eq!(keys(&info["lan"]), set(&["enabled", "urls", "fingerprint"]));
    assert_eq!(
        info,
        json!({
            "name": "splat-live",
            "version": "0.1.0",
            "protocol": 1,
            "http_url": format!("http://127.0.0.1:{port}"),
            "ws_path": "/ws",
            "lan": { "enabled": false, "urls": [], "fingerprint": "" },
            "clients": 0
        })
    );
    // The Rust API reports the same thing.
    assert_eq!(serde_json::to_value(server.handle.info()).unwrap(), info);

    // `clients` counts WebSocket viewers.
    let mut a = Client::connect(port).await;
    let b = Client::connect(port).await;
    a.snapshot().await;
    assert_eq!(get(port, "/api/info").await.json()["clients"], 2);
    drop(b);
    eventually("one viewer left", || server.handle.info().clients == 1).await;
    assert_eq!(get(port, "/api/info").await.json()["clients"], 1);
    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lan_listener_opens_and_closes_on_request() {
    let server = start_server(small_stage(30.0), |_| {}).await;
    let port = server.port();
    assert!(server.handle.lan_addr().is_none());
    assert_eq!(get(port, "/api/qr.svg").await.status, 404, "no QR code while LAN is off");

    let mut first: Option<(u16, String)> = None;
    for round in 0..3 {
        // On.
        let response = post_lan(port, true, &[]).await;
        assert_eq!(response.status, 200, "{}", response.text());
        let info = response.json();
        assert_eq!(keys(&info), set(&["name", "version", "protocol", "http_url", "ws_path", "lan", "clients"]));
        assert_eq!(info["lan"]["enabled"], true);
        let fingerprint = info["lan"]["fingerprint"].as_str().unwrap().to_string();
        assert!(is_fingerprint(&fingerprint), "odd fingerprint {fingerprint}");
        let lan_port = server.lan_port();
        let urls: Vec<String> =
            info["lan"]["urls"].as_array().unwrap().iter().map(|u| u.as_str().unwrap().to_string()).collect();
        let expected: Vec<String> = gs_server::lan_ipv4().iter().map(|ip| format!("https://{ip}:{lan_port}")).collect();
        assert_eq!(urls, expected, "one URL per LAN address, in order");
        // The same port and certificate every time: a headset keeps its
        // bookmark and its trust.
        match &first {
            None => first = Some((lan_port, fingerprint.clone())),
            Some(first) => assert_eq!((lan_port, &fingerprint), (first.0, &first.1), "round {round}"),
        }
        // Asking again changes nothing.
        assert_eq!(post_lan(port, true, &[]).await.json()["lan"], info["lan"]);
        assert_eq!(server.lan_port(), lan_port);

        // The listener really is there: TLS with a client that trusts only
        // the stored certificate, on loopback and on every LAN address.
        let tls = server.tls();
        let lan_loopback = loopback(lan_port);
        let mut addresses = vec![lan_loopback];
        addresses.extend(gs_server::lan_ipv4().into_iter().map(|ip| SocketAddr::new(IpAddr::V4(ip), lan_port)));
        for addr in &addresses {
            let stream = tls_stream(*addr, &tls, &[b"http/1.1"]).await.unwrap_or_else(|e| panic!("TLS to {addr}: {e}"));
            let presented = stream.get_ref().1.peer_certificates().unwrap()[0].to_vec();
            assert_eq!(gs_server::fingerprint(&presented), fingerprint, "/api/info announces another certificate");
            let over_tls = request_on(Box::new(stream), "GET", &addr.to_string(), "/api/info", &[], &[]).await;
            assert_eq!(over_tls.json()["lan"], info["lan"]);
            assert_eq!(over_tls.json()["http_url"], info["http_url"]);
        }

        // The certificate also names `localhost`, and nothing else.
        let named = |name: &'static str| rustls::pki_types::ServerName::try_from(name).unwrap();
        assert!(tls_stream_as(lan_loopback, named("localhost"), &tls, &[]).await.is_ok());
        assert!(tls_stream_as(lan_loopback, named("splat.example"), &tls, &[]).await.is_err());
        assert!(tls_stream_as(lan_loopback, named("10.250.250.250"), &tls, &[]).await.is_err());

        // WSS over the self-signed certificate.
        let mut viewer = Client::connect_tls(lan_loopback, &tls).await.expect("wss handshake");
        viewer.snapshot().await;
        viewer.until("a live layer over wss", |_, m| m.is_dynamic()).await;
        assert_eq!(server.handle.info().clients, 1);

        // The QR code is the first URL.
        let qr = get(port, "/api/qr.svg").await;
        assert_eq!(qr.status, 200);
        assert_eq!(qr.header("content-type"), Some("image/svg+xml"));
        let svg = qr.text();
        assert!(
            svg.starts_with("<?xml") && svg.contains("<svg") && svg.trim_end().ends_with("</svg>"),
            "not an SVG document"
        );
        if let Some(url) = urls.first() {
            assert_eq!(svg, gs_server::qr_svg(url).unwrap());
            assert_eq!(get(port, "/api/qr.svg?i=0").await.body, qr.body);
            assert_eq!(https(lan_loopback, &tls, "GET", "/api/qr.svg", &[], &[]).await.body, qr.body);
        }
        assert_eq!(get(port, &format!("/api/qr.svg?i={}", urls.len())).await.status, 404);
        assert_eq!(get(port, "/api/qr.svg?i=nope").await.status, 400);

        // The switch cannot be operated from the network.
        for enabled in [false, true] {
            let body = json!({ "enabled": enabled }).to_string();
            let refused = https(lan_loopback, &tls, "POST", "/api/lan", &[JSON], body.as_bytes()).await;
            assert_eq!(refused.status, 403, "POST /api/lan must be refused on the LAN listener");
        }
        assert!(server.handle.info().lan.enabled);

        // Off: the viewer is told to go, the port closes.
        let response = post_lan(port, false, &[]).await;
        assert_eq!(response.status, 200);
        assert_eq!(response.json()["lan"], json!({ "enabled": false, "urls": [], "fingerprint": "" }));
        assert_eq!(viewer.closed().await.map(|(code, _)| code), Some(1001));
        assert!(!accepts(lan_loopback).await, "the LAN port still accepts connections");
        assert!(server.handle.lan_addr().is_none());
        assert_eq!(get(port, "/api/qr.svg").await.status, 404);
        eventually("the wss viewer to be gone", || server.handle.info().clients == 0).await;
        assert_eq!(post_lan(port, false, &[]).await.json()["lan"]["enabled"], false);
    }

    // Malformed requests change nothing.
    for (headers, body, status) in [
        (vec![JSON], "{\"enabled\": \"yes\"}", 422),
        (vec![JSON], "{}", 422),
        (vec![JSON], "not json", 400),
        (vec![], "{\"enabled\": true}", 415),
        (vec![("content-type", "text/plain")], "{\"enabled\": true}", 415),
    ] {
        let response = http(port, "POST", "/api/lan", &headers, body.as_bytes()).await;
        assert_eq!(response.status, status, "body {body:?}");
        assert!(response.json()["error"].is_string());
    }
    assert_eq!(get(port, "/api/lan").await.status, 405);
    assert!(!server.handle.info().lan.enabled);
    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lan_at_start_and_certificate_kept_between_runs() {
    let certs = tempfile::tempdir().unwrap();
    let config = |dir: &Path| ServerConfig {
        pipeline: small_stage(30.0),
        port: 0,
        lan_port: 0,
        lan: true,
        cert_dir: Some(dir.to_path_buf()),
        ..ServerConfig::default()
    };
    let first = gs_server::start(config(certs.path())).await.unwrap();
    let info = first.info();
    assert!(info.lan.enabled && is_fingerprint(&info.lan.fingerprint));
    let lan = first.lan_addr().unwrap();
    assert!(lan.ip().is_unspecified(), "the LAN listener must listen on every interface");
    assert!(accepts(loopback(lan.port())).await);
    first.shutdown().await;
    assert!(!accepts(loopback(lan.port())).await);

    // A second run with the same directory serves the same certificate.
    let second = gs_server::start(config(certs.path())).await.unwrap();
    assert_eq!(second.info().lan.fingerprint, info.lan.fingerprint);
    let tls = tls_trusting(&certs.path().join("cert.pem"));
    let lan = loopback(second.lan_addr().unwrap().port());
    assert_eq!(https(lan, &tls, "GET", "/api/info", &[], &[]).await.status, 200);
    second.shutdown().await;

    // Another directory, another certificate; the first client must not
    // trust it.
    let other = tempfile::tempdir().unwrap();
    let third = gs_server::start(config(other.path())).await.unwrap();
    assert_ne!(third.info().lan.fingerprint, info.lan.fingerprint);
    let lan = loopback(third.lan_addr().unwrap().port());
    assert!(tls_stream(lan, &tls, &[]).await.is_err(), "a certificate that was never stored was trusted");
    third.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lan_speaks_http2_and_plain_http_is_not_served_there() {
    let server = start_server(small_stage(30.0), |config| config.lan = true).await;
    let lan = loopback(server.lan_port());
    let stream = tls_stream(lan, &server.tls(), &[b"h2", b"http/1.1"]).await.unwrap();
    assert_eq!(stream.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));
    let (mut sender, connection) =
        hyper::client::conn::http2::handshake(TokioExecutor::new(), TokioIo::new(stream)).await.unwrap();
    tokio::spawn(connection);
    for path in ["/api/info", "/"] {
        let request = hyper::Request::get(format!("https://{lan}{path}")).body(Empty::<Bytes>::new()).unwrap();
        let response = sender.send_request(request).await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.version(), hyper::Version::HTTP_2);
        assert!(!response.into_body().collect().await.unwrap().to_bytes().is_empty());
    }
    // No plain HTTP on the TLS port, and no TLS port on... the plain one.
    let (status, _) = raw_get(lan.port(), "/api/info").await;
    assert_ne!(status, 200);
    assert!(tls_stream(loopback(server.port()), &server.tls(), &[]).await.is_err());
    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn busy_ports_fall_back_to_the_next_free_ones() {
    let config = || ServerConfig {
        pipeline: small_stage(30.0),
        port: 7850,
        lan_port: 7851,
        lan: true,
        ..ServerConfig::default()
    };
    let first = gs_server::start(config()).await.unwrap();
    let second = gs_server::start(config()).await.unwrap();
    let ports = |handle: &gs_server::ServerHandle| (handle.http_addr().port(), handle.lan_addr().unwrap().port());
    let (a, b) = (ports(&first), ports(&second));
    assert!(a.0 >= 7850 && a.1 >= 7851 && a.0 != a.1);
    assert!(b.0 > a.0 && b.1 > a.1 && b.0 != b.1, "second server on {b:?}, first on {a:?}");
    let all = BTreeSet::from([a.0, a.1, b.0, b.1]);
    assert_eq!(all.len(), 4, "two listeners share a port: {a:?} {b:?}");
    // What is reported is what answers.
    for (handle, (http_port, lan_port)) in [(&first, a), (&second, b)] {
        let info = handle.info();
        assert_eq!(info.http_url, format!("http://127.0.0.1:{http_port}"));
        assert_eq!(get(http_port, "/api/info").await.json()["http_url"], info.http_url);
        assert!(info.lan.urls.iter().all(|url| url.ends_with(&format!(":{lan_port}"))));
        assert!(accepts(loopback(lan_port)).await);
    }
    first.shutdown().await;
    second.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn static_files_mime_types_caching_and_spa_fallback() {
    let root = fake_dist();
    let dist = root.path().join("dist");
    let server = server_with_dist(&dist).await;
    let port = server.port();

    // The index, under both names, never cached without revalidation.
    for path in ["/", "/index.html"] {
        let response = get(port, path).await;
        assert_eq!(response.status, 200);
        assert!(response.text().contains("INDEX-PAGE"));
        assert_eq!(response.header("content-type"), Some("text/html; charset=utf-8"));
        assert_eq!(response.header("cache-control"), Some("no-cache"));
        assert_eq!(response.header("x-content-type-options"), Some("nosniff"));
        assert!(response.header("etag").is_some());
    }

    // MIME types, wasm first: browsers refuse to stream-compile anything else.
    let wasm = get(port, "/app-0123456789abcdef_bg.wasm").await;
    assert_eq!(wasm.status, 200);
    assert_eq!(wasm.header("content-type"), Some("application/wasm"));
    assert_eq!(wasm.body, b"\0asm\x01\0\0\0");
    // Content-hashed names never change: cached for good.
    assert_eq!(wasm.header("cache-control"), Some("public, max-age=31536000, immutable"));
    let js = get(port, "/app-0123456789abcdef.js").await;
    assert_eq!(js.header("content-type"), Some("text/javascript; charset=utf-8"));
    assert_eq!(js.header("cache-control"), Some("public, max-age=31536000, immutable"));
    assert_eq!(get(port, "/style-fedcba9876543210.css").await.header("content-type"), Some("text/css; charset=utf-8"));
    let data = get(port, "/sub/data.json").await;
    assert_eq!(data.header("content-type"), Some("application/json; charset=utf-8"));
    assert_eq!(data.text(), "{\"a\": 1}");
    // The worker script has no hash in its name: always revalidated.
    let worker = get(port, "/worker.js").await;
    assert_eq!(worker.header("cache-control"), Some("no-cache"));

    // Revalidation is cheap.
    let etag = worker.header("etag").unwrap().to_string();
    let unchanged = http(port, "GET", "/worker.js", &[("if-none-match", &etag)], &[]).await;
    assert_eq!(unchanged.status, 304);
    assert!(unchanged.body.is_empty());
    std::fs::write(dist.join("worker.js"), "self.onmessage = () => { /* rebuilt */ };").unwrap();
    let rebuilt = http(port, "GET", "/worker.js", &[("if-none-match", &etag)], &[]).await;
    assert_eq!(rebuilt.status, 200);
    assert!(rebuilt.text().contains("rebuilt"));

    // HEAD, and methods that make no sense on a file.
    let head = http(port, "HEAD", "/app-0123456789abcdef.js", &[], &[]).await;
    assert_eq!(head.status, 200);
    assert!(head.body.is_empty());
    assert_eq!(head.header("content-length"), Some(js.body.len().to_string().as_str()));
    for method in ["POST", "PUT", "DELETE"] {
        assert_eq!(http(port, method, "/index.html", &[], b"x").await.status, 405);
    }

    // Compression for those who ask.
    let gzip = http(port, "GET", "/app-0123456789abcdef.js", &[("accept-encoding", "gzip")], &[]).await;
    assert_eq!(gzip.header("content-encoding"), Some("gzip"));
    assert!(gzip.body.len() < js.body.len() / 4, "gzip did not shrink the script");
    let mut unpacked = Vec::new();
    flate2::read::GzDecoder::new(&gzip.body[..]).read_to_end(&mut unpacked).unwrap();
    assert_eq!(unpacked, js.body);
    let brotli = http(port, "GET", "/app-0123456789abcdef.js", &[("accept-encoding", "br")], &[]).await;
    assert_eq!(brotli.header("content-encoding"), Some("br"));
    assert!(brotli.body.len() < js.body.len() / 4);
    assert_eq!(js.header("content-encoding"), None);

    // Client-side routes get the application; missing files do not.
    for route in ["/scan", "/a/b/c", "/sub/", "/deep/route/"] {
        let response = get(port, route).await;
        assert_eq!(response.status, 200, "{route}");
        assert!(response.text().contains("INDEX-PAGE"), "{route}");
        assert_eq!(response.header("cache-control"), Some("no-cache"));
    }
    for missing in
        ["/missing.wasm", "/app-ffffffffffffffff.js", "/sub/none.json", "/favicon.ico", "/api/unknown", "/api/"]
    {
        assert_eq!(get(port, missing).await.status, 404, "{missing}");
    }

    // The build directory may be replaced while the server runs.
    std::fs::remove_file(dist.join("index.html")).unwrap();
    let placeholder = get(port, "/").await;
    assert_eq!(placeholder.status, 200);
    assert!(placeholder.text().contains("trunk build --release") && !placeholder.text().contains("INDEX-PAGE"));
    std::fs::write(dist.join("index.html"), "<p>INDEX-PAGE v2</p>").unwrap();
    assert!(get(port, "/").await.text().contains("v2"));
    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn path_traversal_is_refused() {
    let root = fake_dist();
    let server = server_with_dist(&root.path().join("dist")).await;
    let port = server.port();
    // Sanity: the file exists right above the served directory.
    assert_eq!(std::fs::read_to_string(root.path().join("secret.txt")).unwrap(), SECRET);

    let attempts = [
        "/../secret.txt",
        "/..",
        "/../",
        "/sub/../../secret.txt",
        "/sub/../../../../../../etc/passwd",
        "/%2e%2e/secret.txt",
        "/%2E%2E/secret.txt",
        "/.%2e/secret.txt",
        "/%2e./secret.txt",
        "/..%2fsecret.txt",
        "/..%2Fsecret.txt",
        "/%2e%2e%2fsecret.txt",
        "/sub/..%2f..%2fsecret.txt",
        "/sub/%2e%2e/%2e%2e/secret.txt",
        "/..%5csecret.txt",
        "/sub/..\\..\\secret.txt",
        "//../secret.txt",
        "/./../secret.txt",
        "/sub/./../../secret.txt",
        "/%00/../secret.txt",
        "/sub/%2e%2e%2f%2e%2e%2fsecret.txt",
        "/..;/secret.txt",
        "/....//secret.txt",
        "/%c0%ae%c0%ae/secret.txt",
        "/%252e%252e/secret.txt",
        "/%252e%252e%252fsecret.txt",
        "/leak.txt",
        "/.hidden",
        "/sub/../.hidden",
        "/C:/Windows/win.ini",
        "/index.html/../../secret.txt",
        "http://127.0.0.1/../secret.txt",
    ];
    for target in attempts {
        let (status, answer) = raw_get(port, target).await;
        assert!(!answer.contains(SECRET), "{target} leaked the file outside the dist");
        assert!(!answer.contains("HIDDEN-FILE"), "{target} served a hidden file");
        assert!(!answer.contains("root:"), "{target} served /etc/passwd");
        assert!(matches!(status, 400 | 404), "{target} answered {status}");
    }
    // A path that only looks dangerous: it stays inside the dist, where it
    // names no file, so it is a client-side route like any other.
    let (status, answer) = raw_get(port, "/etc/passwd").await;
    assert!(status == 200 && answer.contains("INDEX-PAGE") && !answer.contains("root:"));
    // The server is still fine afterwards.
    assert_eq!(get(port, "/").await.status, 200);
    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn placeholder_page_without_a_frontend() {
    // No directory given and (in a default build) nothing compiled in.
    let server = start_server(small_stage(30.0), |_| {}).await;
    let port = server.port();
    if gs_server::has_embedded_frontend() {
        // Built with `embed-dist` and a web build: that build is served.
        let index = get(port, "/").await;
        assert_eq!(index.status, 200);
        assert!(!index.text().contains("trunk build --release"));
        assert_eq!(get(port, "/some/route").await.body, index.body);
    } else {
        for path in ["/", "/index.html", "/some/route"] {
            let page = get(port, path).await;
            assert_eq!(page.status, 200);
            assert_eq!(page.header("content-type"), Some("text/html; charset=utf-8"));
            assert_eq!(page.header("cache-control"), Some("no-cache"));
            let text = page.text();
            assert!(
                text.contains("lang=\"fr\"") && text.contains("trunk build --release"),
                "not the French placeholder"
            );
            assert!(!text.contains('\u{2014}'), "em dash in user-facing text");
        }
        assert_eq!(get(port, "/app.wasm").await.status, 404);
    }
    // A directory that does not exist behaves the same way, and the API
    // works regardless.
    let nowhere = server_with_dist(Path::new("/nonexistent/splat-live/dist")).await;
    let page = get(nowhere.port(), "/").await;
    assert_eq!(page.status, 200);
    assert!(page.text().contains("trunk build --release"));
    assert_eq!(get(nowhere.port(), "/missing.js").await.status, 404);
    assert_eq!(get(nowhere.port(), "/api/info").await.status, 200);
    let mut viewer = Client::connect(nowhere.port()).await;
    viewer.snapshot().await;
    nowhere.handle.shutdown().await;
    server.handle.shutdown().await;
}

/// The status of a WebSocket handshake sent to the loopback listener with
/// the given `Host` (taken from `url`) and `Origin`.
async fn ws_status(port: u16, url: &str, origin: Option<&str>) -> u16 {
    let headers: Vec<(&str, &str)> = origin.map(|o| ("origin", o)).into_iter().collect();
    let io: BoxIo = Box::new(tcp(loopback(port), None).await);
    match handshake(io, url, &headers).await {
        Ok(_) => 101,
        Err(tokio_tungstenite::tungstenite::Error::Http(response)) => response.status().as_u16(),
        Err(other) => panic!("handshake failed without an HTTP status: {other}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn origin_rules_on_websocket_and_post() {
    let server = start_server(small_stage(30.0), |config| config.lan = true).await;
    let port = server.port();
    let own = format!("http://127.0.0.1:{port}");
    let own_localhost = format!("http://localhost:{port}");
    let ws = format!("ws://127.0.0.1:{port}/ws");
    let ws_localhost = format!("ws://localhost:{port}/ws");

    // Accepted: no Origin (not a browser), the server's own origin under
    // either loopback name, and the Tauri webview.
    assert_eq!(ws_status(port, &ws, None).await, 101);
    assert_eq!(ws_status(port, &ws, Some(&own)).await, 101);
    assert_eq!(ws_status(port, &ws_localhost, Some(&own_localhost)).await, 101);
    assert_eq!(ws_status(port, &ws, Some(&own.to_uppercase().replace("HTTP", "http"))).await, 101);
    for tauri in ["tauri://localhost", "http://tauri.localhost", "TAURI://LOCALHOST"] {
        assert_eq!(ws_status(port, &ws, Some(tauri)).await, 101, "{tauri}");
    }

    // Refused: every other site, including look-alikes.
    let other_port = format!("http://127.0.0.1:{}", port.wrapping_add(1));
    let wrong_scheme = format!("https://127.0.0.1:{port}");
    let foreign = [
        "http://evil.example",
        "https://evil.example",
        "null",
        "",
        "file://",
        "http://127.0.0.1",
        other_port.as_str(),
        wrong_scheme.as_str(),
        "https://tauri.localhost",
        "tauri://localhost.evil.example",
        "http://tauri.localhost.evil.example",
        "http://tauri.localhost:8080",
        "http://localhost.evil.example:7878",
    ];
    for origin in foreign {
        assert_eq!(ws_status(port, &ws, Some(origin)).await, 403, "WebSocket from {origin:?}");
        let response = post_lan(port, false, &[("origin", origin)]).await;
        assert_eq!(response.status, 403, "POST from {origin:?}");
    }
    // The loopback name used does not matter, as long as both sides agree.
    assert_eq!(ws_status(port, &ws, Some(&own_localhost)).await, 403);
    // A page that points its own domain at 127.0.0.1 (DNS rebinding) is
    // "same host" by name only.
    let rebound = format!("http://evil.example:{port}");
    assert_eq!(ws_status(port, &format!("ws://evil.example:{port}/ws"), Some(&rebound)).await, 403);
    let io: BoxIo = Box::new(tcp(loopback(port), None).await);
    let body = json!({ "enabled": false }).to_string();
    let response = request_on(
        io,
        "POST",
        &format!("evil.example:{port}"),
        "/api/lan",
        &[JSON, ("origin", &rebound)],
        body.as_bytes(),
    )
    .await;
    assert_eq!(response.status, 403);
    assert!(server.handle.info().lan.enabled, "a refused POST switched LAN off");

    // Accepted POSTs.
    for origin in [own.as_str(), "tauri://localhost", "http://tauri.localhost"] {
        assert_eq!(post_lan(port, true, &[("origin", origin)]).await.status, 200, "POST from {origin}");
    }
    // Reading the info needs no permission (CORS decides who may see it).
    assert_eq!(http(port, "GET", "/api/info", &[("origin", "http://evil.example")], &[]).await.status, 200);

    // The same rules on the LAN listener, with its own scheme and host.
    let lan = loopback(server.lan_port());
    let tls = server.tls();
    let wss = format!("wss://{lan}/ws");
    let lan_status = |origin: Option<String>| {
        let (tls, wss) = (tls.clone(), wss.clone());
        async move {
            let headers: Vec<(&str, &str)> = origin.as_deref().map(|o| ("origin", o)).into_iter().collect();
            let io: BoxIo = Box::new(tls_stream(lan, &tls, &[b"http/1.1"]).await.unwrap());
            match handshake(io, &wss, &headers).await {
                Ok(_) => 101,
                Err(tokio_tungstenite::tungstenite::Error::Http(response)) => response.status().as_u16(),
                Err(other) => panic!("{other}"),
            }
        }
    };
    assert_eq!(lan_status(None).await, 101);
    assert_eq!(lan_status(Some(format!("https://{lan}"))).await, 101);
    assert_eq!(lan_status(Some("tauri://localhost".into())).await, 101);
    assert_eq!(lan_status(Some(format!("http://{lan}"))).await, 403, "wrong scheme");
    assert_eq!(lan_status(Some("https://evil.example".into())).await, 403);
    assert_eq!(lan_status(Some(own.clone())).await, 403, "the loopback origin is another origin");
    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn extra_origins_are_opt_in() {
    let dev = "http://127.0.0.1:8100";
    let plain = start_server(small_stage(30.0), |_| {}).await;
    let ws = format!("ws://127.0.0.1:{}/ws", plain.port());
    assert_eq!(ws_status(plain.port(), &ws, Some(dev)).await, 403);
    assert_eq!(
        http(plain.port(), "GET", "/api/info", &[("origin", dev)], &[]).await.header("access-control-allow-origin"),
        None
    );
    plain.handle.shutdown().await;

    let server = start_server(small_stage(30.0), |config| config.extra_origins = vec![dev.to_string()]).await;
    let port = server.port();
    assert_eq!(ws_status(port, &format!("ws://127.0.0.1:{port}/ws"), Some(dev)).await, 101);
    assert_eq!(post_lan(port, false, &[("origin", dev)]).await.status, 200);
    assert_eq!(
        http(port, "GET", "/api/info", &[("origin", dev)], &[]).await.header("access-control-allow-origin"),
        Some(dev)
    );
    assert_eq!(ws_status(port, &format!("ws://127.0.0.1:{port}/ws"), Some("http://127.0.0.1:8101")).await, 403);
    server.handle.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cors_allows_only_the_tauri_origins_and_only_on_loopback() {
    let server = start_server(small_stage(30.0), |config| config.lan = true).await;
    let port = server.port();

    for origin in ["tauri://localhost", "http://tauri.localhost"] {
        // Simple request.
        let response = http(port, "GET", "/api/info", &[("origin", origin)], &[]).await;
        assert_eq!(response.status, 200);
        assert_eq!(response.header("access-control-allow-origin"), Some(origin));
        // Preflight of the JSON POST.
        let preflight = http(
            port,
            "OPTIONS",
            "/api/lan",
            &[
                ("origin", origin),
                ("access-control-request-method", "POST"),
                ("access-control-request-headers", "content-type"),
            ],
            &[],
        )
        .await;
        assert!(matches!(preflight.status, 200 | 204), "preflight answered {}", preflight.status);
        assert_eq!(preflight.header("access-control-allow-origin"), Some(origin));
        assert!(preflight.header("access-control-allow-methods").unwrap().contains("POST"));
        assert!(preflight
            .header("access-control-allow-headers")
            .unwrap()
            .to_ascii_lowercase()
            .contains("content-type"));
        // And the POST itself.
        let posted = post_lan(port, true, &[("origin", origin)]).await;
        assert_eq!(posted.status, 200);
        assert_eq!(posted.header("access-control-allow-origin"), Some(origin));
        // Static files and the QR code too (the webview shows it).
        let qr = http(port, "GET", "/api/qr.svg", &[("origin", origin)], &[]).await;
        assert_eq!(qr.header("access-control-allow-origin"), Some(origin));
    }
    // Credentials are never allowed, and no wildcard is ever sent.
    let response = http(port, "GET", "/api/info", &[("origin", "tauri://localhost")], &[]).await;
    assert_eq!(response.header("access-control-allow-credentials"), None);

    for origin in ["http://evil.example", "null", "https://tauri.localhost", "http://localhost:3000"] {
        let response = http(port, "GET", "/api/info", &[("origin", origin)], &[]).await;
        assert_eq!(response.header("access-control-allow-origin"), None, "{origin} may read the API");
        let preflight = http(
            port,
            "OPTIONS",
            "/api/lan",
            &[
                ("origin", origin),
                ("access-control-request-method", "POST"),
                ("access-control-request-headers", "content-type"),
            ],
            &[],
        )
        .await;
        assert_eq!(preflight.header("access-control-allow-origin"), None, "{origin} passed the preflight");
    }

    // The LAN listener grants nothing cross-origin, Tauri or not.
    let lan = loopback(server.lan_port());
    for origin in ["tauri://localhost", "http://tauri.localhost", "https://evil.example"] {
        let response = https(lan, &server.tls(), "GET", "/api/info", &[("origin", origin)], &[]).await;
        assert_eq!(response.status, 200);
        assert_eq!(
            response.header("access-control-allow-origin"),
            None,
            "the LAN listener sent CORS headers to {origin}"
        );
    }
    server.handle.shutdown().await;
}

/// Only with `--features embed-dist`. When the build was pointed at a
/// directory with `GS_DIST_DIR`, every file of that directory must be
/// served byte for byte.
#[cfg(feature = "embed-dist")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn embedded_frontend_is_served_like_a_directory() {
    if !gs_server::has_embedded_frontend() {
        eprintln!("embed-dist is on but no web build was found at compile time: nothing to check");
        return;
    }
    let server = start_server(small_stage(30.0), |_| {}).await;
    let port = server.port();

    let index = get(port, "/").await;
    assert_eq!(index.status, 200);
    assert_eq!(index.header("content-type"), Some("text/html; charset=utf-8"));
    assert_eq!(index.header("cache-control"), Some("no-cache"));
    assert!(!index.text().contains("trunk build --release"), "the placeholder is served instead of the embedded build");
    assert_eq!(get(port, "/index.html").await.body, index.body);
    assert_eq!(get(port, "/some/route").await.body, index.body);
    let etag = index.header("etag").expect("no ETag on an embedded file").to_string();
    assert_eq!(http(port, "GET", "/", &[("if-none-match", &etag)], &[]).await.status, 304);
    assert_eq!(get(port, "/missing.wasm").await.status, 404);
    assert_eq!(raw_get(port, "/../Cargo.toml").await.0, 400);
    assert_eq!(raw_get(port, "/%2e%2e/Cargo.toml").await.0, 400);

    if let Some(dir) = option_env!("GS_DIST_DIR") {
        let mut checked = 0;
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !entry.path().is_file() || name.starts_with('.') {
                continue;
            }
            let served = get(port, &format!("/{name}")).await;
            assert_eq!(served.status, 200, "{name}");
            assert_eq!(
                served.body,
                std::fs::read(entry.path()).unwrap(),
                "{name} differs from the file that was embedded"
            );
            if name.ends_with(".wasm") {
                assert_eq!(served.header("content-type"), Some("application/wasm"));
            }
            checked += 1;
        }
        assert!(checked >= 2, "only {checked} embedded files checked");
        eprintln!("embedded: {checked} files of {dir} served byte for byte");
    }
    server.handle.shutdown().await;
}
