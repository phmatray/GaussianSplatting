//! Shared helpers of the integration tests: a server on ephemeral ports,
//! a WebSocket viewer that keeps the state a real viewer would keep, and
//! small HTTP clients (hyper over TCP or rustls, and raw sockets for
//! requests no well-behaved client would send).

#![allow(dead_code)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use gs_core::codec::{self, Body, LayerMode};
use gs_core::pipeline::{LAYER_DYNAMIC, LAYER_STATIC};
use gs_core::protocol::{PipelineConfig, SessionState, Stats};
use gs_core::rig::RingRig;
use gs_core::scene::SceneKind;
use gs_core::Splat;
use gs_server::{ServerConfig, ServerHandle};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpSocket, TcpStream};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::{CloseFrame, WebSocketConfig};
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::WebSocketStream;

/// Longest wait for one expected message before a test gives up.
pub const PATIENCE: Duration = Duration::from_secs(30);

// -- server -------------------------------------------------------------------

/// A server on ephemeral ports with its own certificate directory.
pub struct TestServer {
    pub handle: ServerHandle,
    pub cert_dir: tempfile::TempDir,
}

impl TestServer {
    pub fn port(&self) -> u16 {
        self.handle.http_addr().port()
    }

    pub fn lan_port(&self) -> u16 {
        self.handle.lan_addr().expect("LAN is off").port()
    }

    /// A TLS client configuration that trusts exactly the certificate the
    /// server stored: a handshake only succeeds when the served certificate
    /// is that one and names the address that was dialled.
    pub fn tls(&self) -> Arc<rustls::ClientConfig> {
        tls_trusting(&self.cert_dir.path().join("cert.pem"))
    }
}

pub fn tls_trusting(cert_pem: &std::path::Path) -> Arc<rustls::ClientConfig> {
    let cert = CertificateDer::from_pem_file(cert_pem).expect("no stored certificate");
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert).expect("certificate not usable as a trust anchor");
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    Arc::new(config)
}

/// Starts a server on ephemeral ports. `adjust` edits the configuration.
pub async fn start_server(pipeline: PipelineConfig, adjust: impl FnOnce(&mut ServerConfig)) -> TestServer {
    let cert_dir = tempfile::tempdir().unwrap();
    let mut config = ServerConfig {
        pipeline,
        port: 0,
        lan_port: 0,
        cert_dir: Some(cert_dir.path().to_path_buf()),
        ..ServerConfig::default()
    };
    adjust(&mut config);
    let handle = gs_server::start(config).await.expect("server did not start");
    TestServer { handle, cert_dir }
}

/// A stage small enough to tick in a few milliseconds.
pub fn small_stage(hz: f32) -> PipelineConfig {
    PipelineConfig {
        rig: RingRig { count: 4, width: 64, height: 48, ..RingRig::default() },
        target_hz: hz,
        ..PipelineConfig::for_scene(SceneKind::Stage)
    }
}

/// A coarse house whose whole scan takes `frames` handheld frames at most a
/// few seconds apart in wall time.
pub fn small_house(width: u32, height: u32, voxel: f32, speed: f32, fps: f32) -> PipelineConfig {
    let mut config = PipelineConfig::for_scene(SceneKind::House);
    config.scan.width = width;
    config.scan.height = height;
    config.scan.voxel_size = voxel;
    config.scan.speed = speed;
    config.scan.frames_per_second = fps;
    config
}

/// Taken by the tests that measure rates or rely on a client being slow:
/// they run one at a time, so that they do not compete with each other for
/// the processor.
pub async fn serial() -> tokio::sync::MutexGuard<'static, ()> {
    static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    LOCK.lock().await
}

