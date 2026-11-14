//! The [`Viewer`]: GL state, frame loop and the public API.

mod input;
mod xr;

use std::cell::{Cell, OnceCell, RefCell};
use std::rc::{Rc, Weak};

use glam::{Mat4, Vec3};
use gs_core::codec::{self, Body, Kind};
use gs_core::raster::Image;
use gs_core::splat::ViewParams;
use gs_core::{Camera, Splat};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{
    Event, HtmlCanvasElement, Performance, WebGl2RenderingContext as GL, WebGlContextAttributes, WebGlPowerPreference, Window, XrFrame,
};

use crate::gizmo::{GizmoScene, LineBatch};
use crate::lines::LineRenderer;
use crate::nav::{perspective, NavMode, Navigator};
use crate::normalized::Output;
use crate::params::{
    ActiveBlend, Applied, BlendQuality, BlendStatus, LayerContent, RenderParams, SceneHints, ViewerOptions, ViewerStats, XrInfo, XrOptions,
    LAYER_COUNT,
};
use crate::splats::{SortKey, SortOutcome, SplatScene};

use input::{Listener, Pointers};
use xr::XrState;

/// Near plane of the desktop camera and of the VR session, view-space meters.
const NEAR: f32 = 0.05;
/// Far plane. Only the gizmo lines are clipped by it: sorted splats have no
/// depth, and the normalised mode encodes depth on its own, far beyond it.
const FAR: f32 = 1000.0;
/// Weight of a new sample in the smoothed frame statistics.
const STATS_SMOOTHING: f32 = 0.1;
/// Upper bound of the drawing-buffer size per axis, pixels.
const MAX_CANVAS_SIZE: f32 = 8192.0;

/// Handle to a running splat viewer. Cloning is cheap (reference counted);
/// the viewer stops and frees its resources when the last handle is dropped
/// or [`Viewer::destroy`] is called.
///
/// Not `Send`: it lives on the browser's main thread like the canvas it
/// draws to.
#[derive(Clone)]
pub struct Viewer {
    inner: Rc<Inner>,
}

/// The closures handed to the browser. They only hold weak references, so
/// the viewer is not kept alive by its own callbacks (see [`with_inner`]).
struct Callbacks {
    desktop_frame: Closure<dyn FnMut(f64)>,
    xr_frame: Closure<dyn FnMut(f64, XrFrame)>,
    xr_end: Closure<dyn FnMut(Event)>,
}

/// Callback of [`Viewer::on_xr_change`].
type XrListener = Rc<dyn Fn(bool)>;

struct Inner {
    window: Window,
    performance: Performance,
    canvas: HtmlCanvasElement,
    gl: GL,
    max_dpr: f32,
    fov_y: Cell<f32>,
    params: Cell<RenderParams>,
    hints: Cell<SceneHints>,
    xr_options: Cell<XrOptions>,
    scene: RefCell<SplatScene>,
    lines: RefCell<LineRenderer>,
    /// The user's gizmo scene as line segments (kept to survive a context loss).
    gizmo_lines: RefCell<LineBatch>,
    nav: RefCell<Navigator>,
    pointers: RefCell<Pointers>,
    /// `Some` while an immersive session runs.
    xr: RefCell<Option<XrState>>,
    /// `requestSession` was called and the session is not installed yet.
    xr_pending: Cell<bool>,
    /// `exit_xr` was called while the session was starting.
    xr_cancelled: Cell<bool>,
    xr_error: RefCell<Option<String>>,
    xr_listeners: RefCell<Vec<XrListener>>,
    /// Miniature mode requested through the API, applied by the next XR frame.
    xr_miniature_request: Cell<Option<bool>>,
    stats: Cell<ViewerStats>,
    /// Timestamp of the previous desktop frame, ms.
    last_frame_ms: Cell<Option<f64>>,
    /// Something changed that the desktop loop has to redraw.
    dirty: Cell<bool>,
    alive: Cell<bool>,
    context_lost: Cell<bool>,
    raf_id: Cell<Option<i32>>,
    callbacks: OnceCell<Callbacks>,
    listeners: RefCell<Vec<Listener>>,
}

