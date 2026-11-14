//! `?test=scene&scene=stage|house`: the product's own cloud in the real
//! viewer, for pictures, timings and the checks of the blending modes.
//!
//! The cloud comes from `gs_core::Pipeline` with its default configuration
//! through `Viewer::apply_message` ([`crate::scenes`]). On the stage,
//! `animate=1` keeps the pipeline ticking (30 scene ticks per second of
//! wall time, as far as the browser keeps up).
//!
//! Query parameters: `scene`, `time=<s>` (instant of the dance, default
//! 2.4), `blend`, `quality`, `mask_float=1`, `light=1` (the light pipeline
//! preset), `patch=<JSON>` (merged into the pipeline configuration),
//! `gizmos=1`, `max_dpr=<n>` (how much of the device pixel ratio the canvas
//! follows, default 1).
//!
//! For scripts, `window.__scene`:
//!
//! * `state()`: splat counts, blending status, statistics, the session.
//! * `cmd(name, arg)`: `view [ex, ey, ez, tx, ty, tz]`, `blend`, `quality`,
//!   `clipY`, `fov`, `points`, `gizmos`, `splatScale`, `sortedScale`,
//!   `cullBackfaces`, `showDynamic`, `liveWeight`, `weights [static, live]`,
//!   `renderScale`, `content`, `nav`, `enterXr`, `exitXr`, `miniature`,
//!   `pause`.
//! * `timing(frames)`: draws `frames` frames of a slow orbit synchronously
//!   (each one read back, so the GPU work is included) at the canvas size
//!   and returns milliseconds per frame (mean, fastest, median) and the
//!   part of it spent sorting.
//! * `popping(steps, degrees)`: renders `steps` views of an orbit that turns
//!   `degrees` per step and returns how much consecutive pictures differ.
//! * `flicker(ticks)` (with `animate=1`): ticks the pipeline `ticks` times
//!   under a fixed camera and returns how much consecutive pictures differ.
//!   With the scene frozen (`patch={"time_scale":0,...}`) and sensor noise
//!   on, that is the flicker of the renderer on a noisy live layer.
//! * `fragments()`: fragments per pixel of the current view and mode,
//!   counted by the CPU reference.
//! * `compareCanvas()` (a promise): the canvas as the frame loop draws it
//!   against the CPU reference of the current mode, camera and canvas size.
//!   The check to run after anything that could leave the renderer in a
//!   wrong state: a resize, a mode switch, a lost context.
//! * `orbitCheck(steps, degrees)`: every view of an orbit on the GPU and on
//!   the CPU: how well each pair agrees, and how much consecutive pictures
//!   differ in either sequence. A renderer that pops or flickers on its own
//!   changes more from frame to frame than its reference does.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use glam::Vec3;
use gs_core::experimental::wblend::{self, WblendOptions};
use gs_core::pipeline::Pipeline;
use gs_core::raster::{mean_abs_error, psnr, Image};
use gs_core::{Aabb, Camera, Splat};
use gs_render::{
    ActiveBlend, GizmoScene, GridGizmo, LayerContent, NavMode, RenderParams, SceneHints, Viewer, ViewerOptions, LAYER_DYNAMIC, LAYER_STATIC,
};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::HtmlCanvasElement;

use crate::compare::reference_image;
use crate::dom;
use crate::live::{mask_float_targets, parse_blend, parse_quality};
use crate::scenes::{self, SceneCloud};

struct Page {
    viewer: Viewer,
    canvas: HtmlCanvasElement,
    cloud: SceneCloud,
    /// The ticking pipeline of `animate=1`.
    pipeline: RefCell<Option<Pipeline>>,
    paused: Cell<bool>,
    ticks: Cell<u32>,
}

fn numbers_of(arg: &JsValue) -> Vec<f32> {
    js_sys::Array::from(arg).iter().map(|x| x.as_f64().unwrap_or(f64::NAN) as f32).collect()
}

/// Largest per-channel difference of every pixel of two pictures.
fn pixel_differences<'a>(a: &'a Image, b: &'a Image) -> impl Iterator<Item = u8> + 'a {
    a.rgb.chunks_exact(3).zip(b.rgb.chunks_exact(3)).map(|(p, q)| p.iter().zip(q).map(|(x, y)| x.abs_diff(*y)).max().unwrap_or(0))
}

