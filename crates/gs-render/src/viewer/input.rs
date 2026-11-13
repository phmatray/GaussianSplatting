//! DOM event listeners: pointer, wheel and keyboard input for the desktop
//! navigation, and WebGL context loss.

use std::rc::Rc;

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{AddEventListenerOptions, Event, EventTarget, KeyboardEvent, PointerEvent, WheelEvent};

use super::Inner;
use crate::nav::{NavKey, NavMode};

/// An event listener that removes itself when dropped.
pub(super) struct Listener {
    target: EventTarget,
    kind: &'static str,
    closure: Closure<dyn FnMut(Event)>,
}

impl Listener {
    /// `passive: false` lets the handler call `preventDefault()` on events
    /// that are passive by default (`wheel`).
    fn new(target: &EventTarget, kind: &'static str, passive: bool, handler: impl FnMut(Event) + 'static) -> Result<Self, JsValue> {
        let closure = Closure::<dyn FnMut(Event)>::new(handler);
        let options = AddEventListenerOptions::new();
        options.set_passive(passive);
        target.add_event_listener_with_callback_and_add_event_listener_options(kind, closure.as_ref().unchecked_ref(), &options)?;
        Ok(Self { target: target.clone(), kind, closure })
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        let _ = self.target.remove_event_listener_with_callback(self.kind, self.closure.as_ref().unchecked_ref());
    }
}

#[derive(Clone, Copy, Debug)]
struct Pointer {
    id: i32,
    x: f32,
    y: f32,
    touch: bool,
    /// The drag pans instead of rotating (right or middle button, or shift).
    pan: bool,
}

/// Pointers currently pressed on the canvas.
#[derive(Debug, Default)]
pub(super) struct Pointers {
    active: Vec<Pointer>,
}

impl Pointers {
    /// Forgets every pointer (mode switch, VR session).
    pub fn reset(&mut self) {
        self.active.clear();
    }

    fn centroid_and_spread(&self) -> ([f32; 2], f32) {
        let n = self.active.len().max(1) as f32;
        let cx = self.active.iter().map(|p| p.x).sum::<f32>() / n;
        let cy = self.active.iter().map(|p| p.y).sum::<f32>() / n;
        let spread = self.active.iter().map(|p| (p.x - cx).hypot(p.y - cy)).sum::<f32>() / n;
        ([cx, cy], spread)
    }
}

/// True when the event comes from a text field, where keys must type.
fn typing_target(event: &Event) -> bool {
    let Some(element) = event.target().and_then(|t| t.dyn_into::<web_sys::HtmlElement>().ok()) else { return false };
    matches!(element.tag_name().as_str(), "INPUT" | "TEXTAREA" | "SELECT") || element.is_content_editable()
}

impl Inner {
    fn on_pointer_down(&self, e: &PointerEvent) {
        if self.xr.borrow().is_some() {
            return;
        }
        // Keep receiving the drag when the pointer leaves the canvas.
        let _ = self.canvas.set_pointer_capture(e.pointer_id());
        let touch = e.pointer_type() == "touch";
        let pan = !touch && (e.button() != 0 || e.shift_key());
        let mut pointers = self.pointers.borrow_mut();
        pointers.active.retain(|p| p.id != e.pointer_id());
        pointers.active.push(Pointer { id: e.pointer_id(), x: e.client_x() as f32, y: e.client_y() as f32, touch, pan });
        e.prevent_default();
    }

    fn on_pointer_move(&self, e: &PointerEvent) {
        let mut pointers = self.pointers.borrow_mut();
        let Some(index) = pointers.active.iter().position(|p| p.id == e.pointer_id()) else { return };
        let (x, y) = (e.client_x() as f32, e.client_y() as f32);
        let before = pointers.centroid_and_spread();
        let pointer = &mut pointers.active[index];
        let (dx, dy) = (x - pointer.x, y - pointer.y);
        (pointer.x, pointer.y) = (x, y);
        let pointer = *pointer;
        if dx == 0.0 && dy == 0.0 {
            return;
        }
        let viewport_height = self.canvas.client_height().max(1) as f32;
        let mut nav = self.nav.borrow_mut();
        if pointers.active.len() == 1 {
            if pointer.pan {
                nav.pan(dx, dy, self.fov_y.get(), viewport_height);
            } else {
                nav.rotate(dx, dy, pointer.touch);
            }
        } else {
            // Two (or more) fingers: the centroid pans, the spread zooms.
            let (center, spread) = pointers.centroid_and_spread();
            nav.pan(center[0] - before.0[0], center[1] - before.0[1], self.fov_y.get(), viewport_height);
            if before.1 > 1.0 && spread > 1.0 {
                nav.zoom(before.1 / spread);
            }
        }
    }

