//! `?test=live`: a continuously animated cloud through the wire-message path.
//!
//! The stage scene is captured by a ring of simulated RGB-D cameras; the
//! empty stage goes once into the static layer (as `Append` chunks), and
//! every tick the foreground pixels are unprojected into surfels, encoded
//! with `gs_core::codec` and handed to `Viewer::apply_message` as a `Replace`
//! of the dynamic layer. Navigation, gizmos and WebXR are all live.
//!
//! Query parameters: `nav=fly`, `autorotate=1`, `ceiling=<meters>` (scene
//! hint for the miniature clip), `tick=<ms>`, `static_cams=<n>` and
//! `capture_scale=<factor>` (number and resolution of the simulated cameras;
//! the scripted checks use small values because a headless browser
//! rasterizes in software).
//!
//! For scripts: `window.__harness.state()` returns a snapshot of the viewer,
//! `window.__harness.cmd(name, arg)` changes it (see [`Live::command`]),
//! `window.__harness.apply(bytes)` feeds raw bytes to `Viewer::apply_message`
//! (hostile-input checks) and `window.__harness.xrReference(eye)` renders what
//! one eye of the running VR session shows with the CPU reference rasterizer.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use glam::{Affine3A, Mat4, Vec3};
use gs_core::capture::{render_rgbd, CaptureOptions};
use gs_core::codec::{self, LayerMode};
use gs_core::raster::{rasterize, RasterOptions};
use gs_core::rig::RingRig;
use gs_core::scene::stage::{CAPTURE_HEIGHT, STAGE_RADIUS};
use gs_core::scene::{Scene, StageScene};
use gs_core::{Aabb, Camera, RgbdFrame, Splat};
use gs_render::{
    Applied, FrustumGizmo, GizmoScene, GridGizmo, NavMode, RenderParams, SceneHints, Viewer, ViewerOptions, XrInfo, LAYER_DYNAMIC,
    LAYER_STATIC,
};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{HtmlButtonElement, HtmlCanvasElement};

use crate::clouds::{unproject_frame, UnprojectOptions};
use crate::dom;

/// Splats per `Append` message of the static layer.
const STATIC_CHUNK: usize = 4_000;
/// A pixel is foreground when it is this much closer than the empty stage, meters.
const FOREGROUND_MARGIN: f32 = 0.04;

struct Live {
    viewer: Viewer,
    scene: StageScene,
    /// Cameras of the dynamic capture with the depth of the empty stage.
    rig: Vec<(Camera, RgbdFrame)>,
    button: HtmlButtonElement,
    paused: Cell<bool>,
    ticks: Cell<u32>,
    /// Wire messages applied, and the last sequence number.
    seq: Cell<u32>,
    /// Duration of the last tick (capture + encode + apply) and of its
    /// `apply_message` call alone, ms.
    tick_ms: Cell<f64>,
    apply_ms: Cell<f64>,
    scratch: RefCell<(Vec<Splat>, Vec<u8>)>,
    xr_supported: Cell<Option<bool>>,
    xr_changes: RefCell<Vec<bool>>,
}

/// Resolution of the simulated cameras relative to the default.
fn capture_scale() -> f32 {
    dom::query_number("capture_scale", 1.0f32).clamp(0.1, 4.0)
}

fn frustum(cam: &Camera, color: [f32; 3], size: f32) -> FrustumGizmo {
    FrustumGizmo {
        world_from_cam: Mat4::from(cam.world_from_cam),
        fov_y: cam.fov_y(),
        aspect: cam.width as f32 / cam.height as f32,
        color,
        size,
    }
}

impl Live {
    /// Captures the empty stage and sends it as the static layer.
    fn upload_static(&self) -> Result<(), String> {
        let snapshot = self.scene.snapshot_static();
        let scale = capture_scale();
        let rig = RingRig {
            count: dom::query_number("static_cams", 8),
            width: (192.0 * scale) as u32,
            height: (144.0 * scale) as u32,
            ..RingRig::default()
        };
        let mut splats = Vec::new();
        for cam in rig.cameras() {
            let frame = render_rgbd(&snapshot, &cam, &CaptureOptions::default());
            unproject_frame(&frame, &cam, &UnprojectOptions::default(), |_, _, _| true, &mut splats);
        }
        let mut message = Vec::new();
        for (i, chunk) in splats.chunks(STATIC_CHUNK).enumerate() {
            let mode = if i == 0 { LayerMode::Replace } else { LayerMode::Append };
            codec::encode_layer(self.next_seq(), 0, LAYER_STATIC, mode, chunk, &mut message);
            self.viewer.apply_message(&message)?;
        }
        Ok(())
    }

