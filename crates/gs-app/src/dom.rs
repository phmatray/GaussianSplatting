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

/// Offers `bytes` as a file download named `name`.
pub fn download(name: &str, bytes: &[u8]) -> Result<(), String> {
    let document = window().document().ok_or("pas de document")?;
    let parts = js_sys::Array::of1(&js_sys::Uint8Array::from(bytes));
    let options = BlobPropertyBag::new();
    options.set_type("application/octet-stream");
    let blob = Blob::new_with_u8_array_sequence_and_options(&parts, &options).map_err(describe)?;
    let url = Url::create_object_url_with_blob(&blob).map_err(describe)?;
    let anchor: HtmlAnchorElement = document.create_element("a").map_err(describe)?.unchecked_into();
    anchor.set_href(&url);
    anchor.set_download(name);
    anchor.set_hidden(true);
    let body = document.body().ok_or("pas de document")?;
    body.append_child(&anchor).map_err(describe)?;
    anchor.click();
    anchor.remove();
    // The download has its own reference by now; free ours a little later.
    let revoke = Closure::once_into_js(move || {
        let _ = Url::revoke_object_url(&url);
    });
    let _ = window().set_timeout_with_callback_and_timeout_and_arguments_0(revoke.unchecked_ref(), 30_000);
    Ok(())
}

/// Reads a whole file into memory.
pub async fn read_file(file: &web_sys::File) -> Result<Vec<u8>, String> {
    let buffer = JsFuture::from(file.array_buffer()).await.map_err(describe)?;
    Ok(js_sys::Uint8Array::new(&buffer).to_vec())
}