impl Viewer {
    /// Creates the WebGL2 context on `canvas`, attaches the input listeners
    /// and starts the animation loop.
    ///
    /// The canvas should get its display size from CSS (for example
    /// `width: 100%; height: 100%`): the viewer sets the drawing-buffer size
    /// (`canvas.width/height`) from the CSS size and the device pixel ratio
    /// every frame. A canvas without a CSS size is pinned to the size it has
    /// (with a console warning). The viewer also sets `touch-action: none`
    /// on the canvas.
    ///
    /// Fails when WebGL2 is not available or a shader does not compile.
    pub fn new(canvas: HtmlCanvasElement, opts: ViewerOptions) -> Result<Viewer, JsValue> {
        let window = web_sys::window().ok_or("no window")?;
        let performance = window.performance().ok_or("no performance clock")?;

        let attrs = WebGlContextAttributes::new();
        // Created XR compatible so that entering VR never needs a context
        // loss/restore cycle (`makeXRCompatible` is then a no-op).
        attrs.set_xr_compatible(true);
        // Splat quads have soft edges and the gizmo lines are anti-aliased in
        // the shader: MSAA would only cost fill-rate. No depth: sorted splats
        // have none, and the normalised mode keeps its own in a texture,
        // which is also what hides the lines behind the splats.
        attrs.set_antialias(false);
        attrs.set_depth(false);
        attrs.set_stencil(false);
        attrs.set_alpha(false);
        attrs.set_power_preference(WebGlPowerPreference::HighPerformance);
        let gl: GL = canvas
            .get_context_with_context_options("webgl2", &attrs)?
            .ok_or("WebGL2 is not available")?
            .dyn_into()
            .map_err(|_| "WebGL2 is not available")?;
        // 8-bit blending must be reproducible.
        gl.disable(GL::DITHER);
        let _ = canvas.style().set_property("touch-action", "none");

        let inner = Rc::new(Inner {
            window,
            performance,
            scene: RefCell::new(SplatScene::new(&gl)?),
            lines: RefCell::new(LineRenderer::new(&gl)?),
            canvas,
            gl,
            max_dpr: if opts.max_dpr.is_finite() { opts.max_dpr.clamp(0.25, 8.0) } else { 2.0 },
            fov_y: Cell::new(sanitize_fov(opts.fov_y)),
            params: Cell::new(opts.params.sanitized()),
            hints: Cell::new(SceneHints::default()),
            xr_options: Cell::new(opts.xr),
            gizmo_lines: RefCell::new(LineBatch::default()),
            nav: RefCell::new({
                let mut nav = Navigator::new(opts.nav_mode, opts.eye, opts.target);
                nav.set_auto_rotate(opts.auto_rotate);
                nav
            }),
            pointers: RefCell::new(Pointers::default()),
            xr: RefCell::new(None),
            xr_pending: Cell::new(false),
            xr_cancelled: Cell::new(false),
            xr_error: RefCell::new(None),
            xr_listeners: RefCell::new(Vec::new()),
            xr_miniature_request: Cell::new(None),
            stats: Cell::new(ViewerStats::default()),
            last_frame_ms: Cell::new(None),
            dirty: Cell::new(true),
            alive: Cell::new(true),
            context_lost: Cell::new(false),
            raf_id: Cell::new(None),
            callbacks: OnceCell::new(),
            listeners: RefCell::new(Vec::new()),
        });

        let weak = Rc::downgrade(&inner);
        let callbacks = Callbacks {
            desktop_frame: {
                let weak = weak.clone();
                Closure::new(move |time_ms: f64| with_inner(&weak, |inner| inner.on_desktop_frame(time_ms)))
            },
            xr_frame: {
                let weak = weak.clone();
                Closure::new(move |time_ms: f64, frame: XrFrame| with_inner(&weak, |inner| inner.on_xr_frame(time_ms, frame)))
            },
            xr_end: Closure::new(move |_event: Event| with_inner(&weak, |inner| inner.on_xr_end())),
        };
        let _ = inner.callbacks.set(callbacks);
        input::attach(&inner)?;
        inner.schedule_desktop_frame();
        Ok(Viewer { inner })
    }

    /// The canvas the viewer draws to.
    pub fn canvas(&self) -> HtmlCanvasElement {
        self.inner.canvas.clone()
    }

    // ------------------------------------------------------------------ data