/// Polls `condition` until it holds; panics with `what` after [`PATIENCE`].
pub async fn eventually(what: &str, mut condition: impl FnMut() -> bool) {
    let start = Instant::now();
    while !condition() {
        assert!(start.elapsed() < PATIENCE, "timed out waiting for: {what}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// -- streams --------------------------------------------------------------------

pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
pub type BoxIo = Box<dyn Io>;

pub fn loopback(port: u16) -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port)
}

/// Connects over TCP. A `recv_buffer` makes the kernel hold very little on
/// behalf of a client that does not read, so that the server feels a
/// stalled reader after kilobytes instead of megabytes.
pub async fn tcp(addr: SocketAddr, recv_buffer: Option<u32>) -> TcpStream {
    let socket = TcpSocket::new_v4().unwrap();
    if let Some(size) = recv_buffer {
        socket.set_recv_buffer_size(size).unwrap();
    }
    let stream = socket.connect(addr).await.unwrap_or_else(|e| panic!("cannot connect to {addr}: {e}"));
    stream.set_nodelay(true).unwrap();
    stream
}

/// Connects over TLS, verifying the certificate against `tls` for the
/// address that is dialled. `alpn` lists the protocols to offer.
pub async fn tls_stream(
    addr: SocketAddr,
    tls: &Arc<rustls::ClientConfig>,
    alpn: &[&[u8]],
) -> std::io::Result<tokio_rustls::client::TlsStream<TcpStream>> {
    tls_stream_as(addr, ServerName::IpAddress(addr.ip().into()), tls, alpn).await
}

/// Like [`tls_stream`], but the certificate must be valid for `name`.
pub async fn tls_stream_as(
    addr: SocketAddr,
    name: ServerName<'static>,
    tls: &Arc<rustls::ClientConfig>,
    alpn: &[&[u8]],
) -> std::io::Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let mut config = (**tls).clone();
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let stream = TcpStream::connect(addr).await?;
    stream.set_nodelay(true)?;
    connector.connect(name, stream).await
}

// -- WebSocket viewer -------------------------------------------------------------

/// What a decoded server message was.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    State,
    Layer { id: u8, mode: LayerMode, count: usize },
    Stats,
    Preview { camera: u8 },
}

/// One received message: its header and what it was.
#[derive(Clone, Debug, PartialEq)]
pub struct Received {
    pub seq: u32,
    pub time_ms: u32,
    pub at: Instant,
    pub bytes: usize,
    pub event: Event,
}

impl Received {
    pub fn is_dynamic(&self) -> bool {
        matches!(self.event, Event::Layer { id: LAYER_DYNAMIC, .. })
    }

    pub fn is_static(&self) -> bool {
        matches!(self.event, Event::Layer { id: LAYER_STATIC, .. })
    }

    /// State or a layer-0 message: the reliable part of the stream.
    pub fn is_reliable(&self) -> bool {
        self.event == Event::State || self.is_static()
    }
}

/// The scene as a viewer holds it after applying every message in order.
#[derive(Default)]
pub struct Viewer {
    pub state: Option<SessionState>,
    pub layers: [Vec<Splat>; 2],
    pub stats: Option<Stats>,
    /// Every message received, in order.
    pub log: Vec<Received>,
    /// What layer 0 held right before each of its `Replace` messages, with
    /// the `seq` of that message.
    pub replaced: Vec<(u32, Vec<Splat>)>,
    /// Size of layer 0 after each of its messages, with the `seq`.
    pub static_sizes: Vec<(u32, usize)>,
    /// The latest layer-0 `Replace` exactly as it came off the wire.
    pub static_replace_wire: Vec<u8>,
}

impl Viewer {
    /// The latest layer-0 `Replace` without the message header (`seq` and
    /// time differ between a broadcast and a snapshot of the same layer):
    /// layer header, bounds and the quantized splats, byte for byte.
    pub fn static_replace_payload(&self) -> &[u8] {
        &self.static_replace_wire[codec::HEADER_SIZE..]
    }
}

