//! Recording: the section of the panel with the record button and the list
//! of finished recordings, and the share dialog of a recording.

use gs_core::protocol::ScanState;
use leptos::prelude::*;
use leptos::task::spawn_local;

use super::icons::Icon;
use super::widgets::Section;
use crate::core::with_core;
use crate::dom;
use crate::logic::replay;
use crate::logic::share::{iframe_snippet, lan_share_link};
use crate::logic::source::{Link, SourceKind};
use crate::logic::qr;
use crate::state::{use_ui, Take, TakePlace, Ui};

/// True when a recording can start: a live source is on screen.
pub fn can_record(ui: Ui) -> bool {
    ui.replay.with(Option::is_none) && ui.file.with(Option::is_none) && ui.link.get() == Link::Connected && ui.scene.get().is_some()
}

/// `0:12 · 3,4 Mo` of the recording in progress.
fn progress_text(ui: Ui) -> String {
    ui.recording.with(|r| r.as_ref().map(|r| format!("{} · {}", replay::clock(r.duration_s, false), replay::size(r.bytes))).unwrap_or_default())
}

/// The record button of the top bar: a red dot, and while recording the
/// elapsed time and the size.
#[component]
pub fn RecordButton() -> impl IntoView {
    let ui = use_ui();
    let recording = move || ui.recording.with(Option::is_some);
    view! {
        <button
            type="button"
            id="btn-record"
            class="icon-btn record-btn"
            class:recording=recording
            disabled=move || ui.record_busy.get() || !(recording() || can_record(ui))
            aria-pressed=move || recording().to_string()
            title=move || if recording() { "Arrêter l'enregistrement (R)" } else { "Enregistrer (R)" }
            aria-label=move || if recording() { "Arrêter l'enregistrement" } else { "Enregistrer" }
            on:click=move |_| with_core(|core| core.toggle_record())
        >
            {move || if recording() { view! { <Icon name="stop" /> } } else { view! { <Icon name="record" /> } }}
            <Show when=recording>
                <span class="record-time" id="record-time">{move || progress_text(ui)}</span>
            </Show>
        </button>
    }
}

#[component]
fn TakeRow(take: Take) -> impl IntoView {
    let ui = use_ui();
    let id = take.id;
    let server = take.place == TakePlace::Server;
    let meta = {
        let length = replay::length_label(take.duration_s);
        let place = if server { "sur le serveur" } else { "en mémoire" };
        format!("{length} · {} · {place}", replay::size(take.bytes))
    };
    let file = take.file.clone();
    let playing = move || {
        ui.replay.with(|view| {
            view.as_ref().is_some_and(|v| match &v.origin {
                crate::state::ReplayOrigin::Memory(playing) => !server && *playing == id,
                crate::state::ReplayOrigin::Url(url) => server && url.strip_prefix("recordings/").is_some_and(|name| name == file),
                crate::state::ReplayOrigin::File => false,
            })
        })
    };
    let label = |action: &str| format!("{action} «\u{a0}{}\u{a0}»", take.title);
    let (play_label, download_label, share_label, delete_label) = (label("Relire"), label("Télécharger"), label("Partager"), label("Supprimer"));
    view! {
        <li class="take" class:playing=playing data-place=if server { "server" } else { "memory" } data-file=take.file.clone()>
            <div class="take-text">
                <span class="take-title">{take.title.clone()}</span>
                <span class="take-meta">{meta}</span>
            </div>
            <div class="take-actions">
                <button type="button" class="icon-btn take-play" title="Relire" aria-label=play_label on:click=move |_| with_core(|core| core.replay_take(id))>
                    <Icon name="play" />
                </button>
                <button
                    type="button"
                    class="icon-btn take-download"
                    title="Télécharger le fichier .gsrec"
                    aria-label=download_label
                    on:click=move |_| with_core(|core| core.download_take(id))
                >
                    <Icon name="download" />
                </button>
                <Show when=move || server && ui.lan_admin.get()>
                    <button type="button" class="icon-btn take-share" title="Partager" aria-label=share_label.clone() on:click=move |_| with_core(|core| core.share_take(id))>
                        <Icon name="send" />
                    </button>
                </Show>
                <Show when=move || !server || ui.lan_admin.get()>
                    <button
                        type="button"
                        class="icon-btn take-delete"
                        title="Supprimer"
                        aria-label=delete_label.clone()
                        on:click=move |_| with_core(|core| core.delete_take(id))
                    >
                        <Icon name="trash" />
                    </button>
                </Show>
            </div>
        </li>
    }
}

