//! The time line of a replay: play and pause, scrubbing, speed, loop,
//! current time and duration. In the embedded viewer it also carries the
//! full screen and VR buttons, and it is the only interface there is.

use leptos::prelude::*;

use super::icons::Icon;
use crate::core::with_core;
use crate::dom;
use crate::logic::replay::{self, SPEEDS};
use crate::state::{use_ui, ReplayOrigin};

/// Full screen and VR, for the embedded viewer.
#[component]
fn EmbedButtons() -> impl IntoView {
    let ui = use_ui();
    let vr_ready = move || ui.xr_supported.get() == Some(true);
    view! {
        <Show when=vr_ready>
            <button
                type="button"
                id="tl-vr"
                class="tl-btn"
                title="Entrer en réalité virtuelle"
                aria-label="Entrer en réalité virtuelle"
                // Synchronous: the browser only grants an immersive session
                // inside the click itself.
                on:click=move |_| with_core(|core| core.enter_vr())
            >
                <Icon name="vr" />
            </button>
        </Show>
        <button
            type="button"
            id="tl-fullscreen"
            class="tl-btn"
            aria-pressed=move || ui.fullscreen.get().to_string()
            title=move || if ui.fullscreen.get() { "Quitter le plein écran" } else { "Plein écran" }
            aria-label=move || if ui.fullscreen.get() { "Quitter le plein écran" } else { "Plein écran" }
            on:click=move |_| {
                // The button changes when the browser says the page did.
                if !dom::toggle_fullscreen() {
                    with_core(|core| core.notify(true, "Le plein écran n'est pas autorisé ici.".to_string()));
                }
            }
        >
            {move || if ui.fullscreen.get() { view! { <Icon name="fullscreen-exit" /> } } else { view! { <Icon name="fullscreen" /> } }}
        </button>
    }
}

