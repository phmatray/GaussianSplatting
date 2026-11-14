//! Routes of real capture, recording and sharing.
//!
//! | Route | Listener | Guard |
//! |---|---|---|
//! | `GET /api/pair`, `GET /api/pair.svg` | loopback | `Origin` |
//! | `POST /api/captures` | loopback | `Origin` |
//! | `POST /api/captures` | LAN | pairing token (`Authorization: Bearer`) |
//! | `GET /api/captures`, `POST /api/captures/open`, `GET`/`DELETE /captures/<file>` | loopback | `Origin` on what changes something |
//! | `GET /api/record`, `GET /api/recordings`, `GET /recordings/<file>`, `GET /api/share/<name>.zip` | both | read-only |
//! | `POST /api/record`, `DELETE /recordings/<file>` | loopback | `Origin` |
//! | `GET /api/refine`, `POST /api/refine`, `DELETE /refined/<file>` | loopback | `Origin` on what changes something |
//! | `GET /api/refined`, `GET /refined/<file>`, `GET /api/share/refined/<name>.zip` | both | read-only |
//!
//! Errors are JSON `{"error": "<English sentence>", "code": "<stable code>"}`;
//! an interface shows its own text for the code.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{FromRequest, Path as UrlPath, Query, Request, State};
use axum::http::header::{self, HeaderMap, HeaderValue};
use axum::http::{Method, StatusCode, Uri, Version};
use axum::response::{IntoResponse, Response};
use axum::Json;
use bytes::Bytes;
use futures_util::{StreamExt, TryStreamExt};
use gs_core::ingest::file::FILE_MAGIC;
use gs_core::ingest::ImportError;
use serde::Deserialize;
use serde_json::json;
use tokio::io::AsyncWriteExt;
use tokio::sync::oneshot;

use crate::capture::{capture_stem, import_capture, Imported};
use crate::files;
use crate::http::{json_error, no_store, origin_allowed, AppState, Listener};
use crate::hub::{Command, RecordRefusal};
use crate::net;
use crate::refine::{Refusal, Task};
use crate::store::{self, Kind};
use crate::zip::{Part, ZipBuilder, ZipPlan};

/// Longest name of a recording, in characters (as for a capture).
const MAX_TITLE_CHARS: usize = 120;
/// How much of a request body is read and thrown away before an early
/// refusal is answered over HTTP/1, and for how long. A client that is
/// still sending when the connection closes gets a reset and may never
/// see the answer; a small upload is therefore read to its end first.
const DRAIN_BYTES: u64 = 64 << 20;
const DRAIN_TIME: Duration = Duration::from_secs(10);
/// Name of the recording inside a share archive, and of the file that
/// names it.
const SHARED_RECORDING: &str = "recording.gsrec";
const SHARE_MANIFEST: &str = "share.json";
/// Name of a refined result inside its share archive.
const SHARED_SCENE: &str = "scene.ply";
/// Below this much free space where captures are written the interface is
/// told that the disk is nearly full: a phone sends up to 9 MB per second.
const LOW_DISK_BYTES: u64 = 2 << 30;
/// An upload is refused when the disk would be left with less than this.
const UPLOAD_MARGIN_BYTES: u64 = 64 << 20;
/// The web build of a share archive is held in memory while the archive
/// is laid out; a "web build" larger than this is not one.
const MAX_SHARED_ASSETS: u64 = 256 << 20;

fn loopback_only(state: &AppState) -> Option<Response> {
    (state.listener != Listener::Loopback)
        .then(|| json_error(StatusCode::FORBIDDEN, "loopback_only", "only available on the machine itself"))
}

fn origin_checked(state: &AppState, headers: &HeaderMap, uri: &Uri) -> Option<Response> {
    (!origin_allowed(state, headers, uri))
        .then(|| json_error(StatusCode::FORBIDDEN, "origin_not_allowed", "origin not allowed"))
}

fn ok_json(value: serde_json::Value) -> Response {
    no_store(Json(value).into_response())
}

fn internal(context: &str, err: impl std::fmt::Display) -> Response {
    log::error!("{context} : {err}");
    json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", err.to_string())
}

fn pipeline_gone() -> Response {
    json_error(StatusCode::SERVICE_UNAVAILABLE, "unavailable", "the pipeline is not running")
}

/// Compares two byte strings in time that depends only on their lengths.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// -- pairing ------------------------------------------------------------------

#[derive(Deserialize)]
pub(crate) struct IndexQuery {
    /// Index into the LAN addresses, as for `/api/qr.svg`.
    #[serde(default)]
    i: usize,
}

/// The pairing URI of the LAN address number `index`, or the refusal to
/// answer. With it, every URI.
fn pairing_uri(state: &AppState, headers: &HeaderMap, uri: &Uri, index: usize) -> Result<(String, Vec<String>), Box<Response>> {
    // The token opens `/ingest`: it is shown at the machine, never handed
    // out over the network.
    if let Some(refusal) = loopback_only(state).or_else(|| origin_checked(state, headers, uri)) {
        return Err(Box::new(refusal));
    }
    if !state.core.info().lan.enabled {
        let refusal = json_error(StatusCode::NOT_FOUND, "lan_off", "LAN access is off: a phone cannot reach this server");
        return Err(Box::new(refusal));
    }
    let uris = state.core.pairing_uris();
    match uris.get(index) {
        Some(uri) => Ok((uri.clone(), uris)),
        None if uris.is_empty() => {
            Err(Box::new(json_error(StatusCode::NOT_FOUND, "no_address", "this machine has no network address")))
        }
        None => Err(Box::new(json_error(StatusCode::NOT_FOUND, "no_address", "no such LAN address"))),
    }
}

