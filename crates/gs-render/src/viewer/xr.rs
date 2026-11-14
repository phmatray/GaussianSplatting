//! The WebXR `immersive-vr` session.
//!
//! Pattern (verified in the WebXR spike): the frame and `end` callbacks are
//! created once in `Viewer::new` and reused by every session; poses are
//! queried in the runtime's reference space and moved into the world with the
//! rig matrix of [`crate::rig`]; no `RefCell` borrow is ever held across a
//! call that can dispatch events (`XRSession.end()` fires `end`
//! synchronously in some runtimes) or across user callbacks.

use std::rc::Rc;

use glam::Mat4;
use gs_core::splat::ViewParams;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{
    Gamepad, GamepadButton, WebGl2RenderingContext as GL, XrFrame, XrHandedness, XrReferenceSpace, XrReferenceSpaceType, XrRenderStateInit,
    XrSession, XrSessionInit, XrSessionMode, XrSpace, XrSystem, XrWebGlLayer, XrWebGlLayerInit,
};

use super::{Inner, FAR, NEAR};
use crate::gizmo::LineBatch;
use crate::normalized::Output;
use crate::params::{XrInfo, XrViewInfo};
use crate::rig::{Locomotion, LocomotionContext, PadState, Rig, NOMINAL_EYE_HEIGHT};

/// A button counts as pressed above this analog value.
const BUTTON_THRESHOLD: f64 = 0.5;
/// Gizmo line widths are specified for a display about this many pixels
/// high; they are scaled up for the denser eye buffers.
const LINE_REFERENCE_HEIGHT: f32 = 800.0;

/// Everything that exists only while a session runs.
pub(super) struct XrState {
    session: XrSession,
    /// The space returned by `requestReferenceSpace` (never offset).
    space: XrReferenceSpace,
    space_name: &'static str,
    /// Height of the real floor in that space.
    floor_rig_y: f32,
    locomotion: Locomotion,
    /// Most recent head pose in the reference space.
    rig_from_head: Mat4,
    last_time_ms: Option<f64>,
    frames: u32,
    /// Pending `XRSession.requestAnimationFrame` handle.
    raf_handle: u32,
    views: Vec<XrViewInfo>,
    controllers: Vec<(bool, Mat4)>,
    /// Scratch for the controller gizmos.
    overlay: LineBatch,
}

impl XrState {
    pub fn info(&self) -> XrInfo {
        let rig = self.locomotion.rig();
        XrInfo {
            reference_space: self.space_name,
            frames: self.frames,
            rig_position: rig.pos,
            rig_yaw: rig.yaw,
            world_scale: rig.scale,
            miniature: self.locomotion.is_miniature(),
            world_from_head: rig.world_from_rig() * self.rig_from_head,
            views: self.views.clone(),
            controllers: self.controllers.clone(),
        }
    }
}

/// `navigator.xr`, or `None` where WebXR does not exist. The web-sys getter
/// is typed as non-optional, but the property is `undefined` on insecure
/// origins and in browsers without WebXR.
fn xr_system() -> Option<XrSystem> {
    let navigator = web_sys::window()?.navigator();
    let present = js_sys::Reflect::get(&navigator, &"xr".into()).map(|v| v.is_object()).unwrap_or(false);
    present.then(|| navigator.xr())
}

pub(super) async fn supported() -> bool {
    let Some(xr) = xr_system() else { return false };
    match xr.is_session_supported(XrSessionMode::ImmersiveVr).await {
        Ok(supported) => supported.value_of(),
        Err(_) => false,
    }
}

/// Reads the `xr-standard` gamepad of a controller: `axes[2..4]` is the
/// thumbstick (`[0..2]` are the unused touchpad slots), `buttons[0]` the
/// trigger, `[4]` A/X and `[5]` B/Y.
fn read_pad(gamepad: &Gamepad) -> PadState {
    let axes = gamepad.axes();
    let axis = |i: u32| axes.get(i).as_f64().unwrap_or(0.0) as f32;
    let buttons = gamepad.buttons();
    let button = |i: u32| {
        let value = buttons.get(i);
        if value.is_object() {
            value.unchecked_into::<GamepadButton>().value()
        } else {
            0.0
        }
    };
    PadState {
        stick: [axis(2), axis(3)],
        trigger: button(0) as f32,
        primary: button(4) > BUTTON_THRESHOLD,
        secondary: button(5) > BUTTON_THRESHOLD,
    }
}

