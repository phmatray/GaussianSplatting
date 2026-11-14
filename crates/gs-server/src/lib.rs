//! The Splat Live server: one capture pipeline, any number of viewers.
//!
//! [`start`] runs a [`gs_core::pipeline::Pipeline`] on its own thread and
//! serves it through two listeners, as specified in the "Server interface"
//! section of `docs/ARCHITECTURE.md`:
//!
//! * **loopback HTTP** (`127.0.0.1:7878` by default), always on, for the
//!   desktop window and a browser on the same machine;
//! * **LAN HTTPS** (`0.0.0.0:7879` by default) with a self-signed
//!   certificate, for a VR headset on the same network. Off by default,
//!   switched with [`ServerHandle::set_lan`] or `POST /api/lan`.
//!
//! Both serve the web build, `GET /api/info`, `GET /api/qr.svg` and the
//! `/ws` WebSocket that carries the binary frames of `gs_core::codec` one
//! way and JSON control messages the other way.
//!
//! ```no_run
//! # async fn demo() -> Result<(), gs_server::Error> {
//! let server = gs_server::start(gs_server::ServerConfig::default()).await?;
//! println!("open {}", server.info().http_url);
//! server.set_lan(true).await?;
//! // ...
//! server.shutdown().await;
//! # Ok(())
//! # }
//! ```
//!
//! # Delivery
//!
//! Every viewer has its own queue. Reliable messages (State, layer 0) are
//! delivered in order; latest-wins messages (live layer, Stats, previews)
//! overwrite the unsent one of their slot, so a slow viewer skips stale
//! frames instead of falling behind. A viewer whose reliable backlog
//! exceeds [`QueueLimits`] is resynchronised with a fresh snapshot. The
//! pipeline thread never waits for a socket, and it does not tick while
//! nobody is connected.
//!
//! # Connections
//!
//! A connection that asks for nothing does not stay: a silent socket, a
//! request that trickles in, an idle kept-alive connection and an
//! unfinished TLS handshake are closed after
//! [`ServerConfig::request_timeout`]. A viewer is dropped when its socket
//! accepts nothing for [`ServerConfig::send_timeout`] or when it leaves a
//! ping unanswered for [`ServerConfig::idle_timeout`].
//!
//! While the LAN listener is open, the URLs of [`ServerInfo`] and the
//! certificate follow the addresses of the machine (another Wi-Fi network,
//! a new DHCP lease) within a few seconds; the port and the connected
//! viewers stay.

mod assets;
mod http;
mod hub;
mod logging;
mod net;
mod outbox;
mod tls;
mod ws;

use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Duration;

use axum_server::tls_rustls::{RustlsAcceptor, RustlsConfig};
use gs_core::protocol::{PipelineConfig, PROTOCOL_VERSION};
use serde::{Deserialize, Serialize};
use tokio::sync::{watch, Notify};
use tokio::task::JoinHandle;

pub use logging::log_to_stderr;
pub use net::{lan_ipv4, qr_svg, qr_terminal};
pub use outbox::QueueLimits;
pub use tls::fingerprint;

use assets::Assets;
use http::Listener;
use hub::Hub;

/// Default port of the loopback HTTP listener.
pub const DEFAULT_PORT: u16 = 7878;
/// Default port of the LAN HTTPS listener.
pub const DEFAULT_LAN_PORT: u16 = 7879;
/// Path of the WebSocket endpoint.
pub const WS_PATH: &str = "/ws";
/// The `name` field of [`ServerInfo`].
pub const SERVER_NAME: &str = "splat-live";
/// Origins of the Tauri webview (Linux and macOS, then Windows). Requests
/// from them are accepted like same-origin ones.
pub const TAURI_ORIGINS: [&str; 2] = ["tauri://localhost", "http://tauri.localhost"];

/// Whether a web build was compiled in (`embed-dist` feature with a built
/// frontend). Without one, [`ServerConfig::dist`] has to name a directory
/// or the placeholder page is served.
pub fn has_embedded_frontend() -> bool {
    assets::has_embedded_index()
}

/// How long in-flight requests and closing handshakes may take when a
/// listener stops.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// Interval of the HTTP/2 keepalive pings on the LAN listener. A connection
/// that leaves one unanswered for [`ServerConfig::request_timeout`] is
/// closed, so a headset that left the network does not keep its connection
/// (and file descriptor) forever.
const HTTP2_PING_INTERVAL: Duration = Duration::from_secs(20);