pub(crate) async fn pair(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Query(query): Query<IndexQuery>,
) -> Response {
    match pairing_uri(&state, &headers, &uri, query.i) {
        Ok((uri, uris)) => ok_json(json!({ "uri": uri, "uris": uris })),
        Err(refusal) => *refusal,
    }
}

pub(crate) async fn pair_svg(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    Query(query): Query<IndexQuery>,
) -> Response {
    let uri = match pairing_uri(&state, &headers, &uri, query.i) {
        Ok((uri, _)) => uri,
        Err(refusal) => return *refusal,
    };
    match net::qr_svg(&uri) {
        Some(svg) => no_store(([(header::CONTENT_TYPE, HeaderValue::from_static("image/svg+xml"))], svg).into_response()),
        None => json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", "the URI does not fit in a QR code"),
    }
}

// -- the record button --------------------------------------------------------

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum RecordRequest {
    Start {
        #[serde(default)]
        name: Option<String>,
    },
    Stop,
}

fn record_refusal(refusal: RecordRefusal) -> Response {
    match refusal {
        RecordRefusal::AlreadyRecording(status) => no_store(
            (
                StatusCode::CONFLICT,
                Json(json!({ "error": "a recording is already running", "code": "already_recording", "recording": status })),
            )
                .into_response(),
        ),
        RecordRefusal::NotRecording => json_error(StatusCode::CONFLICT, "not_recording", "no recording is running"),
        RecordRefusal::NoDirectory => no_recordings_dir(),
        RecordRefusal::Disk(reason) => internal("impossible de créer l'enregistrement", reason),
    }
}

fn no_recordings_dir() -> Response {
    json_error(StatusCode::SERVICE_UNAVAILABLE, "no_recordings_dir", "the server has no recordings directory")
}

/// Reads a JSON body, but not for ever.
async fn json_body<T: serde::de::DeserializeOwned>(state: &AppState, request: Request) -> Result<T, Response> {
    let body: Result<Result<Json<T>, JsonRejection>, _> =
        tokio::time::timeout(state.core.settings.request_timeout, Json::from_request(request, &())).await;
    match body {
        Ok(Ok(Json(body))) => Ok(body),
        Ok(Err(rejection)) => Err(json_error(rejection.status(), "bad_request", rejection.body_text())),
        Err(_) => Err(json_error(StatusCode::REQUEST_TIMEOUT, "timeout", "the request body did not arrive in time")),
    }
}

pub(crate) async fn record(State(state): State<AppState>, request: Request) -> Response {
    if let Some(refusal) =
        loopback_only(&state).or_else(|| origin_checked(&state, request.headers(), request.uri()))
    {
        return refusal;
    }
    let request: RecordRequest = match json_body(&state, request).await {
        Ok(request) => request,
        Err(refusal) => return refusal,
    };
    match request {
        RecordRequest::Start { name } => {
            // Control characters have no place in a name (a tab or a line
            // break is still a separation between two words).
            let title = name.map(|name| {
                name.chars().map(|c| if c.is_control() { ' ' } else { c }).take(MAX_TITLE_CHARS).collect::<String>()
            });
            let (reply, answer) = oneshot::channel();
            if !state.core.hub.send(Command::RecordStart { title, reply }) {
                return pipeline_gone();
            }
            match answer.await {
                Ok(Ok(status)) => ok_json(json!({ "recording": status })),
                Ok(Err(refusal)) => record_refusal(refusal),
                Err(_) => pipeline_gone(),
            }
        }
        RecordRequest::Stop => {
            let (reply, answer) = oneshot::channel();
            if !state.core.hub.send(Command::RecordStop { reply }) {
                return pipeline_gone();
            }
            match answer.await {
                Ok(Ok(saved)) => ok_json(json!({ "recording": null, "saved": saved })),
                Ok(Err(refusal)) => record_refusal(refusal),
                Err(_) => pipeline_gone(),
            }
        }
    }
}

pub(crate) async fn record_status(State(state): State<AppState>) -> Response {
    let record = state.core.record.lock().unwrap_or_else(|p| p.into_inner()).clone();
    let mut answer = serde_json::to_value(record).unwrap_or_default();
    // The state of the disk is the business of the machine's own user: the
    // interface there polls this route anyway, and learns from it that the
    // disk is filling up or has refused a capture.
    if state.listener == Listener::Loopback {
        let store = &state.core.store;
        let free = store.free_bytes(Kind::Capture).or_else(|| store.free_bytes(Kind::Recording));
        answer["storage"] = json!({
            "free_bytes": free,
            "low": free.is_some_and(|free| free < LOW_DISK_BYTES),
            "problem": store.problem(),
        });
    }
    ok_json(answer)
}

// -- recordings -----------------------------------------------------------------

pub(crate) async fn recordings(State(state): State<AppState>) -> Response {
    let store = state.core.store.clone();
    let Ok(list) = tokio::task::spawn_blocking(move || store.list_recordings()).await else {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", "internal error");
    };
    let recording = state.core.record.lock().unwrap_or_else(|p| p.into_inner()).recording.clone();
    let mut answer = json!({ "recordings": list, "recording": recording });
    // Where the files are is the business of the machine's own user.
    if state.listener == Listener::Loopback {
        answer["dir"] = json!(state.core.store.dir(Kind::Recording).map(|dir| dir.display().to_string()));
    }
    ok_json(answer)
}

#[derive(Deserialize)]
pub(crate) struct FileQuery {
    /// Present: answer with `Content-Disposition: attachment`.
    download: Option<String>,
}

