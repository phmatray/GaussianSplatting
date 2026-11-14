//! The sections of the side panel.

use gs_core::protocol::{ControlMsg, PipelineConfig, ScanState};
use gs_core::scene::SceneKind;
use gs_render::NavMode;
use leptos::prelude::*;
use wasm_bindgen::JsCast;

use super::icons::Icon;
use super::widgets::{Section, Segmented, Slider, StatRow, Toggle};
use crate::core::with_core;
use crate::logic::config::{self, CAMERA_COUNT_RANGE, RESOLUTIONS, WALK_SPEED_RANGE};
use crate::logic::files::FileFormat;
use crate::logic::format;
use crate::logic::source::SourceKind;
use crate::state::{use_ui, Display, ScanView, Ui};

/// A value of the echoed configuration (of the default configuration
/// before the first State message).
fn config_value<T: Send + Sync + 'static>(ui: Ui, read: fn(&PipelineConfig) -> T) -> Signal<T> {
    Signal::derive(move || ui.config.with(|c| read(c.as_ref().unwrap_or(&PipelineConfig::default()))))
}

fn display_value<T: Send + Sync + 'static>(ui: Ui, read: fn(&Display) -> T) -> Signal<T> {
    Signal::derive(move || ui.display.with(read))
}

fn patch(value: serde_json::Value) {
    with_core(|core| core.queue_patch(value));
}

fn control(msg: ControlMsg) {
    with_core(|core| core.send(msg));
}

// ---------------------------------------------------------------- Capture

#[component]
fn StageCapture() -> impl IntoView {
    let ui = use_ui();
    let resolution_text = move || {
        ui.config.get().map_or(String::new(), |c| format!("{} × {} pixels par caméra", c.rig.width, c.rig.height))
    };
    view! {
        <Slider
            id="cam-count"
            label="Caméras"
            min=f64::from(CAMERA_COUNT_RANGE.0)
            max=f64::from(CAMERA_COUNT_RANGE.1)
            step=1.0
            value=config_value(ui, |c| f64::from(c.rig.count))
            on_input=Callback::new(|count: f64| patch(config::patch_camera_count(count.round() as u32)))
            format=|v| format!("{v:.0}")
            remote=true
        />
        <Segmented
            id="res"
            label="Résolution"
            options=RESOLUTIONS.iter().map(|r| r.label).collect()
            selected=Signal::derive(move || ui.config.with(|c| c.as_ref().and_then(config::resolution_index)))
            on_select=Callback::new(|index: usize| patch(config::patch_resolution(RESOLUTIONS[index])))
            remote=true
        />
        <p class="hint" id="res-text">{resolution_text}</p>
        <Toggle
            id="toggle-bg"
            label="Soustraction du fond"
            hint="Le décor vide est capturé une fois, seul le sujet est transmis"
            checked=config_value(ui, |c| c.background_subtraction)
            on_change=Callback::new(|on: bool| patch(config::patch_background_subtraction(on)))
            remote=true
        />
    }
}