    /// Applies one binary wire message (`gs_core::codec`).
    ///
    /// `Layer` messages are decoded straight into the viewer's layers and
    /// uploaded to the GPU: a `Replace` costs O(message), an `Append`
    /// O(message) amortized, independent of what the other layer holds.
    /// The layer then counts as [`LayerContent::Surfels`], which is what the
    /// capture pipeline sends.
    /// `State`, `Stats` and `Preview` messages are decoded and returned
    /// untouched for the caller to interpret.
    ///
    /// On error nothing changes.
    pub fn apply_message(&self, bytes: &[u8]) -> Result<Applied, String> {
        let kind = codec::peek_kind(bytes).map_err(|e| e.to_string())?;
        if kind == Kind::Layer {
            let (header, total) = self.inner.scene.borrow_mut().apply_layer_message(bytes)?;
            self.inner.dirty.set(true);
            return Ok(Applied::Layer {
                layer: header.layer_id,
                mode: header.mode,
                count: header.count,
                layer_total: total as u32,
                seq: header.seq,
                time_ms: header.time_ms,
            });
        }
        match codec::decode(bytes).map_err(|e| e.to_string())?.body {
            Body::State(json) => Ok(Applied::State(json)),
            Body::Stats(json) => Ok(Applied::Stats(json)),
            Body::Preview(preview) => Ok(Applied::Preview(preview)),
            Body::Layer(_) => Err("layer message changed kind while decoding".to_string()),
        }
    }

    /// Replaces the content of a layer ([`crate::LAYER_STATIC`] or
    /// [`crate::LAYER_DYNAMIC`]) with an imported file: the layer counts as
    /// [`LayerContent::Imported`], which [`crate::BlendMode::Auto`] draws
    /// sorted. For surfels use [`Viewer::set_surfel_layer`], or correct the
    /// label afterwards with [`Viewer::set_layer_content`].
    pub fn set_layer(&self, layer: u8, splats: &[Splat]) -> Result<(), String> {
        let layer = layer_index(layer)?;
        self.inner.dirty.set(true);
        self.inner.scene.borrow_mut().replace(layer, splats, LayerContent::Imported)
    }

    /// Replaces the content of a layer with sensor surfels, as a `Replace`
    /// wire message would: the layer counts as [`LayerContent::Surfels`].
    pub fn set_surfel_layer(&self, layer: u8, splats: &[Splat]) -> Result<(), String> {
        let layer = layer_index(layer)?;
        self.inner.dirty.set(true);
        self.inner.scene.borrow_mut().replace(layer, splats, LayerContent::Surfels)
    }

    /// Adds splats to a layer. The layer keeps the content label it has; an
    /// empty one becomes [`LayerContent::Imported`].
    pub fn append_layer(&self, layer: u8, splats: &[Splat]) -> Result<(), String> {
        let layer = layer_index(layer)?;
        self.inner.dirty.set(true);
        self.inner.scene.borrow_mut().append(layer, splats, LayerContent::Imported)
    }

    /// Declares what a layer holds (see [`LayerContent`]). The data is not
    /// touched; with [`crate::BlendMode::Auto`] the blending mode follows.
    pub fn set_layer_content(&self, layer: u8, content: LayerContent) -> Result<(), String> {
        let layer = layer_index(layer)?;
        self.inner.scene.borrow_mut().set_content(layer, content);
        self.inner.dirty.set(true);
        Ok(())
    }

    /// What a layer is taken to hold ([`LayerContent::Surfels`] for an
    /// unknown layer id).
    pub fn layer_content(&self, layer: u8) -> LayerContent {
        layer_index(layer).map(|l| self.inner.scene.borrow().content(l)).unwrap_or_default()
    }

    /// Empties both layers.
    pub fn clear_layers(&self) {
        self.inner.scene.borrow_mut().clear();
        self.inner.dirty.set(true);
    }

    /// Copy of every splat the viewer holds (layer 0, then layer 1), for export.
    pub fn splats(&self) -> Vec<Splat> {
        self.inner.scene.borrow().splats()
    }

    /// Number of splats in a layer (0 for an unknown layer id).
    pub fn layer_len(&self, layer: u8) -> usize {
        layer_index(layer).map(|l| self.inner.scene.borrow().layer_len(l)).unwrap_or(0)
    }

    // --------------------------------------------------------------- display

    /// Sets the render options (sanitized: out-of-range values are clamped).
    pub fn set_params(&self, params: RenderParams) {
        self.inner.params.set(params.sanitized());
        self.inner.dirty.set(true);
    }

    pub fn params(&self) -> RenderParams {
        self.inner.params.get()
    }

    /// The blending mode the next frame is drawn with, and why: what
    /// [`RenderParams::blend`] and [`RenderParams::quality`] ask for, what
    /// the visible layers hold, and what this GPU can do. A
    /// [`crate::BlendReason`] with `is_fallback()` means the normalised mode
    /// was wanted and could not be had.
    pub fn blend_status(&self) -> BlendStatus {
        self.inner.scene.borrow().blend_status(&self.inner.params.get())
    }