impl Viewer {
    fn apply(&mut self, bytes: &[u8]) -> Received {
        let message = codec::decode(bytes).expect("the server sent an undecodable frame");
        let event = match message.body {
            Body::State(json) => {
                self.state = Some(serde_json::from_str(&json).expect("State is not a SessionState"));
                Event::State
            }
            Body::Stats(json) => {
                self.stats = Some(serde_json::from_str(&json).expect("Stats is not Stats"));
                Event::Stats
            }
            Body::Layer(layer) => {
                let event = Event::Layer { id: layer.layer_id, mode: layer.mode, count: layer.splats.len() };
                let target = &mut self.layers[layer.layer_id as usize];
                match layer.mode {
                    LayerMode::Replace => {
                        let previous = std::mem::replace(target, layer.splats);
                        if layer.layer_id == LAYER_STATIC {
                            self.replaced.push((message.seq, previous));
                            self.static_replace_wire = bytes.to_vec();
                        }
                    }
                    LayerMode::Append => target.extend_from_slice(&layer.splats),
                }
                if layer.layer_id == LAYER_STATIC {
                    self.static_sizes.push((message.seq, target.len()));
                }
                event
            }
            Body::Preview(preview) => Event::Preview { camera: preview.camera_id },
        };
        let received =
            Received { seq: message.seq, time_ms: message.time_ms, at: Instant::now(), bytes: bytes.len(), event };
        self.log.push(received.clone());
        received
    }

    pub fn scene(&self) -> &str {
        self.state.as_ref().map_or("", |s| s.scene.as_str())
    }

    /// State of the home scan according to the latest Stats.
    pub fn scan(&self) -> Option<gs_core::protocol::ScanStats> {
        self.stats.and_then(|stats| stats.scan)
    }

    /// The reliable part of the stream: `(seq, event)` of State and layer 0
    /// messages.
    pub fn reliable_log(&self) -> Vec<(u32, Event)> {
        self.log.iter().filter(|m| m.is_reliable()).map(|m| (m.seq, m.event.clone())).collect()
    }

    pub fn dynamic_frames(&self) -> usize {
        self.log.iter().filter(|r| r.is_dynamic()).count()
    }

    /// Delivered live layers per second over the messages received at or
    /// after `since`.
    pub fn dynamic_rate_since(&self, since: Instant) -> f64 {
        let times: Vec<Instant> = self.log.iter().filter(|r| r.is_dynamic() && r.at >= since).map(|r| r.at).collect();
        match (times.first(), times.last()) {
            (Some(first), Some(last)) if times.len() > 1 => {
                (times.len() - 1) as f64 / last.duration_since(*first).as_secs_f64()
            }
            _ => 0.0,
        }
    }
}

/// What [`Client::next`] returned.
#[derive(Debug)]
pub enum Incoming {
    Message(Received),
    /// The server closed the connection, with this close frame if any.
    Closed(Option<(u16, String)>),
}

pub struct Client {
    pub ws: WebSocketStream<BoxIo>,
    pub viewer: Viewer,
}

/// Limits no message of these tests can reach (the client library's
/// defaults would refuse a single frame above 16 MiB).
fn roomy() -> WebSocketConfig {
    WebSocketConfig::default().max_message_size(Some(256 << 20)).max_frame_size(Some(256 << 20))
}

/// Performs the WebSocket handshake on `io` with optional extra headers.
pub async fn handshake(
    io: BoxIo,
    url: &str,
    headers: &[(&str, &str)],
) -> Result<WebSocketStream<BoxIo>, tungstenite::Error> {
    let mut request = url.into_client_request().unwrap();
    for (name, value) in headers {
        request
            .headers_mut()
            .insert(hyper::header::HeaderName::from_bytes(name.as_bytes()).unwrap(), value.parse().unwrap());
    }
    let (ws, _response) = tokio_tungstenite::client_async_with_config(request, io, Some(roomy())).await?;
    Ok(ws)
}

impl Client {
    /// A viewer on the loopback listener.
    pub async fn connect(port: u16) -> Self {
        Self::connect_with(port, None, &[]).await.expect("WebSocket handshake refused")
    }

    pub async fn connect_with(
        port: u16,
        recv_buffer: Option<u32>,
        headers: &[(&str, &str)],
    ) -> Result<Self, tungstenite::Error> {
        let io: BoxIo = Box::new(tcp(loopback(port), recv_buffer).await);
        let ws = handshake(io, &format!("ws://127.0.0.1:{port}/ws"), headers).await?;
        Ok(Self { ws, viewer: Viewer::default() })
    }

    /// A viewer on the LAN listener, over TLS.
    pub async fn connect_tls(addr: SocketAddr, tls: &Arc<rustls::ClientConfig>) -> Result<Self, tungstenite::Error> {
        let io: BoxIo = Box::new(tls_stream(addr, tls, &[b"http/1.1"]).await.map_err(tungstenite::Error::Io)?);
        let ws = handshake(io, &format!("wss://{addr}/ws"), &[]).await?;
        Ok(Self { ws, viewer: Viewer::default() })
    }