/// How often the addresses of the machine are compared with the ones the
/// LAN URLs were built from, while the LAN listener is open.
const ADDRESS_POLL: Duration = Duration::from_millis(if cfg!(test) { 100 } else { 5000 });

/// Lists the addresses the LAN URLs are made of. [`net::lan_ipv4`] outside
/// of tests.
type AddressSource = Box<dyn Fn() -> Vec<Ipv4Addr> + Send + Sync>;

/// Everything [`start`] needs. `ServerConfig::default()` is the desktop
/// setup: default stage, port 7878, LAN off, embedded web build.
#[derive(Clone, Debug)]
pub struct ServerConfig {
    /// Initial configuration of the pipeline.
    pub pipeline: PipelineConfig,
    /// Port of the loopback HTTP listener. The next free port is taken
    /// when it is busy; 0 lets the system choose.
    pub port: u16,
    /// Port of the LAN HTTPS listener, with the same fallback.
    pub lan_port: u16,
    /// Open the LAN listener at start.
    pub lan: bool,
    /// Directory of the web build (`crates/gs-app/dist`). `None` serves the
    /// build compiled in with the `embed-dist` feature. Either way, a page
    /// explaining how to build the frontend is shown when there is none.
    pub dist: Option<PathBuf>,
    /// Where the self-signed certificate is kept between runs, so that the
    /// headset keeps trusting it. `None`: a new certificate at every start.
    pub cert_dir: Option<PathBuf>,
    /// Origins accepted besides the server's own and [`TAURI_ORIGINS`], for
    /// example the address of a frontend development server
    /// (`http://127.0.0.1:1420`).
    pub extra_origins: Vec<String>,
    /// Bound of the reliable backlog of one viewer.
    pub queue: QueueLimits,
    /// Size of the kernel send buffer of every connection, in bytes; `None`
    /// leaves it to the system. What sits in that buffer is already
    /// "sent" as far as the server can tell and can no longer be replaced
    /// by a newer frame, so on a link slower than the stream it is pure
    /// latency: left alone, Linux grows it to several megabytes, seconds
    /// of video. The default keeps about two live layers there.
    pub send_buffer: Option<usize>,
    /// A connection that has not sent a complete request head within this
    /// time is closed: a new one that says nothing (or trickles its headers
    /// in, "slowloris"), and a kept-alive one that stays idle. A TLS
    /// handshake, the body of a `POST` and an unanswered HTTP/2 ping get
    /// the same time. WebSocket connections are not concerned once they
    /// are upgraded.
    pub request_timeout: Duration,
    /// A viewer whose socket does not accept one message within this time
    /// is disconnected.
    pub send_timeout: Duration,
    /// Interval of the WebSocket keepalive pings.
    pub ping_interval: Duration,
    /// A viewer that leaves a ping unanswered for this long is considered
    /// gone.
    pub idle_timeout: Duration,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            pipeline: PipelineConfig::default(),
            port: DEFAULT_PORT,
            lan_port: DEFAULT_LAN_PORT,
            lan: false,
            dist: None,
            cert_dir: None,
            extra_origins: Vec::new(),
            queue: QueueLimits::default(),
            send_buffer: Some(512 << 10),
            request_timeout: Duration::from_secs(15),
            send_timeout: Duration::from_secs(30),
            ping_interval: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(30),
        }
    }
}

/// The LAN part of [`ServerInfo`].
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LanInfo {
    /// Whether the LAN HTTPS listener is open.
    pub enabled: bool,
    /// `https://<address>:<port>` for every non-loopback IPv4 address of
    /// the machine, the most plausible one first. Empty while LAN is off.
    pub urls: Vec<String>,
    /// SHA-256 of the certificate (`AB:CD:…`), to compare with what the
    /// headset browser shows. Empty while LAN is off.
    pub fingerprint: String,
}

/// The answer of `GET /api/info` and of the Tauri command `server_info`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerInfo {
    /// Always [`SERVER_NAME`].
    pub name: String,
    /// Version of this crate.
    pub version: String,
    /// `gs_core::protocol::PROTOCOL_VERSION`.
    pub protocol: u32,
    /// Base URL of the loopback listener, without a trailing slash.
    pub http_url: String,
    /// Always [`WS_PATH`].
    pub ws_path: String,
    pub lan: LanInfo,
    /// Viewers connected to the WebSocket right now.
    pub clients: usize,
}