async fn stored_file(
    state: &AppState,
    kind: Kind,
    file: &str,
    method: &Method,
    headers: &HeaderMap,
    query: &FileQuery,
) -> Response {
    match state.core.store.existing(kind, file) {
        Some(path) => {
            let stall = state.core.settings.send_timeout;
            files::serve(&path, method, headers, query.download.as_ref().map(|_| file), stall).await
        }
        None => json_error(StatusCode::NOT_FOUND, "not_found", "no such file"),
    }
}

pub(crate) async fn recording_file(
    State(state): State<AppState>,
    UrlPath(file): UrlPath<String>,
    Query(query): Query<FileQuery>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    stored_file(&state, Kind::Recording, &file, &method, &headers, &query).await
}

async fn delete_stored(state: &AppState, kind: Kind, file: String, headers: &HeaderMap, uri: &Uri) -> Response {
    if let Some(refusal) = loopback_only(state).or_else(|| origin_checked(state, headers, uri)) {
        return refusal;
    }
    let store = state.core.store.clone();
    match tokio::task::spawn_blocking(move || store.delete(kind, &file)).await {
        Ok(Ok(true)) => ok_json(json!({ "ok": true })),
        Ok(Ok(false)) => json_error(StatusCode::NOT_FOUND, "not_found", "no such file"),
        Ok(Err(err)) if err.kind() == io::ErrorKind::ResourceBusy => {
            json_error(StatusCode::CONFLICT, "in_use", "the file is being written")
        }
        Ok(Err(err)) => internal("suppression impossible", err),
        Err(_) => json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", "internal error"),
    }
}

pub(crate) async fn delete_recording(
    State(state): State<AppState>,
    UrlPath(file): UrlPath<String>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    delete_stored(&state, Kind::Recording, file, &headers, &uri).await
}

// -- sharing ----------------------------------------------------------------------

