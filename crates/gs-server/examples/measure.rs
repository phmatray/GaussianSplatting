//! Measures a real `gs-server` process from the outside.
//!
//! ```text
//! cargo build --release -p gs-server
//! cargo run --release -p gs-server --example measure -- [path/to/gs-server] [--port 7890]
//! ```
//!
//! 1. Default stage with 0, 1, 3 and 8 viewers: CPU time of the server
//!    process (from `/proc`, Linux only), pipeline tick rate and the rate
//!    of live layers each viewer receives.
//! 2. Default house: the scan is run to its end, then the time a late
//!    joiner needs to receive the finished house is measured, over `ws://`
//!    and over `wss://` with the self-signed certificate.
//!
//! The viewers run in this process and do not decode splats, so their own
//! cost stays out of the server's numbers.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use gs_core::codec::{self, Body, Kind};
use gs_core::pipeline::{LAYER_DYNAMIC, LAYER_STATIC};
use gs_core::protocol::{ScanState, Stats};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
type Ws = WebSocketStream<Box<dyn Io>>;

/// The server process; killed when dropped.
struct Server {
    child: Child,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    async fn spawn(binary: &PathBuf, port: u16, extra: &[&str]) -> Self {
        let child = Command::new(binary)
            .args(["--port", &port.to_string()])
            .args(extra)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|e| panic!("cannot start {}: {e}", binary.display()));
        let server = Self { child };
        let start = Instant::now();
        while http_get(port, "/api/info").await.is_none() {
            assert!(start.elapsed() < Duration::from_secs(20), "the server did not come up");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        server
    }

    /// User plus system CPU seconds consumed so far, and resident bytes.
    fn usage(&self) -> (f64, u64) {
        let pid = self.child.id();
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        // Fields after the command name, which may contain spaces.
        let fields: Vec<&str> = stat.rsplit(')').next().unwrap_or("").split_whitespace().collect();
        let ticks = |i: usize| fields.get(i).and_then(|f| f.parse::<f64>().ok()).unwrap_or(0.0);
        // utime and stime are fields 14 and 15 of the file, in clock ticks
        // (100 per second on Linux).
        let cpu = (ticks(11) + ticks(12)) / 100.0;
        let rss_pages = fields.get(21).and_then(|f| f.parse::<u64>().ok()).unwrap_or(0);
        (cpu, rss_pages * 4096)
    }
}

async fn http_get(port: u16, path: &str) -> Option<String> {
    let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await.ok()?;
    let request = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.ok()?;
    let mut answer = String::new();
    stream.read_to_string(&mut answer).await.ok()?;
    answer.starts_with("HTTP/1.1 200").then(|| answer.split("\r\n\r\n").nth(1).unwrap_or("").to_string())
}

fn limits() -> WebSocketConfig {
    WebSocketConfig::default().max_message_size(Some(256 << 20)).max_frame_size(Some(256 << 20))
}

async fn connect(port: u16) -> Ws {
    let stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).await.expect("connect");
    stream.set_nodelay(true).unwrap();
    let io: Box<dyn Io> = Box::new(stream);
    tokio_tungstenite::client_async_with_config(format!("ws://127.0.0.1:{port}/ws"), io, Some(limits()))
        .await
        .expect("handshake")
        .0
}

async fn connect_tls(addr: SocketAddr, tls: &Arc<rustls::ClientConfig>) -> Ws {
    let stream = TcpStream::connect(addr).await.expect("connect");
    stream.set_nodelay(true).unwrap();
    let connector = tokio_rustls::TlsConnector::from(tls.clone());
    let stream = connector.connect(ServerName::IpAddress(addr.ip().into()), stream).await.expect("TLS handshake");
    let io: Box<dyn Io> = Box::new(stream);
    tokio_tungstenite::client_async_with_config(format!("wss://{addr}/ws"), io, Some(limits()))
        .await
        .expect("handshake")
        .0
}

/// What one viewer saw during a measurement window.
#[derive(Default, Clone, Copy)]
struct Seen {
    dynamic_layers: u32,
    bytes: u64,
    tick_hz: f32,
    total_ms: f32,
    splats_dynamic: u32,
    splats_static: u32,
}

