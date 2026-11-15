//! Test page for `gs-render`, built with Trunk (`trunk build --release` in
//! this directory) and driven by the Playwright scripts in `e2e/render`.
//!
//! The check to run is chosen with the URL query:
//!
//! * `?test=compare` — GPU renderer against the CPU reference rasterizer on
//!   deterministic clouds; PSNR per case in `window.__results`.
//! * `?test=adversarial` — the same comparison on hand-built surfel clouds
//!   and viewpoints that stress one decision of the normalised mode each
//!   (near plane, grazing angles, the depth tolerance, culling, far views,
//!   extreme layer weights, empty layers).
//! * `?test=live` (default) — animated cloud through `Viewer::apply_message`,
//!   with navigation, gizmos and an "Enter VR" button.
//! * `?test=perf&n=200000` — sort and frame times for `n` splats in
//!   `window.__perf`.
//! * `?test=scene&scene=stage|house` — the product's own cloud from
//!   `gs_core::Pipeline` in the real viewer: pictures, timings per blending
//!   mode, popping and fragment counts through `window.__scene`.
//! * `?test=drop&when=end` — the last viewer handle is dropped from inside
//!   one of the viewer's own callbacks.
//!
//! On targets other than wasm32 the binary is a stub, so that `cargo build`
//! at the workspace root keeps working; the cloud generators are still
//! compiled and unit-tested there.

#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
mod clouds;

#[cfg(target_arch = "wasm32")]
mod adversarial;
#[cfg(target_arch = "wasm32")]
mod compare;
#[cfg(target_arch = "wasm32")]
mod dom;
#[cfg(target_arch = "wasm32")]
mod drop_in_callback;
#[cfg(target_arch = "wasm32")]
mod live;
#[cfg(target_arch = "wasm32")]
mod perf;
#[cfg(target_arch = "wasm32")]
mod scene;
#[cfg(target_arch = "wasm32")]
mod scenes;

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!("gs-render-harness is a browser page: run `trunk build --release` in crates/gs-render-harness");
}

#[cfg(target_arch = "wasm32")]
fn main() {
    console_error_panic_hook::set_once();
    let test = dom::query("test").unwrap_or_else(|| "live".to_string());
    dom::set_global("__test", &test.as_str().into());
    let started = match test.as_str() {
        "compare" => compare::start(),
        "adversarial" => adversarial::start(),
        "perf" => perf::start(),
        "scene" => scene::start(),
        "drop" => drop_in_callback::start(),
        _ => live::start(),
    };
    if let Err(error) = started {
        web_sys::console::error_2(&"gs-render-harness failed to start:".into(), &error);
        dom::set_status("failed to start (see the console)");
        dom::set_global("__error", &error);
    }
}