/// Starts entering VR. Must run synchronously inside a user-gesture handler.
pub(super) fn enter(inner: &Rc<Inner>) {
    if !inner.alive.get() || inner.xr_pending.get() || inner.xr.borrow().is_some() {
        return;
    }
    let Some(xr) = xr_system() else {
        inner.set_xr_error("WebXR is not available in this browser (or the page is not served over HTTPS)".into());
        return;
    };
    *inner.xr_error.borrow_mut() = None;
    let init = XrSessionInit::new();
    init.set_optional_features(&[JsValue::from_str("local-floor")]);
    // requestSession consumes the transient user activation, so it is called
    // right here; only the waiting happens asynchronously.
    let promise = xr.request_session_with_options(XrSessionMode::ImmersiveVr, &init);
    inner.xr_pending.set(true);
    inner.xr_cancelled.set(false);
    let inner = inner.clone();
    wasm_bindgen_futures::spawn_local(async move {
        let result = match promise.await {
            Ok(session) => inner.start_session(session).await,
            Err(e) => Err(e),
        };
        inner.xr_pending.set(false);
        match result {
            Ok(true) => inner.notify_xr(true),
            // Cancelled while starting: not an error, and nobody was told
            // that a session had begun.
            Ok(false) => {}
            Err(e) => inner.set_xr_error(format!("could not start the VR session: {}", describe(&e))),
        }
    });
}

/// Human-readable form of a JS exception.
fn describe(error: &JsValue) -> String {
    error
        .dyn_ref::<js_sys::Error>()
        .map(|e| String::from(e.message()))
        .or_else(|| error.as_string())
        .unwrap_or_else(|| format!("{error:?}"))
}

impl Inner {
    fn set_xr_error(&self, message: String) {
        web_sys::console::warn_1(&format!("gs-render: {message}").into());
        *self.xr_error.borrow_mut() = Some(message);
    }

    /// Calls the `on_xr_change` listeners. They are cloned out first: a
    /// listener may register another one or call back into the viewer.
    fn notify_xr(&self, active: bool) {
        let listeners: Vec<_> = self.xr_listeners.borrow().clone();
        for listener in listeners {
            listener(active);
        }
    }

    /// `exit_xr` or `destroy` was called while the session was starting.
    fn xr_start_cancelled(&self) -> bool {
        !self.alive.get() || self.xr_cancelled.get()
    }

    /// Returns whether the session runs now (`false`: cancelled meanwhile).
    async fn start_session(&self, session: XrSession) -> Result<bool, JsValue> {
        let result = self.configure_session(&session).await;
        if !matches!(result, Ok(true)) {
            // Half-started or unwanted session: do not leave the user in the void.
            let end = session.end();
            let _ = end.await;
        }
        result
    }

    /// Sets the session up and installs it as the running one. Returns
    /// `Ok(false)`, with nothing installed, when the start was cancelled.
    async fn configure_session(&self, session: &XrSession) -> Result<bool, JsValue> {
        let gl = &self.gl;
        // A no-op for a context created with `xrCompatible: true`.
        gl.make_xr_compatible().await?;
        let options = self.xr_options.get();

        let layer_init = XrWebGlLayerInit::new();
        // Same reasoning as for the canvas context: no MSAA, no depth.
        layer_init.set_antialias(false);
        layer_init.set_depth(false);
        layer_init.set_stencil(false);
        layer_init.set_alpha(false);
        let scale = options.framebuffer_scale;
        if scale.is_finite() && scale > 0.0 {
            layer_init.set_framebuffer_scale_factor(scale.clamp(0.1, 4.0) as f64);
        }
        let layer = XrWebGlLayer::new_with_web_gl2_rendering_context_and_layer_init(session, gl, &layer_init)?;
        if let Some(level) = options.foveation.filter(|l| l.is_finite()) {
            // Not in web-sys; only some runtimes (Quest) have the property.
            let key = JsValue::from_str("fixedFoveation");
            if js_sys::Reflect::has(&layer, &key).unwrap_or(false) {
                let _ = js_sys::Reflect::set(&layer, &key, &JsValue::from_f64(level.clamp(0.0, 1.0) as f64));
            }
        }
        let render_state = XrRenderStateInit::new();
        render_state.set_base_layer(Some(&layer));
        render_state.set_depth_near(NEAR as f64);
        render_state.set_depth_far(FAR as f64);
        session.update_render_state_with_state(&render_state);

        // `local-floor` is only an optional feature: fall back to the
        // eye-level `local` space and assume a standing user.
        let (space, space_name, floor_rig_y) = match session.request_reference_space(XrReferenceSpaceType::LocalFloor).await {
            Ok(space) => (space, "local-floor", 0.0),
            Err(_) => (session.request_reference_space(XrReferenceSpaceType::Local).await?, "local", -NOMINAL_EYE_HEIGHT),
        };

        // Last await above: from here on the session is installed
        // synchronously, so this is the one place to honor a cancellation.
        if self.xr_start_cancelled() {
            return Ok(false);
        }

        // The user starts where the desktop camera is, standing on the floor
        // and facing the same way.
        let (eye, yaw) = {
            let nav = self.nav.borrow();
            (nav.eye(), nav.yaw())
        };
        let home = Rig::standing(eye, yaw, self.hints.get().floor_center.y, floor_rig_y);

        let callbacks = self.callbacks();
        session.add_event_listener_with_callback("end", callbacks.xr_end.as_ref().unchecked_ref())?;
        self.pointers.borrow_mut().reset();
        self.nav.borrow_mut().clear_keys();
        self.xr_miniature_request.set(None);
        let raf_handle = session.request_animation_frame(callbacks.xr_frame.as_ref().unchecked_ref());
        *self.xr.borrow_mut() = Some(XrState {
            session: session.clone(),
            space,
            space_name,
            floor_rig_y,
            locomotion: Locomotion::new(home),
            rig_from_head: Mat4::from_translation(glam::Vec3::new(0.0, NOMINAL_EYE_HEIGHT + floor_rig_y, 0.0)),
            last_time_ms: None,
            frames: 0,
            raf_handle,
            views: Vec::new(),
            controllers: Vec::new(),
            overlay: LineBatch::default(),
        });
        Ok(true)
    }