/// Reads for `window` and counts, looking only at message headers (and at
/// the small Stats JSON).
async fn watch(mut ws: Ws, warm_up: Duration, window: Duration) -> Seen {
    let mut seen = Seen::default();
    let start = Instant::now() + warm_up;
    let end = start + window;
    while Instant::now() < end {
        let Ok(Some(Ok(Message::Binary(bytes)))) = tokio::time::timeout(Duration::from_secs(5), ws.next()).await else {
            continue;
        };
        if Instant::now() < start {
            continue;
        }
        seen.bytes += bytes.len() as u64;
        match codec::peek_kind(&bytes) {
            // The layer id is the first byte after the 16-byte header.
            Ok(Kind::Layer) if bytes.get(codec::HEADER_SIZE) == Some(&LAYER_DYNAMIC) => seen.dynamic_layers += 1,
            Ok(Kind::Stats) => {
                if let Ok(codec::Message { body: Body::Stats(json), .. }) = codec::decode(&bytes) {
                    if let Ok(stats) = serde_json::from_str::<Stats>(&json) {
                        seen.tick_hz = stats.tick_hz;
                        seen.total_ms = stats.total_ms;
                        seen.splats_dynamic = stats.splats_dynamic;
                        seen.splats_static = stats.splats_static;
                    }
                }
            }
            _ => {}
        }
    }
    let _ = ws.close(None).await;
    seen
}

