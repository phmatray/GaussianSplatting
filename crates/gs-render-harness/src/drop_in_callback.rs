//! `?test=drop&when=start|end`: the last [`Viewer`] handle is dropped from
//! inside one of the viewer's own callbacks.
//!
//! The `on_xr_change` listener owns the only handle and drops it when the VR
//! session starts (`when=start`) or ends (`when=end`). In the second case the
//! listener runs inside the viewer's `end` event closure, i.e. the viewer is
//! asked to go away while one of the closures it owns is executing; it must
//! shut down (loop stopped, session ended, listeners removed) without the
//! browser ever calling into freed memory.
//!
//! For scripts: `window.__enter()` requests the session, `window.__events`
//! lists the `on_xr_change` calls and `window.__dropped` is set once the
//! handle is gone.

use std::cell::RefCell;
use std::rc::Rc;

use glam::Vec3;
use gs_render::{Viewer, ViewerOptions, LAYER_STATIC};
use wasm_bindgen::prelude::*;
use web_sys::HtmlCanvasElement;

use crate::clouds;
use crate::dom;

pub fn start() -> Result<(), JsValue> {
    let canvas: HtmlCanvasElement = dom::by_id("gl");
    let viewer = Viewer::new(canvas, ViewerOptions::default())?;
    viewer.set_layer(LAYER_STATIC, &clouds::sphere(Vec3::new(0.0, 1.0, 0.0), 0.6, 4000))?;

    let drop_on_start = dom::query("when").as_deref() == Some("start");
    let events = js_sys::Array::new();
    dom::set_global("__events", &events);
    let slot: Rc<RefCell<Option<Viewer>>> = Rc::new(RefCell::new(None));
    viewer.on_xr_change({
        let slot = slot.clone();
        move |active| {
            events.push(&active.into());
            if active == drop_on_start {
                // Released before the viewer is dropped: its shutdown may call
                // this listener again.
                let viewer = slot.borrow_mut().take();
                if viewer.is_some() {
                    dom::set_global("__dropped", &true.into());
                }
                drop(viewer);
            }
        }
    });
    *slot.borrow_mut() = Some(viewer);

    let enter = Closure::<dyn FnMut()>::new(move || {
        // Cloned out of the cell: `enter_xr` must not run under the borrow.
        let viewer = slot.borrow().clone();
        if let Some(viewer) = viewer {
            viewer.enter_xr();
        }
    });
    dom::set_global("__enter", enter.as_ref());
    enter.forget();
    dom::set_status("drop: ready");
    Ok(())
}