#[component]
fn HouseCapture() -> impl IntoView {
    let ui = use_ui();
    let scan = move || ui.scan.get().unwrap_or(ScanView { state: ScanState::Idle, progress: 0.0 });
    let state = move || scan().state;
    let splats = move || ui.stats.get().map_or(0, |s| s.splats_static);
    view! {
        <div class="scan" data-state=move || config::scan_id(state())>
            <div class="scan-head">
                <span class="scan-label" id="scan-state">{move || config::scan_label(state())}</span>
                <span class="scan-percent" id="scan-progress">{move || format::percent(scan().progress)}</span>
            </div>
            <div
                class="progress"
                role="progressbar"
                aria-valuemin="0"
                aria-valuemax="100"
                aria-valuenow=move || (scan().progress.clamp(0.0, 1.0) * 100.0).floor().to_string()
            >
                <div class="progress-fill" style:width=move || format!("{:.1}%", scan().progress.clamp(0.0, 1.0) * 100.0)></div>
            </div>
            <p class="hint">{move || format!("{} splats dans la maquette", format::count(u64::from(splats())))}</p>
            <div class="button-grid">
                <button
                    type="button"
                    id="scan-start"
                    class="btn primary"
                    disabled=move || state() == ScanState::Scanning
                    on:click=move |_| control(ControlMsg::ScanStart)
                >
                    <Icon name="play" />
                    {move || match state() {
                        ScanState::Paused => "Reprendre",
                        ScanState::Done => "Nouveau scan",
                        _ => "Démarrer",
                    }}
                </button>
                <button
                    type="button"
                    id="scan-pause"
                    class="btn"
                    disabled=move || state() != ScanState::Scanning
                    on:click=move |_| control(ControlMsg::ScanPause)
                >
                    <Icon name="pause" />
                    "Pause"
                </button>
                <button
                    type="button"
                    id="scan-finish"
                    class="btn"
                    disabled=move || state() == ScanState::Done
                    title="Parcourt le reste du trajet aussi vite que possible"
                    on:click=move |_| control(ControlMsg::ScanFinish)
                >
                    <Icon name="finish" />
                    "Terminer maintenant"
                </button>
                <button
                    type="button"
                    id="scan-reset"
                    class="btn"
                    disabled=move || state() == ScanState::Idle && splats() == 0
                    title="Efface la maquette et revient au début du trajet"
                    on:click=move |_| control(ControlMsg::ScanReset)
                >
                    <Icon name="restart" />
                    "Recommencer"
                </button>
            </div>
        </div>
        <Slider
            id="walk-speed"
            label="Vitesse de marche"
            min=f64::from(WALK_SPEED_RANGE.0)
            max=f64::from(WALK_SPEED_RANGE.1)
            step=0.1
            value=config_value(ui, |c| f64::from(c.scan.speed))
            on_input=Callback::new(|speed: f64| patch(config::patch_walk_speed((speed * 10.0).round() as f32 / 10.0)))
            format=|v| format!("{} m/s", format::decimal(v as f32, 1))
            remote=true
        />
        <p class="hint">"Changer la vitesse relance le scan depuis le début."</p>
    }
}

#[component]
pub fn CaptureSection() -> impl IntoView {
    let ui = use_ui();
    let house = move || ui.scene.get() == Some(SceneKind::House);
    view! {
        <Section id="section-capture" title="Capture" icon="capture">
            <Show when=house fallback=|| view! { <StageCapture /> }>
                <HouseCapture />
            </Show>
            <Toggle
                id="toggle-noise"
                label="Bruit du capteur"
                hint="Imite une caméra de profondeur grand public"
                checked=config_value(ui, |c| !c.noise.is_none())
                on_change=Callback::new(|on: bool| patch(config::patch_noise(on)))
                remote=true
            />
            <Toggle
                id="toggle-shadows"
                label="Ombres"
                checked=config_value(ui, |c| c.shadows)
                on_change=Callback::new(|on: bool| patch(config::patch_shadows(on)))
                remote=true
            />
        </Section>
    }
}

// -------------------------------------------------------------- Affichage

fn set_display(change: impl FnOnce(&mut Display)) {
    with_core(|core| core.set_display(change));
}

