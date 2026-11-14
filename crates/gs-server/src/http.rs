//! Routes shared by the two listeners, origin checks and CORS.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, Query, Request, State};
use axum::http::header::{self, HeaderMap, HeaderValue};
use axum::http::{Method, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use tokio::sync::watch;
use tower_http::compression::CompressionLayer;
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::{net, ws, Core, TAURI_ORIGINS, WS_PATH};

/// Which of the two listeners a request came through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Listener {
    /// Plain HTTP on 127.0.0.1.
    Loopback,
    /// HTTPS on every interface.
    Lan,
}

impl Listener {
    fn scheme(self) -> &'static str {
        match self {
            Self::Loopback => "http",
            Self::Lan => "https",
        }
    }
}

#[derive(Clone)]
pub(crate) struct AppState {
    pub core: Arc<Core>,
    pub listener: Listener,
    /// Becomes `true` when the WebSocket sessions of this listener must
    /// close (LAN switched off, or shutdown).
    pub stop: watch::Receiver<bool>,
}

/// Socket options of every accepted connection.
#[derive(Clone, Copy, Debug)]
pub(crate) struct TcpTuning {
    /// See `ServerConfig::send_buffer`.
    pub send_buffer: Option<usize>,
}

impl<S> axum_server::accept::Accept<tokio::net::TcpStream, S> for TcpTuning {
    type Stream = tokio::net::TcpStream;
    type Service = S;
    type Future = std::future::Ready<std::io::Result<(Self::Stream, Self::Service)>>;

    fn accept(&self, stream: tokio::net::TcpStream, service: S) -> Self::Future {
        // Frames must leave at once: Nagle's algorithm would hold back the
        // tail of each one.
        let tuned = stream.set_nodelay(true).and_then(|()| match self.send_buffer {
            Some(bytes) => socket2::SockRef::from(&stream).set_send_buffer_size(bytes),
            None => Ok(()),
        });
        std::future::ready(tuned.map(|()| (stream, service)))
    }
}

pub(crate) fn router(core: Arc<Core>, listener: Listener, stop: watch::Receiver<bool>) -> Router {
    let mut app = Router::new()
        .route("/api/info", get(api_info))
        .route("/api/lan", post(api_lan))
        .route("/api/qr.svg", get(api_qr))
        .fallback(static_files)
        // The wasm and JavaScript of the web build shrink by a factor of
        // three or more; on the headset's Wi-Fi that is worth the CPU.
        .layer(CompressionLayer::new().gzip(true).br(true))
        // Added after the compression layer on purpose: the WebSocket
        // handshake has nothing to compress.
        .route(WS_PATH, get(ws::upgrade));
    if listener == Listener::Loopback {
        // Only the Tauri webview needs cross-origin access (its pages come
        // from `tauri://localhost`); a browser tab talks to its own origin.
        // The LAN listener grants no cross-origin access at all.
        let origins: Vec<HeaderValue> = TAURI_ORIGINS
            .iter()
            .copied()
            .chain(core.settings.extra_origins.iter().map(String::as_str))
            .filter_map(|origin| HeaderValue::from_str(origin).ok())
            .collect();
        app = app.layer(
            CorsLayer::new()
                .allow_origin(AllowOrigin::list(origins))
                .allow_methods([Method::GET, Method::POST])
                .allow_headers([header::CONTENT_TYPE]),
        );
    }
    app.layer(axum::middleware::from_fn(move |request: Request, next: Next| access_log(listener, request, next)))
        .with_state(AppState { core, listener, stop })
}

/// One debug line per request: what was asked, by which origin, and what
/// was answered. Costs nothing unless debug logging is on.
async fn access_log(listener: Listener, request: Request, next: Next) -> Response {
    if !log::log_enabled!(log::Level::Debug) {
        return next.run(request).await;
    }
    let method = request.method().clone();
    let target = request.uri().path_and_query().map_or_else(|| request.uri().path().to_string(), |pq| pq.to_string());
    let origin = request.headers().get(header::ORIGIN).and_then(|o| o.to_str().ok()).unwrap_or("-").to_string();
    let response = next.run(request).await;
    log::debug!("{listener:?} {method} {target} (origin {origin}) -> {}", response.status().as_u16());
    response
}

/// The host a request was addressed to: the `Host` header, or the
/// authority of an HTTP/2 request.
fn request_host<'a>(headers: &'a HeaderMap, uri: &'a Uri) -> Option<&'a str> {
    headers.get(header::HOST).and_then(|h| h.to_str().ok()).or_else(|| uri.authority().map(|a| a.as_str()))
}