    pub(super) fn exit_xr(&self) {
        if self.xr_pending.get() {
            // Still starting: the session is ended as soon as it exists.
            self.xr_cancelled.set(true);
        }
        // Clone the session out: `end()` may dispatch `end` re-entrantly.
        let session = self.xr.borrow().as_ref().map(|x| x.session.clone());
        if let Some(session) = session {
            let end = session.end();
            // Awaited only so that a rejection (already ended) is not reported
            // as an unhandled promise rejection.
            wasm_bindgen_futures::spawn_local(async move {
                let _ = end.await;
            });
        }
    }

    /// The session ended, from either side.
    pub(super) fn on_xr_end(&self) {
        let Some(state) = self.xr.borrow_mut().take() else { return };
        let callbacks = self.callbacks();
        let _ = state.session.remove_event_listener_with_callback("end", callbacks.xr_end.as_ref().unchecked_ref());
        drop(state);
        if self.has_gl_objects() {
            // The layer's framebuffer is gone; the desktop loop draws to the canvas again.
            self.gl.bind_framebuffer(GL::FRAMEBUFFER, None);
            self.lines.borrow_mut().set_overlay(&LineBatch::default());
        }
        self.last_frame_ms.set(None);
        self.dirty.set(true);
        self.notify_xr(false);
    }

    /// Ends the session as part of `shutdown`.
    pub(super) fn shutdown_xr(&self) {
        let Some(state) = self.xr.borrow_mut().take() else { return };
        if let Some(callbacks) = self.callbacks.get() {
            let _ = state.session.remove_event_listener_with_callback("end", callbacks.xr_end.as_ref().unchecked_ref());
        }
        state.session.cancel_animation_frame(state.raf_handle);
        let end = state.session.end();
        wasm_bindgen_futures::spawn_local(async move {
            let _ = end.await;
        });
        self.notify_xr(false);
    }

