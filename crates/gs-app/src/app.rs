//! Root of the interface: the full-window canvas, the layers above it and
//! the window-level listeners (keyboard, drag and drop).

use leptos::ev;
use leptos::html;
use leptos::prelude::*;
use wasm_bindgen::JsCast;

use crate::core::{self, with_core};
use crate::dom;
use crate::logic::replay::{SKIP_S, STEP_S};
use crate::state::Ui;
use crate::ui::{Overlays, Panel, ShareDialog, Timeline, TopBar};

/// Up to this window width the panel starts closed and is a sheet at the
/// bottom (the same breakpoint as in `style.css`).
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

/// Keeps the Tab key inside the share dialog while it is open: the
/// interface behind it is not reachable by pointer either.
fn trap_tab(event: &ev::KeyboardEvent) {
    let Some(dialog) = dom::element_by_id::<web_sys::Element>("share-dialog") else { return };
    let Ok(nodes) = dialog.query_selector_all("button:not([disabled]), a[href], input:not([disabled]), textarea") else { return };
    let stops: Vec<web_sys::HtmlElement> = (0..nodes.length()).filter_map(|i| nodes.get(i)?.dyn_into().ok()).collect();
    let (Some(first), Some(last)) = (stops.first(), stops.last()) else { return };
    let active = web_sys::window().and_then(|w| w.document()).and_then(|d| d.active_element());
    let inside = active.as_ref().is_some_and(|a| dialog.contains(Some(a)));
    let on = |stop: &web_sys::HtmlElement| active.as_ref().is_some_and(|a| a.is_same_node(Some(stop)));
    let wrap_to = if event.shift_key() {
        (!inside || on(first)).then_some(last)
    } else {
        (!inside || on(last)).then_some(first)
    };
    if let Some(stop) = wrap_to {
        event.prevent_default();
        let _ = stop.focus();
    }
}

fn dragged_files(event: &ev::DragEvent) -> bool {
    event.data_transfer().is_some_and(|data| data.types().iter().any(|t| t.as_string().as_deref() == Some("Files")))
}

fn install_window_listeners(ui: Ui) {
    // H hides the interface, P pauses. Never while typing, never with a
    // modifier (browser shortcuts).
    let _ = window_event_listener(ev::keydown, move |event| {
        // The share dialog is modal: while it is open the shortcuts of the
        // interface behind it (record, pause, hide) are off.
        if ui.share.with_untracked(Option::is_some) {
            match event.key().as_str() {
                "Escape" => ui.share.set(None),
                "Tab" => trap_tab(&event),
                _ => {}
            }
            return;
        }
        if event.ctrl_key() || event.meta_key() || event.alt_key() || event.repeat() || typing(&event) {
            return;
        }
        let replay = ui.replay.with_untracked(Option::is_some);
        match event.key().to_lowercase().as_str() {
            "h" if !ui.embed.get_untracked() => ui.ui_hidden.update(|hidden| *hidden = !*hidden),
            "p" => with_core(|core| core.toggle_pause()),
            "r" if !replay => with_core(|core| core.toggle_record()),
            "escape" => ui.ui_hidden.set(false),
            // The time line of a replay. None of these is a fly key.
            "k" if replay => with_core(|core| core.replay_toggle()),
            "j" if replay => with_core(|core| core.replay_seek_by(-SKIP_S)),
            "l" if replay => with_core(|core| core.replay_seek_by(SKIP_S)),
            "," if replay => with_core(|core| core.replay_seek_by(-STEP_S)),
            "." if replay => with_core(|core| core.replay_seek_by(STEP_S)),
            "home" if replay => with_core(|core| core.replay_seek(0.0)),
            "end" if replay => with_core(|core| core.replay_end()),
            "b" if replay => with_core(|core| {
                let looping = core.ui.replay.with_untracked(|view| view.as_ref().is_some_and(|v| v.status.looping));
                core.replay_loop(!looping);
            }),
            _ => {}
        }
    });

    // A slider or a switch keeps the focus after a click; the fly keys
    // would then seem dead. Pointer users get the focus back on the view.
    let _ = window_event_listener(ev::pointerup, move |event| {
        let Some(element) = event.target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()) else { return };
        if element.closest("input[type=range], .toggle, .segmented, .tile, .tl-btn").ok().flatten().is_some() {
            request_animation_frame(dom::blur_active_element);
        }
    });

    // Full screen is entered and left asynchronously, and also by the
    // browser's own keys: the button follows the event.
    let _ = window_event_listener_untyped("fullscreenchange", move |_| ui.fullscreen.set(dom::is_fullscreen()));

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
            with_core(|core| core.open_file(file));
        }
    });
}

#[component]
pub fn App() -> impl IntoView {
    let ui = Ui::new(dom::window_width() > NARROW_WIDTH);
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
            // The embedded viewer has no panel for the overlays to keep clear of.
            class:panel-open=move || ui.panel_open.get() && !ui.embed.get()
            class:embed=move || ui.embed.get()
            class:replay=move || ui.replay.with(Option::is_some)
        >
            <div class="viewport">
                <canvas id="view" node_ref=canvas></canvas>
            </div>
            // The embedded viewer is the canvas and its small controls.
            <Show when=move || !ui.embed.get()>
                <TopBar />
                <Panel />
            </Show>
            <Overlays />
            <Timeline />
            <ShareDialog />
        </div>
    }
}
