//! `window.__splatLive`: a read-only view of the application for scripted
//! checks (see `e2e/app`).
//!
//! * `state()` returns a plain JSON-serialisable object: source, link,
//!   scene, configuration summary, layer counts, viewer statistics, camera,
//!   display settings, VR session, last Stats, message counters, thumbnails,
//!   scan progress, imported file, errors.
//! * `viewerEye()` returns the desktop camera position `[x, y, z]`.

use gs_render::{NavMode, LAYER_DYNAMIC, LAYER_STATIC};
use leptos::prelude::*;
use serde_json::{json, Value};
use wasm_bindgen::prelude::*;

use crate::core::{with_core, Core};
use crate::logic::config::scan_id;

fn vec3(v: glam::Vec3) -> Value {
    json!([v.x, v.y, v.z])
}

fn state(core: &Core) -> Value {
    let ui = core.ui;
    let viewer = &core.viewer;
    let stats = viewer.stats();
    let params = viewer.params();
    let display = ui.display.get_untracked();
    let config = ui.config.get_untracked();
    let link = ui.link.get_untracked();
    let counters = core.counters();
    let xr_session = viewer.xr_info().map(|info| {
        json!({
            "frames": info.frames,
            "referenceSpace": info.reference_space,
            "rig": [info.rig_position.x, info.rig_position.y, info.rig_position.z, info.rig_yaw],
            "scale": info.world_scale,
            "miniature": info.miniature,
            "viewports": info.views.iter().map(|v| v.viewport).collect::<Vec<_>>(),
            "controllers": info.controllers.len(),
        })
    });
    json!({
        "source": core.source().map(|s| s.id()),
        "link": link.id(),
        "connected": link == crate::logic::source::Link::Connected,
        "scene": ui.scene.get_untracked().map(|s| serde_json::to_value(s).unwrap_or(Value::Null)),
        "config": config.map(|c| json!({
            "cameras": c.rig.count,
            "width": c.rig.width,
            "height": c.rig.height,
            "noise": !c.noise.is_none(),
            "shadows": c.shadows,
            "backgroundSubtraction": c.background_subtraction,
            "paused": c.paused,
            "targetHz": c.target_hz,
            "scanSpeed": c.scan.speed,
        })),
        "layers": {
            "static": viewer.layer_len(LAYER_STATIC),
            "dynamic": viewer.layer_len(LAYER_DYNAMIC),
        },
        "viewer": {
            "fps": stats.fps,
            "frameMs": stats.frame_ms,
            "sortMs": stats.sort_ms,
            "frames": stats.frames,
            "framesRendered": stats.frames_rendered,
            "splatsTotal": stats.splats_total,
            "splatsDrawn": stats.splats_drawn,
            "canvasWidth": stats.canvas_width,
            "canvasHeight": stats.canvas_height,
            "sorts": stats.sorts,
        },
        "nav": match viewer.nav_mode() { NavMode::Orbit => "orbit", NavMode::Fly => "fly" },
        "eye": vec3(viewer.eye()),
        "target": vec3(viewer.target()),
        "display": {
            "splatScale": params.splat_scale,
            "pointMode": params.point_mode,
            "gizmos": params.show_gizmos,
            "showStatic": params.show_static,
            "showDynamic": params.show_dynamic,
            "clipY": params.clip_y_max,
            "dollhouse": display.dollhouse,
            "autoRotate": display.auto_rotate,
        },
        "xr": {
            "supported": ui.xr_supported.get_untracked(),
            "active": viewer.xr_active(),
            "error": viewer.xr_error(),
            "session": xr_session,
        },
        "stats": core.last_stats().map(|s| serde_json::to_value(s).unwrap_or(Value::Null)),
        "counts": {
            "messages": counters.messages,
            "bytes": counters.bytes,
            "states": counters.states,
            "stats": counters.stats,
            "layers": counters.layers,
            "previews": counters.previews,
            "droppedLayers": counters.dropped_layers,
            "batches": core.batches_received(),
        },
        "previewCount": counters.previews,
        "previews": {
            "ids": ui.camera_ids.get_untracked(),
            "painted": core.preview_paints().into_iter().map(|(id, n)| (id.to_string(), json!(n))).collect::<serde_json::Map<_, _>>(),
            "depth": ui.preview_depth.get_untracked(),
        },
        "scan": ui.scan.get_untracked().map(|s| json!({ "state": scan_id(s.state), "progress": s.progress })),
        "file": ui.file.get_untracked().map(|f| json!({ "name": f.name, "splats": f.splats })),
        "fileMode": core.is_file_mode(),
        "fileError": ui.file_error.get_untracked(),
        "notice": ui.notice.get_untracked().map(|n| json!({ "error": n.error, "text": n.text })),
        "transferBytesPerSecond": ui.transfer_rate.get_untracked(),
        "server": ui.server.get_untracked().map(|s| serde_json::to_value(s).unwrap_or(Value::Null)),
        "panelOpen": ui.panel_open.get_untracked(),
        "uiHidden": ui.ui_hidden.get_untracked(),
        "errors": core.errors(),
    })
}

/// Installs `window.__splatLive`.
pub fn install() {
    let Some(window) = web_sys::window() else { return };
    let hook = js_sys::Object::new();
    let state_fn = Closure::<dyn Fn() -> JsValue>::new(|| {
        let mut out = JsValue::NULL;
        with_core(|core| out = js_sys::JSON::parse(&state(core).to_string()).unwrap_or(JsValue::NULL));
        out
    });
    let eye_fn = Closure::<dyn Fn() -> JsValue>::new(|| {
        let mut out = JsValue::NULL;
        with_core(|core| {
            let eye = core.viewer.eye();
            out = js_sys::Array::of3(&eye.x.into(), &eye.y.into(), &eye.z.into()).into();
        });
        out
    });
    let _ = js_sys::Reflect::set(&hook, &"state".into(), state_fn.as_ref());
    let _ = js_sys::Reflect::set(&hook, &"viewerEye".into(), eye_fn.as_ref());
    let _ = js_sys::Reflect::set(&window, &"__splatLive".into(), &hook);
    // The hook lives as long as the page.
    state_fn.forget();
    eye_fn.forget();
}