/// Counters for diagnostics and tests.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Metrics {
    /// Viewers connected to the WebSocket.
    pub clients: usize,
    /// Pipeline ticks since the start.
    pub ticks: u64,
    /// Scene time of the pipeline in seconds.
    pub scene_time: f32,
    /// Control messages applied to the pipeline.
    pub controls: u64,
    /// Client text frames that were not a valid control message.
    pub rejected_controls: u64,
    /// Snapshots sent to viewers whose reliable backlog had overflowed.
    pub resyncs: u64,
    /// Bytes produced for viewers and not yet handed to their sockets,
    /// over all viewers, as of the latest broadcast. A viewer that keeps
    /// up holds at most the messages of one tick here; so does one that
    /// reads nothing, because newer messages replace the unsent ones.
    pub queued_bytes: u64,
}

/// Why the server could not start or open the LAN listener.
#[derive(Debug)]
pub enum Error {
    /// No port could be bound at or after `addr`.
    Bind { addr: SocketAddr, source: io::Error },
    /// The self-signed certificate could not be created or used.
    Certificate(String),
    /// Another I/O failure.
    Io(io::Error),
    /// The server has been shut down.
    Closed,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bind { addr, source } => write!(f, "cannot listen on {addr}: {source}"),
            Self::Certificate(reason) => write!(f, "cannot prepare the TLS certificate: {reason}"),
            Self::Io(err) => write!(f, "{err}"),
            Self::Closed => f.write_str("the server has been shut down"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Bind { source, .. } | Self::Io(source) => Some(source),
            Self::Certificate(_) | Self::Closed => None,
        }
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

/// Connected WebSocket sessions.
#[derive(Default)]
pub(crate) struct Sessions {
    count: AtomicUsize,
    next_id: AtomicU64,
    changed: Notify,
}

/// Keeps a session counted until it is dropped.
pub(crate) struct SessionGuard(Arc<Core>);

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.0.sessions.count.fetch_sub(1, Ordering::AcqRel);
        self.0.sessions.changed.notify_waiters();
    }
}

/// The parts of [`ServerConfig`] the request handlers read.
pub(crate) struct Settings {
    pub extra_origins: Vec<String>,
    pub queue: QueueLimits,
    pub send_buffer: Option<usize>,
    pub request_timeout: Duration,
    pub send_timeout: Duration,
    pub ping_interval: Duration,
    pub idle_timeout: Duration,
}

/// The open LAN listener.
struct LanListener {
    addr: SocketAddr,
    control: axum_server::Control,
    task: JoinHandle<io::Result<()>>,
    /// Tells the WebSocket sessions of this listener to close.
    stop: watch::Sender<bool>,
    /// The certificate in use; replaced in place when the addresses of the
    /// machine change (see [`Core::refresh_lan`]).
    tls: RustlsConfig,
    /// The addresses the URLs and the certificate were made for.
    addresses: Vec<Ipv4Addr>,
    /// Watches the addresses of the machine while the listener is open.
    watcher: JoinHandle<()>,
}

/// Applies [`ServerConfig::request_timeout`] to a listener: no connection
/// may sit there without asking for anything.
fn bound_idle_connections<A: axum_server::Address, Acc>(
    server: axum_server::Server<A, Acc>,
    timeout: Duration,
) -> axum_server::Server<A, Acc> {
    // Until the first request is dispatched (this also covers the protocol
    // detection, which has no timeout of its own)...
    let mut server = server.first_request_timeout(Some(timeout));
    // ...and between requests on a kept-alive connection.
    server.http_builder().http1().header_read_timeout(timeout);
    server.http_builder().http2().keep_alive_interval(Some(HTTP2_PING_INTERVAL)).keep_alive_timeout(timeout);
    server
}

/// State shared by the handle, both listeners and every session.
pub(crate) struct Core {
    pub settings: Settings,
    pub hub: Hub,
    pub assets: Assets,
    pub sessions: Sessions,
    pub rejected_controls: AtomicU64,
    http_addr: SocketAddr,
    http_control: axum_server::Control,
    http_task: Mutex<Option<JoinHandle<io::Result<()>>>>,
    /// Tells the WebSocket sessions of the loopback listener to close.
    stop: watch::Sender<bool>,
    cert_dir: Option<PathBuf>,
    addresses: AddressSource,
    /// Port to try first for the LAN listener: the configured one, then
    /// whatever was actually bound, so that the URL a headset has
    /// bookmarked survives switching LAN off and on.
    lan_port: AtomicU16,
    /// Serialises LAN switches and shutdown.
    lan: tokio::sync::Mutex<Option<LanListener>>,
    /// What [`Core::info`] reports about the LAN listener.
    lan_view: RwLock<(LanInfo, Option<SocketAddr>)>,
    closed: AtomicBool,
}

