//! Reading bytes in a worker: slices of a `Blob`, `fetch` with a `Range`
//! header, the body of a response as it downloads.

use js_sys::{Reflect, Uint8Array};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{AbortSignal, Blob, DedicatedWorkerGlobalScope, Headers, ReadableStreamDefaultReader, RequestInit, Response};

/// Bytes `[start, end)` of a `Blob` (fewer when it ends before `end`).
pub async fn read_blob(blob: &Blob, start: f64, end: f64) -> Result<Vec<u8>, JsValue> {
    let slice = blob.slice_with_f64_and_f64(start, end)?;
    let buffer = JsFuture::from(slice.array_buffer()).await?;
    Ok(Uint8Array::new(&buffer).to_vec())
}

/// `GET url`, with a `Range` header when `range` is given
/// (`bytes=0-4095`). Resolves when the headers are there.
pub async fn fetch(scope: &DedicatedWorkerGlobalScope, url: &str, range: Option<&str>, signal: Option<&AbortSignal>) -> Result<Response, JsValue> {
    let init = RequestInit::new();
    if let Some(range) = range {
        let headers = Headers::new()?;
        headers.set("Range", range)?;
        init.set_headers(&headers);
    }
    init.set_signal(signal);
    JsFuture::from(scope.fetch_with_str_and_init(url, &init)).await?.dyn_into()
}

/// The whole body of a response.
pub async fn body_bytes(response: &Response) -> Result<Vec<u8>, JsValue> {
    let buffer = JsFuture::from(response.array_buffer()?).await?;
    Ok(Uint8Array::new(&buffer).to_vec())
}

/// Total size announced by a `Content-Range: bytes 0-4095/123456` header,
/// if the header is there, readable (CORS may hide it) and names a size.
pub fn content_range_total(response: &Response) -> Option<u64> {
    let value = response.headers().get("Content-Range").ok().flatten()?;
    let total = value.rsplit_once('/')?.1.trim();
    total.parse().ok()
}

/// Size announced by `Content-Length`, if any.
pub fn content_length(response: &Response) -> Option<u64> {
    response.headers().get("Content-Length").ok().flatten()?.trim().parse().ok()
}

/// Reads a response body chunk by chunk.
pub struct BodyReader {
    reader: ReadableStreamDefaultReader,
}

impl BodyReader {
    /// `None` when the response has no body stream.
    pub fn new(response: &Response) -> Option<Self> {
        let reader = response.body()?.get_reader().dyn_into().ok()?;
        Some(Self { reader })
    }

    /// The next chunk; `None` at the end of the body.
    pub async fn next(&self) -> Result<Option<Vec<u8>>, JsValue> {
        let result = JsFuture::from(self.reader.read()).await?;
        if Reflect::get(&result, &"done".into())?.is_truthy() {
            return Ok(None);
        }
        let value = Reflect::get(&result, &"value".into())?;
        Ok(Some(value.dyn_into::<Uint8Array>()?.to_vec()))
    }
}