    fn next_seq(&self) -> u32 {
        let seq = self.seq.get() + 1;
        self.seq.set(seq);
        seq
    }

    /// One capture tick: the dancer at the current time becomes the dynamic layer.
    fn tick(&self) {
        if self.paused.get() {
            return;
        }
        let start = dom::now();
        let time = (start / 1000.0) as f32;
        let snapshot = self.scene.snapshot(time);
        let mut scratch = self.scratch.borrow_mut();
        let (splats, message) = &mut *scratch;
        splats.clear();
        for (cam, plate) in &self.rig {
            let frame = render_rgbd(&snapshot, cam, &CaptureOptions::default());
            let foreground = |x: u32, y: u32, depth: f32| {
                let empty = plate.depth_at(x, y);
                empty == 0.0 || depth < empty - FOREGROUND_MARGIN
            };
            unproject_frame(&frame, cam, &UnprojectOptions::default(), foreground, splats);
        }
        codec::encode_layer(self.next_seq(), start as u32, LAYER_DYNAMIC, LayerMode::Replace, splats, message);
        let apply_start = dom::now();
        if let Err(error) = self.viewer.apply_message(message) {
            web_sys::console::error_1(&format!("[live] apply_message failed: {error}").into());
        }
        let end = dom::now();
        self.apply_ms.set(end - apply_start);
        self.tick_ms.set(end - start);
        self.ticks.set(self.ticks.get() + 1);
    }

    fn gizmos(&self) -> GizmoScene {
        // A made-up scan path around the stage with a camera on it.
        let path: Vec<Vec3> = (0..=48)
            .map(|i| {
                let a = i as f32 / 48.0 * std::f32::consts::TAU;
                Vec3::new(2.7 * a.cos(), 1.4 + 0.25 * (3.0 * a).sin(), 2.7 * a.sin())
            })
            .collect();
        let handheld = Camera::look_at(path[5], Vec3::new(0.0, 1.0, 0.0), Vec3::Y, 60f32.to_radians(), 4, 3);
        GizmoScene {
            grid: Some(GridGizmo { half_extent: 4.0, step: 0.5, ..GridGizmo::default() }),
            axes: Some(0.5),
            frusta: self.rig.iter().map(|(cam, _)| frustum(cam, [0.35, 0.7, 1.0], 0.3)).collect(),
            path,
            handheld: Some(frustum(&handheld, [1.0, 0.4, 0.3], 0.35)),
            ..GizmoScene::default()
        }
    }

    fn on_button(&self) {
        // Synchronous: `enter_xr` needs the user activation of this click.
        if self.viewer.xr_active() {
            self.viewer.exit_xr();
        } else {
            self.viewer.enter_xr();
        }
    }

    fn update_button(&self) {
        let (label, enabled) = match (self.viewer.xr_active(), self.xr_supported.get()) {
            (true, _) => ("Exit VR", true),
            (false, Some(true)) => ("Enter VR", true),
            (false, Some(false)) => ("VR not supported", false),
            (false, None) => ("Checking VR...", false),
        };
        self.button.set_text_content(Some(label));
        self.button.set_disabled(!enabled);
    }

