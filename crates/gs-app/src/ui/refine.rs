//! The "Captures" section of the panel: the raw captures the native server
//! keeps, "Affiner cette capture" with its progress, and the refined
//! results.

use leptos::prelude::*;

use super::icons::Icon;
use super::takes::CopyField;
use super::widgets::Segmented;
use crate::core::with_core;
use crate::logic::refine::{self, Preset};
use crate::state::use_ui;
use crate::tauri;

/// How long a bin stays armed after the first click.
const DELETE_CONFIRM: std::time::Duration = std::time::Duration::from_secs(4);

/// A bin that asks once more: the first click arms it for a few seconds,
/// the second one deletes.
#[component]
pub(super) fn ConfirmDelete(class: &'static str, what: String, on_delete: Callback<()>) -> impl IntoView {
    let armed = RwSignal::new(false);
    let click = move |_| {
        if armed.get_untracked() {
            on_delete.run(());
        } else {
            armed.set(true);
            // The row may be gone by then.
            set_timeout(
                move || {
                    armed.try_set(false);
                },
                DELETE_CONFIRM,
            );
        }
    };
    let (delete_label, confirm_label) = (format!("Supprimer «\u{a0}{what}\u{a0}»"), format!("Confirmer la suppression de «\u{a0}{what}\u{a0}»"));
    view! {
        <button
            type="button"
            class=format!("icon-btn take-delete {class}")
            class:confirm=move || armed.get()
            title=move || if armed.get() { "Cliquer encore pour supprimer définitivement" } else { "Supprimer" }
            aria-label=move || if armed.get() { confirm_label.clone() } else { delete_label.clone() }
            on:click=click
        >
            <Icon name="trash" />
            <Show when=move || armed.get()>
                <span class="take-confirm">"Supprimer\u{a0}?"</span>
            </Show>
        </button>
    }
}

/// The form that starts a refinement (or a dataset export) of one capture.
#[component]
pub(super) fn RefineForm(file: String) -> impl IntoView {
    let ui = use_ui();
    let preset = RwSignal::new(0usize);
    let beyond = RwSignal::new(0usize);
    let blocked = move || ui.refine_busy.get() || ui.job.with(Option::is_some);
    let chosen = move || Preset::ALL[preset.get().min(Preset::ALL.len() - 1)];
    let (start_file, colmap_file, nerf_file) = (file.clone(), file.clone(), file);
    view! {
        <div class="refine-form" id="refine-form">
            <Segmented
                id="refine-preset"
                label="Durée et finesse"
                options=Preset::ALL.iter().map(|p| p.label()).collect()
                selected=Signal::derive(move || Some(preset.get()))
                on_select=Callback::new(move |index| preset.set(index))
            />
            <p class="hint" id="refine-preset-hint">{move || chosen().hint()}</p>
            <Segmented
                id="refine-beyond"
                label="Là où le capteur de profondeur n'a rien mesuré"
                options=vec!["Peindre d'après les photos", "Laisser les trous"]
                selected=Signal::derive(move || Some(beyond.get()))
                on_select=Callback::new(move |index| beyond.set(index))
            />
            <p class="hint">
                "Vitres, miroirs, surfaces sombres, tout ce qui est à plus de 5 mètres\u{a0}: peindre complète l'image vue depuis le trajet du téléphone, au prix d'un voile mal placé vu d'ailleurs. Lequel convient à une vraie pièce n'est pas encore connu\u{a0}: essayez les deux."
            </p>
            <button
                type="button"
                id="refine-start"
                class="btn primary wide"
                disabled=blocked
                on:click=move |_| with_core(|core| core.start_refine(&start_file, chosen(), beyond.get_untracked() == 0))
            >
                <Icon name="sparkle" />
                "Affiner cette capture"
            </button>
            <p class="hint" id="refine-honest">{refine::HONEST_HINT}</p>
            <div class="field">
                <span class="field-label">"Exporter pour un autre logiciel d'entraînement"</span>
                <div class="button-row">
                    <button type="button" id="export-colmap" class="btn" disabled=blocked on:click=move |_| with_core(|core| core.export_dataset(&colmap_file, "colmap"))>
                        "COLMAP"
                    </button>
                    <button type="button" id="export-nerfstudio" class="btn" disabled=blocked on:click=move |_| with_core(|core| core.export_dataset(&nerf_file, "nerfstudio"))>
                        "nerfstudio"
                    </button>
                </div>
                <p class="hint">"Photos, poses et nuage de départ sont écrits dans un dossier de cet ordinateur, que la suite vous indique."</p>
            </div>
        </div>
    }
}