impl Core {
    /// Registers a WebSocket session and returns its id.
    pub fn enter_session(self: &Arc<Self>) -> (u64, SessionGuard) {
        self.sessions.count.fetch_add(1, Ordering::AcqRel);
        let id = self.sessions.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        (id, SessionGuard(self.clone()))
    }

    fn tuning(&self) -> http::TcpTuning {
        http::TcpTuning { send_buffer: self.settings.send_buffer }
    }

    pub fn info(&self) -> ServerInfo {
        let lan = self.lan_view.read().unwrap_or_else(|p| p.into_inner()).0.clone();
        ServerInfo {
            name: SERVER_NAME.to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            protocol: PROTOCOL_VERSION,
            http_url: format!("http://{}", self.http_addr),
            ws_path: WS_PATH.to_string(),
            lan,
            clients: self.sessions.count.load(Ordering::Acquire),
        }
    }

    fn set_lan_view(&self, info: LanInfo, addr: Option<SocketAddr>) {
        *self.lan_view.write().unwrap_or_else(|p| p.into_inner()) = (info, addr);
    }

    /// Opens or closes the LAN listener. Idempotent.
    pub async fn set_lan(self: &Arc<Self>, enabled: bool) -> Result<ServerInfo, Error> {
        let mut lan = self.lan.lock().await;
        if self.closed.load(Ordering::Acquire) {
            return Err(Error::Closed);
        }
        match (enabled, lan.is_some()) {
            (true, false) => *lan = Some(self.open_lan().await?),
            (false, true) => self.close_lan(lan.take()).await,
            _ => {}
        }
        // The server may have been shut down while the listener was being
        // opened (the shutdown could not see it yet): do not leave it open.
        if self.closed.load(Ordering::Acquire) {
            self.close_lan(lan.take()).await;
            return Err(Error::Closed);
        }
        Ok(self.info())
    }

    /// Loads or creates the certificate for `addresses` and returns the TLS
    /// configuration serving it, with its fingerprint.
    async fn identity_for(&self, addresses: &[Ipv4Addr]) -> Result<(Arc<rustls::ServerConfig>, String), Error> {
        let (dir, addresses) = (self.cert_dir.clone(), addresses.to_vec());
        // Key generation and file access: off the async threads.
        let identity = tokio::task::spawn_blocking(move || {
            tls::load_or_create(dir.as_deref(), &addresses, time::OffsetDateTime::now_utc())
        })
        .await
        .map_err(|e| Error::Certificate(e.to_string()))?
        .map_err(|e| Error::Certificate(e.to_string()))?
        .0;
        let config = identity.server_config().map_err(|e| Error::Certificate(e.to_string()))?;
        Ok((config, identity.fingerprint()))
    }

    fn lan_info(addresses: &[Ipv4Addr], port: u16, fingerprint: String) -> LanInfo {
        LanInfo {
            enabled: true,
            urls: addresses.iter().map(|ip| format!("https://{ip}:{port}")).collect(),
            fingerprint,
        }
    }

