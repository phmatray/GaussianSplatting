//! Entry point of the `splat-worker` Web Worker: glue between the library
//! (local pipeline, relay of a native server's WebSocket, replay of a
//! recording, import of a capture file) and the browser. See the library
//! documentation for the protocol.

#[cfg(target_arch = "wasm32")]
mod glue;

#[cfg(target_arch = "wasm32")]
fn main() {
    glue::run();
}

/// The worker only exists in the browser.
#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!("splat-worker is a Web Worker: build it for wasm32-unknown-unknown (trunk build in crates/gs-app).");
}