async fn stage(binary: &PathBuf, port: u16) {
    const WARM_UP: Duration = Duration::from_secs(2);
    const WINDOW: Duration = Duration::from_secs(10);
    println!("Default stage (8 cameras 160x120, 30 Hz), {} s per row, machine with {} CPUs", WINDOW.as_secs(), cpus());
    println!(
        "{:>7} | {:>10} | {:>8} | {:>9} | {:>22} | {:>11} | {:>9} | {:>8}",
        "viewers",
        "server CPU",
        "of a core",
        "tick rate",
        "live layers/s (min..max)",
        "MB/s/viewer",
        "tick cost",
        "RSS"
    );
    let server = Server::spawn(binary, port, &[]).await;
    for viewers in [0usize, 1, 3, 8] {
        let mut tasks = Vec::new();
        for _ in 0..viewers {
            tasks.push(tokio::spawn(watch(connect(port).await, WARM_UP, WINDOW)));
        }
        tokio::time::sleep(WARM_UP).await;
        let (cpu_before, _) = server.usage();
        let t0 = Instant::now();
        tokio::time::sleep(WINDOW).await;
        let (cpu_after, rss) = server.usage();
        let elapsed = t0.elapsed().as_secs_f64();
        let mut seen = Vec::new();
        for task in tasks {
            seen.push(task.await.unwrap());
        }
        let cpu = (cpu_after - cpu_before) / elapsed;
        let rates: Vec<f64> = seen.iter().map(|s| f64::from(s.dynamic_layers) / WINDOW.as_secs_f64()).collect();
        let (min, max) = rates.iter().fold((f64::INFINITY, 0.0f64), |(lo, hi), r| (lo.min(*r), hi.max(*r)));
        let first = seen.first().copied().unwrap_or_default();
        let mbps =
            seen.iter().map(|s| s.bytes as f64).sum::<f64>() / WINDOW.as_secs_f64() / 1e6 / viewers.max(1) as f64;
        // Without a viewer nothing ticks and nothing is sent.
        let or_dash = |text: String| if viewers == 0 { "-".to_string() } else { text };
        println!(
            "{:>7} | {:>9.1}% | {:>7.1}% | {:>9} | {:>24} | {:>11} | {:>9} | {:>5.0} MB",
            viewers,
            cpu * 100.0 / cpus() as f64,
            cpu * 100.0,
            or_dash(format!("{:.1} Hz", first.tick_hz)),
            or_dash(format!("{min:.1}..{max:.1}")),
            or_dash(format!("{mbps:.2}")),
            or_dash(format!("{:.1} ms", first.total_ms)),
            rss as f64 / 1e6,
        );
        if viewers == 1 {
            println!(
                "          (live layer: {} splats, static layer: {} splats)",
                first.splats_dynamic, first.splats_static
            );
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    println!("  server CPU: share of the whole machine; of a core: the same in units of one core.");
}

fn cpus() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

/// Connects and reads one snapshot; returns the time from before the TCP
/// connect to the last message, and the size of the layer 0 message.
async fn time_snapshot(connecting: impl std::future::Future<Output = Ws>) -> (Duration, usize) {
    let start = Instant::now();
    let mut ws = connecting.await;
    let mut layer0 = 0;
    let mut messages = 0;
    while messages < 4 {
        match ws.next().await {
            Some(Ok(Message::Binary(bytes))) => {
                messages += 1;
                if codec::peek_kind(&bytes) == Ok(Kind::Layer) && bytes.get(codec::HEADER_SIZE) == Some(&LAYER_STATIC) {
                    layer0 = bytes.len();
                }
            }
            Some(Ok(_)) => {}
            other => panic!("snapshot interrupted: {other:?}"),
        }
    }
    let took = start.elapsed();
    let _ = ws.close(None).await;
    (took, layer0)
}

async fn house(binary: &PathBuf, port: u16) {
    let certs = std::env::temp_dir().join(format!("gs-measure-{}", std::process::id()));
    let lan_port = port + 1;
    let server = Server::spawn(
        binary,
        port,
        &["--scene", "house", "--lan", "--lan-port", &lan_port.to_string(), "--cert-dir", &certs.to_string_lossy()],
    )
    .await;

    // Run the default scan to its end as fast as the pipeline goes.
    let mut ws = connect(port).await;
    ws.send(Message::Text(r#"{"type":"scan_finish"}"#.into())).await.unwrap();
    let start = Instant::now();
    let mut appends = 0u32;
    let splats = loop {
        let Some(Ok(Message::Binary(bytes))) = ws.next().await else { panic!("stream ended during the scan") };
        match codec::peek_kind(&bytes) {
            Ok(Kind::Layer) if bytes.get(codec::HEADER_SIZE + 1) == Some(&1) => appends += 1,
            Ok(Kind::Stats) => {
                if let Ok(codec::Message { body: Body::Stats(json), .. }) = codec::decode(&bytes) {
                    let stats: Stats = serde_json::from_str(&json).unwrap();
                    if stats.scan.is_some_and(|scan| scan.state == ScanState::Done) {
                        break stats.splats_static;
                    }
                }
            }
            _ => {}
        }
    };
    println!();
    println!(
        "Default house: scan fused in {:.1} s (scan_finish), {appends} Appends, {splats} splats",
        start.elapsed().as_secs_f64()
    );
    drop(ws);
    tokio::time::sleep(Duration::from_millis(300)).await;

    let cert = CertificateDer::from_pem_file(certs.join("cert.pem")).expect("the server stored no certificate");
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert).unwrap();
    let tls = Arc::new(
        rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    );
    let lan = SocketAddr::from((Ipv4Addr::LOCALHOST, lan_port));

    for (label, secure) in [("ws://  (loopback)", false), ("wss:// (TLS)", true)] {
        let mut times = Vec::new();
        let mut size = 0;
        for _ in 0..7 {
            let (took, layer0) =
                if secure { time_snapshot(connect_tls(lan, &tls)).await } else { time_snapshot(connect(port)).await };
            times.push(took.as_secs_f64() * 1e3);
            size = layer0;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        times.sort_by(f64::total_cmp);
        println!(
            "  late joiner over {label}: finished house ({:.2} MB) received in {:.0} ms median ({:.0}..{:.0} ms, 7 runs)",
            size as f64 / 1e6,
            times[times.len() / 2],
            times[0],
            times[times.len() - 1]
        );
    }
    println!("  (time from before the TCP connect to the last of the four snapshot messages)");
    drop(server);
    let _ = std::fs::remove_dir_all(certs);
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    let mut binary = std::env::current_exe().ok().and_then(|exe| Some(exe.parent()?.parent()?.join("gs-server")));
    let mut port = 7890u16;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => port = args.next().and_then(|p| p.parse().ok()).expect("--port needs a number"),
            path => binary = Some(PathBuf::from(path)),
        }
    }
    let binary = binary.expect("pass the path of the gs-server binary");
    assert!(binary.is_file(), "{} not found: build it with `cargo build --release -p gs-server`", binary.display());
    stage(&binary, port).await;
    house(&binary, port).await;
}