/// The job running on the server: what it does, how far it is, the way
/// to stop it.
#[component]
pub(super) fn JobCard() -> impl IntoView {
    let ui = use_ui();
    let job = move || ui.job.get().unwrap_or_default();
    let fraction = move || job().fraction();
    let chip = move || {
        let job = job();
        match (job.preset.as_deref().and_then(Preset::from_id), job.format.as_deref()) {
            (Some(preset), _) => preset.label().to_string(),
            (None, Some("nerfstudio")) => "nerfstudio".to_string(),
            (None, Some(_)) => "COLMAP".to_string(),
            (None, None) => String::new(),
        }
    };
    view! {
        <div class="capture-card" id="refine-job" data-stage=move || job().stage data-kind=move || job().kind>
            <div class="scan-head">
                <span class="scan-label" id="refine-stage">{move || job().stage_label()}</span>
                <span class="chip" id="refine-chip">{chip}</span>
            </div>
            <p class="capture-name" id="refine-title">
                {move || {
                    let job = job();
                    if job.is_export() { format!("Export de «\u{a0}{}\u{a0}»", job.title) } else { format!("Affinage de «\u{a0}{}\u{a0}»", job.title) }
                }}
            </p>
            <div
                class="progress"
                class:indeterminate=move || fraction().is_none()
                role="progressbar"
                aria-label="Avancement du calcul"
                aria-valuemin="0"
                aria-valuemax="100"
                aria-valuenow=move || fraction().map(|f| (f * 100.0).floor().to_string())
            >
                <div class="progress-fill" style:width=move || format!("{:.1}%", fraction().unwrap_or(0.0) * 100.0)></div>
            </div>
            <p class="hint" id="refine-detail" role="status">{move || job().detail()}</p>
            <p class="hint" id="refine-clock">{move || job().clock()}</p>
            <button
                type="button"
                id="refine-cancel"
                class="btn wide"
                disabled=move || job().cancelling || ui.refine_busy.get()
                on:click=move |_| with_core(|core| core.cancel_refine())
            >
                <Icon name="stop" />
                "Annuler"
            </button>
            <Show when=move || !job().is_export()>
                <p class="hint">{refine::CANCEL_HINT}</p>
            </Show>
        </div>
    }
}

/// How the latest job ended, and what it left.
#[component]
pub(super) fn LastJob() -> impl IntoView {
    let ui = use_ui();
    let last = move || ui.job_end.get().unwrap_or_default();
    let result = move || last().result.filter(|_| last().is_done());
    let dataset = move || last().dataset.filter(|_| last().is_done());
    let dataset_dir = Signal::derive(move || dataset().map(|d| d.dir).unwrap_or_default());
    view! {
        <div class="file-card" id="refine-last" data-outcome=move || last().outcome data-kind=move || last().kind>
            <p class="hint" class:error=move || last().text().0 id="refine-summary" role="status">{move || last().text().1}</p>
            <Show when=move || result().is_some()>
                <button
                    type="button"
                    id="refine-view"
                    class="btn primary wide"
                    disabled=move || ui.refined_loading.with(Option::is_some)
                    on:click=move |_| {
                        if let Some(result) = result() {
                            with_core(|core| core.view_refined(&result.file));
                        }
                    }
                >
                    <Icon name="eye" />
                    {move || if ui.refined_loading.with(Option::is_some) { "Chargement du résultat…" } else { "Voir le résultat" }}
                </button>
                <p class="hint">
                    "Le résultat s'affiche comme un fichier importé. «\u{a0}Revenir au direct\u{a0}», dans la section Fichier, ramène à la reconstruction d'origine pour comparer."
                </p>
            </Show>
            <Show when=move || dataset().is_some()>
                <CopyField id="dataset-dir" label="Dossier du jeu de données, sur cet ordinateur" text=dataset_dir />
                <Show when=tauri::is_tauri>
                    <button type="button" id="dataset-reveal" class="btn wide" on:click=move |_| with_core(|core| core.reveal_folder("datasets"))>
                        <Icon name="folder" />
                        "Afficher le dossier"
                    </button>
                </Show>
                <p class="hint">
                    "Pour nerfstudio, gardez le repère de la capture\u{a0}: --orientation-method none --center-method none --auto-scale-poses False."
                </p>
            </Show>
            <button type="button" id="refine-dismiss" class="btn wide" on:click=move |_| with_core(|core| core.dismiss_job_end())>"Fermer"</button>
        </div>
    }
}