    /// `window.__harness.cmd(name, arg)`.
    fn command(&self, name: &str, arg: &JsValue) {
        let viewer = &self.viewer;
        let flag = arg.as_bool().unwrap_or(true);
        let params = viewer.params();
        match name {
            "pause" => self.paused.set(flag),
            "nav" => viewer.set_nav_mode(if arg.as_string().as_deref() == Some("fly") { NavMode::Fly } else { NavMode::Orbit }),
            "view" => {
                let v: Vec<f32> = js_sys::Array::from(arg).iter().map(|x| x.as_f64().unwrap_or(f64::NAN) as f32).collect();
                if v.len() == 6 {
                    viewer.set_view(Vec3::new(v[0], v[1], v[2]), Vec3::new(v[3], v[4], v[5]));
                }
            }
            "gizmos" => viewer.set_params(RenderParams { show_gizmos: flag, ..params }),
            "points" => viewer.set_params(RenderParams { point_mode: flag, ..params }),
            "showStatic" => viewer.set_params(RenderParams { show_static: flag, ..params }),
            "showDynamic" => viewer.set_params(RenderParams { show_dynamic: flag, ..params }),
            "splatScale" => viewer.set_params(RenderParams { splat_scale: arg.as_f64().unwrap_or(1.0) as f32, ..params }),
            "renderScale" => viewer.set_params(RenderParams { render_scale: arg.as_f64().unwrap_or(1.0) as f32, ..params }),
            "clipY" => viewer.set_params(RenderParams { clip_y_max: arg.as_f64().map(|y| y as f32), ..params }),
            "autoRotate" => viewer.set_auto_rotate(arg.as_f64().map(|s| s as f32)),
            "clear" => viewer.clear_layers(),
            "emptyDynamic" => {
                if let Err(error) = viewer.set_layer(LAYER_DYNAMIC, &[]) {
                    web_sys::console::error_1(&format!("[live] set_layer failed: {error}").into());
                }
            }
            "fov" => viewer.set_fov_y(arg.as_f64().unwrap_or(f64::NAN) as f32),
            "maxRadius" => viewer.set_params(RenderParams { max_radius_px: arg.as_f64().unwrap_or(f64::NAN) as f32, ..params }),
            "miniature" => viewer.set_xr_miniature(flag),
            "enterXr" => viewer.enter_xr(),
            "exitXr" => viewer.exit_xr(),
            "destroy" => viewer.destroy(),
            _ => web_sys::console::warn_1(&format!("[live] unknown command {name}").into()),
        }
    }

    /// `window.__harness.apply(bytes)`: what `Viewer::apply_message` makes of
    /// arbitrary bytes, as a short string (`"error: ..."` when it refuses).
    fn apply_raw(&self, bytes: &[u8]) -> String {
        match self.viewer.apply_message(bytes) {
            Ok(Applied::Layer { layer, count, layer_total, .. }) => format!("layer {layer}: {count} splats, {layer_total} total"),
            Ok(Applied::State(json)) => format!("state: {json}"),
            Ok(Applied::Stats(json)) => format!("stats: {json}"),
            Ok(Applied::Preview(preview)) => format!("preview: camera {} {}x{}", preview.camera_id, preview.width, preview.height),
            Err(error) => format!("error: {error}"),
        }
    }

    /// `window.__harness.xrReference(eye)`: the picture eye `eye` of the
    /// running VR session should show (splats only), rendered by the CPU
    /// reference rasterizer from the view and projection matrices the viewer
    /// reports: `{ width, height, viewport, rgb }`, or `null` without a session.
    fn xr_reference(&self, eye: u32) -> JsValue {
        let Some(info) = self.viewer.xr_info() else { return JsValue::NULL };
        let Some(view) = info.views.get(eye as usize) else { return JsValue::NULL };
        let [_, _, width, height] = view.viewport.map(|v| v.max(0) as u32);
        let (w, h, p) = (width as f32, height as f32, view.projection);
        // The pinhole camera of an OpenGL projection matrix (possibly off-axis).
        let cam = Camera {
            width,
            height,
            fx: p.x_axis.x * w / 2.0,
            fy: p.y_axis.y * h / 2.0,
            cx: (1.0 - p.z_axis.x) * w / 2.0,
            cy: (1.0 + p.z_axis.y) * h / 2.0,
            world_from_cam: Affine3A::from_mat4(view.view.inverse()),
        };
        let params = self.viewer.params();
        let options = RasterOptions {
            // What an 8-bit framebuffer makes of the clear color.
            background: params.background.map(|c| (c * 255.0).round() as u8),
            splat_scale: params.splat_scale,
            point_mode: params.point_mode,
            point_sigma_px: params.point_sigma_px,
            // The near plane of the session, in view units.
            near: p.w_axis.z / (p.z_axis.z - 1.0),
        };
        let image = rasterize(&self.viewer.splats(), &cam, &options);
        let out = js_sys::Object::new();
        dom::set(&out, "width", width);
        dom::set(&out, "height", height);
        dom::set(&out, "viewport", dom::numbers(&view.viewport.map(|v| v as f32)));
        dom::set(&out, "rgb", js_sys::Uint8Array::from(image.rgb.as_slice()));
        out.into()
    }