#[component]
pub fn RecordSection() -> impl IntoView {
    let ui = use_ui();
    let recording = move || ui.recording.with(Option::is_some);
    let on_server = move || ui.source.get() == Some(SourceKind::Server) && ui.lan_admin.get();
    let scan_done = move || {
        ui.capture.with(|c| c.as_ref().is_some_and(|c| c.stats.is_some_and(|s| s.finished))) || ui.scan.get().is_some_and(|s| s.state == ScanState::Done)
    };
    // Share of the memory bound an in-memory recording has used.
    let used = move || ui.recording.with(|r| r.as_ref().and_then(|r| r.max_bytes.map(|max| r.bytes as f64 / max.max(1) as f64)));
    view! {
        <Section id="section-record" title="Enregistrement" icon="film">
            <button
                type="button"
                id="record-toggle"
                class="btn wide"
                class:primary=move || !recording()
                class:danger=recording
                disabled=move || ui.record_busy.get() || !(recording() || can_record(ui))
                on:click=move |_| with_core(|core| core.toggle_record())
            >
                {move || if recording() { view! { <Icon name="stop" /> } } else { view! { <Icon name="record" /> } }}
                {move || if recording() { "Arrêter l'enregistrement" } else { "Enregistrer en 4D" }}
            </button>
            <Show when=recording>
                <div class="record-status" id="record-status" role="status">
                    <span class="record-dot" aria-hidden="true"></span>
                    <span>{move || progress_text(ui)}</span>
                </div>
                <Show when=move || used().is_some()>
                    <div
                        class="progress"
                        role="progressbar"
                        aria-label="Mémoire utilisée par l'enregistrement"
                        aria-valuemin="0"
                        aria-valuemax="100"
                        aria-valuenow=move || (used().unwrap_or(0.0).clamp(0.0, 1.0) * 100.0).floor().to_string()
                    >
                        <div class="progress-fill" style:width=move || format!("{:.1}%", used().unwrap_or(0.0).clamp(0.0, 1.0) * 100.0)></div>
                    </div>
                </Show>
            </Show>
            <p class="hint" id="record-hint">
                {move || {
                    if on_server() {
                        "L'enregistrement est écrit sur le disque de cet ordinateur. Il se relit ici, dans un casque, ou sur toute page web."
                    } else {
                        "Sans serveur natif, l'enregistrement reste dans la mémoire de cette page, jusqu'à 268 Mo, puis s'arrête de lui-même. Téléchargez-le pour le garder."
                    }
                }}
            </p>
            <Show when=move || scan_done() && can_record(ui) && !recording()>
                <button type="button" id="save-still" class="btn wide" disabled=move || ui.record_busy.get() on:click=move |_| with_core(|core| core.save_still())>
                    <Icon name="download" />
                    "Enregistrer le scan terminé"
                </button>
            </Show>
            <Show when=move || !ui.takes.with(Vec::is_empty)>
                <ul class="takes" id="takes" aria-label="Enregistrements">
                    <For each=move || ui.takes.get() key=|take| (take.id, take.bytes, take.duration_s.to_bits()) let:take>
                        <TakeRow take=take />
                    </For>
                </ul>
            </Show>
            <Show when=move || ui.takes.with(Vec::is_empty) && !recording()>
                <p class="hint" id="takes-empty">"Aucun enregistrement pour l'instant. Un fichier .gsrec peut aussi être ouvert depuis la section Fichier."</p>
            </Show>
        </Section>
    }
}

/// A read-only text with a button that copies it.
#[component]
fn CopyField(id: &'static str, label: &'static str, #[prop(into)] text: Signal<String>, #[prop(optional)] multiline: bool) -> impl IntoView {
    let copied = RwSignal::new(false);
    let copy = move |_| {
        let text = text.get_untracked();
        spawn_local(async move {
            if dom::copy_text(&text).await {
                copied.set(true);
                set_timeout(move || copied.set(false), std::time::Duration::from_secs(2));
            } else {
                with_core(|core| core.notify(true, "La copie n'est pas autorisée ici\u{a0}: sélectionnez le texte et copiez-le à la main.".to_string()));
            }
        });
    };
    let select_all = |ev: leptos::ev::FocusEvent| {
        use wasm_bindgen::JsCast;
        if let Some(target) = ev.target() {
            if let Some(input) = target.dyn_ref::<web_sys::HtmlInputElement>() {
                input.select();
            } else if let Some(area) = target.dyn_ref::<web_sys::HtmlTextAreaElement>() {
                area.select();
            }
        }
    };
    view! {
        <div class="field">
            <label class="field-label" for=id>{label}</label>
            <div class="copy-field">
                {if multiline {
                    view! { <textarea id=id readonly rows="4" spellcheck="false" prop:value=move || text.get() on:focus=select_all></textarea> }.into_any()
                } else {
                    view! { <input type="text" id=id readonly spellcheck="false" prop:value=move || text.get() on:focus=select_all /> }.into_any()
                }}
                <button type="button" id=format!("{id}-copy") class="btn" on:click=copy>
                    {move || if copied.get() { view! { <Icon name="check" /> } } else { view! { <Icon name="copy" /> } }}
                    {move || if copied.get() { "Copié" } else { "Copier" }}
                </button>
            </div>
        </div>
    }
}

