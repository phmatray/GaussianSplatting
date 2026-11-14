//! WebGL2 Gaussian splat viewer for the browser: renderer, gizmos, desktop
//! navigation and the WebXR session, behind one small handle, [`Viewer`].
//!
//! No UI framework is involved: the crate needs a `<canvas>` and is driven
//! with plain data. Conventions are those of `docs/ARCHITECTURE.md` (meters,
//! right-handed, +Y up, camera looks along −Z, sRGB blending).
//!
//! # Quick start
//!
//! ```ignore
//! use gs_render::{GizmoScene, GridGizmo, NavMode, RenderParams, SceneHints, Viewer, ViewerOptions};
//!
//! // `canvas` gets its size from CSS; the viewer owns `canvas.width/height`.
//! let viewer = Viewer::new(canvas, ViewerOptions::default())?;
//!
//! // Binary frames from the WebSocket or the worker go straight in. Layer
//! // messages are uploaded to the GPU; everything else comes back decoded.
//! match viewer.apply_message(&bytes)? {
//!     gs_render::Applied::Layer { .. } => {}
//!     gs_render::Applied::State(json) => { /* serde_json::from_str::<SessionState>(&json) */ }
//!     gs_render::Applied::Stats(json) => {}
//!     gs_render::Applied::Preview(preview) => {}
//! }
//!
//! viewer.set_gizmos(&GizmoScene { grid: Some(GridGizmo::default()), ..Default::default() });
//! viewer.set_nav_mode(NavMode::Fly);
//! viewer.set_view(eye, target);
//! viewer.set_params(RenderParams { splat_scale: 0.8, ..viewer.params() });
//! viewer.set_scene_hints(SceneHints { ceiling_height: Some(2.5), ..Default::default() });
//!
//! // VR: `enter_xr` must be called synchronously from the click handler.
//! if Viewer::xr_supported().await { /* show the button */ }
//! viewer.on_xr_change(|active| { /* update the button label */ });
//! button.on_click(move |_| viewer.enter_xr());
//! ```
//!
//! # What it does
//!
//! * **Splats** ([`Viewer::apply_message`], [`Viewer::set_layer`],
//!   [`Viewer::append_layer`]). Two layers as in the wire protocol:
//!   [`LAYER_STATIC`] (background plate or accumulated scan, `Replace` or
//!   `Append`, up to millions of splats) and [`LAYER_DYNAMIC`] (replaced every
//!   tick). Each layer has its own `RGBA32UI` data texture (three texels per
//!   splat: center, six covariance terms and the color as raw 32-bit words —
//!   core WebGL2, no extension, bit-exact floats). Replacing the dynamic
//!   layer costs O(its size), appending costs O(chunk) amortized (textures
//!   grow geometrically); the other layer is never touched.
//! * **Drawing**: one quad per splat in sorted order. The order lives in an
//!   `R32UI` texture and the quads are plain indexed triangles whose vertex
//!   shader derives quad and corner from `gl_VertexID` — the same work as an
//!   instanced draw on a GPU, but 25× faster on the software rasterizer of
//!   headless browsers (which runs instances one draw call at a time), and
//!   that is where every automated check of this project runs.
//! * **Shading** follows the numbered recipe of `gs_core::splat` step by
//!   step, so the image matches `gs_core::raster::rasterize` (the harness
//!   crate asserts ≥ 35 dB PSNR): EWA projection with the 0.3 px² low-pass,
//!   3σ quads from the eigen-decomposition, `α = o·exp(−4.5·|q|²)`,
//!   premultiplied back-to-front blending on sRGB values. Focal lengths come
//!   from the projection matrix and the viewport, so asymmetric WebXR
//!   frusta work unchanged.
//! * **Sorting**: `gs_core::sort::DepthSorter` on the main thread, at most
//!   once per frame and shared by both eyes. The order only depends on the
//!   view *direction*, so it is kept while the direction stays within a
//!   fraction of a degree and the data is unchanged; a sort slower than 4 ms
//!   is repeated at most every other frame. Depths far outside the bulk of
//!   the scene are compressed logarithmically before they are quantized, so
//!   that stray far-away splats (floaters of an imported file) do not cost
//!   the scene its depth resolution.
//! * **Gizmos** ([`Viewer::set_gizmos`]): floor grid, world axes, camera
//!   frusta, the scan path and the handheld camera, as anti-aliased lines
//!   drawn on top. In VR the two controllers are added automatically.
//! * **Desktop navigation** ([`NavMode`]): orbit (drag rotates, wheel or
//!   pinch zooms, right/middle/shift-drag pans, optional idle auto-rotation)
//!   and fly (WASD or arrows, Q/E down/up, drag looks, shift is faster, the
//!   wheel changes the speed), both damped, with touch support (one finger
//!   rotates, two fingers pinch and pan). The drawing buffer follows the CSS
//!   size × `min(devicePixelRatio, max_dpr)` × `RenderParams::render_scale`.
//!   The loop only redraws when something changed.
//! * **WebXR** `immersive-vr` ([`Viewer::enter_xr`]): `local-floor` reference
//!   space (falls back to `local` + 1.6 m), locomotion by moving the tracking
//!   space with a matrix ([`rig`]). Left stick: move relative to the head
//!   (trigger = faster). Right stick: snap turn (x), up/down (y). Right A:
//!   life-size ↔ miniature "dollhouse" (uniform world scale, the model on a
//!   virtual table in front of the user, ceiling clipped when
//!   [`SceneHints::ceiling_height`] is known). Left X: toggle gizmos. Right
//!   B: reset the position. The desktop loop idles while a session runs.
//!
//! # Lifetime
//!
//! [`Viewer`] is a cheap reference-counted handle. The browser callbacks only
//! hold weak references, so dropping the last handle (or calling
//! [`Viewer::destroy`]) stops the loop, ends a running VR session, removes
//! every listener and frees the GPU objects. A lost WebGL context is restored
//! from the CPU copies of the data.
//!
//! # Targets and build
//!
//! Everything that talks to the browser (`Viewer` and the GL code) only
//! exists on `wasm32`. The pure parts — [`nav`], [`rig`], [`gizmo`],
//! [`params`] and the GPU packing — compile and are unit-tested on any
//! target, so `cargo build` / `cargo test` at the workspace root keep working.
//!
//! WebXR bindings are still "unstable" in web-sys: the wasm build needs
//! `--cfg=web_sys_unstable_apis`, which the workspace sets in
//! `.cargo/config.toml` (an exported `RUSTFLAGS` variable would replace it).
//! That cfg also makes `MouseEvent::client_x()` and friends return `f64`.

// Without the browser half only the unit tests use the crate-private helpers.
#![cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]

mod depth;
pub mod gizmo;
pub mod nav;
mod normalized;
mod pack;
pub mod params;
pub mod rig;

#[cfg(target_arch = "wasm32")]
mod glutil;
#[cfg(target_arch = "wasm32")]
mod lines;
#[cfg(target_arch = "wasm32")]
mod shaders;
#[cfg(target_arch = "wasm32")]
mod splats;
#[cfg(target_arch = "wasm32")]
mod viewer;

pub use gizmo::{FrustumGizmo, GizmoScene, GridGizmo};
pub use nav::NavMode;
pub use params::{
    ActiveBlend, Applied, BlendMode, BlendQuality, BlendReason, BlendStatus, LayerContent, RenderParams, SceneHints, ViewerOptions,
    ViewerStats, XrInfo, XrOptions, XrViewInfo, LAYER_COUNT, LAYER_DYNAMIC, LAYER_STATIC, LIVE_LAYER_WEIGHT,
};
#[cfg(target_arch = "wasm32")]
pub use viewer::Viewer;
