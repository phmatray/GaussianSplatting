//! Small helpers over the DOM.

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{Blob, BlobPropertyBag, HtmlAnchorElement, HtmlElement, Url};

fn window() -> web_sys::Window {
    web_sys::window().expect("the application runs in a window")
}

/// Milliseconds of the page's monotonic clock.
pub fn now_ms() -> f64 {
    window().performance().map_or_else(js_sys::Date::now, |p| p.now())
}

/// Value of a query parameter of the page URL.
pub fn query_param(name: &str) -> Option<String> {
    let search = window().location().search().ok()?;
    web_sys::UrlSearchParams::new_with_str(&search).ok()?.get(name)
}

/// Width of the window in CSS pixels.
pub fn window_width() -> f64 {
    window().inner_width().ok().and_then(|w| w.as_f64()).unwrap_or(1280.0)
}

/// Resolves after `ms` milliseconds.
pub async fn sleep_ms(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _reject| {
        let _ = window().set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
    });
    let _ = JsFuture::from(promise).await;
}

/// The element with this id, if it exists and has the expected type.
pub fn element_by_id<T: JsCast>(id: &str) -> Option<T> {
    window().document()?.get_element_by_id(id)?.dyn_into::<T>().ok()
}

/// Takes the keyboard focus away from the focused control, so that the
/// keys go to the 3D view again.
pub fn blur_active_element() {
    let active = window().document().and_then(|d| d.active_element());
    if let Some(element) = active.and_then(|e| e.dyn_into::<HtmlElement>().ok()) {
        let _ = element.blur();
    }
}

fn describe(error: JsValue) -> String {
    error
        .dyn_ref::<js_sys::Error>()
        .map(|e| String::from(e.message()))
        .or_else(|| error.as_string())
        .unwrap_or_else(|| "erreur inconnue".to_string())
}

/// A binary `Blob` made of `parts` (`ArrayBuffer`s or typed arrays), in order.
pub fn blob_of(parts: &js_sys::Array) -> Result<Blob, String> {
    let options = BlobPropertyBag::new();
    options.set_type("application/octet-stream");
    Blob::new_with_buffer_source_sequence_and_options(parts, &options).map_err(describe)
}

/// Clicks a temporary link to `href`, which the browser saves as `name`.
fn click_download(name: &str, href: &str) -> Result<(), String> {
    let document = window().document().ok_or("pas de document")?;
    let anchor: HtmlAnchorElement = document.create_element("a").map_err(describe)?.unchecked_into();
    anchor.set_href(href);
    anchor.set_download(name);
    anchor.set_hidden(true);
    let body = document.body().ok_or("pas de document")?;
    body.append_child(&anchor).map_err(describe)?;
    anchor.click();
    anchor.remove();
    Ok(())
}

/// Offers `blob` as a file download named `name`.
pub fn download_blob(name: &str, blob: &Blob) -> Result<(), String> {
    let url = Url::create_object_url_with_blob(blob).map_err(describe)?;
    click_download(name, &url)?;
    // The download has its own reference by now; free ours a little later.
    let revoke = Closure::once_into_js(move || {
        let _ = Url::revoke_object_url(&url);
    });
    let _ = window().set_timeout_with_callback_and_timeout_and_arguments_0(revoke.unchecked_ref(), 30_000);
    Ok(())
}

/// Offers `bytes` as a file download named `name`.
pub fn download(name: &str, bytes: &[u8]) -> Result<(), String> {
    download_blob(name, &blob_of(&js_sys::Array::of1(&js_sys::Uint8Array::from(bytes)))?)
}

/// Lets the browser download `url` as a file named `name`.
pub fn download_url(name: &str, url: &str) -> Result<(), String> {
    click_download(name, url)
}

/// Every `(name, value)` of the page's query string, decoded.
pub fn query_pairs() -> Vec<(String, String)> {
    let Ok(search) = window().location().search() else { return Vec::new() };
    let Ok(params) = web_sys::UrlSearchParams::new_with_str(&search) else { return Vec::new() };
    let mut pairs = Vec::new();
    for entry in js_sys::Array::from(&params).iter() {
        let entry = js_sys::Array::from(&entry);
        if let (Some(name), Some(value)) = (entry.get(0).as_string(), entry.get(1).as_string()) {
            pairs.push((name, value));
        }
    }
    pairs
}

/// The page's own address without query and fragment
/// (`https://host/dir/`).
pub fn page_base() -> String {
    let location = window().location();
    format!("{}{}", location.origin().unwrap_or_default(), location.pathname().unwrap_or_default())
}

/// A reference of a link (`recordings/a.gsrec`, `https://host/a.gsrec`)
/// as an absolute URL, resolved against the page like any link. `None`
/// unless the result is `http:` or `https:`, or lies on the page's own
/// origin: whatever the reference looked like, nothing else is fetched.
pub fn resolve_url(reference: &str) -> Option<String> {
    let location = window().location();
    let base = location.href().ok()?;
    let url = Url::new_with_base(reference, &base).ok()?;
    let same_origin = location.origin().is_ok_and(|origin| origin == url.origin());
    (matches!(url.protocol().as_str(), "http:" | "https:") || same_origin).then(|| url.href())
}

/// Copies `text` to the clipboard. Resolves to `false` where the browser
/// refuses (no permission, an insecure page).
pub async fn copy_text(text: &str) -> bool {
    let clipboard = window().navigator().clipboard();
    if clipboard.is_undefined() {
        return false;
    }
    JsFuture::from(clipboard.write_text(text)).await.is_ok()
}

/// True while an element is shown full screen.
pub fn is_fullscreen() -> bool {
    window().document().and_then(|d| d.fullscreen_element()).is_some()
}

/// Shows the page full screen, or leaves full screen. Must be called from
/// a user gesture. `false` when the browser refuses outright (an
/// `<iframe>` without `allowfullscreen`).
pub fn toggle_fullscreen() -> bool {
    let Some(document) = window().document() else { return false };
    if document.fullscreen_element().is_some() {
        document.exit_fullscreen();
        return true;
    }
    document.fullscreen_enabled() && document.document_element().is_some_and(|root| root.request_fullscreen().is_ok())
}

/// Local time as `2026-11-14T16:05:09.123Z`-shaped text (the ISO format
/// with the local clock in it): for default names of recordings.
pub fn local_iso_time() -> String {
    let now = js_sys::Date::new_0();
    let shifted = js_sys::Date::new(&JsValue::from_f64(now.get_time() - now.get_timezone_offset() * 60_000.0));
    String::from(shifted.to_iso_string())
}

/// Reads a whole file into memory.
pub async fn read_file(file: &web_sys::File) -> Result<Vec<u8>, String> {
    let buffer = JsFuture::from(file.array_buffer()).await.map_err(describe)?;
    Ok(js_sys::Uint8Array::new(&buffer).to_vec())
}

/// A value of the page's local storage. `None` when there is none, or when
/// storage is not available (a sandboxed frame, private browsing).
pub fn stored(key: &str) -> Option<String> {
    window().local_storage().ok().flatten()?.get_item(key).ok().flatten()
}

/// Remembers `value` under `key`; silently does nothing where storage is
/// not available.
pub fn store(key: &str, value: &str) {
    if let Ok(Some(storage)) = window().local_storage() {
        let _ = storage.set_item(key, value);
    }
}