/// How much consecutive pictures of a sequence differ.
#[derive(Default)]
struct Changes {
    previous: Option<Image>,
    pairs: u32,
    /// Sum over the pairs of the mean absolute difference per channel.
    mean_abs: f64,
    /// Sum of the fractions of pixels that jump by more than 8 / 32 levels.
    jumps8: f64,
    jumps32: f64,
    /// Largest fraction above 8 levels of a single pair.
    worst8: f64,
}

impl Changes {
    fn push(&mut self, image: Image) {
        if let Some(before) = &self.previous {
            let pixels = (image.rgb.len() / 3).max(1) as f64;
            let total: u64 = image.rgb.iter().zip(&before.rgb).map(|(a, b)| a.abs_diff(*b) as u64).sum();
            let over = |level: u8| pixel_differences(&image, before).filter(|d| *d > level).count() as f64 / pixels;
            let over8 = over(8);
            self.mean_abs += total as f64 / image.rgb.len().max(1) as f64;
            self.jumps8 += over8;
            self.jumps32 += over(32);
            self.worst8 = self.worst8.max(over8);
            self.pairs += 1;
        }
        self.previous = Some(image);
    }

    /// `{ blend, pairs, meanAbsDiff, jumpFraction8, jumpFraction32,
    /// worstJumpFraction8 }`, the fractions averaged over the pairs.
    fn report(&self, blend: &str) -> js_sys::Object {
        let n = self.pairs.max(1) as f64;
        let out = js_sys::Object::new();
        dom::set(&out, "blend", blend);
        dom::set(&out, "pairs", self.pairs);
        dom::set(&out, "meanAbsDiff", self.mean_abs / n);
        dom::set(&out, "jumpFraction8", self.jumps8 / n);
        dom::set(&out, "jumpFraction32", self.jumps32 / n);
        dom::set(&out, "worstJumpFraction8", self.worst8);
        out
    }
}

impl Page {
    /// The desktop camera as a pinhole camera of the canvas size.
    fn camera(&self) -> Camera {
        let (width, height) = (self.canvas.width().max(1), self.canvas.height().max(1));
        Camera::look_at(self.viewer.eye(), self.viewer.target(), Vec3::Y, self.viewer.fov_y(), width, height)
    }

    /// The camera `angle` radians further around the orbit target.
    fn orbited(&self, angle: f32) -> Camera {
        let (eye, target) = (self.viewer.eye(), self.viewer.target());
        let offset = glam::Quat::from_rotation_y(angle) * (eye - target);
        Camera::look_at(target + offset, target, Vec3::Y, self.viewer.fov_y(), self.canvas.width().max(1), self.canvas.height().max(1))
    }

    fn command(&self, name: &str, arg: &JsValue) {
        let viewer = &self.viewer;
        let flag = arg.as_bool().unwrap_or(true);
        let params = viewer.params();
        match name {
            "view" => {
                let v = numbers_of(arg);
                if v.len() == 6 {
                    viewer.set_view(Vec3::new(v[0], v[1], v[2]), Vec3::new(v[3], v[4], v[5]));
                }
            }
            "nav" => viewer.set_nav_mode(if arg.as_string().as_deref() == Some("fly") { NavMode::Fly } else { NavMode::Orbit }),
            "blend" => viewer.set_params(RenderParams { blend: parse_blend(arg.as_string().as_deref()), ..params }),
            "quality" => viewer.set_params(RenderParams { quality: parse_quality(arg.as_string().as_deref()), ..params }),
            "clipY" => viewer.set_params(RenderParams { clip_y_max: arg.as_f64().map(|y| y as f32), ..params }),
            "fov" => viewer.set_fov_y(arg.as_f64().unwrap_or(f64::NAN) as f32),
            "points" => viewer.set_params(RenderParams { point_mode: flag, ..params }),
            "gizmos" => viewer.set_params(RenderParams { show_gizmos: flag, ..params }),
            "splatScale" => viewer.set_params(RenderParams { splat_scale: arg.as_f64().unwrap_or(1.0) as f32, ..params }),
            "sortedScale" => viewer.set_params(RenderParams { sorted_surfel_scale: arg.as_f64().unwrap_or(1.0) as f32, ..params }),
            "cullBackfaces" => viewer.set_params(RenderParams { cull_backfaces: flag, ..params }),
            "showDynamic" => viewer.set_params(RenderParams { show_dynamic: flag, ..params }),
            "liveWeight" => {
                let weight = arg.as_f64().unwrap_or(f64::NAN) as f32;
                viewer.set_params(RenderParams { layer_weights: [params.layer_weights[0], weight], ..params });
            }
            "weights" => {
                let v = numbers_of(arg);
                if v.len() == 2 {
                    viewer.set_params(RenderParams { layer_weights: [v[0], v[1]], ..params });
                }
            }
            "renderScale" => viewer.set_params(RenderParams { render_scale: arg.as_f64().unwrap_or(1.0) as f32, ..params }),
            // What both layers are taken to hold: "imported" draws them as
            // the viewer drew everything before the blending modes existed
            // (sorted, plain dilation), "surfels" is what the wire delivers.
            "content" => {
                let content = if arg.as_string().as_deref() == Some("imported") { LayerContent::Imported } else { LayerContent::Surfels };
                for layer in [LAYER_STATIC, LAYER_DYNAMIC] {
                    let _ = viewer.set_layer_content(layer, content);
                }
            }
            "pause" => self.paused.set(flag),
            "miniature" => viewer.set_xr_miniature(flag),
            "enterXr" => viewer.enter_xr(),
            "exitXr" => viewer.exit_xr(),
            _ => web_sys::console::warn_1(&format!("[scene] unknown command {name}").into()),
        }
    }