    pub(super) fn on_xr_frame(&self, time_ms: f64, frame: XrFrame) {
        let start = self.now();
        let session = frame.session();
        let mut guard = self.xr.borrow_mut();
        // A frame can still be delivered around `end`.
        let Some(x) = guard.as_mut() else { return };
        // Re-arm first, like the JS idiom; the callback is persistent, so
        // this allocates nothing.
        x.raf_handle = session.request_animation_frame(self.callbacks().xr_frame.as_ref().unchecked_ref());
        if self.context_lost.get() {
            // Nothing can be drawn until `webglcontextrestored`.
            return;
        }
        let interval = x.last_time_ms.replace(time_ms).map(|last| time_ms - last);
        let dt = interval.map_or(0.0, |ms| (ms / 1000.0) as f32);
        x.frames += 1;

        // ---- controllers -> locomotion (uses the head pose of the previous frame)
        let (mut left, mut right) = (PadState::default(), PadState::default());
        let mut grips: [Option<(bool, XrSpace)>; 2] = [None, None];
        let sources = session.input_sources();
        for i in 0..sources.length() {
            let Some(source) = sources.get(i) else { continue };
            let is_left = match source.handedness() {
                XrHandedness::Left => true,
                XrHandedness::Right => false,
                _ => continue,
            };
            if let Some(gamepad) = source.gamepad() {
                *(if is_left { &mut left } else { &mut right }) = read_pad(&gamepad);
            }
            if let Some(grip) = source.grip_space() {
                grips[!is_left as usize] = Some((is_left, grip));
            }
        }
        let ctx = LocomotionContext { options: self.xr_options.get(), hints: self.hints.get(), floor_rig_y: x.floor_rig_y };
        let events = x.locomotion.update(&left, &right, &x.rig_from_head, dt, &ctx);
        if let Some(on) = self.xr_miniature_request.take() {
            x.locomotion.set_miniature(on, &x.rig_from_head, &ctx);
        }
        if events.toggle_gizmos {
            let mut params = self.params.get();
            params.show_gizmos = !params.show_gizmos;
            self.params.set(params);
        }

        // ---- poses
        // Tracking lost: nothing to draw this frame.
        let Some(pose) = frame.get_viewer_pose(&x.space) else { return };
        let Some(layer) = session.render_state().base_layer() else { return };
        x.rig_from_head = Mat4::from_cols_slice(&pose.transform().matrix());
        let rig = x.locomotion.rig();
        let (world_from_rig, rig_from_world) = (rig.world_from_rig(), rig.rig_from_world());

        x.controllers.clear();
        x.overlay.clear();
        for (is_left, grip) in grips.iter().flatten() {
            if let Some(grip_pose) = frame.get_pose(grip, &x.space) {
                let world_from_grip = world_from_rig * Mat4::from_cols_slice(&grip_pose.transform().matrix());
                x.controllers.push((*is_left, world_from_grip));
                x.overlay.controller(&world_from_grip, *is_left);
            }
        }

        // ---- draw
        let params = self.params.get();
        let clip_y = self.clip_y(&params, x.locomotion.is_miniature());
        let gl = &self.gl;
        // An opaque framebuffer on a headset; emulators may hand out null (the canvas).
        let framebuffer = layer.framebuffer();
        gl.bind_framebuffer(GL::FRAMEBUFFER, framebuffer.as_ref());
        let [r, g, b] = params.background;
        gl.clear_color(r, g, b, 1.0);
        gl.clear(GL::COLOR_BUFFER_BIT);

        let mut scene = self.scene.borrow_mut();
        let mut lines = self.lines.borrow_mut();
        lines.set_overlay(&x.overlay);
        // Sorted mode: one sort per frame for both eyes, along the head
        // direction. Eyes look (almost) the same way, so the order is right
        // for both. The normalised mode has no order; it runs its passes
        // once per eye, in off-screen targets of the eye's size, and
        // resolves into the layer's framebuffer (which needs no depth and
        // may be multisampled).
        let head_view = x.rig_from_head.inverse() * rig_from_world;
        let status = scene.blend_status(&params);
        self.prepare_order(&mut scene, status, &head_view, &params, clip_y, false);

        x.views.clear();
        let mut size = (0, 0);
        for view in pose.views().iter() {
            let Some(viewport) = layer.get_viewport(&view) else { continue };
            let (width, height) = (viewport.width(), viewport.height());
            if width <= 0 || height <= 0 {
                continue;
            }
            gl.viewport(viewport.x(), viewport.y(), width, height);
            // Column-major Float32Arrays, exactly glam's layout. The
            // projection is used as given: per-eye frusta are asymmetric.
            let projection = Mat4::from_cols_slice(&view.projection_matrix());
            // XRView.transform is the eye's pose; the view matrix is its
            // inverse, followed by the rig (which carries the world scale).
            let view_matrix = Mat4::from_cols_slice(&view.transform().inverse().matrix()) * rig_from_world;
            let vp = ViewParams::from_projection(view_matrix, &projection, width as f32, height as f32);
            // Re-evaluated per eye: a failure in the first eye switches the
            // second one to the sorted fallback as well.
            let status = scene.blend_status(&params);
            let output = Output { framebuffer: framebuffer.as_ref(), viewport: [viewport.x(), viewport.y(), width, height] };
            self.draw_splats(&mut scene, status, &vp, &params, clip_y, output);
            let px_scale = (height as f32 / LINE_REFERENCE_HEIGHT).max(1.0);
            lines.draw(&(projection * view_matrix), [width as f32, height as f32], vp.near, px_scale, params.show_gizmos);
            x.views.push(XrViewInfo { viewport: [viewport.x(), viewport.y(), width, height], view: view_matrix, projection });
            size = (width as u32, height as u32);
        }
        drop(scene);
        drop(lines);
        drop(guard);
        self.record_frame(interval, self.now() - start, Some(size));
    }
}