    /// `window.__harness.state()`.
    fn state(&self) -> JsValue {
        let viewer = &self.viewer;
        let stats = viewer.stats();
        let params = viewer.params();
        let state = js_sys::Object::new();
        dom::set(&state, "ticks", self.ticks.get());
        dom::set(&state, "messages", self.seq.get());
        dom::set(&state, "tickMs", self.tick_ms.get());
        dom::set(&state, "applyMs", self.apply_ms.get());
        dom::set(&state, "paused", self.paused.get());
        dom::set(&state, "staticSplats", viewer.layer_len(LAYER_STATIC) as u32);
        dom::set(&state, "dynamicSplats", viewer.layer_len(LAYER_DYNAMIC) as u32);
        dom::set(&state, "fps", stats.fps);
        dom::set(&state, "frameMs", stats.frame_ms);
        dom::set(&state, "sortMs", stats.sort_ms);
        dom::set(&state, "sorts", stats.sorts as f64);
        dom::set(&state, "splatsTotal", stats.splats_total);
        dom::set(&state, "splatsDrawn", stats.splats_drawn);
        dom::set(&state, "frames", stats.frames as f64);
        dom::set(&state, "framesRendered", stats.frames_rendered as f64);
        dom::set(&state, "canvasWidth", stats.canvas_width);
        dom::set(&state, "canvasHeight", stats.canvas_height);
        dom::set(&state, "navMode", if viewer.nav_mode() == NavMode::Fly { "fly" } else { "orbit" });
        dom::set(&state, "flySpeed", viewer.fly_speed());
        dom::set(&state, "eye", dom::numbers(&viewer.eye().to_array()));
        dom::set(&state, "target", dom::numbers(&viewer.target().to_array()));
        dom::set(&state, "view", dom::numbers(&viewer.view_matrix().to_cols_array()));
        dom::set(&state, "showGizmos", params.show_gizmos);
        dom::set(&state, "xrActive", stats.xr_active);
        dom::set(&state, "xrSupported", self.xr_supported.get().map_or(JsValue::NULL, JsValue::from));
        dom::set(&state, "xrError", viewer.xr_error().map_or(JsValue::NULL, JsValue::from));
        dom::set(&state, "xrChanges", self.xr_changes.borrow().iter().map(|b| JsValue::from(*b)).collect::<js_sys::Array>());
        dom::set(&state, "xr", viewer.xr_info().map_or(JsValue::NULL, |info| xr_state(&info)));
        state.into()
    }
}

fn xr_state(info: &XrInfo) -> JsValue {
    let xr = js_sys::Object::new();
    dom::set(&xr, "referenceSpace", info.reference_space);
    dom::set(&xr, "frames", info.frames);
    let p = info.rig_position;
    dom::set(&xr, "rig", dom::numbers(&[p.x, p.y, p.z, info.rig_yaw]));
    dom::set(&xr, "scale", info.world_scale);
    dom::set(&xr, "miniature", info.miniature);
    dom::set(&xr, "head", dom::numbers(&info.world_from_head.to_cols_array()));
    let views = js_sys::Array::new();
    for view in &info.views {
        let v = js_sys::Object::new();
        dom::set(&v, "viewport", dom::numbers(&view.viewport.map(|x| x as f32)));
        dom::set(&v, "view", dom::numbers(&view.view.to_cols_array()));
        dom::set(&v, "projection", dom::numbers(&view.projection.to_cols_array()));
        views.push(&v);
    }
    dom::set(&xr, "views", views);
    let controllers = js_sys::Array::new();
    for (left, pose) in &info.controllers {
        let c = js_sys::Object::new();
        dom::set(&c, "left", *left);
        dom::set(&c, "pose", dom::numbers(&pose.to_cols_array()));
        controllers.push(&c);
    }
    dom::set(&xr, "controllers", controllers);
    xr.into()
}