    async fn open_lan(self: &Arc<Self>) -> Result<LanListener, Error> {
        let addresses = (self.addresses)();
        let (config, fingerprint) = self.identity_for(&addresses).await?;

        let wanted = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), self.lan_port.load(Ordering::Relaxed));
        let listener =
            net::bind_near(wanted.ip(), wanted.port()).map_err(|source| Error::Bind { addr: wanted, source })?;
        let addr = listener.local_addr()?;
        self.lan_port.store(addr.port(), Ordering::Relaxed);

        let (stop, stop_rx) = watch::channel(false);
        let control = axum_server::Control::new();
        let app = http::router(self.clone(), Listener::Lan, stop_rx);
        let tls = RustlsConfig::from_config(config);
        // The TLS handshake gets the same time as a request head.
        let acceptor =
            RustlsAcceptor::new(tls.clone()).handshake_timeout(self.settings.request_timeout).acceptor(self.tuning());
        let server = axum_server::from_tcp(listener)?
            .acceptor(acceptor)
            .handle(axum_server::Handle::with_control(control.clone()));
        let server = bound_idle_connections(server, self.settings.request_timeout);
        let task = tokio::spawn(server.serve(app.into_make_service()));

        let info = Self::lan_info(&addresses, addr.port(), fingerprint);
        log::info!("LAN listener open on {addr} ({} address(es))", info.urls.len());
        self.set_lan_view(info, Some(addr));
        let watcher = tokio::spawn(watch_addresses(Arc::downgrade(self)));
        Ok(LanListener { addr, control, task, stop, tls, addresses, watcher })
    }

    /// Whether the machine has other addresses than the ones the LAN URLs
    /// were made of. `false` while a switch is in progress (the next poll
    /// looks again).
    fn lan_addresses_changed(&self) -> bool {
        self.lan.try_lock().is_ok_and(|lan| lan.as_ref().is_some_and(|open| open.addresses != (self.addresses)()))
    }

    /// Brings the LAN URLs and the certificate in line with the addresses
    /// the machine has now (another Wi-Fi network, a new DHCP lease). The
    /// listener is bound to every interface, so it stays as it is: only
    /// what `/api/info` announces changes, and the certificate when it does
    /// not name a new address. Does nothing while LAN is off or when the
    /// addresses are the same.
    async fn refresh_lan(&self) {
        let mut lan = self.lan.lock().await;
        let Some(listener) = lan.as_mut() else { return };
        let addresses = (self.addresses)();
        if addresses == listener.addresses {
            return;
        }
        match self.identity_for(&addresses).await {
            Ok((config, fingerprint)) => {
                // New handshakes get the new certificate; established
                // connections are not disturbed.
                listener.tls.reload_from_config(config);
                log::info!("LAN addresses changed: {:?} -> {addresses:?}", listener.addresses);
                self.set_lan_view(Self::lan_info(&addresses, listener.addr.port(), fingerprint), Some(listener.addr));
                listener.addresses = addresses;
            }
            // The old URLs stay announced; the next poll tries again.
            Err(err) => {
                log::warn!("les adresses réseau ont changé, mais le certificat n'a pas pu être mis à jour : {err}")
            }
        }
    }

    async fn close_lan(&self, listener: Option<LanListener>) {
        let Some(listener) = listener else { return };
        listener.watcher.abort();
        self.set_lan_view(LanInfo::default(), None);
        // Upgraded connections are no longer tracked by the HTTP server:
        // the sessions get their own signal.
        let _ = listener.stop.send(true);
        listener.control.graceful_shutdown(Some(SHUTDOWN_GRACE));
        // Once the task has ended the socket is closed: the port is free
        // again when this function returns.
        let _ = listener.task.await;
        log::info!("LAN listener on {} closed", listener.addr);
    }

    /// Starts the shutdown without waiting for it: every session and both
    /// listeners are told to stop, and so is the pipeline thread.
    fn begin_shutdown(&self) {
        self.closed.store(true, Ordering::Release);
        let _ = self.stop.send(true);
        self.http_control.graceful_shutdown(Some(SHUTDOWN_GRACE));
        if let Ok(lan) = self.lan.try_lock() {
            if let Some(listener) = lan.as_ref() {
                let _ = listener.stop.send(true);
                listener.control.graceful_shutdown(Some(SHUTDOWN_GRACE));
            }
        }
    }

    async fn shutdown(&self) {
        self.begin_shutdown();
        {
            let mut lan = self.lan.lock().await;
            self.close_lan(lan.take()).await;
        }
        let http_task = self.http_task.lock().unwrap_or_else(|p| p.into_inner()).take();
        if let Some(task) = http_task {
            let _ = task.await;
        }
        // Let the sessions finish their closing handshake, so that viewers
        // see a clean close (1001) instead of a dropped connection.
        let deadline = tokio::time::Instant::now() + SHUTDOWN_GRACE;
        loop {
            let changed = self.sessions.changed.notified();
            if self.sessions.count.load(Ordering::Acquire) == 0 {
                break;
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                log::warn!(
                    "arrêt : {} connexion(s) ne se sont pas fermées à temps",
                    self.sessions.count.load(Ordering::Acquire)
                );
                break;
            }
        }
        if let Some(thread) = self.hub.stop() {
            // A tick may be in progress; joining must not block the runtime.
            let _ = tokio::task::spawn_blocking(move || thread.join()).await;
        }
    }
}