    /// The next server message, applied to the viewer. Pings are answered
    /// by the library and skipped here.
    pub async fn next(&mut self) -> Incoming {
        loop {
            let frame = tokio::time::timeout(PATIENCE, self.ws.next()).await.expect("no message from the server");
            match frame {
                Some(Ok(Message::Binary(bytes))) => return Incoming::Message(self.viewer.apply(&bytes)),
                Some(Ok(Message::Close(frame))) => {
                    return Incoming::Closed(frame.map(|f: CloseFrame| (u16::from(f.code), f.reason.to_string())))
                }
                Some(Ok(Message::Text(text))) => panic!("unexpected text frame from the server: {text}"),
                Some(Ok(_)) => {}
                Some(Err(_)) | None => return Incoming::Closed(None),
            }
        }
    }

    /// The next message; panics if the connection closes instead.
    pub async fn message(&mut self) -> Received {
        match self.next().await {
            Incoming::Message(received) => received,
            Incoming::Closed(frame) => panic!("connection closed unexpectedly: {frame:?}"),
        }
    }

    /// Reads until `done` holds for the viewer (checked after every
    /// message). Panics after [`PATIENCE`].
    pub async fn until(&mut self, what: &str, mut done: impl FnMut(&Viewer, &Received) -> bool) -> Received {
        let start = Instant::now();
        loop {
            let received = self.message().await;
            if done(&self.viewer, &received) {
                return received;
            }
            assert!(start.elapsed() < PATIENCE, "timed out waiting for: {what}");
        }
    }

    /// Reads everything that arrives during `duration`.
    pub async fn pump(&mut self, duration: Duration) {
        let end = tokio::time::Instant::now() + duration;
        loop {
            match tokio::time::timeout_at(end, self.ws.next()).await {
                Err(_) => return,
                Ok(Some(Ok(Message::Binary(bytes)))) => {
                    self.viewer.apply(&bytes);
                }
                Ok(Some(Ok(Message::Close(frame)))) => panic!("connection closed unexpectedly: {frame:?}"),
                Ok(Some(Ok(_))) => {}
                Ok(Some(Err(err))) => panic!("connection failed: {err}"),
                Ok(None) => panic!("connection ended unexpectedly"),
            }
        }
    }

    /// Reads the four messages of a snapshot and checks their kinds.
    pub async fn snapshot(&mut self) -> [Received; 4] {
        let messages = [self.message().await, self.message().await, self.message().await, self.message().await];
        assert_eq!(messages[0].event, Event::State, "a snapshot starts with State");
        assert!(
            matches!(messages[1].event, Event::Layer { id: LAYER_STATIC, mode: LayerMode::Replace, .. }),
            "then layer 0 Replace, got {:?}",
            messages[1].event
        );
        assert!(
            matches!(messages[2].event, Event::Layer { id: LAYER_DYNAMIC, mode: LayerMode::Replace, .. }),
            "then layer 1 Replace, got {:?}",
            messages[2].event
        );
        assert_eq!(messages[3].event, Event::Stats, "then Stats");
        messages
    }

    pub async fn send_text(&mut self, text: impl Into<String>) {
        self.ws.send(Message::Text(text.into().into())).await.expect("cannot send");
    }

    pub async fn send_json(&mut self, value: serde_json::Value) {
        self.send_text(value.to_string()).await;
    }

    /// Reads until the server closes; returns the close frame if any.
    pub async fn closed(&mut self) -> Option<(u16, String)> {
        loop {
            if let Incoming::Closed(frame) = self.next().await {
                return frame;
            }
        }
    }
}

// -- HTTP -------------------------------------------------------------------------

pub struct HttpResponse {
    pub status: u16,
    pub headers: hyper::HeaderMap,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|e| panic!("not JSON ({e}): {}", self.text()))
    }
}