    fn state(&self) -> JsValue {
        let viewer = &self.viewer;
        let stats = viewer.stats();
        let state = js_sys::Object::new();
        dom::set(&state, "scene", self.cloud.state.scene.as_str());
        dom::set(&state, "staticSplats", viewer.layer_len(LAYER_STATIC) as u32);
        dom::set(&state, "splatsTotal", stats.splats_total);
        dom::set(&state, "splatsDrawn", stats.splats_drawn);
        dom::set(&state, "blend", stats.blend.active.as_str());
        dom::set(&state, "blendReason", stats.blend.reason.as_str());
        dom::set(&state, "blendFallback", stats.blend.reason.is_fallback());
        dom::set(&state, "offscreen", dom::numbers(&[stats.offscreen_width as f32, stats.offscreen_height as f32]));
        dom::set(&state, "sorts", stats.sorts as f64);
        dom::set(&state, "sortMs", stats.sort_ms);
        dom::set(&state, "frameMs", stats.frame_ms);
        dom::set(&state, "fps", stats.fps);
        dom::set(&state, "frames", stats.frames as f64);
        dom::set(&state, "framesRendered", stats.frames_rendered as f64);
        dom::set(&state, "canvasWidth", stats.canvas_width);
        dom::set(&state, "canvasHeight", stats.canvas_height);
        dom::set(&state, "ticks", self.ticks.get());
        dom::set(&state, "xrActive", stats.xr_active);
        dom::set(&state, "xrError", viewer.xr_error().map_or(JsValue::NULL, JsValue::from));
        dom::set(&state, "xrFrames", viewer.xr_info().map_or(0, |info| info.frames));
        dom::set(&state, "eye", dom::numbers(&viewer.eye().to_array()));
        dom::set(&state, "target", dom::numbers(&viewer.target().to_array()));
        let session = &self.cloud.state;
        dom::set(&state, "viewerStart", dom::numbers(&[session.viewer_start.eye, session.viewer_start.target].concat()));
        dom::set(&state, "bounds", dom::numbers(&[session.bounds.min.to_array(), session.bounds.max.to_array()].concat()));
        dom::set(&state, "ceiling", session.ceiling_height.map_or(JsValue::NULL, JsValue::from));
        state.into()
    }

