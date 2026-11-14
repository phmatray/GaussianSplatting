//! Where the splats come from: a native `gs-server` or the local worker.

use serde::{Deserialize, Serialize};

/// The kind of pipeline host the page is connected to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    /// A native `gs-server` reached over a WebSocket.
    Server,
    /// The `splat-worker` Web Worker running the pipeline in WebAssembly.
    Worker,
}

impl SourceKind {
    /// Label of the source badge.
    pub fn label(self) -> &'static str {
        match self {
            Self::Server => "Serveur natif",
            Self::Worker => "Local (WASM)",
        }
    }

    /// Identifier used by the `?source=` query parameter and the debug hook.
    pub fn id(self) -> &'static str {
        match self {
            Self::Server => "server",
            Self::Worker => "worker",
        }
    }
}

/// State of the link to the source.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link {
    /// Looking for a source, or connecting for the first time.
    Connecting,
    Connected,
    /// The connection dropped; attempt number `attempt` is scheduled or
    /// under way.
    Reconnecting { attempt: u32 },
    /// No source could be started.
    Failed,
}

impl Link {
    pub fn id(self) -> &'static str {
        match self {
            Self::Connecting => "connecting",
            Self::Connected => "connected",
            Self::Reconnecting { .. } => "reconnecting",
            Self::Failed => "failed",
        }
    }

    /// Short text shown next to the source name; empty when all is well.
    pub fn label(self) -> &'static str {
        match self {
            Self::Connecting => "Connexion…",
            Self::Connected => "",
            Self::Reconnecting { .. } => "Reconnexion…",
            Self::Failed => "Hors ligne",
        }
    }
}

/// `lan` section of the server info.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LanInfo {
    pub enabled: bool,
    /// `https://<lan-ip>:<port>` for every LAN address; empty while off.
    pub urls: Vec<String>,
    /// SHA-256 fingerprint of the self-signed certificate.
    pub fingerprint: String,
}

/// Answer of `GET /api/info`, `POST /api/lan` and the Tauri command
/// `server_info` (see "Server interface" in `docs/ARCHITECTURE.md`).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
    pub protocol: u32,
    pub http_url: String,
    pub ws_path: String,
    pub lan: LanInfo,
    pub clients: u32,
}

impl ServerInfo {
    /// Name every `gs-server` reports; anything else answering on
    /// `/api/info` is not our server.
    pub const NAME: &'static str = "splat-live";

    /// Parses an info document, rejecting answers of other services (a
    /// static host that serves its index page for unknown paths, say).
    pub fn parse(json: &str) -> Result<Self, String> {
        let info: Self = serde_json::from_str(json).map_err(|e| format!("réponse illisible\u{a0}: {e}"))?;
        if info.name != Self::NAME {
            return Err("ce n'est pas un serveur Splat Live".to_string());
        }
        if info.protocol != gs_core::protocol::PROTOCOL_VERSION {
            return Err(format!(
                "version de protocole {} incompatible (attendu\u{a0}: {})",
                info.protocol,
                gs_core::protocol::PROTOCOL_VERSION
            ));
        }
        Ok(info)
    }

    /// WebSocket path, `/ws` when the server did not say.
    pub fn ws_path(&self) -> &str {
        if self.ws_path.is_empty() { "/ws" } else { &self.ws_path }
    }
}

/// `base` without trailing slashes.
fn trim_base(base: &str) -> &str {
    base.trim_end_matches('/')
}

/// URL of a server route: `api_url("http://127.0.0.1:7878", "/api/info")`.
pub fn api_url(base: &str, path: &str) -> String {
    format!("{}/{}", trim_base(base), path.trim_start_matches('/'))
}

/// WebSocket URL for an HTTP base: `http` becomes `ws`, `https` becomes
/// `wss`. `None` for any other scheme.
pub fn ws_url(base: &str, ws_path: &str) -> Option<String> {
    let base = trim_base(base);
    let rest = match base.strip_prefix("https://") {
        Some(rest) => format!("wss://{rest}"),
        None => format!("ws://{}", base.strip_prefix("http://")?),
    };
    Some(format!("{rest}/{}", ws_path.trim_start_matches('/')))
}

/// True when `base` is the server's loopback listener (plain HTTP on a
/// loopback address, such as `http://127.0.0.1:7878`): the page then runs
/// on the machine of the server and may switch LAN sharing. The server
/// refuses that on its LAN listener, which is the HTTPS one, from whatever
/// address it is reached.
pub fn is_loopback_base(base: &str) -> bool {
    let Some(rest) = base.strip_prefix("http://") else { return false };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    // No user info in our URLs; an IPv6 host is bracketed.
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => authority.rsplit_once(':').map_or(authority, |(host, _port)| host),
    };
    let host = host.to_ascii_lowercase();
    host == "localhost"
        || host.ends_with(".localhost")
        || host == "::1"
        || host.parse::<std::net::Ipv4Addr>().is_ok_and(|ip| ip.is_loopback())
}