#[component]
pub fn DisplaySection() -> impl IntoView {
    let ui = use_ui();
    let house = move || ui.scene.get() == Some(SceneKind::House) && ui.file.get().is_none();
    let orbit = move || ui.display.get().nav == NavMode::Orbit;
    view! {
        <Section id="section-display" title="Affichage" icon="eye">
            <Segmented
                id="nav"
                label="Navigation"
                options=vec!["Orbite", "Vol libre"]
                selected=Signal::derive(move || Some(usize::from(!orbit())))
                on_select=Callback::new(|index: usize| {
                    set_display(|d| d.nav = if index == 0 { NavMode::Orbit } else { NavMode::Fly })
                })
            />
            <p class="hint" id="nav-hint">
                {move || {
                    if orbit() {
                        "Glisser pour tourner, molette pour zoomer, clic droit pour déplacer."
                    } else {
                        "ZQSD ou WASD pour avancer, glisser pour regarder, E et A (Q) pour monter et descendre, Maj pour accélérer."
                    }
                }}
            </p>
            <Show when=house>
                <Toggle
                    id="toggle-dollhouse"
                    label="Vue maquette"
                    hint="Retire le plafond et montre la maison d'en haut"
                    checked=display_value(ui, |d| d.dollhouse)
                    on_change=Callback::new(|on: bool| set_display(|d| d.dollhouse = on))
                />
            </Show>
            <Slider
                id="splat-scale"
                label="Taille des splats"
                min=0.2
                max=2.0
                step=0.05
                value=display_value(ui, |d| f64::from(d.splat_scale))
                on_input=Callback::new(|scale: f64| set_display(|d| d.splat_scale = scale as f32))
                format=|v| format!("× {}", format::decimal(v as f32, 2))
            />
            <Toggle
                id="toggle-points"
                label="Mode points"
                hint="Chaque splat devient un point de taille fixe"
                checked=display_value(ui, |d| d.point_mode)
                on_change=Callback::new(|on: bool| set_display(|d| d.point_mode = on))
            />
            <Toggle
                id="toggle-gizmos"
                label="Repères"
                hint="Caméras, trajet du scan, grille au sol"
                checked=display_value(ui, |d| d.gizmos)
                on_change=Callback::new(|on: bool| set_display(|d| d.gizmos = on))
            />
            <Toggle
                id="toggle-static"
                label="Calque statique"
                checked=display_value(ui, |d| d.show_static)
                on_change=Callback::new(|on: bool| set_display(|d| d.show_static = on))
            />
            <Toggle
                id="toggle-dynamic"
                label="Calque dynamique"
                checked=display_value(ui, |d| d.show_dynamic)
                on_change=Callback::new(|on: bool| set_display(|d| d.show_dynamic = on))
            />
            <Show when=orbit>
                <Toggle
                    id="toggle-rotate"
                    label="Rotation automatique"
                    checked=display_value(ui, |d| d.auto_rotate)
                    on_change=Callback::new(|on: bool| set_display(|d| d.auto_rotate = on))
                />
            </Show>
        </Section>
    }
}

// ---------------------------------------------------------------- Caméras

#[component]
pub fn CamerasSection() -> impl IntoView {
    let ui = use_ui();
    let house = move || ui.scene.get() == Some(SceneKind::House);
    // New tiles start empty: draw what is already known once they exist.
    Effect::new(move |_| {
        ui.camera_ids.track();
        request_animation_frame(|| with_core(|core| core.repaint_previews()));
    });
    view! {
        <Section id="section-cameras" title="Caméras" icon="camera">
            <Segmented
                id="preview-mode"
                label="Flux"
                options=vec!["RGB", "Profondeur"]
                selected=Signal::derive(move || Some(usize::from(ui.preview_depth.get())))
                on_select=Callback::new(|index: usize| with_core(|core| core.set_preview_depth(index == 1)))
            />
            <div class="tiles" class:single=house id="tiles">
                <For each=move || ui.camera_ids.get() key=|id| *id let:id>
                    <button
                        type="button"
                        class="tile"
                        title=move || if house() { "Caméra portée à la main" } else { "Voir depuis cette caméra" }
                        on:click=move |_| with_core(|core| core.view_from_camera(id))
                    >
                        <canvas id=format!("preview-{id}") width="4" height="3"></canvas>
                        <span class="tile-label">
                            {move || if house() { "Caméra mobile".to_string() } else { format!("Cam {}", id + 1) }}
                        </span>
                    </button>
                </For>
            </div>
            <Show when=move || ui.camera_ids.with(Vec::is_empty)>
                <p class="hint">"En attente des premières images…"</p>
            </Show>
        </Section>
    }
}

// ----------------------------------------------------------- Statistiques

