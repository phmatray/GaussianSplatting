//! The top bar: title, mode switch, source badge, pause and VR.

use gs_core::scene::SceneKind;
use leptos::prelude::*;

use super::icons::Icon;
use crate::core::with_core;
use crate::logic::source::Link;
use crate::state::use_ui;

#[component]
fn ModeButton(id: &'static str, scene: SceneKind, label: &'static str, short: &'static str, icon: &'static str) -> impl IntoView {
    let ui = use_ui();
    // The mode the user asked for wins until the pipeline confirms it.
    let active = move || ui.pending_scene.get().or_else(|| ui.scene.get()) == Some(scene);
    view! {
        <button
            type="button"
            role="tab"
            id=id
            class="mode"
            class:active=active
            class:pending=move || ui.pending_scene.get() == Some(scene)
            aria-selected=move || active().to_string()
            title=label
            on:click=move |_| with_core(|core| core.set_scene(scene))
        >
            <Icon name=icon />
            <span class="mode-long">{label}</span>
            <span class="mode-short">{short}</span>
        </button>
    }
}

#[component]
fn SourceBadge() -> impl IntoView {
    let ui = use_ui();
    let name = move || ui.source.get().map_or("Recherche de la source…", |s| s.label());
    let state = move || ui.link.get().label();
    view! {
        <div
            id="source-badge"
            class="badge"
            data-link=move || ui.link.get().id()
            data-source=move || ui.source.get().map_or("", |s| s.id())
            title=move || {
                let state = state();
                if state.is_empty() { format!("Source: {}", name()) } else { format!("Source: {} ({state})", name()) }
            }
        >
            <span class="badge-dot" aria-hidden="true"></span>
            <span class="badge-name">{name}</span>
            <Show when=move || !state().is_empty()>
                <span class="badge-state">{state}</span>
            </Show>
        </div>
    }
}

#[component]
pub fn TopBar() -> impl IntoView {
    let ui = use_ui();
    let paused = move || ui.config.get().is_some_and(|c| c.paused);
    let can_pause = move || ui.config.get().is_some() && ui.file.get().is_none() && ui.link.get() == Link::Connected;
    let vr_ready = move || ui.xr_supported.get() == Some(true);
    view! {
        <header class="topbar">
            <div class="brand">
                <span class="logo" aria-hidden="true"><i></i><i></i><i></i></span>
                <h1>"Splat Live"</h1>
            </div>
            <nav class="modes" role="tablist" aria-label="Mode">
                <ModeButton id="mode-stage" scene=SceneKind::Stage label="Scène en direct" short="Scène" icon="stage" />
                <ModeButton id="mode-house" scene=SceneKind::House label="Scan de maison" short="Maison" icon="house" />
            </nav>
            <div class="actions">
                <SourceBadge />
                <button
                    type="button"
                    id="btn-pause"
                    class="icon-btn"
                    class:active=paused
                    disabled=move || !can_pause()
                    aria-pressed=move || paused().to_string()
                    title=move || if paused() { "Reprendre (P)" } else { "Mettre en pause (P)" }
                    on:click=move |_| with_core(|core| core.toggle_pause())
                >
                    {move || if paused() { view! { <Icon name="play" /> } } else { view! { <Icon name="pause" /> } }}
                </button>
                <button
                    type="button"
                    id="btn-vr"
                    class="btn vr-btn"
                    class:ready=vr_ready
                    title=move || if vr_ready() { "Entrer en réalité virtuelle" } else { "Réalité virtuelle: voir le panneau Casque VR" }
                    // Synchronous on purpose: the browser only grants an
                    // immersive session inside the click itself.
                    on:click=move |_| {
                        if vr_ready() {
                            with_core(|core| core.enter_vr());
                        } else {
                            ui.panel_open.set(true);
                            if let Some(section) = crate::dom::element_by_id::<web_sys::Element>("section-vr") {
                                section.scroll_into_view();
                            }
                        }
                    }
                >
                    <Icon name="vr" />
                    <span>"VR"</span>
                </button>
                <button
                    type="button"
                    id="btn-panel"
                    class="icon-btn"
                    class:active=move || ui.panel_open.get()
                    aria-pressed=move || ui.panel_open.get().to_string()
                    title="Afficher ou masquer les réglages"
                    on:click=move |_| ui.panel_open.update(|open| *open = !*open)
                >
                    <Icon name="panel" />
                </button>
            </div>
        </header>
    }
}