/// One HTTP/1.1 request through hyper on an established stream.
pub async fn request_on(
    io: BoxIo,
    method: &str,
    host: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> HttpResponse {
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(io)).await.unwrap();
    let driver = tokio::spawn(connection);
    let mut request = hyper::Request::builder().method(method).uri(path).header("host", host);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let request = request.body(Full::new(Bytes::copy_from_slice(body))).unwrap();
    let response = sender.send_request(request).await.expect("request failed");
    let (parts, body) = response.into_parts();
    let body = body.collect().await.expect("body failed").to_bytes().to_vec();
    drop(sender);
    let _ = driver.await;
    HttpResponse { status: parts.status.as_u16(), headers: parts.headers, body }
}

/// A request to the loopback listener.
pub async fn http(port: u16, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> HttpResponse {
    let io: BoxIo = Box::new(tcp(loopback(port), None).await);
    request_on(io, method, &format!("127.0.0.1:{port}"), path, headers, body).await
}

pub async fn get(port: u16, path: &str) -> HttpResponse {
    http(port, "GET", path, &[], &[]).await
}

/// A request to the LAN listener over TLS (HTTP/1.1).
pub async fn https(
    addr: SocketAddr,
    tls: &Arc<rustls::ClientConfig>,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> HttpResponse {
    let io: BoxIo = Box::new(tls_stream(addr, tls, &[b"http/1.1"]).await.expect("TLS handshake failed"));
    request_on(io, method, &addr.to_string(), path, headers, body).await
}

/// Sends `request` byte for byte and returns the status code and the whole
/// answer. For request targets that an HTTP library would normalise or
/// refuse to send.
pub async fn raw_http(port: u16, request: &str) -> (u16, String) {
    let mut stream = tcp(loopback(port), None).await;
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut answer = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut answer)).await;
    let text = String::from_utf8_lossy(&answer).into_owned();
    let status = text.split_whitespace().nth(1).and_then(|code| code.parse().ok()).unwrap_or(0);
    (status, text)
}

/// A raw `GET` for `target`, exactly as written.
pub async fn raw_get(port: u16, target: &str) -> (u16, String) {
    raw_http(port, &format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")).await
}

/// Whether something accepts TCP connections on `addr`.
pub async fn accepts(addr: SocketAddr) -> bool {
    matches!(tokio::time::timeout(Duration::from_secs(2), TcpStream::connect(addr)).await, Ok(Ok(_)))
}

/// Resident memory of this process in bytes.
pub fn resident_bytes() -> u64 {
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    status
        .lines()
        .find_map(|line| line.strip_prefix("VmRSS:"))
        .and_then(|rest| rest.split_whitespace().next()?.parse::<u64>().ok())
        .map_or(0, |kb| kb * 1024)
}

// -- background reader ----------------------------------------------------------------

/// A client that is read continuously on its own task, like a healthy
/// viewer, while the test does something else.
pub struct Background {
    stop: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<Client>,
    /// `seq` of the newest live layer received so far.
    pub last_dynamic_seq: Arc<std::sync::atomic::AtomicU32>,
    /// Reliable messages (State, layer 0) received so far.
    pub reliable_count: Arc<std::sync::atomic::AtomicU32>,
}

impl Background {
    pub fn spawn(mut client: Client) -> Self {
        let (stop, mut stopped) = tokio::sync::oneshot::channel();
        let last_dynamic_seq = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let seq = last_dynamic_seq.clone();
        let reliable_count = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let reliable = reliable_count.clone();
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = &mut stopped => return client,
                    incoming = client.next() => match incoming {
                        Incoming::Message(received) => {
                            if received.is_dynamic() {
                                seq.store(received.seq, std::sync::atomic::Ordering::Relaxed);
                            }
                            if received.is_reliable() {
                                reliable.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            }
                        }
                        // Left to the test to notice through the log.
                        Incoming::Closed(_) => return client,
                    },
                }
            }
        });
        Self { stop, task, last_dynamic_seq, reliable_count }
    }

    pub fn dynamic_seq(&self) -> u32 {
        self.last_dynamic_seq.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn reliable(&self) -> u32 {
        self.reliable_count.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Stops reading and hands the client back with everything it saw.
    pub async fn stop(self) -> Client {
        let _ = self.stop.send(());
        self.task.await.expect("background reader panicked")
    }
}
