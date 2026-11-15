//! Small `fetch` helpers for the server's HTTP API and for files a link
//! names.

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{AbortController, RequestInit, Response};

/// Status and body of an HTTP answer.
pub struct Answer {
    pub status: u16,
    pub body: String,
}

impl Answer {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// What a request sends.
pub enum Body<'a> {
    None,
    Json(&'a str),
    /// A file, sent as it is (`application/octet-stream`).
    File(&'a web_sys::Blob),
}

fn describe(error: JsValue) -> String {
    error
        .dyn_ref::<js_sys::Error>()
        .map(|e| String::from(e.message()))
        .or_else(|| error.as_string())
        .unwrap_or_else(|| "erreur réseau".to_string())
}

/// Sends the request and resolves with the response once its headers are
/// there. The request is given up after `timeout_ms` (headers and body).
async fn fetch(method: &str, url: &str, body: Body<'_>, timeout_ms: i32) -> Result<(Response, impl FnOnce()), String> {
    let window = web_sys::window().ok_or("pas de fenêtre")?;
    let init = RequestInit::new();
    init.set_method(method);
    let headers = web_sys::Headers::new().map_err(describe)?;
    match body {
        Body::None => {}
        Body::Json(json) => {
            init.set_body(&JsValue::from_str(json));
            headers.set("Content-Type", "application/json").map_err(describe)?;
        }
        Body::File(file) => {
            init.set_body(file);
            headers.set("Content-Type", "application/octet-stream").map_err(describe)?;
        }
    }
    init.set_headers(&headers);
    // A host that never answers must not keep the application waiting.
    let abort = AbortController::new().map_err(describe)?;
    init.set_signal(Some(&abort.signal()));
    let cancel = Closure::once_into_js(move || abort.abort());
    let timer = window.set_timeout_with_callback_and_timeout_and_arguments_0(cancel.unchecked_ref(), timeout_ms).map_err(describe)?;
    let done = {
        let window = window.clone();
        move || window.clear_timeout_with_handle(timer)
    };
    match JsFuture::from(window.fetch_with_str_and_init(url, &init)).await.and_then(|r| r.dyn_into::<Response>()) {
        Ok(response) => Ok((response, done)),
        Err(error) => {
            done();
            Err(describe(error))
        }
    }
}

/// `method url` with `body`, answer read as text.
pub async fn send(method: &str, url: &str, body: Body<'_>, timeout_ms: i32) -> Result<Answer, String> {
    let (response, done) = fetch(method, url, body, timeout_ms).await?;
    let text = async { JsFuture::from(response.text()?).await }.await;
    done();
    Ok(Answer { status: response.status(), body: text.map_err(describe)?.as_string().unwrap_or_default() })
}

/// `GET url`, or `POST url` with a JSON body, given up after `timeout_ms`.
pub async fn request(url: &str, json_body: Option<&str>, timeout_ms: i32) -> Result<Answer, String> {
    match json_body {
        Some(json) => send("POST", url, Body::Json(json), timeout_ms).await,
        None => send("GET", url, Body::None, timeout_ms).await,
    }
}

/// Why a download failed.
pub enum DownloadError {
    /// The server answered with this status.
    Status(u16),
    /// The file is larger than the limit (its announced size, if known).
    TooLarge,
    Network(String),
}

/// Reads the body of `response` chunk by chunk, giving up as soon as more
/// than `max_bytes` have arrived: a host that announces no length (or a
/// false one) cannot make the page hold more than the limit.
async fn bounded_body(response: &Response, max_bytes: u64) -> Result<Vec<u8>, DownloadError> {
    let network = |error: JsValue| DownloadError::Network(describe(error));
    let Some(stream) = response.body() else { return Ok(Vec::new()) };
    let reader: web_sys::ReadableStreamDefaultReader = stream.get_reader().dyn_into().map_err(|_| DownloadError::Network("corps illisible".to_string()))?;
    let mut bytes = Vec::new();
    loop {
        let result = JsFuture::from(reader.read()).await.map_err(network)?;
        if js_sys::Reflect::get(&result, &"done".into()).map_err(network)?.is_truthy() {
            return Ok(bytes);
        }
        let chunk: js_sys::Uint8Array = js_sys::Reflect::get(&result, &"value".into()).map_err(network)?.dyn_into().map_err(network)?;
        if bytes.len() as u64 + u64::from(chunk.length()) > max_bytes {
            // Stops the download; nothing is waiting for the answer.
            let _ = reader.cancel();
            return Err(DownloadError::TooLarge);
        }
        let start = bytes.len();
        bytes.resize(start + chunk.length() as usize, 0);
        chunk.copy_to(&mut bytes[start..]);
    }
}

/// `GET url` as bytes, refusing answers above `max_bytes`.
pub async fn download(url: &str, max_bytes: u64, timeout_ms: i32) -> Result<Vec<u8>, DownloadError> {
    let (response, done) = fetch("GET", url, Body::None, timeout_ms).await.map_err(DownloadError::Network)?;
    let result = async {
        if !(200..300).contains(&response.status()) {
            return Err(DownloadError::Status(response.status()));
        }
        let announced = response.headers().get("Content-Length").ok().flatten().and_then(|v| v.trim().parse::<u64>().ok());
        if announced.is_some_and(|length| length > max_bytes) {
            return Err(DownloadError::TooLarge);
        }
        bounded_body(&response, max_bytes).await
    }
    .await;
    done();
    result
}
