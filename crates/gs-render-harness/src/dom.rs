//! Small DOM helpers of the harness page.

use gs_core::raster::Image;
use wasm_bindgen::prelude::*;
use wasm_bindgen::{Clamped, JsCast};
use wasm_bindgen_futures::JsFuture;
use web_sys::{CanvasRenderingContext2d, Document, HtmlCanvasElement, HtmlElement, ImageData, Window};

pub fn window() -> Window {
    web_sys::window().expect("the harness runs in a window")
}

pub fn document() -> Document {
    window().document().expect("the window has a document")
}

/// Milliseconds since the page was loaded.
pub fn now() -> f64 {
    window().performance().map_or(0.0, |p| p.now())
}

/// Element of `index.html` with the given id.
pub fn by_id<T: JsCast>(id: &str) -> T {
    document()
        .get_element_by_id(id)
        .unwrap_or_else(|| panic!("index.html has no #{id}"))
        .dyn_into()
        .unwrap_or_else(|_| panic!("#{id} has an unexpected element type"))
}

/// Value of a URL query parameter.
pub fn query(name: &str) -> Option<String> {
    let search = window().location().search().ok()?;
    web_sys::UrlSearchParams::new_with_str(&search).ok()?.get(name)
}

/// Query parameter parsed as a number, or `default`.
pub fn query_number<T: std::str::FromStr>(name: &str, default: T) -> T {
    query(name).and_then(|v| v.parse().ok()).unwrap_or(default)
}

pub fn set_status(text: &str) {
    by_id::<HtmlElement>("status").set_text_content(Some(text));
}

/// Publishes `value` as `window[name]` for the Playwright scripts.
pub fn set_global(name: &str, value: &JsValue) {
    let _ = js_sys::Reflect::set(&window(), &name.into(), value);
}

/// Sets `object[key] = value`.
pub fn set(object: &js_sys::Object, key: &str, value: impl Into<JsValue>) {
    let _ = js_sys::Reflect::set(object, &key.into(), &value.into());
}

/// JS array of numbers.
pub fn numbers(values: &[f32]) -> JsValue {
    values.iter().map(|v| JsValue::from_f64(*v as f64)).collect::<js_sys::Array>().into()
}

/// Resolves in the next animation frame, after the callbacks that were
/// already registered for it (the viewer's own frame callback included).
pub async fn next_frame() {
    let promise = js_sys::Promise::new(&mut |resolve, _reject| {
        let _ = window().request_animation_frame(&resolve);
    });
    let _ = JsFuture::from(promise).await;
}

/// Resolves after `ms` milliseconds; gives the browser a chance to paint.
pub async fn sleep(ms: i32) {
    let promise = js_sys::Promise::new(&mut |resolve, _reject| {
        let _ = window().set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
    });
    let _ = JsFuture::from(promise).await;
}

fn context_2d(canvas: &HtmlCanvasElement) -> CanvasRenderingContext2d {
    canvas.get_context("2d").ok().flatten().expect("2d context").dyn_into().expect("2d context type")
}

/// Appends a `<canvas>` showing `image` to `parent`.
pub fn append_image(parent: &HtmlElement, image: &Image, title: &str) {
    let canvas: HtmlCanvasElement = document().create_element("canvas").expect("createElement").dyn_into().expect("canvas");
    canvas.set_width(image.width);
    canvas.set_height(image.height);
    canvas.set_title(title);
    let mut rgba = Vec::with_capacity(image.rgb.len() / 3 * 4);
    for pixel in image.rgb.chunks_exact(3) {
        rgba.extend_from_slice(&[pixel[0], pixel[1], pixel[2], 255]);
    }
    if let Ok(data) = ImageData::new_with_u8_clamped_array_and_sh(Clamped(&rgba), image.width, image.height) {
        let _ = context_2d(&canvas).put_image_data(&data, 0, 0);
    }
    let _ = parent.append_child(&canvas);
}

/// Copies the current content of a WebGL canvas into an [`Image`].
///
/// Only meaningful in the animation frame in which the canvas was drawn:
/// without `preserveDrawingBuffer` the buffer is cleared after compositing.
pub fn read_canvas(source: &HtmlCanvasElement) -> Option<Image> {
    let (width, height) = (source.width(), source.height());
    let copy: HtmlCanvasElement = document().create_element("canvas").ok()?.dyn_into().ok()?;
    copy.set_width(width);
    copy.set_height(height);
    let ctx = context_2d(&copy);
    ctx.draw_image_with_html_canvas_element(source, 0.0, 0.0).ok()?;
    let rgba = ctx.get_image_data(0, 0, width as i32, height as i32).ok()?.data();
    let rgb = rgba.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect();
    Some(Image { width, height, rgb })
}
