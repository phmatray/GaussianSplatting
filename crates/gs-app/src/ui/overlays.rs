//! What floats over the 3D view: the scan progress, navigation hints,
//! notices, the drop target, the VR card and the fatal error screen.

use gs_core::protocol::ScanState;
use gs_core::scene::SceneKind;
use gs_render::NavMode;
use leptos::prelude::*;

use super::icons::Icon;
use crate::core::with_core;
use crate::logic::{config, format};
use crate::state::use_ui;

#[component]
pub fn Overlays() -> impl IntoView {
    let ui = use_ui();
    let house = move || ui.scene.get() == Some(SceneKind::House) && ui.file.get().is_none();
    let scan = move || ui.scan.get();
    let splats = move || {
        let stats = ui.viewer_stats.get();
        format!("{} splats", format::count(u64::from(stats.splats_total)))
    };
    view! {
        // Scan progress, in the house while there is something to follow.
        <Show when=move || house() && scan().is_some_and(|s| s.state != ScanState::Done)>
            <div class="pill" id="scan-pill" data-state=move || scan().map(|s| config::scan_id(s.state))>
                <span class="pill-dot" aria-hidden="true"></span>
                <span>{move || scan().map(|s| config::scan_label(s.state))}</span>
                <strong>{move || scan().map(|s| format::percent(s.progress))}</strong>
            </div>
        </Show>
        // The scan is done and the house is still seen from above: offer the visit.
        <Show when=move || house() && scan().is_some_and(|s| s.state == ScanState::Done) && ui.display.get().dollhouse>
            <div class="pill" id="done-pill" data-state="done">
                <Icon name="check" />
                <span>"Scan terminé"</span>
                <button type="button" id="visit-house" class="pill-action" on:click=move |_| with_core(|core| core.visit_house())>
                    "Visiter la maison"
                </button>
            </div>
        </Show>
        <Show when=move || ui.file.get().is_some()>
            <div class="pill" id="file-pill">
                <Icon name="file" />
                <span>{move || ui.file.get().map(|f| f.name)}</span>
                <button type="button" class="pill-action" on:click=move |_| with_core(|core| core.return_to_live())>"Revenir au direct"</button>
            </div>
        </Show>

        <div class="hud" id="hud">
            <span id="hud-fps">{move || format::fps(ui.viewer_stats.get().fps)}</span>
            <span id="hud-splats">{splats}</span>
            <span class="hud-nav">
                {move || match ui.display.get().nav {
                    NavMode::Orbit => "Orbite",
                    NavMode::Fly => "Vol libre",
                }}
            </span>
        </div>

        <Show when=move || ui.notice.get().is_some()>
            <div class="toast" id="toast" role="status" class:error=move || ui.notice.get().is_some_and(|n| n.error)>
                <Show when=move || ui.notice.get().is_some_and(|n| n.error)>
                    <Icon name="alert" />
                </Show>
                <span>{move || ui.notice.get().map(|n| n.text)}</span>
                <button type="button" class="toast-close" title="Fermer" on:click=move |_| ui.notice.set(None)>
                    <Icon name="close" />
                </button>
            </div>
        </Show>

        <Show when=move || ui.dragging.get()>
            <div class="drop" id="drop-overlay">
                <div class="drop-card">
                    <Icon name="upload" />
                    <p>"Déposez un fichier .ply ou .splat pour l'afficher"</p>
                </div>
            </div>
        </Show>

        // During an immersive session the page itself is not looked at; on
        // a desktop with an emulator this card is the way out.
        <Show when=move || ui.xr_active.get()>
            <div class="xr-card" id="xr-card">
                <Icon name="vr" />
                <p>"Session VR en cours"</p>
                <button type="button" id="exit-vr" class="btn primary" on:click=move |_| with_core(|core| core.exit_vr())>"Quitter la VR"</button>
            </div>
        </Show>

        <Show when=move || ui.fatal.get().is_some()>
            <div class="fatal" id="fatal" role="alert">
                <div class="fatal-card">
                    <Icon name="alert" />
                    <h2>"Splat Live ne peut pas démarrer"</h2>
                    <p>{move || ui.fatal.get()}</p>
                </div>
            </div>
        </Show>
    }
}
