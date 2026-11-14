//! Static files: the web build, from a directory or compiled in.
//!
//! One handler serves both sources with the same rules:
//!
//! * the request path is percent-decoded once, then every segment is
//!   checked; `.` and `..` are refused outright (400) and hidden files are
//!   not served (404), so nothing outside the root is ever opened;
//! * a path that does not name a file and has no extension is a client-side
//!   route and gets `index.html`; a missing file with an extension is a 404
//!   (answering HTML for a missing `.wasm` only hides the problem);
//! * content-hashed files (Trunk puts 16 hex digits in the name) are
//!   immutable and cached for a year, everything else is revalidated with
//!   an `ETag` on every load, so a rebuilt frontend is picked up at once;
//! * when there is no `index.html` at all, a short page explains how to
//!   build the frontend.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use axum::body::Body;
use axum::http::header::{self, HeaderMap, HeaderValue};
use axum::http::{Method, Response, StatusCode};
use bytes::Bytes;
use percent_encoding::percent_decode_str;

/// Files compiled in by `build.rs` (empty without the `embed-dist` feature
/// or without a built frontend).
static EMBEDDED: &[(&str, &[u8])] = include!(concat!(env!("OUT_DIR"), "/embedded_dist.rs"));

const INDEX: &str = "index.html";
const NO_CACHE: &str = "no-cache";
const IMMUTABLE: &str = "public, max-age=31536000, immutable";