    /// Replaces the gizmo scene (grid, camera frusta, scan path, axes).
    /// Whether it is drawn is `RenderParams::show_gizmos`.
    pub fn set_gizmos(&self, scene: &GizmoScene) {
        let mut batch = self.inner.gizmo_lines.borrow_mut();
        batch.clear();
        batch.scene(scene);
        // Without GL objects (context lost) the batch waits for the restore.
        if self.inner.has_gl_objects() {
            self.inner.lines.borrow_mut().set_scene(&batch);
        }
        self.inner.dirty.set(true);
    }

    /// What the viewer should know about the scene (see [`SceneHints`]).
    pub fn set_scene_hints(&self, hints: SceneHints) {
        self.inner.hints.set(hints);
        self.inner.dirty.set(true);
    }

    pub fn scene_hints(&self) -> SceneHints {
        self.inner.hints.get()
    }

    /// Renders the splats (no gizmos) as seen by `cam` into an off-screen
    /// 8-bit buffer of the camera's size and returns the pixels. Uses the
    /// current [`RenderParams`] and the blending mode of
    /// [`Viewer::blend_status`]; the canvas is not touched.
    ///
    /// Fails for cameras larger than 8192 pixels on a side, on a lost WebGL
    /// context and after [`Viewer::destroy`]; a camera without pixels gives
    /// an empty image.
    ///
    /// This is the same shader, blending and projection-matrix path as the
    /// on-screen views (the camera is turned into a view and a projection
    /// matrix first). Its CPU counterpart, with `near = 0.05`, is
    /// `gs_core::raster::rasterize` in the sorted mode and
    /// `gs_core::experimental::wblend::render` with the preset of the
    /// quality level in the normalised mode. Useful for thumbnails, exports
    /// and tests.
    pub fn snapshot(&self, cam: &Camera) -> Result<Image, JsValue> {
        self.inner.snapshot(cam)
    }

    // ------------------------------------------------------------ navigation

    pub fn set_nav_mode(&self, mode: NavMode) {
        self.inner.nav.borrow_mut().set_mode(mode);
        self.inner.pointers.borrow_mut().reset();
        self.inner.dirty.set(true);
    }

    pub fn nav_mode(&self) -> NavMode {
        self.inner.nav.borrow().mode()
    }

    /// Places the desktop camera at `eye` looking at `target`, without
    /// animation. In orbit mode `target` becomes the orbit center.
    pub fn set_view(&self, eye: Vec3, target: Vec3) {
        self.inner.nav.borrow_mut().set_view(eye, target);
        self.inner.dirty.set(true);
    }

    /// Current desktop camera position.
    pub fn eye(&self) -> Vec3 {
        self.inner.nav.borrow().eye()
    }

    /// Current look-at point (the orbit center in orbit mode).
    pub fn target(&self) -> Vec3 {
        self.inner.nav.borrow().target()
    }

    /// World-to-camera matrix of the desktop camera.
    pub fn view_matrix(&self) -> Mat4 {
        self.inner.nav.borrow().view_matrix()
    }

    /// Vertical field of view of the desktop camera, radians.
    pub fn set_fov_y(&self, fov_y: f32) {
        self.inner.fov_y.set(sanitize_fov(fov_y));
        self.inner.dirty.set(true);
    }

    pub fn fov_y(&self) -> f32 {
        self.inner.fov_y.get()
    }

    /// Orbit mode: rotate slowly around the target after a few seconds
    /// without input, at `speed` rad/s. `None` turns it off.
    pub fn set_auto_rotate(&self, speed: Option<f32>) {
        self.inner.nav.borrow_mut().set_auto_rotate(speed);
    }

    /// Fly speed in meters per second (also changed by the mouse wheel).
    pub fn set_fly_speed(&self, speed: f32) {
        self.inner.nav.borrow_mut().set_fly_speed(speed);
    }

    pub fn fly_speed(&self) -> f32 {
        self.inner.nav.borrow().fly_speed()
    }

    // ----------------------------------------------------------------- stats

    pub fn stats(&self) -> ViewerStats {
        let mut stats = self.inner.stats.get();
        let scene = self.inner.scene.borrow();
        stats.splats_total = scene.total() as u32;
        stats.sort_ms = scene.sort_ms as f32;
        stats.sorts = scene.sort_count;
        stats.xr_active = self.xr_active();
        stats.blend = scene.blend_status(&self.inner.params.get());
        (stats.offscreen_width, stats.offscreen_height) = scene.normalized_target_size().unwrap_or((0, 0));
        stats
    }

    // -------------------------------------------------------------------- XR

    /// Whether this browser can start an `immersive-vr` session. `false`
    /// without WebXR (including insecure origins, where `navigator.xr` does
    /// not exist).
    pub async fn xr_supported() -> bool {
        xr::supported().await
    }