    /// `timing(frames)`: see the module documentation.
    fn timing(&self, frames: u32) -> JsValue {
        let frames = frames.clamp(1, 1000);
        // One frame first: target allocation and shader warm-up are not
        // part of a frame.
        let _ = self.viewer.snapshot(&self.camera());
        let sorts = self.viewer.stats().sorts;
        let mut times = Vec::with_capacity(frames as usize);
        let mut sort_ms = 0.0;
        for i in 0..frames {
            // Two degrees per frame: every sorted frame needs a new order.
            let cam = self.orbited((2.0 * (i + 1) as f32).to_radians());
            let start = dom::now();
            if let Err(error) = self.viewer.snapshot(&cam) {
                web_sys::console::error_2(&"[scene] snapshot failed:".into(), &error);
            }
            times.push(dom::now() - start);
            let stats = self.viewer.stats();
            if stats.blend.active == ActiveBlend::Sorted {
                sort_ms += stats.sort_ms as f64;
            }
        }
        let total: f64 = times.iter().sum();
        // On a shared machine the fastest and the median frame say more
        // than the mean.
        times.sort_by(f64::total_cmp);
        let out = js_sys::Object::new();
        dom::set(&out, "frames", frames);
        dom::set(&out, "blend", self.viewer.blend_status().active.as_str());
        dom::set(&out, "width", self.canvas.width());
        dom::set(&out, "height", self.canvas.height());
        dom::set(&out, "frameMs", total / frames as f64);
        dom::set(&out, "minMs", times[0]);
        dom::set(&out, "medianMs", times[times.len() / 2]);
        dom::set(&out, "sortMs", sort_ms / frames as f64);
        dom::set(&out, "sorts", (self.viewer.stats().sorts - sorts) as f64);
        out.into()
    }

    /// `popping(steps, degrees)`: see the module documentation.
    fn popping(&self, steps: u32, degrees: f32) -> JsValue {
        let mut changes = Changes::default();
        for i in 0..steps.clamp(2, 400) {
            let cam = self.orbited((degrees * i as f32).to_radians());
            if let Ok(image) = self.viewer.snapshot(&cam) {
                changes.push(image);
            }
        }
        changes.report(self.viewer.blend_status().active.as_str()).into()
    }

    /// `flicker(ticks)`: see the module documentation.
    fn flicker(&self, ticks: u32) -> JsValue {
        let cam = self.camera();
        let mut changes = Changes::default();
        for _ in 0..ticks.clamp(2, 400) {
            self.tick_now(1.0 / 30.0);
            if let Ok(image) = self.viewer.snapshot(&cam) {
                changes.push(image);
            }
        }
        let out = changes.report(self.viewer.blend_status().active.as_str());
        dom::set(&out, "dynamicSplats", self.viewer.layer_len(LAYER_DYNAMIC) as u32);
        out.into()
    }

    /// The CPU reference of what the viewer draws for `cam` right now.
    fn reference(&self, cam: &Camera) -> Image {
        let params = self.viewer.params();
        // The clear colour as the framebuffer rounds it.
        let background = params.background.map(|c| (c * 255.0).round() as u8);
        reference_image(&self.viewer, cam, &params, background, gs_core::splat::DEFAULT_NEAR)
    }

    /// `compareCanvas()`: see the module documentation.
    async fn compare_canvas(&self) -> Result<JsValue, JsValue> {
        for _ in 0..600 {
            let rendered = self.viewer.stats().frames_rendered;
            // Marks the view dirty, so that the next frame draws.
            self.viewer.set_view(self.viewer.eye(), self.viewer.target());
            dom::next_frame().await;
            if self.viewer.stats().frames_rendered == rendered {
                continue;
            }
            // Still inside the frame that drew: the buffer is intact.
            let gpu = dom::read_canvas(&self.canvas).ok_or("could not read the canvas back")?;
            let cam = Camera::look_at(self.viewer.eye(), self.viewer.target(), Vec3::Y, self.viewer.fov_y(), gpu.width, gpu.height);
            let cpu = self.reference(&cam);
            let out = js_sys::Object::new();
            dom::set(&out, "blend", self.viewer.blend_status().active.as_str());
            dom::set(&out, "width", gpu.width);
            dom::set(&out, "height", gpu.height);
            // JSON has no Infinity: identical images report 99 dB.
            dom::set(&out, "psnr", psnr(&gpu, &cpu).min(99.0));
            dom::set(&out, "mae", mean_abs_error(&gpu, &cpu));
            return Ok(out.into());
        }
        Err("the viewer never drew a frame".into())
    }

