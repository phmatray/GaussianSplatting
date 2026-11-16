//! The top bar: title, mode switch, source badge, pause and VR.

use gs_core::scene::SceneKind;
use leptos::prelude::*;

use super::icons::Icon;
use super::takes::RecordButton;
use crate::core::with_core;
use crate::logic::source::Link;
use crate::state::{use_ui, Workspace};

#[component]
fn ModeButton(id: &'static str, scene: SceneKind, label: &'static str, short: &'static str, icon: &'static str) -> impl IntoView {
    let ui = use_ui();
    // The mode the user asked for wins until the pipeline confirms it. A
    // recording or a capture of the phone is neither mode.
    let other = move || ui.replay.with(Option::is_some) || ui.capture.with(Option::is_some);
    let active = move || match ui.pending_scene.get() {
        Some(pending) => pending == scene,
        None => !other() && ui.scene.get() == Some(scene),
    };
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
    let replay = move || ui.replay.with(|view| view.as_ref().map(|v| (v.status.ready, v.status.error.is_some())));
    let name = move || match (replay(), ui.source.get()) {
        (Some(_), _) => "Enregistrement",
        (None, Some(source)) => source.label(),
        (None, None) => "Recherche de la source…",
    };
    let state = move || match replay() {
        Some((_, true)) => "Illisible",
        Some((false, _)) => "Chargement…",
        Some((true, _)) => "",
        None => ui.link.get().label(),
    };
    let link_id = move || match replay() {
        Some((_, true)) => "failed",
        Some((false, _)) => "connecting",
        Some((true, _)) => "connected",
        None => ui.link.get().id(),
    };
    view! {
        <div
            id="source-badge"
            class="badge"
            data-link=link_id
            data-source=move || if replay().is_some() { "replay" } else { ui.source.get().map_or("", |s| s.id()) }
            title=move || {
                let state = state();
                if state.is_empty() { format!("Source\u{a0}: {}", name()) } else { format!("Source\u{a0}: {} ({state})", name()) }
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

/// Direct (the live capture) or Pièces (the gallery of filmed rooms).
#[component]
fn WorkspaceTabs() -> impl IntoView {
    let ui = use_ui();
    let tab = move |workspace: Workspace, id: &'static str, label: &'static str, icon: &'static str| {
        let on = move || ui.workspace.get() == workspace;
        view! {
            <button
                type="button"
                id=id
                role="tab"
                class="ws-tab"
                class:active=on
                aria-selected=move || on().to_string()
                on:click=move |_| {
                    if ui.workspace.get_untracked() == workspace {
                        // A second click on Pièces goes back to the gallery.
                        if workspace == Workspace::Rooms {
                            ui.room.set(None);
                        }
                        return;
                    }
                    ui.workspace.set(workspace);
                    if workspace == Workspace::Direct && ui.version.get_untracked().is_some() {
                        super::rooms::leave_versions(ui);
                    }
                }
            >
                <Icon name=icon />
                <span>{label}</span>
            </button>
        }
    };
    view! {
        <nav class="workspaces" role="tablist" aria-label="Espace de travail">
            {tab(Workspace::Direct, "ws-direct", "Direct", "capture")}
            {tab(Workspace::Rooms, "ws-rooms", "Pièces", "layers")}
        </nav>
    }
}

/// Where the gallery is: Pièces, then the open room.
#[component]
fn RoomCrumb() -> impl IntoView {
    let ui = use_ui();
    let title = move || {
        let key = ui.room.get()?;
        let from_captures = ui.captures.with(|list| list.iter().find(|c| c.file == key).map(|c| c.title.clone()));
        from_captures.or_else(|| key.strip_prefix("titre:").map(str::to_string))
    };
    view! {
        <nav class="crumb" aria-label="Fil d'Ariane">
            <Show when=move || title().is_some()>
                <button type="button" class="crumb-link" on:click=move |_| ui.room.set(None)>"Pièces"</button>
                <Icon name="chevron" />
                <span class="crumb-here" aria-current="page">{title}</span>
            </Show>
        </nav>
    }
}

#[component]
pub fn TopBar() -> impl IntoView {
    let ui = use_ui();
    // In a replay the button is the play head's, like the P key.
    let replay = move || ui.replay.with(|view| view.as_ref().map(|v| (v.status.playing, v.status.ready)));
    let paused = move || match replay() {
        Some((playing, _)) => !playing,
        None => ui.config.get().is_some_and(|c| c.paused),
    };
    // A capture is paused on the phone: the server ignores the viewers'
    // controls while one is shown, so the button would do nothing.
    let capture = move || replay().is_none() && ui.capture.with(Option::is_some);
    let can_pause = move || match replay() {
        Some((_, ready)) => ready,
        None => ui.config.get().is_some() && ui.file.get().is_none() && ui.link.get() == Link::Connected && !capture(),
    };
    let vr_ready = move || ui.xr_supported.get() == Some(true);
    view! {
        <header class="topbar">
            <div class="brand">
                <span class="logo" aria-hidden="true"><i></i><i></i><i></i></span>
                <h1>"Splat Live"</h1>
                <WorkspaceTabs />
            </div>
            <Show
                when=move || ui.workspace.get() == Workspace::Direct
                fallback=move || view! { <RoomCrumb /> }
            >
                <nav class="modes" role="tablist" aria-label="Scène de démonstration">
                    <ModeButton id="mode-stage" scene=SceneKind::Stage label="Scène en direct" short="Scène" icon="stage" />
                    <ModeButton id="mode-house" scene=SceneKind::House label="Scan de maison" short="Maison" icon="house" />
                </nav>
            </Show>
            <div class="actions">
                <SourceBadge />
                <RecordButton />
                <button
                    type="button"
                    id="btn-pause"
                    class="icon-btn"
                    hidden=capture
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
                    title=move || if vr_ready() { "Entrer en réalité virtuelle" } else { "Réalité virtuelle\u{a0}: voir le panneau Casque VR" }
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