    /// Requests an immersive VR session. **Call this synchronously from a
    /// user-gesture handler** (click): `requestSession` consumes the
    /// transient user activation. The session starts asynchronously; listen
    /// with [`Viewer::on_xr_change`]. Failures are reported by
    /// [`Viewer::xr_error`]. Does nothing while a session runs or is starting.
    pub fn enter_xr(&self) {
        xr::enter(&self.inner);
    }

    /// Ends the running session, if any. A session that is still starting
    /// (after [`Viewer::enter_xr`], before the first `on_xr_change(true)`) is
    /// cancelled: it is ended as soon as the browser hands it over, and no
    /// listener is called.
    pub fn exit_xr(&self) {
        self.inner.exit_xr();
    }

    pub fn xr_active(&self) -> bool {
        self.inner.xr.borrow().is_some()
    }

    /// Registers a listener called with `true` when a VR session has started
    /// and with `false` when it has ended (from either side: `exit_xr`, the
    /// headset's system menu, taking the headset off ...).
    pub fn on_xr_change(&self, f: impl Fn(bool) + 'static) {
        self.inner.xr_listeners.borrow_mut().push(Rc::new(f));
    }

    /// Why the last [`Viewer::enter_xr`] failed, if it did.
    pub fn xr_error(&self) -> Option<String> {
        self.inner.xr_error.borrow().clone()
    }

    /// State of the running session (rig, views, controllers).
    pub fn xr_info(&self) -> Option<XrInfo> {
        self.inner.xr.borrow().as_ref().map(XrState::info)
    }

    /// Options for VR. The framebuffer scale and foveation apply to the next
    /// session, the rest immediately.
    pub fn set_xr_options(&self, options: XrOptions) {
        self.inner.xr_options.set(options);
    }

    pub fn xr_options(&self) -> XrOptions {
        self.inner.xr_options.get()
    }

    /// Switches the running session between life-size and miniature, like
    /// the A button of the right controller. Ignored without a session.
    pub fn set_xr_miniature(&self, on: bool) {
        if self.xr_active() {
            self.inner.xr_miniature_request.set(Some(on));
        }
    }

    // -------------------------------------------------------------- lifetime

    /// Stops the animation loop, ends a running VR session, removes every
    /// event listener and frees the GPU resources. Dropping the last handle
    /// does the same.
    ///
    /// The handle and its clones stay safe to use, but nothing is drawn any
    /// more: layer updates only change the CPU copy (what [`Viewer::splats`]
    /// returns), [`Viewer::snapshot`] fails, [`Viewer::enter_xr`] does
    /// nothing, and no WebGL call is made.
    pub fn destroy(&self) {
        self.inner.shutdown();
    }
}

/// Runs `f` on the viewer behind `weak` if it still exists. Every browser
/// callback goes through here.
///
/// The callbacks only hold weak references, so user code running inside `f`
/// (an `on_xr_change` listener, say) may drop the last [`Viewer`] handle.
/// The viewer owns the very closure that is executing at that moment; it
/// must not be freed before that closure has returned, so in that case the
/// final drop is handed to a microtask.
fn with_inner(weak: &Weak<Inner>, f: impl FnOnce(&Inner)) {
    let Some(inner) = weak.upgrade() else { return };
    f(&inner);
    if Rc::strong_count(&inner) == 1 {
        wasm_bindgen_futures::spawn_local(async move { drop(inner) });
    }
}

fn layer_index(layer: u8) -> Result<usize, String> {
    if (layer as usize) < LAYER_COUNT {
        Ok(layer as usize)
    } else {
        Err(format!("unknown layer id {layer}"))
    }
}

fn sanitize_fov(fov_y: f32) -> f32 {
    if fov_y.is_finite() {
        fov_y.clamp(5f32.to_radians(), 150f32.to_radians())
    } else {
        50f32.to_radians()
    }
}

impl Inner {
    fn now(&self) -> f64 {
        self.performance.now()
    }

    fn callbacks(&self) -> &Callbacks {
        self.callbacks.get().expect("callbacks are set in Viewer::new")
    }

    fn schedule_desktop_frame(&self) {
        if !self.alive.get() {
            return;
        }
        let callback = self.callbacks().desktop_frame.as_ref().unchecked_ref();
        self.raf_id.set(self.window.request_animation_frame(callback).ok());
    }