    /// `orbitCheck(steps, degrees)`: see the module documentation.
    fn orbit_check(&self, steps: u32, degrees: f32) -> JsValue {
        let (mut gpu_changes, mut cpu_changes) = (Changes::default(), Changes::default());
        let (mut worst, mut sum, mut frames) = (f32::INFINITY, 0.0f64, 0u32);
        for i in 0..steps.clamp(2, 400) {
            let cam = self.orbited((degrees * i as f32).to_radians());
            let Ok(gpu) = self.viewer.snapshot(&cam) else { continue };
            let cpu = self.reference(&cam);
            let agreement = psnr(&gpu, &cpu).min(99.0);
            worst = worst.min(agreement);
            sum += agreement as f64;
            frames += 1;
            gpu_changes.push(gpu);
            cpu_changes.push(cpu);
        }
        let blend = self.viewer.blend_status().active.as_str();
        let out = js_sys::Object::new();
        dom::set(&out, "blend", blend);
        dom::set(&out, "frames", frames);
        dom::set(&out, "worstPsnr", if frames > 0 { worst } else { 0.0 });
        dom::set(&out, "meanPsnr", sum / frames.max(1) as f64);
        dom::set(&out, "gpu", gpu_changes.report(blend));
        dom::set(&out, "cpu", cpu_changes.report(blend));
        out.into()
    }

    /// `fragments()`: see the module documentation.
    fn fragments(&self) -> JsValue {
        let cam = self.camera();
        let params = self.viewer.params();
        let mut splats = self.viewer.splats();
        let split = self.viewer.layer_len(LAYER_STATIC);
        let visible = |s: &Splat| params.clip_y_max.is_none_or(|clip| s.pos[1] <= clip);
        splats.retain(visible);
        let split = split.min(splats.len());
        let status = self.viewer.blend_status();
        let preset = match status.active {
            ActiveBlend::Sorted => WblendOptions { kernel_scale: params.sorted_surfel_scale, ..WblendOptions::sorted() },
            ActiveBlend::NormalizedHard => WblendOptions { cull_backfaces: params.cull_backfaces, ..WblendOptions::normalized_hard() },
            ActiveBlend::NormalizedSoft => WblendOptions { cull_backfaces: params.cull_backfaces, ..WblendOptions::normalized_soft() },
        };
        let layers = [
            wblend::Layer { splats: &splats[..split], weight: params.layer_weights[0] },
            wblend::Layer { splats: &splats[split..], weight: params.layer_weights[1] },
        ];
        let stats = wblend::render(&layers, &cam, &preset).stats;
        let pixels = (cam.width as f64 * cam.height as f64).max(1.0);
        let out = js_sys::Object::new();
        dom::set(&out, "blend", status.active.as_str());
        dom::set(&out, "splats", stats.splats as f64);
        dom::set(&out, "visibilityPerPixel", stats.visibility as f64 / pixels);
        dom::set(&out, "shadedPerPixel", stats.shaded as f64 / pixels);
        dom::set(&out, "blendedPerPixel", stats.blended as f64 / pixels);
        out.into()
    }

    /// One tick of the animated stage, unless paused.
    fn tick(&self, dt: f32) {
        if !self.paused.get() {
            self.tick_now(dt);
        }
    }

    fn tick_now(&self, dt: f32) {
        let mut guard = self.pipeline.borrow_mut();
        let Some(pipeline) = guard.as_mut() else { return };
        for message in scenes::tick_layers(pipeline, dt) {
            if let Err(error) = self.viewer.apply_message(&message) {
                web_sys::console::error_1(&format!("[scene] apply_message failed: {error}").into());
            }
        }
        self.ticks.set(self.ticks.get() + 1);
    }
}

