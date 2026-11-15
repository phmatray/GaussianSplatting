//! Real capture in the panel: pairing an iPhone, following its capture,
//! importing a capture file; and the cards that replace the simulation's
//! controls while a capture or a recording is on screen.

use leptos::prelude::*;
use wasm_bindgen::JsCast;

use super::icons::Icon;
use super::widgets::Section;
use crate::core::with_core;
use crate::logic::capture::{self, CaptureState};
use crate::logic::replay;
use crate::logic::source::SourceKind;
use crate::logic::format;
use crate::state::use_ui;

/// What a capture session says about itself: shown in the "Capture"
/// section instead of the simulation's controls.
#[component]
pub fn CaptureCard() -> impl IntoView {
    let ui = use_ui();
    let info = move || ui.capture.with(|c| c.as_ref().map(|c| c.info.clone()).unwrap_or_default());
    let stats = move || ui.capture.with(|c| c.as_ref().and_then(|c| c.stats));
    let state = move || CaptureState::of(stats().as_ref());
    let splats = move || ui.viewer_stats.get().splats_total;
    view! {
        <div class="capture-card" id="capture-card" data-state=move || state().id() data-mode=move || info().mode.name()>
            <div class="scan-head">
                <span class="scan-label" id="capture-state">{move || state().label(info().imported)}</span>
                <span class="chip" id="capture-mode" title=move || capture::mode_hint(info().mode)>{move || capture::mode_label(info().mode)}</span>
            </div>
            <p class="capture-name" id="capture-name">{move || info().name}</p>
            <dl class="stats">
                <div class="stat">
                    <dt>"Appareil"</dt>
                    <dd id="capture-device">{move || { let device = info().device; if device.is_empty() { "-".to_string() } else { device } }}</dd>
                </div>
                <div class="stat">
                    <dt>"Images reçues"</dt>
                    <dd id="capture-received">{move || stats().map_or_else(|| "-".to_string(), |s| format::count(u64::from(s.frames_received)))}</dd>
                </div>
                <div class="stat">
                    <dt>"Images utilisées"</dt>
                    <dd id="capture-used">{move || stats().map_or_else(|| "-".to_string(), |s| format::count(u64::from(s.frames_used)))}</dd>
                </div>
                <div class="stat">
                    <dt>"Splats"</dt>
                    <dd id="capture-splats">{move || format::count(u64::from(splats()))}</dd>
                </div>
            </dl>
            <Show when=move || stats().as_ref().and_then(capture::tracking_text).is_some()>
                <p class="warning" id="capture-tracking" role="status">{move || stats().as_ref().and_then(capture::tracking_text)}</p>
            </Show>
            <Show when=move || stats().is_some_and(|s| s.full)>
                <p class="warning" id="capture-full" role="status">
                    "Capacité atteinte\u{a0}: la reconstruction garde un million de cellules. Les nouvelles surfaces ne sont plus ajoutées, celles qui existent continuent de s'affiner."
                </p>
            </Show>
            <Show when=move || !info().floor_known && stats().is_some_and(|s| s.frames_used > 0 && !s.finished)>
                <p class="hint">"Le sol n'est pas encore repéré\u{a0}: la hauteur de la scène sera corrigée quand il le sera."</p>
            </Show>
            <p class="hint">{move || capture::mode_hint(info().mode)}</p>
        </div>
    }
}

/// What a recording says about itself: shown in the "Capture" section
/// while it is replayed.
#[component]
pub fn ReplayCard() -> impl IntoView {
    let ui = use_ui();
    let view = move || ui.replay.get();
    let created = move || {
        view().map(|v| v.status.created).filter(|c| c.len() >= 16 && c.is_ascii()).map(|c| format!("Enregistré le {} à {}h{} (UTC)", &c[0..10], &c[11..13], &c[14..16]))
    };
    view! {
        <div class="file-card" id="replay-card">
            <p class="file-name" id="replay-name">{move || view().map(|v| v.name().to_string())}</p>
            <p class="hint" id="replay-meta">
                {move || {
                    view().map(|v| match v.status.duration {
                        Some(duration) if duration >= replay::STILL_S => format!("Enregistrement de {}.", replay::spoken(duration)),
                        _ if v.status.ready => "Image fixe.".to_string(),
                        _ => "Chargement…".to_string(),
                    })
                }}
            </p>
            <Show when=move || created().is_some()>
                <p class="hint">{created}</p>
            </Show>
            <button type="button" id="replay-close" class="btn primary wide" on:click=move |_| with_core(|core| core.close_replay())>
                <Icon name="back" />
                "Revenir au direct"
            </button>
        </div>
        <p class="hint">
            "Lecture\u{a0}: P ou K. Reculer et avancer\u{a0}: J et L (5 secondes), virgule et point (un vingtième de seconde). Début et fin\u{a0}: touches Début et Fin. Boucle\u{a0}: B."
        </p>
    }
}