#[component]
pub fn StatsSection() -> impl IntoView {
    let ui = use_ui();
    let stat = move |read: fn(&gs_core::protocol::Stats) -> String| {
        Signal::derive(move || ui.stats.with(|s| s.as_ref().map_or_else(|| "-".to_string(), read)))
    };
    let transfer_label = move || if ui.source.get() == Some(SourceKind::Server) { "Réseau" } else { "Transfert" };
    view! {
        <Section id="section-stats" title="Statistiques" icon="chart">
            <dl class="stats">
                <StatRow id="stat-fps" label="Affichage" value=Signal::derive(move || format::fps(ui.viewer_stats.get().fps)) />
                <StatRow id="stat-hz" label="Pipeline" value=stat(|s| format::hz(s.tick_hz)) />
                <StatRow id="stat-capture" label="Capture" value=stat(|s| format::ms(s.capture_ms)) />
                <StatRow id="stat-recon" label="Reconstruction" value=stat(|s| format::ms(s.reconstruct_ms)) />
                <StatRow id="stat-encode" label="Encodage" value=stat(|s| format::ms(s.encode_ms)) />
                <StatRow id="stat-static" label="Splats statiques" value=stat(|s| format::count(u64::from(s.splats_static))) />
                <StatRow id="stat-dynamic" label="Splats dynamiques" value=stat(|s| format::count(u64::from(s.splats_dynamic))) />
                <div class="stat">
                    <dt>{transfer_label}</dt>
                    <dd id="stat-rate">{move || format::rate(ui.transfer_rate.get())}</dd>
                </div>
            </dl>
        </Section>
    }
}

// -------------------------------------------------------------- Casque VR

#[component]
fn LanSharing() -> impl IntoView {
    let ui = use_ui();
    let lan = move || ui.server.get().map(|s| s.lan).unwrap_or_default();
    let enabled = move || lan().enabled;
    view! {
        <Toggle
            id="lan-toggle"
            label="Partager sur le réseau local"
            hint="Ouvre un accès https pour le casque"
            checked=Signal::derive(enabled)
            disabled=Signal::derive(move || ui.lan_busy.get())
            on_change=Callback::new(|on: bool| with_core(|core| core.set_lan(on)))
        />
        <Show when=move || ui.lan_error.get().is_some()>
            <p class="error" id="lan-error">{move || ui.lan_error.get()}</p>
        </Show>
        <Show when=move || enabled() && !lan().urls.is_empty()>
            <div class="lan">
                <ul class="lan-urls" id="lan-urls">
                    <For each=move || lan().urls key=|url| url.clone() let:url>
                        <li><code>{url}</code></li>
                    </For>
                </ul>
                <Show when=move || ui.qr.get().is_some()>
                    <img id="lan-qr" class="qr" alt="QR code de l'adresse à ouvrir dans le casque" src=move || ui.qr.get() />
                </Show>
                <p class="hint">
                    "Ouvrez cette adresse dans le navigateur du casque, ou scannez le QR code, puis appuyez sur Entrer en VR."
                </p>
                <p class="hint">
                    "Le navigateur affichera un avertissement de certificat: vérifiez que l'empreinte est la même que ci-dessous, puis acceptez-le une seule fois."
                </p>
                <p class="fingerprint">
                    <span class="field-label">"Empreinte SHA-256 du certificat"</span>
                    <code id="lan-fingerprint">{move || lan().fingerprint}</code>
                </p>
            </div>
        </Show>
    }
}