/// CRC-32 of the first `len` bytes of the file at `path`.
fn crc_of_file(path: &Path, len: u64) -> io::Result<u32> {
    let mut hasher = crc32fast::Hasher::new();
    let mut file = std::fs::File::open(path)?.take(len);
    let mut block = vec![0u8; 1 << 20];
    let mut seen = 0u64;
    loop {
        match file.read(&mut block) {
            Ok(0) => break,
            Ok(read) => {
                hasher.update(&block[..read]);
                seen += read as u64;
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    if seen != len {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the recording changed while it was read"));
    }
    Ok(hasher.finalize())
}

/// Length and CRC-32 of a recording. The checksum costs a read of the
/// whole file, so it is remembered for as long as the file stays the same
/// (same length, same modification time): sharing a recording twice reads
/// it once.
fn recording_sum(core: &crate::Core, file: &str, path: &Path) -> io::Result<(u64, u32)> {
    let meta = std::fs::metadata(path)?;
    let modified = meta.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
    let key = (meta.len(), modified);
    let known = core.share_sums.lock().unwrap_or_else(|p| p.into_inner()).get(file).copied();
    if let Some((len, modified, crc)) = known {
        if (len, modified) == key {
            return Ok((len, crc));
        }
    }
    let crc = crc_of_file(path, meta.len())?;
    let mut sums = core.share_sums.lock().unwrap_or_else(|p| p.into_inner());
    // Recordings come and go; the table does not grow with them for ever.
    if sums.len() >= 256 {
        sums.clear();
    }
    sums.insert(file.to_string(), (key.0, key.1, crc));
    Ok((meta.len(), crc))
}

/// Lays out a share archive: the web build, the shared file (the first
/// `len` bytes of `file`, of checksum `crc`) under the fixed name `shared`
/// and the `share.json` that names it.
fn plan_archive(
    assets: Vec<(String, Vec<u8>)>,
    file: PathBuf,
    (len, crc): (u64, u32),
    shared: &str,
    manifest: &serde_json::Value,
) -> ZipPlan {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let mut zip = ZipBuilder::new(i64::try_from(now).unwrap_or(0));
    for (name, data) in &assets {
        // A web build that was itself unpacked from a share archive: its
        // manifest and what it shared give way to the ones added below (two
        // entries of one name would be unpacked in an order nobody chose).
        if name != SHARE_MANIFEST && name != SHARED_RECORDING && name != SHARED_SCENE {
            zip.add_bytes(name, data);
        }
    }
    zip.add_bytes(SHARE_MANIFEST, format!("{manifest:#}\n").as_bytes());
    zip.add_stored_file(shared, file, len, crc);
    zip.finish()
}

/// Lays out the share archive of one recording.
fn plan_share(
    assets: Vec<(String, Vec<u8>)>,
    recording: PathBuf,
    (len, crc): (u64, u32),
    entry: &store::RecordingEntry,
) -> io::Result<ZipPlan> {
    // The viewer reads this file when its URL names no recording: `file`
    // is what the `file` URL parameter would be, relative to the page, and
    // `autoplay` and `loop` are those parameters too (a visitor of a shared
    // page should see it move without looking for a button).
    let manifest = json!({
        "file": SHARED_RECORDING,
        "autoplay": true,
        "loop": true,
        "name": entry.title,
        "scene": entry.scene,
        "created": entry.created,
        "duration_s": entry.duration_s,
        "size": len,
    });
    Ok(plan_archive(assets, recording, (len, crc), SHARED_RECORDING, &manifest))
}

/// Lays out the share archive of one refined result: the web build, the
/// result as `scene.ply` and the `share.json` that names it (the viewer
/// shows a `.ply` as an imported scene, drawn sorted).
fn plan_refined_share(assets: Vec<(String, Vec<u8>)>, result: PathBuf, (len, crc): (u64, u32), entry: &store::RefinedEntry) -> ZipPlan {
    let mut manifest = json!({
        "file": SHARED_SCENE,
        "name": entry.meta.title,
        "scene": "refined",
        "created": entry.meta.created,
        "size": len,
    });
    // A room is walked through: the viewer starts inside it, where the
    // phone started when the result says where that was.
    if entry.meta.mode == "room" {
        manifest["nav"] = json!("fly");
        if let Some(view) = entry.meta.view {
            manifest["view"] = json!(view);
        }
    }
    plan_archive(assets, result, (len, crc), SHARED_SCENE, &manifest)
}

/// The archive as a stream: the blocks held in memory, and the file read
/// from the disk as it is sent. `stall`: see [`files::guarded`].
fn plan_body(plan: ZipPlan, stall: Duration) -> Body {
    let parts = futures_util::stream::iter(plan.parts).map(|part| match part {
        Part::Bytes(bytes) => futures_util::stream::once(async move { Ok::<Bytes, io::Error>(bytes) }).boxed(),
        Part::File { path, len } => futures_util::stream::once(async move {
            let file = tokio::fs::File::open(&path).await?;
            if file.metadata().await?.len() < len {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the recording became shorter"));
            }
            Ok(files::file_stream(file, len))
        })
        .try_flatten()
        .boxed(),
    });
    files::guarded(parts.flatten(), stall)
}

pub(crate) async fn share_zip(State(state): State<AppState>, UrlPath(file): UrlPath<String>, method: Method) -> Response {
    let Some(stem) = file.strip_suffix(".zip") else {
        return json_error(StatusCode::NOT_FOUND, "not_found", "share archives are named <recording>.zip");
    };
    let recording = format!("{stem}{}", store::RECORDING_EXT);
    let core = &state.core;
    let Some(path) = core.store.existing(Kind::Recording, &recording) else {
        return json_error(StatusCode::NOT_FOUND, "not_found", "no such recording");
    };
    // A recording that is still growing has no checksum yet.
    if core.store.is_busy(Kind::Recording, &recording) {
        return json_error(StatusCode::CONFLICT, "in_use", "the recording is still being written");
    }
    if !core.assets.has_index() {
        return json_error(StatusCode::SERVICE_UNAVAILABLE, "no_frontend", "there is no web build to share");
    }
    let mut response_headers = vec![
        (header::CONTENT_TYPE, HeaderValue::from_static("application/zip")),
        (header::CACHE_CONTROL, HeaderValue::from_static("no-store")),
        (header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff")),
    ];
    if let Some(value) = files::attachment_header(&file) {
        response_headers.push((header::CONTENT_DISPOSITION, value));
    }
    let with_headers = |mut response: Response| {
        for (name, value) in response_headers {
            response.headers_mut().insert(name, value);
        }
        response
    };
    let head = method == Method::HEAD;
    // The layout reads the recording once from end to end; a few at a time
    // is plenty, and a crowd of them would only fight for the disk.
    let Ok(_permit) = core.zips.clone().try_acquire_owned() else {
        return json_error(StatusCode::SERVICE_UNAVAILABLE, "busy", "too many archives are being prepared; try again");
    };
    let shared = core.clone();
    let plan = tokio::task::spawn_blocking(move || {
        let missing = || io::Error::new(io::ErrorKind::NotFound, "gone");
        let entry = shared.store.recording(&recording).ok_or_else(missing)?;
        // The length of the archive does not depend on the checksum: a
        // HEAD is spared the read of the whole recording.
        let sum = if head { (std::fs::metadata(&path)?.len(), 0) } else { recording_sum(&shared, &recording, &path)? };
        plan_share(shared.assets.collect(MAX_SHARED_ASSETS)?, path, sum, &entry)
    })
    .await;
    match plan {
        Ok(Ok(plan)) => {
            let mut response = Response::new(Body::empty());
            response.headers_mut().insert(header::CONTENT_LENGTH, HeaderValue::from(plan.len));
            if !head {
                *response.body_mut() = plan_body(plan, core.settings.send_timeout);
            }
            with_headers(response)
        }
        Ok(Err(err)) if err.kind() == io::ErrorKind::NotFound => {
            json_error(StatusCode::NOT_FOUND, "not_found", "no such recording")
        }
        Ok(Err(err)) => internal("préparation de l'archive de partage impossible", err),
        Err(_) => json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", "internal error"),
    }
}

/// Writes the share archive of the recording `name` to `destination`
/// (blocking). The same bytes as `GET /api/share/<name>.zip`.
pub(crate) fn export_share(core: &crate::Core, name: &str, destination: &Path) -> io::Result<u64> {
    let recording = format!("{name}{}", store::RECORDING_EXT);
    let missing = || io::Error::new(io::ErrorKind::NotFound, "no such recording");
    let path = core.store.existing(Kind::Recording, &recording).ok_or_else(missing)?;
    if core.store.is_busy(Kind::Recording, &recording) {
        return Err(io::Error::new(io::ErrorKind::ResourceBusy, "the recording is still being written"));
    }
    let entry = core.store.recording(&recording).ok_or_else(missing)?;
    let sum = recording_sum(core, &recording, &path)?;
    let plan = plan_share(core.assets.collect(MAX_SHARED_ASSETS)?, path, sum, &entry)?;
    write_plan(&plan, destination)
}

// -- captures -----------------------------------------------------------------------

pub(crate) async fn captures(State(state): State<AppState>) -> Response {
    // Raw captures are pictures of somebody's home: they stay on the
    // machine.
    if let Some(refusal) = loopback_only(&state) {
        return refusal;
    }
    let store = state.core.store.clone();
    let Ok(list) = tokio::task::spawn_blocking(move || store.list_captures()).await else {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", "internal error");
    };
    ok_json(json!({
        "captures": list,
        "dir": state.core.store.dir(Kind::Capture).map(|dir| dir.display().to_string()),
        "free_bytes": state.core.store.free_bytes(Kind::Capture),
    }))
}

pub(crate) async fn capture_file(
    State(state): State<AppState>,
    UrlPath(file): UrlPath<String>,
    Query(query): Query<FileQuery>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    if let Some(refusal) = loopback_only(&state) {
        return refusal;
    }
    stored_file(&state, Kind::Capture, &file, &method, &headers, &query).await
}

pub(crate) async fn delete_capture(
    State(state): State<AppState>,
    UrlPath(file): UrlPath<String>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    delete_stored(&state, Kind::Capture, file, &headers, &uri).await
}

/// Reads and discards what is left of a request body, within bounds, so
/// that the refusal that follows reaches a client that is still sending.
async fn drain(body: Body) {
    let mut stream = body.into_data_stream();
    let mut seen = 0u64;
    let _ = tokio::time::timeout(DRAIN_TIME, async {
        while let Some(Ok(chunk)) = stream.next().await {
            seen += chunk.len() as u64;
            if seen > DRAIN_BYTES {
                break;
            }
        }
    })
    .await;
}

/// Refuses a request that still has a body on its way.
async fn refuse_upload(request: Request, refusal: Response) -> Response {
    // HTTP/2 can refuse a stream cleanly while its body is in flight.
    if request.version() < Version::HTTP_2 {
        drain(request.into_body()).await;
    }
    refusal
}

fn bearer_token(headers: &HeaderMap) -> Option<&[u8]> {
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let (value, None) = (values.next()?, values.next()) else { return None };
    let value = value.as_bytes();
    let (scheme, token) = value.split_at_checked(7)?;
    scheme.eq_ignore_ascii_case(b"Bearer ").then_some(token)
}

fn import_refusal(error: &ImportError) -> Response {
    match error {
        ImportError::Format(error) => no_store(
            (
                StatusCode::BAD_REQUEST,
                Json(json!({ "ok": false, "error": error.to_string(), "code": error.code.as_str() })),
            )
                .into_response(),
        ),
        ImportError::Io(error) => internal("lecture de la capture impossible", error),
    }
}

/// 507: the disk has no room for what was asked.
fn disk_full(free: u64) -> Response {
    no_store(
        (
            StatusCode::INSUFFICIENT_STORAGE,
            Json(json!({ "error": "not enough free disk space", "code": "disk_full", "free_bytes": free })),
        )
            .into_response(),
    )
}

/// The refusal that follows a failed write: a full disk has its own code,
/// so that the interface can say what to do about it.
fn write_refusal(context: &str, err: io::Error) -> Response {
    if err.kind() == io::ErrorKind::StorageFull {
        log::error!("{context} : {err}");
        return disk_full(0);
    }
    internal(context, err)
}

fn capture_in_progress() -> Response {
    json_error(StatusCode::CONFLICT, "busy", "a phone is capturing: finish that capture first")
}

/// What [`ingest_file`] made of a capture file.
struct Ingested {
    /// The answer of the request, without the name of the kept raw file.
    answer: serde_json::Value,
    /// Stem shared by the files this capture leaves behind: the name of
    /// its recording when one was written.
    stem: String,
}

impl Ingested {
    /// The answer of the request; `file` is the name the raw capture is
    /// kept under, if it is.
    fn into_response(mut self, file: Option<&str>) -> Response {
        self.answer["file"] = json!(file);
        ok_json(self.answer)
    }
}

/// Ingests the `.gscap` at `path` on a blocking thread and shows the result
/// to the viewers. `Err` is the refusal to answer with.
async fn ingest_file(state: &AppState, path: PathBuf) -> Result<Ingested, Response> {
    let core = &state.core;
    let (store, params, max_bytes) = (core.store.clone(), core.settings.ingest, core.settings.max_recording_bytes);
    let imported = tokio::task::spawn_blocking(move || -> Result<(Imported, String), ImportError> {
        let name = store::peek_capture_header(&path).map(|header| header.name).unwrap_or_default();
        let stem = capture_stem(&name);
        let file = std::fs::File::open(&path)?;
        let imported = import_capture(io::BufReader::with_capacity(1 << 20, file), params, &store, &stem, max_bytes)?;
        Ok((imported, stem))
    })
    .await;
    let (imported, stem) = match imported {
        Ok(Ok(imported)) => imported,
        Ok(Err(error)) => return Err(import_refusal(&error)),
        Err(_) => return Err(json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", "the import failed")),
    };
    let summary = imported.summary.clone();
    let recording = imported.saved.as_ref().map(|saved| saved.name.clone());
    let (reply, answer) = oneshot::channel();
    if !core.hub.send(Command::Show { imported: Box::new(imported), reply }) {
        return Err(pipeline_gone());
    }
    let Ok(shown) = answer.await else { return Err(pipeline_gone()) };
    log::info!("capture imported: {} frames ({} used), shown: {shown}", summary.frames, summary.frames_used);
    let answer = json!({
        "ok": true,
        "frames": summary.frames,
        "frames_used": summary.frames_used,
        "truncated": summary.truncated,
        "name": summary.name,
        // The recording the capture was saved as, and whether viewers were
        // switched to it (not while a phone is capturing).
        "recording": recording,
        "shown": shown,
    });
    // The raw file takes the name its recording got (which may carry a
    // number when the plain name was taken), so that the two go together.
    Ok(Ingested { answer, stem: recording.unwrap_or(stem) })
}

/// `POST /api/captures`: a `.gscap` in the body, streamed to the disk and
/// then ingested.
pub(crate) async fn import(State(state): State<AppState>, request: Request) -> Response {
    let core = &state.core;
    match state.listener {
        Listener::Loopback => {
            if let Some(refusal) = origin_checked(&state, request.headers(), request.uri()) {
                return refuse_upload(request, refusal).await;
            }
        }
        // A native client sends no `Origin`; the pairing token is what
        // lets it in.
        Listener::Lan => {
            let authorized = bearer_token(request.headers())
                .is_some_and(|token| constant_time_eq(token, core.pairing_token().as_bytes()));
            if !authorized {
                let mut refusal = no_store(
                    (
                        StatusCode::UNAUTHORIZED,
                        Json(json!({ "ok": false, "error": "missing or wrong pairing token", "code": "bad_token" })),
                    )
                        .into_response(),
                );
                refusal.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
                return refuse_upload(request, refusal).await;
            }
        }
    }
    let limit = core.settings.max_upload_bytes;
    let too_large = || json_error(StatusCode::PAYLOAD_TOO_LARGE, "too_large", format!("a capture may be {limit} bytes at most"));
    let announced = request.headers().get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()?.parse::<u64>().ok());
    if announced.is_some_and(|length| length > limit) {
        // Not drained: there is too much of it.
        return too_large();
    }
    if core.hub.metrics.capturing.load(Ordering::Relaxed) {
        return refuse_upload(request, capture_in_progress()).await;
    }
    // A capture that will not fit is refused before the first byte is
    // written, not after the disk has been filled with half of it.
    let keeping = core.settings.keep_captures && core.store.dir(Kind::Capture).is_some();
    if let (true, Some(length), Some(free)) = (keeping, announced, core.store.free_bytes(Kind::Capture)) {
        if length.saturating_add(UPLOAD_MARGIN_BYTES) > free {
            return refuse_upload(request, disk_full(free)).await;
        }
    }
    // One import at a time: each keeps a core busy for as long as it lasts.
    let Ok(_permit) = core.imports.clone().try_acquire_owned() else {
        let refusal = json_error(StatusCode::CONFLICT, "import_in_progress", "another capture is being imported");
        return refuse_upload(request, refusal).await;
    };

    // To the disk first: the body of a phone's upload arrives at the pace
    // of its Wi-Fi, the reconstruction should not wait on it frame by frame.
    let keep = core.settings.keep_captures && core.store.dir(Kind::Capture).is_some();
    let spool = if keep {
        core.store.create_part(Kind::Capture)
    } else {
        store::temporary_part()
    };
    let (part, file) = match spool {
        Ok(spool) => spool,
        Err(err) => return refuse_upload(request, internal("impossible de recevoir la capture", err)).await,
    };
    let part = PartFile(part);
    let mut file = tokio::fs::File::from_std(file);
    let version = request.version();
    let mut body = request.into_body().into_data_stream();
    let mut received = 0u64;
    let refusal = loop {
        let chunk = match tokio::time::timeout(core.settings.request_timeout, body.next()).await {
            Ok(Some(Ok(chunk))) => chunk,
            Ok(Some(Err(_))) => return json_error(StatusCode::BAD_REQUEST, "bad_request", "the upload was interrupted"),
            Ok(None) => break None,
            // Answered at once: there is nothing to read from a client that
            // has stopped sending.
            Err(_) => return json_error(StatusCode::REQUEST_TIMEOUT, "timeout", "the upload stalled"),
        };
        // Whatever this is, it is not a capture: say so before taking
        // gigabytes of it.
        if received < FILE_MAGIC.len() as u64 {
            let at = received as usize;
            let seen = chunk.len().min(FILE_MAGIC.len() - at);
            if chunk[..seen] != FILE_MAGIC[at..at + seen] {
                break Some(no_store(
                    (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "ok": false, "error": "not a .gscap file", "code": "bad_file_magic" })),
                    )
                        .into_response(),
                ));
            }
        }
        received += chunk.len() as u64;
        if received > limit {
            break Some(too_large());
        }
        if let Err(err) = file.write_all(&chunk).await {
            break Some(write_refusal("écriture de la capture reçue impossible", err));
        }
    };
    if let Some(refusal) = refusal {
        if version < Version::HTTP_2 {
            let _ = tokio::time::timeout(DRAIN_TIME, async {
                let mut seen = 0u64;
                while let Some(Ok(chunk)) = body.next().await {
                    seen += chunk.len() as u64;
                    if seen > DRAIN_BYTES {
                        break;
                    }
                }
            })
            .await;
        }
        return refusal;
    }
    if let Err(err) = file.flush().await {
        return write_refusal("écriture de la capture reçue impossible", err);
    }
    drop(file);

    // Only a file that reads well is kept: it gets its name once it has
    // been ingested.
    let ingested = match ingest_file(&state, part.0.clone()).await {
        Ok(ingested) => ingested,
        Err(refusal) => return refusal,
    };
    if !keep {
        return ingested.into_response(None);
    }
    let store = core.store.clone();
    let (source, stem) = (part.0.clone(), ingested.stem.clone());
    match tokio::task::spawn_blocking(move || store.adopt(Kind::Capture, &source, &stem)).await {
        Ok(Ok(file)) => ingested.into_response(Some(&file)),
        Ok(Err(err)) => {
            log::error!("la capture importée n'a pas pu être conservée : {err}");
            ingested.into_response(None)
        }
        Err(_) => ingested.into_response(None),
    }
}

/// Deletes the received file when the request ends without having given
/// it a name.
struct PartFile(PathBuf);

impl Drop for PartFile {
    fn drop(&mut self) {
        // Gone already when the file was adopted.
        let _ = std::fs::remove_file(&self.0);
    }
}

#[derive(Deserialize)]
struct OpenRequest {
    /// File name in the captures directory.
    file: String,
}

/// `POST /api/captures/open`: ingests a capture kept in the captures
/// directory again (with the present reconstruction settings).
pub(crate) async fn open(State(state): State<AppState>, request: Request) -> Response {
    if let Some(refusal) =
        loopback_only(&state).or_else(|| origin_checked(&state, request.headers(), request.uri()))
    {
        return refusal;
    }
    let request: OpenRequest = match json_body(&state, request).await {
        Ok(request) => request,
        Err(refusal) => return refusal,
    };
    let core = &state.core;
    let Some(path) = core.store.existing(Kind::Capture, &request.file) else {
        return json_error(StatusCode::NOT_FOUND, "not_found", "no such capture");
    };
    if core.store.is_busy(Kind::Capture, &request.file) {
        return json_error(StatusCode::CONFLICT, "in_use", "the capture is still being written");
    }
    if core.hub.metrics.capturing.load(Ordering::Relaxed) {
        return capture_in_progress();
    }
    let Ok(_permit) = core.imports.clone().try_acquire_owned() else {
        return json_error(StatusCode::CONFLICT, "import_in_progress", "another capture is being imported");
    };
    match ingest_file(&state, path).await {
        Ok(ingested) => ingested.into_response(Some(&request.file)),
        Err(refusal) => refusal,
    }
}

// -- offline refinement ---------------------------------------------------------------

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
enum RefineRequest {
    /// Refine the kept capture `capture` (a file name of the captures
    /// directory).
    Start {
        capture: String,
        /// `rapide`, `standard` (default) or `fin`.
        #[serde(default)]
        preset: Option<String>,
        /// Paint from the photographs what the depth sensor did not
        /// measure (default), or leave such places as holes.
        #[serde(default)]
        beyond_sensor: Option<bool>,
    },
    /// Write the capture as a dataset: `colmap` or `nerfstudio`.
    Export { capture: String, format: String },
    /// Stop the job in progress.
    Cancel,
}

fn refine_refusal(refusal: Refusal) -> Response {
    match refusal {
        Refusal::Running(job) => no_store(
            (StatusCode::CONFLICT, Json(json!({ "error": "another job is running", "code": "job_running", "job": job }))).into_response(),
        ),
        Refusal::NoDirectory => {
            json_error(StatusCode::SERVICE_UNAVAILABLE, "no_refined_dir", "the server has no directory for refined results")
        }
        Refusal::NotFound => json_error(StatusCode::NOT_FOUND, "not_found", "no such capture"),
        Refusal::InUse => json_error(StatusCode::CONFLICT, "in_use", "the capture is still being written"),
        Refusal::Unsupported => {
            json_error(StatusCode::BAD_REQUEST, "unsupported_mode", "a live capture shows a moving scene and cannot be refined")
        }
        Refusal::DiskFull { free } => disk_full(free),
        Refusal::Disk(err) => {
            log::error!("la tâche n'a pas pu démarrer : {err}");
            json_error(StatusCode::INTERNAL_SERVER_ERROR, "disk_error", err.to_string())
        }
    }
}

/// `GET /api/refine`: the offline job in progress and the latest that ended.
pub(crate) async fn refine_status(State(state): State<AppState>) -> Response {
    // Names of captures: for the machine's own user.
    if let Some(refusal) = loopback_only(&state) {
        return refusal;
    }
    no_store(Json(state.core.refiner.state()).into_response())
}

/// `POST /api/refine`: starts a refinement or a dataset export of a kept
/// capture, or stops the job in progress.
pub(crate) async fn refine(State(state): State<AppState>, request: Request) -> Response {
    if let Some(refusal) =
        loopback_only(&state).or_else(|| origin_checked(&state, request.headers(), request.uri()))
    {
        return refusal;
    }
    let request: RefineRequest = match json_body(&state, request).await {
        Ok(request) => request,
        Err(refusal) => return refusal,
    };
    let (capture, task) = match request {
        RefineRequest::Cancel => {
            return match state.core.refiner.cancel() {
                Some(job) => ok_json(json!({ "job": job })),
                None => json_error(StatusCode::CONFLICT, "no_job", "no job is running"),
            };
        }
        RefineRequest::Start { capture, preset, beyond_sensor } => {
            let name = preset.as_deref().unwrap_or("standard");
            let Some(preset) = gs_refine::job::Preset::from_name(name) else {
                return json_error(StatusCode::BAD_REQUEST, "bad_request", "preset must be rapide, standard or fin");
            };
            (capture, Task::Refine { preset, beyond_sensor: beyond_sensor.unwrap_or(true) })
        }
        RefineRequest::Export { capture, format } => {
            let Some(format) = gs_refine::export::DatasetFormat::from_name(&format) else {
                return json_error(StatusCode::BAD_REQUEST, "bad_request", "format must be colmap or nerfstudio");
            };
            (capture, Task::Export(format))
        }
    };
    // The start reads the header of the capture and asks the disk how much
    // room it has: off the async threads.
    let refiner = state.core.refiner.clone();
    match tokio::task::spawn_blocking(move || refiner.start(&capture, task)).await {
        Ok(Ok(job)) => ok_json(json!({ "job": job })),
        Ok(Err(refusal)) => refine_refusal(refusal),
        Err(_) => json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", "internal error"),
    }
}

/// `GET /api/refined`: the refined results on disk.
pub(crate) async fn refined(State(state): State<AppState>) -> Response {
    let store = state.core.store.clone();
    let Ok(list) = tokio::task::spawn_blocking(move || store.list_refined()).await else {
        return json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", "internal error");
    };
    let mut answer = json!({ "results": list });
    if state.listener == Listener::Loopback {
        let store = &state.core.store;
        answer["dir"] = json!(store.dir(Kind::Refined).map(|dir| dir.display().to_string()));
        answer["datasets_dir"] = json!(store.datasets_dir().map(|dir| dir.display().to_string()));
    }
    ok_json(answer)
}

pub(crate) async fn refined_file(
    State(state): State<AppState>,
    UrlPath(file): UrlPath<String>,
    Query(query): Query<FileQuery>,
    method: Method,
    headers: HeaderMap,
) -> Response {
    stored_file(&state, Kind::Refined, &file, &method, &headers, &query).await
}

pub(crate) async fn delete_refined(
    State(state): State<AppState>,
    UrlPath(file): UrlPath<String>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    delete_stored(&state, Kind::Refined, file, &headers, &uri).await
}

/// The key of a refined result in the table of share checksums (a
/// recording may bear the same name).
fn refined_sum_key(file: &str) -> String {
    format!("refined/{file}")
}

/// `GET /api/share/refined/<name>.zip`: the web build with the refined
/// result inside, to unpack onto any static web host.
pub(crate) async fn refined_share_zip(State(state): State<AppState>, UrlPath(file): UrlPath<String>, method: Method) -> Response {
    let Some(stem) = file.strip_suffix(".zip") else {
        return json_error(StatusCode::NOT_FOUND, "not_found", "share archives are named <result>.zip");
    };
    let result = format!("{stem}{}", store::REFINED_EXT);
    let core = &state.core;
    let Some(path) = core.store.existing(Kind::Refined, &result) else {
        return json_error(StatusCode::NOT_FOUND, "not_found", "no such result");
    };
    if !core.assets.has_index() {
        return json_error(StatusCode::SERVICE_UNAVAILABLE, "no_frontend", "there is no web build to share");
    }
    let head = method == Method::HEAD;
    let Ok(_permit) = core.zips.clone().try_acquire_owned() else {
        return json_error(StatusCode::SERVICE_UNAVAILABLE, "busy", "too many archives are being prepared; try again");
    };
    let shared = core.clone();
    let plan = tokio::task::spawn_blocking(move || {
        let entry = shared.store.refined(&result).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "gone"))?;
        let sum = if head { (std::fs::metadata(&path)?.len(), 0) } else { recording_sum(&shared, &refined_sum_key(&result), &path)? };
        Ok::<_, io::Error>(plan_refined_share(shared.assets.collect(MAX_SHARED_ASSETS)?, path, sum, &entry))
    })
    .await;
    match plan {
        Ok(Ok(plan)) => {
            let mut response = Response::new(Body::empty());
            let headers = response.headers_mut();
            headers.insert(header::CONTENT_LENGTH, HeaderValue::from(plan.len));
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/zip"));
            headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
            headers.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
            if let Some(value) = files::attachment_header(&file) {
                headers.insert(header::CONTENT_DISPOSITION, value);
            }
            if !head {
                *response.body_mut() = plan_body(plan, core.settings.send_timeout);
            }
            response
        }
        Ok(Err(err)) if err.kind() == io::ErrorKind::NotFound => json_error(StatusCode::NOT_FOUND, "not_found", "no such result"),
        Ok(Err(err)) => internal("préparation de l'archive de partage impossible", err),
        Err(_) => json_error(StatusCode::INTERNAL_SERVER_ERROR, "internal", "internal error"),
    }
}