/// Shown instead of the application while the frontend has not been built.
const PLACEHOLDER: &str = r#"<!doctype html>
<html lang="fr">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Splat Live</title>
<style>
  body { margin: 0; min-height: 100vh; display: grid; place-items: center; background: #101418; color: #e6e9ee;
         font: 16px/1.5 system-ui, sans-serif; }
  main { max-width: 40rem; padding: 2rem; }
  h1 { font-size: 1.5rem; margin: 0 0 1rem; }
  code, pre { font: 0.9rem/1.5 ui-monospace, monospace; background: #1c2229; border-radius: 6px; }
  code { padding: 0.1rem 0.35rem; }
  pre { padding: 0.9rem 1rem; overflow-x: auto; }
  a { color: #7cc4ff; }
</style>
</head>
<body>
<main>
<h1>Splat Live : le serveur fonctionne, l'interface n'est pas encore construite</h1>
<p>Le serveur de capture tourne et répond sur <a href="/api/info"><code>/api/info</code></a>, mais il n'a trouvé
aucune interface web à servir.</p>
<p>Pour la construire, depuis la racine du dépôt :</p>
<pre>cd crates/gs-app
trunk build --release</pre>
<p>Relancez ensuite le serveur en lui indiquant le dossier produit :</p>
<pre>cargo run --release -p gs-server -- --dist crates/gs-app/dist</pre>
<p>Si le serveur a été lancé avec <code>--dist</code>, il suffit de recharger cette page une fois la construction
terminée.</p>
</main>
</body>
</html>
"#;

/// Whether a web build was compiled in.
pub(crate) fn has_embedded_index() -> bool {
    EMBEDDED.iter().any(|(path, _)| *path == INDEX)
}

/// Where the web build comes from.
pub(crate) enum Assets {
    /// A directory read at request time (it may be rebuilt while the
    /// server runs).
    Dir(PathBuf),
    /// The files compiled into the binary, by path.
    Embedded(HashMap<&'static str, EmbeddedFile>),
}

pub(crate) struct EmbeddedFile {
    bytes: &'static [u8],
    etag: String,
}

/// A file ready to be answered.
struct Found {
    /// Path it was found under, for the MIME type and the cache policy.
    name: String,
    etag: String,
    body: FoundBody,
}

enum FoundBody {
    Static(&'static [u8]),
    /// Read only once the conditional request is known to need it.
    File(PathBuf),
}

enum Lookup {
    Found(Found),
    Placeholder,
    NotFound,
    BadPath,
}

/// The decoded segments of a request path, or `None` when the path must be
/// refused: not UTF-8 once decoded, a NUL or a backslash, or a `.` / `..`
/// segment. Decoding happens exactly once, before the split, so `%2e%2e`
/// and `..%2f` are seen as what they mean.
fn decode_path(raw: &str) -> Option<Vec<String>> {
    let decoded = percent_decode_str(raw).decode_utf8().ok()?;
    if decoded.contains(['\0', '\\']) {
        return None;
    }
    let mut segments = Vec::new();
    for segment in decoded.split('/') {
        match segment {
            "" => {}
            "." | ".." => return None,
            // A drive letter or an alternate data stream on Windows.
            s if s.contains(':') => return None,
            s => segments.push(s.to_string()),
        }
    }
    Some(segments)
}

/// A path without a file extension in its last segment names a route of
/// the single-page application, not a file.
fn is_route(segments: &[String]) -> bool {
    segments.last().is_none_or(|last| !last.contains('.'))
}

/// Trunk names its hashed outputs `name-<16 hex digits>.ext` (and
/// `name-<hash>_bg.wasm`).
fn is_content_hashed(name: &str) -> bool {
    let file = name.rsplit('/').next().unwrap_or(name);
    file.split('-').skip(1).any(|part| {
        let digits = part.bytes().take_while(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')).count();
        digits >= 16 && matches!(part.as_bytes().get(digits), Some(b'.' | b'_'))
    })
}

fn content_type(name: &str) -> HeaderValue {
    let mime = mime_guess::from_path(name).first_or_octet_stream();
    let text = matches!(mime.subtype().as_str(), "javascript" | "json" | "xml")
        || mime.type_() == mime_guess::mime::TEXT
        || mime.essence_str() == "image/svg+xml";
    let value = if text && mime.get_param("charset").is_none() {
        format!("{}; charset=utf-8", mime.essence_str())
    } else {
        mime.essence_str().to_string()
    };
    HeaderValue::from_str(&value).unwrap_or(HeaderValue::from_static("application/octet-stream"))
}

/// Whether an `If-None-Match` header names `etag` (weak comparison).
fn etag_matches(header: &HeaderValue, etag: &str) -> bool {
    let strip = |tag: &str| tag.trim().trim_start_matches("W/").to_string();
    let wanted = strip(etag);
    header
        .to_str()
        .is_ok_and(|value| value.split(',').any(|candidate| candidate.trim() == "*" || strip(candidate) == wanted))
}

impl Assets {
    /// The compiled-in web build (possibly empty).
    pub fn embedded() -> Self {
        let files = EMBEDDED
            .iter()
            .map(|(path, bytes)| {
                let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
                let etag: String = digest.as_ref()[..8].iter().map(|b| format!("{b:02x}")).collect();
                (*path, EmbeddedFile { bytes, etag: format!("\"{etag}\"") })
            })
            .collect();
        Self::Embedded(files)
    }

    /// A short description for the logs.
    pub fn describe(&self) -> String {
        match self {
            Self::Dir(dir) => format!("directory {}", dir.display()),
            Self::Embedded(files) if files.contains_key(INDEX) => format!("embedded web build ({} files)", files.len()),
            Self::Embedded(_) => "placeholder page (no web build available)".to_string(),
        }
    }

    /// Whether an `index.html` is available right now.
    pub fn has_index(&self) -> bool {
        match self {
            Self::Dir(dir) => dir.join(INDEX).is_file(),
            Self::Embedded(files) => files.contains_key(INDEX),
        }
    }

    async fn find_in_dir(root: &Path, segments: &[String]) -> Option<Found> {
        // Resolved on every request: the build tool may replace the whole
        // directory while the server runs.
        let root = tokio::fs::canonicalize(root).await.ok()?;
        let mut path = root.clone();
        path.extend(segments);
        // Symbolic links must not lead out of the root either.
        let path = tokio::fs::canonicalize(&path).await.ok()?;
        if !path.starts_with(&root) {
            return None;
        }
        let meta = tokio::fs::metadata(&path).await.ok()?;
        if !meta.is_file() {
            return None;
        }
        let modified = meta.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
        Some(Found {
            name: segments.join("/"),
            etag: format!("W/\"{:x}-{:x}\"", meta.len(), modified),
            body: FoundBody::File(path),
        })
    }

    async fn find(&self, segments: &[String]) -> Option<Found> {
        match self {
            Self::Dir(root) => Self::find_in_dir(root, segments).await,
            Self::Embedded(files) => {
                let name = segments.join("/");
                let file = files.get(name.as_str())?;
                Some(Found { name, etag: file.etag.clone(), body: FoundBody::Static(file.bytes) })
            }
        }
    }

    async fn lookup(&self, raw_path: &str) -> Lookup {
        let Some(segments) = decode_path(raw_path) else { return Lookup::BadPath };
        if segments.iter().any(|s| s.starts_with('.')) {
            return Lookup::NotFound;
        }
        // The API lives next to the files; an unknown endpoint is an error,
        // not a page.
        if segments.first().is_some_and(|s| s == "api") {
            return Lookup::NotFound;
        }
        if !segments.is_empty() {
            if let Some(found) = self.find(&segments).await {
                return Lookup::Found(found);
            }
        }
        // Asking for the index by name is the one "file" that falls
        // through to the placeholder below.
        let wants_index = segments.len() == 1 && segments[0] == INDEX;
        if !is_route(&segments) && !wants_index {
            return Lookup::NotFound;
        }
        match self.find(&[INDEX.to_string()]).await {
            Some(index) => Lookup::Found(index),
            None => Lookup::Placeholder,
        }
    }

    /// Answers a request for a static file.
    pub async fn serve(&self, method: &Method, raw_path: &str, headers: &HeaderMap) -> Response<Body> {
        if method != Method::GET && method != Method::HEAD {
            return plain(StatusCode::METHOD_NOT_ALLOWED, "method not allowed", &[(header::ALLOW, "GET, HEAD")]);
        }
        let found = match self.lookup(raw_path).await {
            Lookup::Found(found) => found,
            Lookup::Placeholder => {
                return file_response(
                    StatusCode::OK,
                    content_type(INDEX),
                    NO_CACHE,
                    None,
                    Bytes::from_static(PLACEHOLDER.as_bytes()),
                )
            }
            Lookup::NotFound => return plain(StatusCode::NOT_FOUND, "not found", &[]),
            Lookup::BadPath => return plain(StatusCode::BAD_REQUEST, "bad path", &[]),
        };
        let cache = if is_content_hashed(&found.name) { IMMUTABLE } else { NO_CACHE };
        let mime = content_type(&found.name);
        if headers.get(header::IF_NONE_MATCH).is_some_and(|value| etag_matches(value, &found.etag)) {
            return file_response(StatusCode::NOT_MODIFIED, mime, cache, Some(&found.etag), Bytes::new());
        }
        let body = match found.body {
            FoundBody::Static(bytes) => Bytes::from_static(bytes),
            FoundBody::File(path) => match tokio::fs::read(&path).await {
                Ok(bytes) => Bytes::from(bytes),
                // Deleted between the lookup and the read (a rebuild).
                Err(_) => return plain(StatusCode::NOT_FOUND, "not found", &[]),
            },
        };
        file_response(StatusCode::OK, mime, cache, Some(&found.etag), body)
    }
}

fn file_response(
    status: StatusCode,
    mime: HeaderValue,
    cache: &'static str,
    etag: Option<&str>,
    body: Bytes,
) -> Response<Body> {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, mime);
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    if let Some(etag) = etag.and_then(|e| HeaderValue::from_str(e).ok()) {
        headers.insert(header::ETAG, etag);
    }
    response
}

fn plain(status: StatusCode, text: &'static str, extra: &[(header::HeaderName, &'static str)]) -> Response<Body> {
    let mut response = Response::new(Body::from(text));
    *response.status_mut() = status;
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    for (name, value) in extra {
        headers.insert(name.clone(), HeaderValue::from_static(value));
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segments(raw: &str) -> Option<Vec<String>> {
        decode_path(raw)
    }

    #[test]
    fn paths_are_decoded_once_and_checked() {
        assert_eq!(segments("/"), Some(vec![]));
        assert_eq!(segments("/a/b.js"), Some(vec!["a".into(), "b.js".into()]));
        assert_eq!(segments("//a///b"), Some(vec!["a".into(), "b".into()]));
        assert_eq!(segments("/caf%C3%A9.txt"), Some(vec!["café.txt".into()]));
        for bad in [
            "/..",
            "/../secret",
            "/a/../../secret",
            "/%2e%2e/secret",
            "/%2E%2E/secret",
            "/.%2e/secret",
            "/a/..%2f..%2fsecret",
            "/a/%2e%2e%2fsecret",
            "/..%5csecret",
            "/a\\..\\secret",
            "/./a",
            "/a/%00",
            "/%ff",
            "/c:/windows",
            "/a/file.txt::$DATA",
        ] {
            assert_eq!(segments(bad), None, "{bad} must be refused");
        }
        // Doubly encoded dots are an ordinary (odd) file name, not a parent.
        assert_eq!(segments("/%252e%252e/x"), Some(vec!["%2e%2e".into(), "x".into()]));
    }

    #[test]
    fn routes_and_files() {
        let path = |s: &str| decode_path(s).unwrap();
        assert!(is_route(&path("/")));
        assert!(is_route(&path("/scan")));
        assert!(is_route(&path("/a/b/")));
        assert!(!is_route(&path("/missing.wasm")));
        assert!(!is_route(&path("/a/b.css")));
    }

    #[test]
    fn hashed_names_are_recognised() {
        for hashed in ["app-4533768f47c08d1a.js", "app-2ea6af47383db65b_bg.wasm", "a/b/style-b5dddad573e6cc72.css"] {
            assert!(is_content_hashed(hashed), "{hashed}");
        }
        for plain in
            ["index.html", "worker.js", "worker_bg.wasm", "app-123.js", "my-deadbeefdeadbeefcafe", "icon-large.png"]
        {
            assert!(!is_content_hashed(plain), "{plain}");
        }
    }

    #[test]
    fn mime_types() {
        assert_eq!(content_type("app_bg.wasm"), "application/wasm");
        assert_eq!(content_type("index.html"), "text/html; charset=utf-8");
        assert_eq!(content_type("app.js"), "text/javascript; charset=utf-8");
        assert_eq!(content_type("style.css"), "text/css; charset=utf-8");
        assert_eq!(content_type("data.json"), "application/json; charset=utf-8");
        assert_eq!(content_type("qr.svg"), "image/svg+xml; charset=utf-8");
        assert_eq!(content_type("icon.png"), "image/png");
        assert_eq!(content_type("scene.splat"), "application/octet-stream");
    }

    #[test]
    fn etag_comparison() {
        let header = |s: &'static str| HeaderValue::from_static(s);
        assert!(etag_matches(&header("\"abc\""), "\"abc\""));
        assert!(etag_matches(&header("W/\"abc\""), "\"abc\""));
        assert!(etag_matches(&header("\"x\", W/\"abc\""), "W/\"abc\""));
        assert!(etag_matches(&header("*"), "\"abc\""));
        assert!(!etag_matches(&header("\"abd\""), "\"abc\""));
    }

    #[test]
    fn placeholder_is_french_without_em_dashes() {
        assert!(PLACEHOLDER.contains("trunk build --release"));
        assert!(PLACEHOLDER.contains("lang=\"fr\""));
        assert!(!PLACEHOLDER.contains('\u{2014}') && !PLACEHOLDER.contains('\u{2013}'));
    }
}