#[component]
pub fn PhoneSection() -> impl IntoView {
    let ui = use_ui();
    let server = move || ui.source.get() == Some(SourceKind::Server);
    let admin = move || server() && ui.lan_admin.get();
    let pair = move || ui.pair.get();
    let import = move || ui.import.get();
    let importing = move || import().is_some_and(|i| !i.progress.done);
    let fraction = move || import().map_or(0.0, |i| if i.progress.total > 0 { i.progress.bytes as f64 / i.progress.total as f64 } else { 0.0 });
    let pick = move |ev: leptos::ev::Event| {
        let Some(input) = ev.target().and_then(|t| t.dyn_into::<web_sys::HtmlInputElement>().ok()) else { return };
        if let Some(file) = input.files().and_then(|list| list.get(0)) {
            with_core(|core| core.import_capture(file));
        }
        // The same file can be picked again.
        input.set_value("");
    };
    view! {
        <Section id="section-phone" title="iPhone" icon="phone">
            <p class="section-lead">"Capturer avec un iPhone"</p>
            <Show when=admin>
                <ol class="steps" id="pair-steps">
                    <li>"Installez l'application Splat Capture sur un iPhone ou un iPad Pro (capteur LiDAR), relié au même réseau Wi-Fi que cet ordinateur."</li>
                    <li>"Affichez le code d'appairage ci-dessous et scannez-le depuis l'application."</li>
                    <li>"Choisissez un mode (pièce, objet, personne ou direct) et filmez\u{a0}: la reconstruction apparaît ici au fur et à mesure."</li>
                </ol>
                <Show
                    when=move || pair().qr.is_some()
                    fallback=move || view! {
                        <button type="button" id="pair-start" class="btn primary wide" disabled=move || pair().busy on:click=move |_| with_core(|core| core.pair_phone())>
                            <Icon name="phone" />
                            {move || if pair().busy { "Préparation du code…" } else { "Afficher le code d'appairage" }}
                        </button>
                        <Show when=move || !ui.server.with(|s| s.as_ref().is_some_and(|s| s.lan.enabled))>
                            <p class="hint">"Le partage sur le réseau local sera activé\u{a0}: le téléphone joint l'ordinateur par ce réseau."</p>
                        </Show>
                    }
                >
                    <div class="pair" id="pair">
                        <img id="pair-qr" class="qr" alt="QR code d'appairage à scanner avec Splat Capture" src=move || pair().qr />
                        <p class="hint">
                            "Ce code donne accès à la capture sur cet ordinateur\u{a0}: ne le montrez qu'à vos appareils. Il change à chaque démarrage de Splat Live."
                        </p>
                        <button type="button" id="pair-hide" class="btn wide" on:click=move |_| with_core(|core| core.hide_pairing())>"Masquer le code"</button>
                    </div>
                </Show>
                <Show when=move || pair().error.is_some()>
                    <p class="error" id="pair-error" role="alert">{move || pair().error}</p>
                </Show>
            </Show>
            <Show when=move || server() && !ui.lan_admin.get()>
                <p class="hint" id="pair-remote">"L'appairage d'un iPhone se fait sur l'ordinateur qui exécute Splat Live. La capture en cours s'affiche ici comme sur ses autres écrans."</p>
            </Show>
            <Show when=move || ui.source.get() == Some(SourceKind::Worker)>
                <p class="hint" id="pair-worker">
                    "La capture en direct demande l'application de bureau Splat Live. Sans elle, l'iPhone enregistre un fichier .gscap, que cette page reconstruit toute seule."
                </p>
            </Show>
            <div class="field">
                <span class="field-label">"Capture enregistrée sur l'iPhone"</span>
                <label class="btn wide" class:disabled=importing id="gscap-btn" for="gscap-input">
                    <Icon name="upload" />
                    "Importer un fichier .gscap"
                </label>
                <input type="file" id="gscap-input" class="visually-hidden" accept=".gscap" disabled=importing on:change=pick />
            </div>
            <Show when=move || import().is_some()>
                <div class="import" id="import" data-done=move || import().is_some_and(|i| i.progress.done).to_string()>
                    <p class="file-name">{move || import().map(|i| if i.progress.name.is_empty() { i.file } else { format!("{} ({})", i.progress.name, i.file) })}</p>
                    <Show when=importing>
                        <div
                            class="progress"
                            class:indeterminate=move || import().is_some_and(|i| i.upload)
                            role="progressbar"
                            aria-label="Avancement de l'import"
                            aria-valuemin="0"
                            aria-valuemax="100"
                            aria-valuenow=move || (fraction().clamp(0.0, 1.0) * 100.0).floor().to_string()
                        >
                            <div class="progress-fill" style:width=move || format!("{:.1}%", fraction().clamp(0.0, 1.0) * 100.0)></div>
                        </div>
                    </Show>
                    <p class="hint" id="import-status" role="status">
                        {move || {
                            import().map(|i| {
                                let p = &i.progress;
                                if let Some(error) = &p.error {
                                    if i.upload { error.clone() } else { capture::import_error_text(error) }
                                } else if i.upload && !p.done {
                                    format!("Envoi au serveur et reconstruction ({})…", replay::size(p.total))
                                } else if p.done {
                                    capture::import_done_text(p.frames, p.used, p.truncated)
                                } else {
                                    format!("{}, {} sur {}.", capture::frames_text(p.frames, p.used).replace("reçue", "lue"), replay::size(p.bytes), replay::size(p.total))
                                }
                            })
                        }}
                    </p>
                    <Show when=move || importing() && import().is_some_and(|i| !i.upload)>
                        <button type="button" id="import-cancel" class="btn wide" on:click=move |_| with_core(|core| core.cancel_import())>"Arrêter l'import"</button>
                    </Show>
                    <Show when=move || !importing()>
                        <button type="button" id="import-dismiss" class="btn wide" on:click=move |_| with_core(|core| core.dismiss_import())>"Fermer"</button>
                    </Show>
                </div>
            </Show>
        </Section>
    }
}