pub fn start() -> Result<(), JsValue> {
    let canvas: HtmlCanvasElement = dom::by_id("gl");
    let button: HtmlButtonElement = dom::by_id("enter-vr");
    button.style().set_property("display", "inline-block")?;
    let scene = StageScene::new();
    let (eye, target) = scene.viewer_start();
    let options = ViewerOptions {
        nav_mode: if dom::query("nav").as_deref() == Some("fly") { NavMode::Fly } else { NavMode::Orbit },
        eye,
        target,
        auto_rotate: dom::query("autorotate").map(|_| 0.3),
        ..ViewerOptions::default()
    };
    let viewer = Viewer::new(canvas, options)?;
    viewer.set_scene_hints(SceneHints {
        ceiling_height: dom::query("ceiling").and_then(|v| v.parse().ok()),
        floor_center: Vec3::ZERO,
        bounds: Some(Aabb::new(Vec3::new(-STAGE_RADIUS, 0.0, -STAGE_RADIUS), Vec3::new(STAGE_RADIUS, CAPTURE_HEIGHT, STAGE_RADIUS))),
    });

    // Depth of the empty stage per camera: the background plate.
    let empty = scene.snapshot_static();
    let scale = capture_scale();
    let rig = RingRig { count: 6, width: (112.0 * scale) as u32, height: (84.0 * scale) as u32, ..RingRig::default() }
        .cameras()
        .into_iter()
        .map(|cam| {
            let plate = render_rgbd(&empty, &cam, &CaptureOptions::default());
            (cam, plate)
        })
        .collect();

    let live = Rc::new(Live {
        viewer,
        scene,
        rig,
        button: button.clone(),
        paused: Cell::new(false),
        ticks: Cell::new(0),
        seq: Cell::new(0),
        tick_ms: Cell::new(0.0),
        apply_ms: Cell::new(0.0),
        scratch: RefCell::new((Vec::new(), Vec::new())),
        xr_supported: Cell::new(None),
        xr_changes: RefCell::new(Vec::new()),
    });
    live.viewer.set_gizmos(&live.gizmos());
    live.upload_static()?;
    live.tick();

    // Everything below lives as long as the page: the closures are leaked on
    // purpose (the Rc cycles with `live` are never broken).
    let harness = js_sys::Object::new();
    let l = live.clone();
    let state = Closure::<dyn FnMut() -> JsValue>::new(move || l.state());
    dom::set(&harness, "state", state.as_ref().clone());
    state.forget();
    let l = live.clone();
    let command = Closure::<dyn FnMut(String, JsValue)>::new(move |name: String, arg: JsValue| l.command(&name, &arg));
    dom::set(&harness, "cmd", command.as_ref().clone());
    command.forget();
    let l = live.clone();
    let apply = Closure::<dyn FnMut(Vec<u8>) -> String>::new(move |bytes: Vec<u8>| l.apply_raw(&bytes));
    dom::set(&harness, "apply", apply.as_ref().clone());
    apply.forget();
    let l = live.clone();
    let reference = Closure::<dyn FnMut(u32) -> JsValue>::new(move |eye: u32| l.xr_reference(eye));
    dom::set(&harness, "xrReference", reference.as_ref().clone());
    reference.forget();
    dom::set_global("__harness", &harness);

    // A timer, not requestAnimationFrame: the window's frame loop stops
    // while an immersive session runs on a real headset.
    let l = live.clone();
    let tick = Closure::<dyn FnMut()>::new(move || l.tick());
    dom::window().set_interval_with_callback_and_timeout_and_arguments_0(tick.as_ref().unchecked_ref(), dom::query_number("tick", 80))?;
    tick.forget();

    let l = live.clone();
    let click = Closure::<dyn FnMut(web_sys::MouseEvent)>::new(move |_| l.on_button());
    button.add_event_listener_with_callback("click", click.as_ref().unchecked_ref())?;
    click.forget();

    let weak = Rc::downgrade(&live);
    live.viewer.on_xr_change(move |active| {
        if let Some(live) = weak.upgrade() {
            live.xr_changes.borrow_mut().push(active);
            live.update_button();
            dom::set_status(if active { "live: in VR" } else { "live: desktop" });
        }
    });
    live.update_button();
    let l = live.clone();
    wasm_bindgen_futures::spawn_local(async move {
        l.xr_supported.set(Some(Viewer::xr_supported().await));
        l.update_button();
    });
    dom::set_status("live: desktop");
    Ok(())
}