#[component]
fn Transport() -> impl IntoView {
    let ui = use_ui();
    let status = move || ui.replay.with(|view| view.as_ref().map(|v| v.status.clone()).unwrap_or_default());
    let playing = move || status().playing;
    let duration = move || status().duration.unwrap_or(0.0);
    // While the user drags, the head is where they hold it.
    let time = move || ui.scrub.get().unwrap_or_else(|| status().time);
    let percent = move |t: f64| {
        let duration = duration();
        if duration > 0.0 { (t / duration * 100.0).clamp(0.0, 100.0) } else { 0.0 }
    };
    let error = move || status().error.map(|code| replay::error_text(&code));
    let state_text = move || {
        let status = status();
        if status.error.is_some() {
            ""
        } else if !status.ready {
            "Chargement…"
        } else if status.buffering {
            "Mise en mémoire tampon…"
        } else if status.damaged {
            "Enregistrement incomplet"
        } else {
            ""
        }
    };
    let title = move || ui.replay.with(|view| view.as_ref().map(|v| v.name().to_string()).unwrap_or_default());
    let can_link = move || ui.replay.with(|view| view.as_ref().is_some_and(|v| matches!(v.origin, ReplayOrigin::Url(_))));
    let tenths = move || duration() < 600.0;
    view! {
        <div class="tl-row tl-head">
            <span class="tl-title" id="tl-title" title=title>{title}</span>
            <span class="tl-state" id="tl-state" role="status">{state_text}</span>
        </div>
        <Show when=move || error().is_some()>
            <p class="error" id="tl-error" role="alert">{error}</p>
        </Show>
        <div class="tl-row">
            <button
                type="button"
                id="tl-play"
                class="tl-btn primary"
                disabled=move || !status().ready
                aria-pressed=move || playing().to_string()
                title=move || if playing() { "Mettre en pause (P ou K)" } else { "Lire (P ou K)" }
                aria-label=move || if playing() { "Mettre en pause" } else { "Lire" }
                on:click=move |_| with_core(|core| core.replay_toggle())
            >
                {move || if playing() { view! { <Icon name="pause" /> } } else { view! { <Icon name="play" /> } }}
            </button>
            <output class="tl-time" id="tl-time" for="tl-scrub">
                {move || format!("{} / {}", replay::clock(time(), tenths()), replay::clock(duration(), false))}
            </output>
            <input
                type="range"
                id="tl-scrub"
                class="tl-scrub"
                min="0"
                max=move || format!("{:.3}", duration().max(0.001))
                step="0.01"
                disabled=move || !status().ready || duration() <= 0.0
                aria-label="Position dans l'enregistrement"
                aria-valuetext=move || format!("{} sur {}", replay::spoken(time()), replay::spoken(duration()))
                style=("--played", move || format!("{:.2}%", percent(time())))
                style=("--buffered", move || format!("{:.2}%", percent(status().buffered)))
                prop:value=move || format!("{:.3}", time())
                on:input=move |ev| {
                    if let Ok(t) = event_target_value(&ev).parse::<f64>() {
                        with_core(|core| core.replay_scrub(t));
                    }
                }
                on:change=move |_| with_core(|core| core.replay_scrub_end())
            />
            <select
                id="tl-speed"
                class="tl-select"
                aria-label="Vitesse de lecture"
                title="Vitesse de lecture"
                prop:value=move || replay::speed_index(status().speed).to_string()
                on:change=move |ev| {
                    if let Some(speed) = event_target_value(&ev).parse::<usize>().ok().and_then(|i| SPEEDS.get(i)) {
                        with_core(|core| core.replay_speed(*speed));
                    }
                }
            >
                {SPEEDS
                    .iter()
                    .enumerate()
                    .map(|(i, speed)| {
                        view! {
                            <option value=i.to_string() selected=move || replay::speed_index(status().speed) == i>
                                {replay::speed_label(*speed)}
                            </option>
                        }
                    })
                    .collect_view()}
            </select>
            <button
                type="button"
                id="tl-loop"
                class="tl-btn"
                class:active=move || status().looping
                aria-pressed=move || status().looping.to_string()
                title="Lecture en boucle (B)"
                aria-label="Lecture en boucle"
                on:click=move |_| with_core(|core| core.replay_loop(!status().looping))
            >
                <Icon name="loop" />
            </button>
            <Show when=move || can_link() && !ui.embed.get()>
                <button
                    type="button"
                    id="tl-link"
                    class="tl-btn"
                    title="Copier le lien vers ce moment et ce point de vue"
                    aria-label="Copier le lien vers ce moment et ce point de vue"
                    on:click=move |_| with_core(|core| core.copy_moment_link())
                >
                    <Icon name="link" />
                </button>
            </Show>
            <Show when=move || ui.embed.get()>
                <EmbedButtons />
            </Show>
            <Show when=move || !ui.embed.get()>
                <button
                    type="button"
                    id="tl-close"
                    class="tl-btn"
                    title="Fermer l'enregistrement et revenir au direct"
                    aria-label="Fermer l'enregistrement et revenir au direct"
                    on:click=move |_| with_core(|core| core.close_replay())
                >
                    <Icon name="close" />
                </button>
            </Show>
        </div>
    }
}

/// The bar at the bottom of the view: the time line of a replay, or, in
/// the embedded viewer showing a still scene, only its buttons.
#[component]
pub fn Timeline() -> impl IntoView {
    let ui = use_ui();
    let replay = move || ui.replay.with(Option::is_some);
    view! {
        <Show when=replay>
            <div class="timeline" id="timeline" role="group" aria-label="Lecture de l'enregistrement">
                <Transport />
            </div>
        </Show>
        <Show when=move || ui.embed.get() && !replay()>
            <div class="timeline bare" id="embed-bar" role="group" aria-label="Affichage">
                <div class="tl-row">
                    <EmbedButtons />
                </div>
            </div>
        </Show>
    }
}