    /// Height above which splats are hidden, for the sorter and the shader.
    fn clip_y(&self, params: &RenderParams, miniature: bool) -> f32 {
        let user = params.clip_y_max.unwrap_or(f32::INFINITY);
        let ceiling = self.hints.get().ceiling_height.filter(|_| miniature).map(|h| h - SceneHints::CEILING_CLIP_MARGIN);
        user.min(ceiling.unwrap_or(f32::INFINITY))
    }

    /// Folds one animation frame into the statistics.
    fn record_frame(&self, interval_ms: Option<f64>, cpu_ms: f64, rendered: Option<(u32, u32)>) {
        let mut stats = self.stats.get();
        stats.frames += 1;
        if let Some(interval) = interval_ms.filter(|i| *i > 0.0) {
            let fps = (1000.0 / interval) as f32;
            stats.fps = if stats.fps > 0.0 { stats.fps + (fps - stats.fps) * STATS_SMOOTHING } else { fps };
        }
        stats.frame_ms += (cpu_ms as f32 - stats.frame_ms) * STATS_SMOOTHING;
        if let Some((width, height)) = rendered {
            stats.frames_rendered += 1;
            stats.canvas_width = width;
            stats.canvas_height = height;
            stats.splats_drawn = self.scene.borrow().drawn();
        }
        self.stats.set(stats);
    }

    /// Matches the drawing buffer to the CSS size. Returns the buffer size,
    /// whether it changed, and the device pixels per CSS pixel.
    fn resize_canvas(&self) -> (u32, u32, bool, f32) {
        let dpr = self.window.device_pixel_ratio() as f32;
        let dpr = if dpr.is_finite() && dpr > 0.0 { dpr.min(self.max_dpr) } else { 1.0 };
        let scale = dpr * self.params.get().render_scale;
        let size = |css: i32| (css as f32 * scale).round().clamp(1.0, MAX_CANVAS_SIZE) as u32;
        let (css_width, css_height) = (self.canvas.client_width(), self.canvas.client_height());
        let (width, height) = (size(css_width), size(css_height));
        let changed = self.canvas.width() != width || self.canvas.height() != height;
        if changed {
            self.canvas.set_width(width);
            self.canvas.set_height(height);
            // A canvas without a CSS size is as large as its drawing buffer:
            // resizing the buffer by any factor but 1 would then resize the
            // element, which would resize the buffer ... until it hits the
            // size limit. Pin the element to the size it had.
            if css_width > 0 && css_height > 0 && (self.canvas.client_width() != css_width || self.canvas.client_height() != css_height) {
                let style = self.canvas.style();
                let _ = style.set_property("width", &format!("{css_width}px"));
                let _ = style.set_property("height", &format!("{css_height}px"));
                web_sys::console::warn_1(
                    &"gs-render: the canvas has no CSS size; it was pinned to its current size (give it a size in CSS to make it responsive)".into(),
                );
            }
        }
        // The browser may allocate less than requested (memory limits).
        let actual = |v: i32, wanted: u32| if v > 0 { v as u32 } else { wanted };
        (actual(self.gl.drawing_buffer_width(), width), actual(self.gl.drawing_buffer_height(), height), changed, scale)
    }

    fn on_desktop_frame(&self, time_ms: f64) {
        self.raf_id.set(None);
        if !self.alive.get() {
            return;
        }
        // Always re-arm: on a headset this loop simply does not fire while an
        // immersive session runs; with emulators it does, so it must idle.
        self.schedule_desktop_frame();
        let start = self.now();
        let interval = self.last_frame_ms.replace(Some(time_ms)).map(|last| time_ms - last);
        if self.context_lost.get() || self.xr.borrow().is_some() {
            return;
        }
        let dt = interval.map_or(0.0, |ms| (ms / 1000.0) as f32);
        let moved = self.nav.borrow_mut().update(dt);
        // The runtime owns the canvas size during a VR session; here it is ours.
        let (width, height, resized, px_scale) = self.resize_canvas();
        if resized {
            // Targets of another size: worth another try.
            self.retry_normalized();
        }
        let hidden = self.canvas.client_width() == 0 || self.canvas.client_height() == 0;
        if hidden || !(moved || resized || self.dirty.get()) {
            self.record_frame(interval, self.now() - start, None);
            return;
        }
        self.dirty.set(false);

        let params = self.params.get();
        let view = self.nav.borrow().view_matrix();
        let proj = perspective(self.fov_y.get(), width as f32 / height as f32, NEAR, FAR);
        let clip_y = self.clip_y(&params, false);
        let gl = &self.gl;
        gl.bind_framebuffer(GL::FRAMEBUFFER, None);
        gl.viewport(0, 0, width as i32, height as i32);
        let [r, g, b] = params.background;
        gl.clear_color(r, g, b, 1.0);
        gl.clear(GL::COLOR_BUFFER_BIT);
        let viewport = [0, 0, width as i32, height as i32];
        let mut scene = self.scene.borrow_mut();
        let vp = ViewParams::from_projection(view, &proj, width as f32, height as f32);
        let status = scene.blend_status(&params);
        if self.prepare_order(&mut scene, status, &view, &params, clip_y, false) == SortOutcome::Deferred {
            // The order on the GPU is one frame old: draw again next frame.
            self.dirty.set(true);
        }
        self.draw_splats(&mut scene, status, &vp, &params, clip_y, Output { framebuffer: None, viewport });
        if params.show_gizmos {
            self.lines.borrow().draw(&(proj * view), viewport, NEAR, px_scale, true, scene.occlusion());
        }
        drop(scene);
        self.record_frame(interval, self.now() - start, Some((width, height)));
    }