/// Writes a laid-out archive to `destination` (blocking).
fn write_plan(plan: &ZipPlan, destination: &Path) -> io::Result<u64> {
    let mut out = io::BufWriter::new(std::fs::File::create(destination)?);
    for part in &plan.parts {
        match part {
            Part::Bytes(bytes) => io::Write::write_all(&mut out, bytes)?,
            Part::File { path, len } => {
                let copied = io::copy(&mut std::fs::File::open(path)?.take(*len), &mut out)?;
                if copied != *len {
                    return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the shared file became shorter"));
                }
            }
        }
    }
    io::Write::flush(&mut out)?;
    out.into_inner().map_err(|e| e.into_error())?.sync_all()?;
    Ok(plan.len)
}

/// Writes the share archive of the refined result `name` to `destination`
/// (blocking). The same bytes as `GET /api/share/refined/<name>.zip`.
pub(crate) fn export_refined_share(core: &crate::Core, name: &str, destination: &Path) -> io::Result<u64> {
    let result = format!("{name}{}", store::REFINED_EXT);
    let missing = || io::Error::new(io::ErrorKind::NotFound, "no such result");
    let path = core.store.existing(Kind::Refined, &result).ok_or_else(missing)?;
    let entry = core.store.refined(&result).ok_or_else(missing)?;
    let sum = recording_sum(core, &refined_sum_key(&result), &path)?;
    write_plan(&plan_refined_share(core.assets.collect(MAX_SHARED_ASSETS)?, path, sum, &entry), destination)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_tokens() {
        let with = |values: &[&'static str]| {
            let mut headers = HeaderMap::new();
            for value in values {
                headers.append(header::AUTHORIZATION, HeaderValue::from_static(value));
            }
            headers
        };
        assert_eq!(bearer_token(&with(&["Bearer abc"])), Some(&b"abc"[..]));
        assert_eq!(bearer_token(&with(&["bearer abc"])), Some(&b"abc"[..]));
        assert_eq!(bearer_token(&with(&["Bearer "])), Some(&b""[..]));
        for bad in [&["Basic abc"][..], &["Bearer"], &["abc"], &[""], &["Bearer a", "Bearer a"], &[]] {
            assert_eq!(bearer_token(&with(bad)), None, "{bad:?}");
        }
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(!constant_time_eq(b"", b"a"));
    }
}