/// Value of the `?source=` query parameter.
pub fn forced_source(value: Option<&str>) -> Option<SourceKind> {
    match value {
        Some("worker") => Some(SourceKind::Worker),
        Some("server") => Some(SourceKind::Server),
        _ => None,
    }
}

/// Delay before reconnection attempt `attempt` (1-based), in milliseconds:
/// 0.5 s doubling up to 5 s.
pub fn reconnect_delay_ms(attempt: u32) -> u32 {
    let doubled = 500u32.saturating_mul(1u32 << attempt.saturating_sub(1).min(8));
    doubled.min(5000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_documented_info() {
        let json = r#"{
          "name": "splat-live", "version": "0.1.0", "protocol": 1,
          "http_url": "http://127.0.0.1:7878", "ws_path": "/ws",
          "lan": { "enabled": true, "urls": ["https://192.168.1.20:7879"], "fingerprint": "AB:CD" },
          "clients": 1
        }"#;
        let info = ServerInfo::parse(json).unwrap();
        assert_eq!(info.http_url, "http://127.0.0.1:7878");
        assert!(info.lan.enabled);
        assert_eq!(info.lan.urls, ["https://192.168.1.20:7879"]);
        assert_eq!(info.lan.fingerprint, "AB:CD");
        assert_eq!(ws_url(&info.http_url, info.ws_path()).unwrap(), "ws://127.0.0.1:7878/ws");
    }

    #[test]
    fn rejects_foreign_answers() {
        assert!(ServerInfo::parse("<!doctype html><html></html>").is_err());
        assert!(ServerInfo::parse(r#"{"name":"something-else","protocol":1}"#).is_err());
        assert!(ServerInfo::parse(r#"{"name":"splat-live","protocol":99}"#).is_err());
        // Missing optional fields are fine.
        let info = ServerInfo::parse(r#"{"name":"splat-live","protocol":1}"#).unwrap();
        assert_eq!(info.ws_path(), "/ws");
        assert!(!info.lan.enabled && info.lan.urls.is_empty());
    }

    #[test]
    fn urls() {
        assert_eq!(ws_url("https://192.168.1.20:7879/", "/ws").unwrap(), "wss://192.168.1.20:7879/ws");
        assert_eq!(ws_url("http://localhost:7878", "ws").unwrap(), "ws://localhost:7878/ws");
        assert_eq!(ws_url("tauri://localhost", "/ws"), None);
        assert_eq!(api_url("http://127.0.0.1:7878/", "/api/lan"), "http://127.0.0.1:7878/api/lan");
        assert_eq!(api_url("http://127.0.0.1:7878", "api/qr.svg?i=1"), "http://127.0.0.1:7878/api/qr.svg?i=1");
    }

    #[test]
    fn loopback_bases() {
        for base in ["http://127.0.0.1:7878", "http://127.0.0.1", "http://localhost:7878/", "http://LOCALHOST", "http://[::1]:7878", "http://127.8.9.10:1", "http://127.0.0.1:7878/x?y"] {
            assert!(is_loopback_base(base), "{base}");
        }
        for base in ["https://192.168.1.20:7879", "https://127.0.0.1:7879", "https://localhost:7879/", "http://10.0.0.1:7878", "http://example.com", "http://127.0.0.1.example.com:7878", "http://localhost.example.com", "tauri://localhost", "", "http://", "http://[fe80::1]:7878", "http://1270.0.0.1"] {
            assert!(!is_loopback_base(base), "{base}");
        }
    }

    #[test]
    fn forced_source_values() {
        assert_eq!(forced_source(Some("worker")), Some(SourceKind::Worker));
        assert_eq!(forced_source(Some("server")), Some(SourceKind::Server));
        assert_eq!(forced_source(Some("bogus")), None);
        assert_eq!(forced_source(None), None);
    }

    #[test]
    fn backoff_grows_and_saturates() {
        let delays: Vec<u32> = (1..=6).map(reconnect_delay_ms).collect();
        assert_eq!(delays, [500, 1000, 2000, 4000, 5000, 5000]);
        assert_eq!(reconnect_delay_ms(0), 500);
        assert_eq!(reconnect_delay_ms(u32::MAX), 5000);
    }
}