    /// Lets the next frame try the normalised mode again after its render
    /// targets could not be allocated: the views are about to change size.
    /// Never panics, whatever event handler it is called from.
    fn retry_normalized(&self) {
        if let Ok(mut scene) = self.scene.try_borrow_mut() {
            scene.retry_normalized();
        }
    }

    /// Makes the draw order valid for `view` when the frame is drawn sorted;
    /// the normalised mode has no order. Once per frame, before its views.
    fn prepare_order(
        &self,
        scene: &mut SplatScene,
        status: BlendStatus,
        view: &Mat4,
        params: &RenderParams,
        clip_y: f32,
        force: bool,
    ) -> SortOutcome {
        if status.active.is_normalized() {
            return SortOutcome::Fresh;
        }
        let key = SortKey { show: [params.show_static, params.show_dynamic], clip_y };
        scene.sort(view, key, &|| self.now(), force)
    }

    /// Draws the splats of one view into `output` (already cleared) with
    /// the blending `status` names. `output` is left bound with its viewport
    /// set, ready for the gizmo lines.
    ///
    /// When the normalised mode fails here (its render targets could not be
    /// allocated) it is suspended and the view is drawn sorted instead;
    /// `blend_status` reports the fallback until the size of the views
    /// changes (a resize, a VR session starting or ending) or the context is
    /// restored, when the mode is tried again.
    fn draw_splats(&self, scene: &mut SplatScene, status: BlendStatus, vp: &ViewParams, params: &RenderParams, clip_y: f32, output: Output) {
        let gl = &self.gl;
        let (framebuffer, [x, y, width, height]) = (output.framebuffer, output.viewport);
        let quality = match status.active {
            ActiveBlend::Sorted => None,
            ActiveBlend::NormalizedHard => Some(BlendQuality::Hard),
            ActiveBlend::NormalizedSoft => Some(BlendQuality::Soft),
        };
        if let Some(quality) = quality {
            match scene.draw_normalized(vp, params, clip_y, quality, output) {
                Ok(()) => return,
                Err(error) => {
                    // Announced when it happens, not on every retry.
                    if scene.suspend_normalized() {
                        web_sys::console::warn_1(
                            &format!("gs-render: normalised blending failed, falling back to sorted blending: {error}").into(),
                        );
                    }
                    let key = SortKey { show: [params.show_static, params.show_dynamic], clip_y };
                    scene.sort(&vp.view, key, &|| self.now(), true);
                }
            }
        }
        // Sorted frames need none of the off-screen targets.
        scene.release_normalized_targets();
        gl.bind_framebuffer(GL::FRAMEBUFFER, framebuffer);
        gl.viewport(x, y, width, height);
        scene.draw(vp, params, clip_y);
    }