/// Runs while the LAN listener is open (it is aborted when the listener
/// closes): notices when the machine changes address. Holds no strong
/// reference, so it never keeps a dropped server alive.
async fn watch_addresses(core: Weak<Core>) {
    let mut poll = tokio::time::interval(ADDRESS_POLL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        poll.tick().await;
        let Some(core) = core.upgrade() else { return };
        if core.closed.load(Ordering::Acquire) {
            return;
        }
        if core.lan_addresses_changed() {
            // On its own task: aborting this one must not cancel a
            // certificate update half-way.
            let _ = tokio::spawn(async move { core.refresh_lan().await }).await;
        }
    }
}

/// Starts the shutdown when the last [`ServerHandle`] goes away without
/// [`ServerHandle::shutdown`] having been called.
struct HandleGuard(Arc<Core>);

impl Drop for HandleGuard {
    fn drop(&mut self) {
        self.0.begin_shutdown();
        self.0.hub.stop();
    }
}

/// A running server. Cheap to clone; the server stops when
/// [`ServerHandle::shutdown`] is called or the last clone is dropped.
#[derive(Clone)]
pub struct ServerHandle {
    core: Arc<Core>,
    _guard: Arc<HandleGuard>,
}

impl ServerHandle {
    /// The current info JSON (`GET /api/info`).
    pub fn info(&self) -> ServerInfo {
        self.core.info()
    }

    /// Address the loopback HTTP listener is bound to (the real port).
    pub fn http_addr(&self) -> SocketAddr {
        self.core.http_addr
    }

    /// Address the LAN HTTPS listener is bound to, while it is open.
    pub fn lan_addr(&self) -> Option<SocketAddr> {
        self.core.lan_view.read().unwrap_or_else(|p| p.into_inner()).1
    }

    /// Opens or closes the LAN HTTPS listener and returns the new info.
    ///
    /// Opening loads or creates the certificate (see
    /// [`ServerConfig::cert_dir`]) and binds the port. Closing disconnects
    /// the viewers that came through it; the port is free again when the
    /// call returns. Asking for the state the listener is already in does
    /// nothing.
    pub async fn set_lan(&self, enabled: bool) -> Result<ServerInfo, Error> {
        self.core.set_lan(enabled).await
    }

    /// Counters for diagnostics.
    pub fn metrics(&self) -> Metrics {
        let hub = &self.core.hub.metrics;
        Metrics {
            clients: self.core.sessions.count.load(Ordering::Acquire),
            ticks: hub.ticks.load(Ordering::Relaxed),
            scene_time: f32::from_bits(hub.scene_time.load(Ordering::Relaxed)),
            controls: hub.controls.load(Ordering::Relaxed),
            rejected_controls: self.core.rejected_controls.load(Ordering::Relaxed),
            resyncs: hub.resyncs.load(Ordering::Relaxed),
            queued_bytes: hub.queued_bytes.load(Ordering::Relaxed),
        }
    }

    /// Stops the server: viewers receive a WebSocket close (1001), both
    /// listeners are closed and the pipeline thread is joined. Returns once
    /// all of that is done (a few seconds at most). Calling it again, or
    /// using the handle afterwards, is harmless.
    pub async fn shutdown(&self) {
        self.core.shutdown().await;
    }
}

/// Starts the pipeline thread and the loopback listener (and the LAN one
/// with [`ServerConfig::lan`]), and returns once they are bound.
///
/// Must be called inside a Tokio runtime, which then runs the listeners.
pub async fn start(config: ServerConfig) -> Result<ServerHandle, Error> {
    start_with(config, Box::new(net::lan_ipv4)).await
}