fn is_loopback_host(host: &str) -> bool {
    let name = match host.rsplit_once(':') {
        // `[::1]:7878` and `127.0.0.1:7878`. Only digits make a port: the
        // colons of `[::1]` do not, and neither does whatever follows the
        // colon in `127.0.0.1:7878@evil.example`.
        Some((name, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => name,
        _ => host,
    };
    name == "127.0.0.1" || name.eq_ignore_ascii_case("localhost") || name == "[::1]"
}

/// The `Origin` policy of the WebSocket and `POST` routes.
///
/// * No `Origin` header: not a browser (browsers always send one on these
///   requests), nothing to protect against.
/// * Several `Origin` headers: refused, there is no telling which one an
///   intermediary or the browser meant.
/// * A Tauri origin, or one the configuration adds: accepted.
/// * Otherwise the origin must be the server itself: same scheme and same
///   host and port as the request was addressed to. On the loopback
///   listener that host must also be a loopback name, which stops a page
///   that points its own domain at 127.0.0.1 (DNS rebinding).
pub(crate) fn origin_allowed(state: &AppState, headers: &HeaderMap, uri: &Uri) -> bool {
    let mut origins = headers.get_all(header::ORIGIN).iter();
    let Some(origin) = origins.next() else { return true };
    if origins.next().is_some() {
        return false;
    }
    let Ok(origin) = origin.to_str() else { return false };
    let known = TAURI_ORIGINS.iter().copied().chain(state.core.settings.extra_origins.iter().map(String::as_str));
    if known.into_iter().any(|allowed| allowed.eq_ignore_ascii_case(origin)) {
        return true;
    }
    let Some(host) = request_host(headers, uri) else { return false };
    if state.listener == Listener::Loopback && !is_loopback_host(host) {
        return false;
    }
    origin.eq_ignore_ascii_case(&format!("{}://{host}", state.listener.scheme()))
}

fn json_error(status: StatusCode, message: impl Into<String>) -> Response {
    let mut response = (status, Json(serde_json::json!({ "error": message.into() }))).into_response();
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn info_response(info: crate::ServerInfo) -> Response {
    let mut response = Json(info).into_response();
    response.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

async fn api_info(State(state): State<AppState>) -> Response {
    info_response(state.core.info())
}

#[derive(Deserialize)]
struct LanRequest {
    enabled: bool,
}

async fn api_lan(State(state): State<AppState>, request: Request) -> Response {
    // Opening the machine to the network is decided at the machine, never
    // from the network.
    if state.listener != Listener::Loopback {
        return json_error(StatusCode::FORBIDDEN, "LAN access can only be changed from the machine itself");
    }
    if !origin_allowed(&state, request.headers(), request.uri()) {
        return json_error(StatusCode::FORBIDDEN, "origin not allowed");
    }
    // The body is read only for a request that may ask, and not for ever: a
    // client that announces a body and never sends it must not keep its
    // connection open.
    let body: Result<Result<Json<LanRequest>, JsonRejection>, _> =
        tokio::time::timeout(state.core.settings.request_timeout, Json::from_request(request, &())).await;
    let Json(request) = match body {
        Ok(Ok(body)) => body,
        Ok(Err(rejection)) => return json_error(rejection.status(), rejection.body_text()),
        Err(_) => return json_error(StatusCode::REQUEST_TIMEOUT, "the request body did not arrive in time"),
    };
    // On its own task: a client that hangs up must not cancel the switch
    // half-way.
    let core = state.core.clone();
    match tokio::spawn(async move { core.set_lan(request.enabled).await }).await {
        Ok(Ok(info)) => info_response(info),
        Ok(Err(crate::Error::Closed)) => json_error(StatusCode::SERVICE_UNAVAILABLE, "the server is shutting down"),
        Ok(Err(err)) => {
            log::error!("impossible de changer l'accès réseau : {err}");
            json_error(StatusCode::INTERNAL_SERVER_ERROR, err.to_string())
        }
        Err(_) => json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
    }
}

#[derive(Deserialize)]
struct QrQuery {
    /// Index into `lan.urls`.
    #[serde(default)]
    i: usize,
}

async fn api_qr(State(state): State<AppState>, Query(query): Query<QrQuery>) -> Response {
    let info = state.core.info();
    let Some(url) = info.lan.urls.get(query.i) else {
        return json_error(StatusCode::NOT_FOUND, "no such LAN URL (is LAN access on?)");
    };
    match net::qr_svg(url) {
        Some(svg) => (
            [
                (header::CONTENT_TYPE, HeaderValue::from_static("image/svg+xml")),
                // The URL list changes with the network and the LAN switch.
                (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
            ],
            svg,
        )
            .into_response(),
        None => json_error(StatusCode::INTERNAL_SERVER_ERROR, "the URL does not fit in a QR code"),
    }
}

async fn static_files(State(state): State<AppState>, request: Request) -> Response<Body> {
    state.core.assets.serve(request.method(), request.uri().path(), request.headers()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_hosts() {
        for host in ["127.0.0.1", "127.0.0.1:7878", "localhost", "LOCALHOST:80", "[::1]", "[::1]:7878"] {
            assert!(is_loopback_host(host), "{host}");
        }
        for host in [
            "evil.example",
            "evil.example:7878",
            "127.0.0.1.evil.example",
            "192.168.1.20:7878",
            "localhost.evil.example",
            // What follows the colon is not a port: the name is not
            // `127.0.0.1` but the whole thing.
            "127.0.0.1:7878@evil.example",
            "localhost:evil.example",
            "127.0.0.1:",
            "[::1]evil",
            "",
        ] {
            assert!(!is_loopback_host(host), "{host}");
        }
    }
}