    fn on_pointer_up(&self, e: &PointerEvent) {
        self.pointers.borrow_mut().active.retain(|p| p.id != e.pointer_id());
    }

    fn on_wheel(&self, e: &WheelEvent) {
        if self.xr.borrow().is_some() {
            return;
        }
        e.prevent_default();
        // Lines and pages to approximate pixels.
        let unit = match e.delta_mode() {
            WheelEvent::DOM_DELTA_LINE => 16.0,
            WheelEvent::DOM_DELTA_PAGE => 400.0,
            _ => 1.0,
        };
        let delta = (e.delta_y() * unit) as f32;
        let mut nav = self.nav.borrow_mut();
        if e.ctrl_key() {
            // Trackpad pinch arrives as ctrl+wheel with small deltas: always a zoom.
            nav.zoom((delta * 0.01).clamp(-1.0, 1.0).exp());
        } else {
            nav.wheel(delta);
        }
    }

    fn on_key(&self, e: &KeyboardEvent, down: bool) {
        let Some(key) = NavKey::from_code(&e.code()) else { return };
        if down {
            if e.ctrl_key() || e.meta_key() || e.alt_key() || typing_target(e) || self.xr.borrow().is_some() {
                return;
            }
            let mut nav = self.nav.borrow_mut();
            if nav.mode() != NavMode::Fly {
                return;
            }
            nav.set_key(key, true);
            // Arrow keys and space would scroll the page.
            if key != NavKey::Fast {
                e.prevent_default();
            }
        } else {
            // Releases always go through, so that no key gets stuck.
            self.nav.borrow_mut().set_key(key, false);
        }
    }
}

/// Registers every listener of the viewer. They hold weak references and are
/// removed by `Inner::shutdown`.
pub(super) fn attach(inner: &Rc<Inner>) -> Result<(), JsValue> {
    let canvas: &EventTarget = inner.canvas.as_ref();
    let window: &EventTarget = inner.window.as_ref();
    let mut listeners = Vec::new();
    let mut listen = |target: &EventTarget, kind: &'static str, passive: bool, handler: fn(&Inner, &Event)| -> Result<(), JsValue> {
        let weak = Rc::downgrade(inner);
        listeners.push(Listener::new(target, kind, passive, move |event: Event| {
            if let Some(inner) = weak.upgrade() {
                handler(&inner, &event);
            }
        })?);
        Ok(())
    };

    listen(canvas, "pointerdown", false, |i, e| i.on_pointer_down(e.unchecked_ref()))?;
    listen(canvas, "pointermove", true, |i, e| i.on_pointer_move(e.unchecked_ref()))?;
    listen(canvas, "pointerup", true, |i, e| i.on_pointer_up(e.unchecked_ref()))?;
    listen(canvas, "pointercancel", true, |i, e| i.on_pointer_up(e.unchecked_ref()))?;
    listen(canvas, "wheel", false, |i, e| i.on_wheel(e.unchecked_ref()))?;
    // Right-drag pans: no context menu on the canvas.
    listen(canvas, "contextmenu", false, |_, e| e.prevent_default())?;
    listen(canvas, "webglcontextlost", false, |i, e| {
        // Without preventDefault the context is never restored.
        e.prevent_default();
        i.on_context_lost();
    })?;
    listen(canvas, "webglcontextrestored", true, |i, _| i.on_context_restored())?;
    listen(window, "keydown", false, |i, e| i.on_key(e.unchecked_ref(), true))?;
    listen(window, "keyup", true, |i, e| i.on_key(e.unchecked_ref(), false))?;
    listen(window, "blur", true, |i, _| {
        i.nav.borrow_mut().clear_keys();
        i.pointers.borrow_mut().reset();
    })?;

    *inner.listeners.borrow_mut() = listeners;
    Ok(())
}