/// The share dialog of a recording of the server: the link on the local
/// network with its QR code, the `<iframe>` snippet, the archive for a web
/// host.
#[component]
pub fn ShareDialog() -> impl IntoView {
    let ui = use_ui();
    let take = move || ui.share.get();
    let lan = move || ui.server.with(|s| s.as_ref().map(|s| s.lan.clone()).unwrap_or_default());
    let lan_on = move || lan().enabled && !lan().urls.is_empty();
    let link = Signal::derive(move || match (take(), lan().urls.first()) {
        (Some(take), Some(base)) => lan_share_link(base, &take.file),
        _ => String::new(),
    });
    let snippet = Signal::derive(move || iframe_snippet(&link.get()));
    let code = move || qr::data_url(&link.get());
    let zip = move || take().and_then(|take| with_core_value(|core| core.share_zip_url(&take)));
    let close = move || ui.share.set(None);
    // The dialog takes the keyboard while it is open.
    Effect::new(move |_| {
        if ui.share.with(Option::is_some) {
            request_animation_frame(|| {
                if let Some(button) = dom::element_by_id::<web_sys::HtmlElement>("share-close") {
                    let _ = button.focus();
                }
            });
        }
    });
    view! {
        <Show when=move || take().is_some()>
            <div class="modal" id="share-dialog" on:click=move |ev| {
                use wasm_bindgen::JsCast;
                // A click on the backdrop, not inside the card.
                if ev.target().and_then(|t| t.dyn_into::<web_sys::Element>().ok()).is_some_and(|e| e.id() == "share-dialog") {
                    close();
                }
            }>
                <div class="modal-card" role="dialog" aria-modal="true" aria-labelledby="share-title">
                    <header class="modal-head">
                        <h2 id="share-title">{move || take().map(|t| format!("Partager «\u{a0}{}\u{a0}»", t.title))}</h2>
                        <button type="button" id="share-close" class="icon-btn" title="Fermer" aria-label="Fermer" on:click=move |_| close()>
                            <Icon name="close" />
                        </button>
                    </header>
                    <p class="hint">
                        {move || take().map(|t| {
                            let length = replay::length_label(t.duration_s);
                            format!("{length}, {}. La visionneuse s'ouvre dans tout navigateur récent, casque compris.", replay::size(t.bytes))
                        })}
                    </p>
                    <Show
                        when=lan_on
                        fallback=move || view! {
                            <div class="share-off" id="share-off">
                                <p>"Pour donner un lien aux appareils de votre réseau, le partage sur le réseau local doit être actif."</p>
                                <button type="button" id="share-lan-on" class="btn primary" disabled=move || ui.lan_busy.get() on:click=move |_| with_core(|core| core.set_lan(true))>
                                    <Icon name="share" />
                                    "Activer le partage sur le réseau local"
                                </button>
                                <Show when=move || ui.lan_error.get().is_some()>
                                    <p class="error">{move || ui.lan_error.get()}</p>
                                </Show>
                            </div>
                        }
                    >
                        <div class="share-grid">
                            <div class="share-main">
                                <CopyField id="share-link" label="Lien sur le réseau local" text=link />
                                <p class="hint">
                                    "Le premier accès affiche un avertissement de certificat\u{a0}: l'empreinte à comparer est dans la section Casque VR."
                                </p>
                                <CopyField id="share-embed" label="Code d'intégration dans une page web" text=snippet multiline=true />
                            </div>
                            <Show when=move || code().is_some()>
                                <img id="share-qr" class="qr" alt="QR code du lien de partage" src=code />
                            </Show>
                        </div>
                    </Show>
                    <div class="field">
                        <span class="field-label">"Publier sur un site web"</span>
                        <a id="share-zip" class="btn wide" href=move || zip().unwrap_or_default() download=move || take().map(|t| format!("{}.zip", t.file.trim_end_matches(".gsrec")))>
                            <Icon name="download" />
                            "Télécharger l'archive web (.zip)"
                        </a>
                        <p class="hint">
                            "L'archive contient la visionneuse et l'enregistrement. Décompressée sur n'importe quel hébergement web statique, elle s'ouvre directement sur lui."
                        </p>
                    </div>

                </div>
            </div>
        </Show>
    }
}

/// Reads a value off the core (`None` before the viewer exists).
fn with_core_value<T>(f: impl FnOnce(&crate::core::Core) -> Option<T>) -> Option<T> {
    let mut out = None;
    with_core(|core| out = f(core));
    out
}