    fn snapshot(&self, cam: &Camera) -> Result<Image, JsValue> {
        let (width, height) = (cam.width, cam.height);
        if width == 0 || height == 0 {
            return Ok(Image { width, height, rgb: Vec::new() });
        }
        if width as f32 > MAX_CANVAS_SIZE || height as f32 > MAX_CANVAS_SIZE {
            return Err(format!("snapshot of {width}x{height} pixels exceeds the limit of {MAX_CANVAS_SIZE} per axis").into());
        }
        if !self.alive.get() {
            return Err("the viewer was destroyed".into());
        }
        if self.context_lost.get() {
            return Err("the WebGL context is lost".into());
        }
        let gl = &self.gl;
        let params = self.params.get();
        let texture = gl.create_texture().ok_or("WebGL: createTexture failed")?;
        gl.bind_texture(GL::TEXTURE_2D, Some(&texture));
        gl.tex_storage_2d(GL::TEXTURE_2D, 1, GL::RGBA8, width as i32, height as i32);
        let framebuffer = gl.create_framebuffer().ok_or("WebGL: createFramebuffer failed")?;
        gl.bind_framebuffer(GL::FRAMEBUFFER, Some(&framebuffer));
        gl.framebuffer_texture_2d(GL::FRAMEBUFFER, GL::COLOR_ATTACHMENT0, GL::TEXTURE_2D, Some(&texture), 0);
        let complete = gl.check_framebuffer_status(GL::FRAMEBUFFER) == GL::FRAMEBUFFER_COMPLETE;

        let mut rgba = vec![0u8; width as usize * height as usize * 4];
        let mut result = Ok(());
        if complete {
            gl.viewport(0, 0, width as i32, height as i32);
            let [r, g, b] = params.background;
            gl.clear_color(r, g, b, 1.0);
            gl.clear(GL::COLOR_BUFFER_BIT);
            let clip_y = self.clip_y(&params, false);
            // Through the projection matrix, like every on-screen view.
            let vp = ViewParams::from_projection(cam.view_matrix(), &cam.projection_matrix(NEAR, FAR), width as f32, height as f32);
            let mut scene = self.scene.borrow_mut();
            let status = scene.blend_status(&params);
            let suspended = scene.normalized_suspended();
            self.prepare_order(&mut scene, status, &vp.view, &params, clip_y, true);
            let output = Output { framebuffer: Some(&framebuffer), viewport: [0, 0, width as i32, height as i32] };
            self.draw_splats(&mut scene, status, &vp, &params, clip_y, output);
            if (width, height) != (self.canvas.width(), self.canvas.height()) {
                // Targets of the snapshot's size are of no use to the next
                // frame, and a large snapshot's would stay allocated until
                // then. Nor does a snapshot too large for the normalised
                // mode say anything about the canvas.
                scene.release_normalized_targets();
                if !suspended {
                    scene.retry_normalized();
                }
            }
            // The order texture now belongs to this camera; the next sorted frame re-sorts.
            scene.invalidate_order();
            result = gl.read_pixels_with_opt_u8_array(0, 0, width as i32, height as i32, GL::RGBA, GL::UNSIGNED_BYTE, Some(&mut rgba));
        }
        gl.bind_framebuffer(GL::FRAMEBUFFER, None);
        gl.delete_framebuffer(Some(&framebuffer));
        gl.delete_texture(Some(&texture));
        self.dirty.set(true);
        if !complete {
            return Err("WebGL: snapshot framebuffer is incomplete".into());
        }
        result?;

        // readPixels returns the bottom row first; images are top row first.
        let stride = width as usize * 4;
        let mut rgb = Vec::with_capacity(width as usize * height as usize * 3);
        for row in rgba.chunks_exact(stride).rev() {
            for pixel in row.chunks_exact(4) {
                rgb.extend_from_slice(&pixel[..3]);
            }
        }
        Ok(Image { width, height, rgb })
    }

    /// The splat and line renderers have their GL objects: the viewer is
    /// alive and its context is not lost.
    fn has_gl_objects(&self) -> bool {
        self.alive.get() && !self.context_lost.get()
    }

    /// `webglcontextlost`: stop drawing until the context comes back.
    fn on_context_lost(&self) {
        self.context_lost.set(true);
        self.scene.borrow_mut().context_lost();
    }

    /// `webglcontextrestored`: every GL object is gone; rebuild them from the
    /// CPU copies.
    fn on_context_restored(&self) {
        let rebuilt = (|| -> Result<(), JsValue> {
            self.gl.disable(GL::DITHER);
            self.scene.borrow_mut().restore()?;
            let mut lines = LineRenderer::new(&self.gl)?;
            lines.set_scene(&self.gizmo_lines.borrow());
            *self.lines.borrow_mut() = lines;
            Ok(())
        })();
        match rebuilt {
            Ok(()) => {
                self.context_lost.set(false);
                self.dirty.set(true);
            }
            Err(e) => web_sys::console::error_2(&"gs-render: could not restore the WebGL context".into(), &e),
        }
    }

    /// Stops everything. Idempotent.
    fn shutdown(&self) {
        if !self.alive.replace(false) {
            return;
        }
        if let Some(id) = self.raf_id.take() {
            let _ = self.window.cancel_animation_frame(id);
        }
        self.listeners.borrow_mut().clear();
        self.shutdown_xr();
        self.scene.borrow_mut().destroy();
        if !self.context_lost.get() {
            self.lines.borrow().destroy();
        }
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.shutdown();
    }
}
