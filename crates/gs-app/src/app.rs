//! Root of the interface: the full-window canvas, the layers above it and
//! the window-level listeners (keyboard, drag and drop).

use leptos::ev;
use leptos::html;
use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::core::{self, with_core};
use crate::dom;
use crate::state::Ui;
use crate::ui::{Overlays, Panel, TopBar};

/// Below this window width the panel starts closed and becomes a sheet at
/// the bottom (the same breakpoint as in `style.css`).
const NARROW_WIDTH: f64 = 760.0;

/// True when a key press belongs to a text field.
fn typing(event: &ev::KeyboardEvent) -> bool {
    let Some(element) = event.target().and_then(|t| t.dyn_into::<web_sys::HtmlElement>().ok()) else { return false };
    if element.is_content_editable() {
        return true;
    }
    match element.tag_name().as_str() {
        "TEXTAREA" | "SELECT" => true,
        "INPUT" => {
            let kind = element.get_attribute("type").unwrap_or_default();
            !matches!(kind.as_str(), "range" | "checkbox" | "radio" | "button" | "file")
        }
        _ => false,
    }
}

fn dragged_files(event: &ev::DragEvent) -> bool {
    event.data_transfer().is_some_and(|data| data.types().iter().any(|t| t.as_string().as_deref() == Some("Files")))
}

fn install_window_listeners(ui: Ui) {
    // H hides the interface, P pauses. Never while typing, never with a
    // modifier (browser shortcuts).
    let _ = window_event_listener(ev::keydown, move |event| {
        if event.ctrl_key() || event.meta_key() || event.alt_key() || event.repeat() || typing(&event) {
            return;
        }
        match event.key().to_lowercase().as_str() {
            "h" => ui.ui_hidden.update(|hidden| *hidden = !*hidden),
            "p" => with_core(|core| core.toggle_pause()),
            "escape" => ui.ui_hidden.set(false),
            _ => {}
        }
    });

    // A slider or a switch keeps the focus after a click; the fly keys
    // would then seem dead. Pointer users get the focus back on the view.
    let _ = window_event_listener(ev::pointerup, move |event| {
        let Some(element) = event.target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()) else { return };
        if element.closest("input[type=range], .toggle, .segmented, .tile").ok().flatten().is_some() {
            request_animation_frame(dom::blur_active_element);
        }
    });

    // Drag and drop of a splat file anywhere on the window.
    let _ = window_event_listener(ev::dragover, move |event| {
        if dragged_files(&event) {
            event.prevent_default();
            ui.dragging.set(true);
        }
    });
    let _ = window_event_listener(ev::dragleave, move |event| {
        // Leaving the window, not moving between two elements of the page.
        if event.related_target().is_none() {
            ui.dragging.set(false);
        }
    });
    let _ = window_event_listener(ev::drop, move |event| {
        ui.dragging.set(false);
        if !dragged_files(&event) {
            return;
        }
        event.prevent_default();
        if let Some(file) = event.data_transfer().and_then(|data| data.files()).and_then(|files| files.get(0)) {
            with_core(|core| core.import_file(file));
        }
    });
}

#[component]
pub fn App() -> impl IntoView {
    let ui = Ui::new(dom::window_width() >= NARROW_WIDTH);
    provide_context(ui);
    install_window_listeners(ui);

    let canvas: NodeRef<html::Canvas> = NodeRef::new();
    let started = StoredValue::new(false);
    Effect::new(move |_| {
        if let Some(canvas) = canvas.get() {
            if !started.get_value() {
                started.set_value(true);
                core::start(canvas, ui);
            }
        }
    });

    view! {
        <div
            class="app"
            id="app"
            class:ui-hidden=move || ui.ui_hidden.get()
            class:xr=move || ui.xr_active.get()
            class:panel-open=move || ui.panel_open.get()
        >
            <div class="viewport">
                <canvas id="view" node_ref=canvas></canvas>
            </div>
            <TopBar />
            <Panel />
            <Overlays />
        </div>
    }
}