async fn start_with(config: ServerConfig, addresses: AddressSource) -> Result<ServerHandle, Error> {
    let wanted = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), config.port);
    let listener = net::bind_near(wanted.ip(), wanted.port()).map_err(|source| Error::Bind { addr: wanted, source })?;
    let http_addr = listener.local_addr()?;

    let assets = match &config.dist {
        Some(dir) => Assets::Dir(dir.clone()),
        None => Assets::embedded(),
    };
    log::info!("serving the frontend from: {}", assets.describe());
    if !assets.has_index() {
        log::warn!("aucune interface web trouvée : une page d'explication est servie à la place");
    }

    let (stop, stop_rx) = watch::channel(false);
    let http_control = axum_server::Control::new();
    let core = Arc::new(Core {
        settings: Settings {
            extra_origins: config.extra_origins,
            queue: config.queue,
            send_buffer: config.send_buffer,
            request_timeout: config.request_timeout,
            send_timeout: config.send_timeout,
            ping_interval: config.ping_interval,
            idle_timeout: config.idle_timeout,
        },
        hub: Hub::start(config.pipeline)?,
        assets,
        sessions: Sessions::default(),
        rejected_controls: AtomicU64::new(0),
        http_addr,
        http_control: http_control.clone(),
        http_task: Mutex::new(None),
        stop,
        cert_dir: config.cert_dir,
        addresses,
        lan_port: AtomicU16::new(config.lan_port),
        lan: tokio::sync::Mutex::new(None),
        lan_view: RwLock::new((LanInfo::default(), None)),
        closed: AtomicBool::new(false),
    });
    // From here on, dropping the guard stops whatever has been started.
    let handle = ServerHandle { core: core.clone(), _guard: Arc::new(HandleGuard(core.clone())) };

    // The LAN listener first: the loopback socket is bound but not served
    // yet, so the very first `/api/info` already tells the truth about LAN.
    if config.lan {
        handle.set_lan(true).await?;
    }

    let app = http::router(core.clone(), Listener::Loopback, stop_rx);
    let server = axum_server::from_tcp(listener)?
        .acceptor(core.tuning())
        .handle(axum_server::Handle::with_control(http_control));
    let server = bound_idle_connections(server, core.settings.request_timeout);
    let task = tokio::spawn(server.serve(app.into_make_service()));
    *core.http_task.lock().unwrap_or_else(|p| p.into_inner()) = Some(task);
    log::info!("loopback listener open on http://{http_addr}");
    Ok(handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;
    use gs_core::pipeline::{Outgoing, SLOT_DYNAMIC, SLOT_RELIABLE};
    use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
    use tokio_tungstenite::tungstenite::Message;

    /// Deterministic filler, so that a truncated or reordered message is
    /// noticed.
    fn pattern(len: usize, seed: u32) -> Vec<u8> {
        let mut state = seed;
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 24) as u8
            })
            .collect()
    }

    /// The real pipeline cannot be made to emit a 40 MB message (its
    /// largest layer is about 20 MB), so two are injected into the
    /// broadcast path: nothing between the pipeline thread and the socket
    /// may limit the size of an outgoing message.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn forty_megabyte_messages_pass() {
        const SIZE: usize = 40 << 20;
        let mut pipeline = PipelineConfig::light(gs_core::scene::SceneKind::Stage);
        pipeline.paused = true;
        let server = start(ServerConfig { pipeline, port: 0, ..ServerConfig::default() }).await.unwrap();

        let url = format!("ws://{}/ws", server.http_addr());
        let limits = WebSocketConfig::default().max_message_size(Some(64 << 20)).max_frame_size(Some(64 << 20));
        let (mut ws, _) = tokio_tungstenite::connect_async_with_config(url, Some(limits), true).await.unwrap();
        // The snapshot first: State, layer 0, layer 1, Stats.
        for _ in 0..4 {
            assert!(matches!(ws.next().await, Some(Ok(Message::Binary(_)))));
        }

        let (reliable, latest) = (pattern(SIZE, 1), pattern(SIZE + 17, 2));
        assert!(server.core.hub.send(hub::Command::Inject(vec![
            Outgoing { bytes: latest.clone(), reliable: false, slot: SLOT_DYNAMIC },
            Outgoing { bytes: reliable.clone(), reliable: true, slot: SLOT_RELIABLE },
        ])));
        let mut received = Vec::new();
        while received.len() < 2 {
            let next =
                tokio::time::timeout(Duration::from_secs(60), ws.next()).await.expect("the large message never came");
            match next {
                // The paused pipeline keeps sending small Stats.
                Some(Ok(Message::Binary(bytes))) if bytes.len() >= SIZE => received.push(bytes),
                Some(Ok(_)) => {}
                other => panic!("connection lost on a large message: {other:?}"),
            }
        }
        // Reliable before latest-wins, both intact.
        assert!(received[0][..] == reliable[..], "the reliable message arrived damaged");
        assert!(received[1][..] == latest[..], "the latest-wins message arrived damaged");
        server.shutdown().await;
    }

    /// A TLS client that trusts exactly the certificate stored in `dir`
    /// and returns the fingerprint of the one the server presented.
    async fn presented_fingerprint(addr: SocketAddr, dir: &std::path::Path) -> std::io::Result<String> {
        use rustls::pki_types::pem::PemObject;
        let stored = rustls::pki_types::CertificateDer::from_pem_file(dir.join("cert.pem")).expect("no certificate");
        let mut roots = rustls::RootCertStore::empty();
        roots.add(stored).unwrap();
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
        let stream = tokio::net::TcpStream::connect(addr).await?;
        let name = rustls::pki_types::ServerName::IpAddress(addr.ip().into());
        let stream = connector.connect(name, stream).await?;
        let (_, session) = stream.get_ref();
        Ok(fingerprint(&session.peer_certificates().expect("no certificate presented")[0]))
    }

    async fn wait_for(what: &str, mut condition: impl FnMut() -> bool) {
        let start = std::time::Instant::now();
        while !condition() {
            assert!(start.elapsed() < Duration::from_secs(20), "timed out waiting for: {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The machine changes network while LAN is on (the real address list
    /// cannot be changed from a test, so it is injected): the URLs, the QR
    /// code and the certificate follow, the port and the connected viewers
    /// stay.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn lan_urls_and_certificate_follow_the_addresses_of_the_machine() {
        let home = Ipv4Addr::new(192, 168, 1, 20);
        let office = Ipv4Addr::new(10, 0, 0, 5);
        let current = Arc::new(Mutex::new(vec![home]));
        let source = {
            let current = current.clone();
            Box::new(move || current.lock().unwrap().clone())
        };
        let dir = tempfile::tempdir().unwrap();
        let config = ServerConfig {
            pipeline: PipelineConfig::light(gs_core::scene::SceneKind::Stage),
            port: 0,
            lan_port: 0,
            lan: true,
            cert_dir: Some(dir.path().to_path_buf()),
            ..ServerConfig::default()
        };
        let server = start_with(config, source).await.unwrap();
        let lan_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), server.lan_addr().unwrap().port());
        let at_home = server.info().lan;
        assert_eq!(at_home.urls, [format!("https://{home}:{}", lan_addr.port())]);
        assert_eq!(presented_fingerprint(lan_addr, dir.path()).await.unwrap(), at_home.fingerprint);

        // A viewer that is connected through the whole move.
        let url = format!("ws://{}/ws", server.http_addr());
        let (mut viewer, _) = tokio_tungstenite::connect_async(url).await.unwrap();

        // Another network: a new URL on the same port, and a certificate
        // that names the new address (the old one stays in it).
        *current.lock().unwrap() = vec![office];
        wait_for("the new address to be announced", || server.info().lan.urls != at_home.urls).await;
        let at_the_office = server.info().lan;
        assert!(at_the_office.enabled);
        assert_eq!(at_the_office.urls, [format!("https://{office}:{}", lan_addr.port())]);
        assert_ne!(at_the_office.fingerprint, at_home.fingerprint, "the certificate does not name the new address");
        assert_eq!(server.lan_addr().unwrap().port(), lan_addr.port());
        // New handshakes get the new certificate, which is also the stored one.
        assert_eq!(presented_fingerprint(lan_addr, dir.path()).await.unwrap(), at_the_office.fingerprint);

        // Back home: the URL returns, the certificate already covers it.
        *current.lock().unwrap() = vec![home];
        wait_for("the first address to be announced again", || server.info().lan.urls == at_home.urls).await;
        assert_eq!(server.info().lan.fingerprint, at_the_office.fingerprint);
        // No address at all (cable pulled): nothing to announce, still on.
        current.lock().unwrap().clear();
        wait_for("the URL list to empty", || server.info().lan.urls.is_empty()).await;
        assert!(server.info().lan.enabled);
        assert_eq!(server.info().lan.fingerprint, at_the_office.fingerprint);

        // The viewer saw none of it.
        for _ in 0..20 {
            assert!(matches!(viewer.next().await, Some(Ok(Message::Binary(_)))), "the viewer lost its stream");
        }

        // Switched off, the watching stops: addresses come and go unnoticed.
        server.set_lan(false).await.unwrap();
        *current.lock().unwrap() = vec![office];
        tokio::time::sleep(4 * ADDRESS_POLL).await;
        assert_eq!(server.info().lan, LanInfo::default());
        // Switched on again, it starts from the addresses of that moment.
        assert_eq!(server.set_lan(true).await.unwrap().lan.urls, [format!("https://{office}:{}", lan_addr.port())]);
        server.shutdown().await;
    }
}