fn export(page: &Rc<Page>) {
    let api = js_sys::Object::new();
    let p = page.clone();
    let state = Closure::<dyn FnMut() -> JsValue>::new(move || p.state());
    dom::set(&api, "state", state.as_ref().clone());
    state.forget();
    let p = page.clone();
    let command = Closure::<dyn FnMut(String, JsValue)>::new(move |name: String, arg: JsValue| p.command(&name, &arg));
    dom::set(&api, "cmd", command.as_ref().clone());
    command.forget();
    let p = page.clone();
    let timing = Closure::<dyn FnMut(u32) -> JsValue>::new(move |frames: u32| p.timing(frames));
    dom::set(&api, "timing", timing.as_ref().clone());
    timing.forget();
    let p = page.clone();
    let popping = Closure::<dyn FnMut(u32, f32) -> JsValue>::new(move |steps: u32, degrees: f32| p.popping(steps, degrees));
    dom::set(&api, "popping", popping.as_ref().clone());
    popping.forget();
    let p = page.clone();
    let flicker = Closure::<dyn FnMut(u32) -> JsValue>::new(move |ticks: u32| p.flicker(ticks));
    dom::set(&api, "flicker", flicker.as_ref().clone());
    flicker.forget();
    let p = page.clone();
    let compare = Closure::<dyn FnMut() -> js_sys::Promise>::new(move || {
        let p = p.clone();
        wasm_bindgen_futures::future_to_promise(async move { p.compare_canvas().await })
    });
    dom::set(&api, "compareCanvas", compare.as_ref().clone());
    compare.forget();
    let p = page.clone();
    let orbit = Closure::<dyn FnMut(u32, f32) -> JsValue>::new(move |steps: u32, degrees: f32| p.orbit_check(steps, degrees));
    dom::set(&api, "orbitCheck", orbit.as_ref().clone());
    orbit.forget();
    let p = page.clone();
    let fragments = Closure::<dyn FnMut() -> JsValue>::new(move || p.fragments());
    dom::set(&api, "fragments", fragments.as_ref().clone());
    fragments.forget();
    dom::set_global("__scene", &api);
}

async fn run(canvas: HtmlCanvasElement) -> Result<(), String> {
    let house = dom::query("scene").as_deref() == Some("house");
    let time = dom::query_number("time", 2.4f32);
    dom::set_status("scene: building the cloud");
    dom::sleep(0).await;
    let start = dom::now();
    let cloud = if house { scenes::house().await? } else { scenes::stage(time)? };
    let build_ms = dom::now() - start;

    if dom::query("mask_float").is_some() {
        mask_float_targets();
    }
    let session = &cloud.state;
    let options = ViewerOptions {
        params: RenderParams {
            blend: parse_blend(dom::query("blend").as_deref()),
            quality: parse_quality(dom::query("quality").as_deref()),
            show_gizmos: dom::query("gizmos").is_some(),
            ..RenderParams::default()
        },
        nav_mode: NavMode::Orbit,
        eye: Vec3::from_array(session.viewer_start.eye),
        target: Vec3::from_array(session.viewer_start.target),
        max_dpr: dom::query_number("max_dpr", 1.0f32),
        ..ViewerOptions::default()
    };
    let viewer = Viewer::new(canvas.clone(), options).map_err(|e| format!("{e:?}"))?;
    let bounds: Aabb = session.bounds;
    viewer.set_scene_hints(SceneHints {
        ceiling_height: session.ceiling_height,
        floor_center: Vec3::new((bounds.min.x + bounds.max.x) * 0.5, 0.0, (bounds.min.z + bounds.max.z) * 0.5),
        bounds: Some(bounds),
    });
    viewer.set_gizmos(&GizmoScene { grid: Some(GridGizmo::default()), axes: Some(0.5), ..GizmoScene::default() });
    cloud.upload(&viewer)?;

    let animate = !house && dom::query("animate").is_some();
    let pipeline = if animate {
        let mut pipeline = scenes::stage_pipeline()?;
        pipeline.tick(time);
        Some(pipeline)
    } else {
        None
    };
    let page =
        Rc::new(Page { viewer, canvas, cloud, pipeline: RefCell::new(pipeline), paused: Cell::new(false), ticks: Cell::new(0) });
    export(&page);
    if animate {
        let p = page.clone();
        let tick = Closure::<dyn FnMut()>::new(move || p.tick(1.0 / 30.0));
        dom::window()
            .set_interval_with_callback_and_timeout_and_arguments_0(tick.as_ref().unchecked_ref(), 33)
            .map_err(|e| format!("{e:?}"))?;
        tick.forget();
    }
    dom::set_global("__buildMs", &build_ms.into());
    dom::set_status("scene: ready");
    dom::set_global("__ready", &true.into());
    // The page lives as long as the window.
    std::mem::forget(page);
    Ok(())
}

pub fn start() -> Result<(), JsValue> {
    let canvas: HtmlCanvasElement = dom::by_id("gl");
    wasm_bindgen_futures::spawn_local(async move {
        if let Err(error) = run(canvas).await {
            dom::set_status(&format!("scene: FAILED: {error}"));
            dom::set_global("__error", &error.into());
        }
    });
    Ok(())
}