#[component]
pub fn VrSection() -> impl IntoView {
    let ui = use_ui();
    let supported = move || ui.xr_supported.get();
    view! {
        <Section id="section-vr" title="Casque VR" icon="vr">
            <Show when=move || supported().is_none()>
                <p class="hint">"Recherche d'un casque…"</p>
            </Show>
            <Show when=move || supported() == Some(true)>
                <button
                    type="button"
                    id="enter-vr"
                    class="btn primary wide"
                    // Synchronous: see the VR button of the top bar.
                    on:click=move |_| with_core(|core| if ui.xr_active.get_untracked() { core.exit_vr() } else { core.enter_vr() })
                >
                    <Icon name="vr" />
                    {move || if ui.xr_active.get() { "Quitter la VR" } else { "Entrer en VR" }}
                </button>
                <dl class="controls">
                    <div><dt>"Joystick gauche"</dt><dd>"se déplacer"</dd></div>
                    <div><dt>"Joystick droit"</dt><dd>"tourner, monter et descendre"</dd></div>
                    <div><dt>"Bouton A"</dt><dd>"maquette"</dd></div>
                    <div><dt>"Bouton B"</dt><dd>"réinitialiser"</dd></div>
                    <div><dt>"Bouton X"</dt><dd>"repères"</dd></div>
                </dl>
            </Show>
            <Show when=move || ui.xr_error.get().is_some()>
                <p class="error" id="xr-error">
                    "La session VR n'a pas pu démarrer: " {move || ui.xr_error.get()}
                </p>
            </Show>
            <Show when=move || supported() == Some(false)>
                <p class="hint" id="vr-unsupported">"Ce navigateur ne peut pas ouvrir de session de réalité virtuelle."</p>
                <Show
                    when=move || ui.source.get() == Some(SourceKind::Server)
                    fallback=|| {
                        view! {
                            <p class="hint" id="vr-worker-help">
                                "Pour voir la scène en VR, ouvrez cette page depuis le navigateur du casque, en https: WebXR exige une connexion sécurisée."
                            </p>
                        }
                    }
                >
                    <LanSharing />
                </Show>
            </Show>
        </Section>
    }
}

// ---------------------------------------------------------------- Fichier

#[component]
pub fn FileSection() -> impl IntoView {
    let ui = use_ui();
    let pick = move |ev: leptos::ev::Event| {
        let Some(input) = ev.target().and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok()) else { return };
        if let Some(file) = input.files().and_then(|list| list.get(0)) {
            with_core(|core| core.import_file(file));
        }
        // The same file can be picked again.
        input.set_value("");
    };
    view! {
        <Section id="section-file" title="Fichier" icon="file">
            <Show when=move || ui.file.get().is_some()>
                <div class="file-card" id="file-card">
                    <p class="file-name">{move || ui.file.get().map(|f| f.name)}</p>
                    <p class="hint">
                        {move || ui.file.get().map(|f| format!("{} splats, le direct est en pause", format::count(f.splats as u64)))}
                    </p>
                    <button type="button" id="back-live" class="btn primary wide" on:click=move |_| with_core(|core| core.return_to_live())>
                        <Icon name="back" />
                        "Revenir au direct"
                    </button>
                </div>
            </Show>
            <div class="field">
                <span class="field-label">"Exporter la reconstruction"</span>
                <div class="button-row">
                    <button type="button" id="export-ply" class="btn" title="Format INRIA 3D Gaussian Splatting" on:click=move |_| with_core(|core| core.export(FileFormat::Ply))>
                        <Icon name="download" />
                        ".ply"
                    </button>
                    <button type="button" id="export-splat" class="btn" title="Format .splat des visionneuses web" on:click=move |_| with_core(|core| core.export(FileFormat::Splat))>
                        <Icon name="download" />
                        ".splat"
                    </button>
                </div>
            </div>
            <div class="field">
                <span class="field-label">"Importer un fichier"</span>
                <label class="btn wide" id="import-btn" for="import-input">
                    <Icon name="upload" />
                    "Choisir un .ply ou un .splat"
                </label>
                <input type="file" id="import-input" class="visually-hidden" accept=".ply,.splat" on:change=pick />
                <p class="hint">"Vous pouvez aussi glisser un fichier dans la fenêtre."</p>
            </div>
            <Show when=move || ui.file_error.get().is_some()>
                <p class="error" id="file-error">{move || ui.file_error.get()}</p>
            </Show>
        </Section>
    }
}
