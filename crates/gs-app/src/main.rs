//! Splat Live: the user interface.
//!
//! A Leptos client-side application built with Trunk. It shows the splats
//! of a capture pipeline in a `gs_render::Viewer` and controls that
//! pipeline, wherever it runs: in a native `gs-server` reached over a
//! WebSocket (inside the Tauri shell or from a browser), or in the
//! `gs-worker` Web Worker when the page is served statically.
//!
//! * [`logic`]: everything that needs no browser (unit-tested natively).
//! * `core`: viewer, transport and message handling.
//! * `state`: the reactive signals the views read.
//! * `ui`: the views.
//! * `debug`: the `window.__splatLive` hook for scripted checks.
#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

mod logic;

#[cfg(target_arch = "wasm32")]
mod app;
#[cfg(target_arch = "wasm32")]
mod core;
#[cfg(target_arch = "wasm32")]
mod debug;
#[cfg(target_arch = "wasm32")]
mod dom;
#[cfg(target_arch = "wasm32")]
mod net;
#[cfg(target_arch = "wasm32")]
mod state;
#[cfg(target_arch = "wasm32")]
mod tauri;
#[cfg(target_arch = "wasm32")]
mod transport;
#[cfg(target_arch = "wasm32")]
mod ui;

#[cfg(target_arch = "wasm32")]
fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(app::App);
}

/// The interface only exists in the browser.
#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!("gs-app is a web application: run `trunk build --release` in crates/gs-app, then serve its dist directory.");
}
