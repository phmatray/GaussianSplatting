//! The "Captures" section of the panel: the raw captures the native server
//! keeps, "Affiner cette capture" with its progress, and the refined
//! results.

use leptos::prelude::*;

use super::icons::Icon;
use super::takes::CopyField;
use super::widgets::{Section, Segmented};
use crate::core::with_core;
use crate::logic::refine::{self, KeptCapture, Preset, Refined};
use crate::logic::source::SourceKind;
use crate::state::use_ui;
use crate::tauri;

/// How long a bin stays armed after the first click.
const DELETE_CONFIRM: std::time::Duration = std::time::Duration::from_secs(4);

/// A bin that asks once more: the first click arms it for a few seconds,
/// the second one deletes.
#[component]
fn ConfirmDelete(class: &'static str, what: String, on_delete: Callback<()>) -> impl IntoView {
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
fn RefineForm(file: String) -> impl IntoView {
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

#[component]
fn CaptureRow(capture: KeptCapture) -> impl IntoView {
    let ui = use_ui();
    let file = capture.file.clone();
    let refinable = capture.refinable();
    let open_form = {
        let file = file.clone();
        Signal::derive(move || ui.refining.with(|open| open.as_deref() == Some(file.as_str())))
    };
    let toggle_form = {
        let file = file.clone();
        move |_| ui.refining.update(|open| *open = if open.as_deref() == Some(file.as_str()) { None } else { Some(file.clone()) })
    };
    let importing = move || ui.import.with(|i| i.as_ref().is_some_and(|i| !i.progress.done));
    let (open_file, delete_file, form_file) = (file.clone(), file.clone(), file.clone());
    let label = |action: &str| format!("{action} «\u{a0}{}\u{a0}»", capture.title);
    let (open_label, refine_label) = (label("Rouvrir"), label("Affiner"));
    view! {
        <li class="take capture" class:open=move || open_form.get() data-file=file data-mode=capture.mode.clone()>
            <div class="take-text">
                <span class="take-title">{capture.title.clone()}</span>
                <span class="take-meta">{capture.meta()}</span>
            </div>
            <div class="take-actions">
                <button
                    type="button"
                    class="icon-btn capture-open"
                    title="Rouvrir cette capture dans la visionneuse"
                    aria-label=open_label
                    disabled=move || capture.capturing || importing()
                    on:click=move |_| with_core(|core| core.open_capture(&open_file))
                >
                    <Icon name="eye" />
                </button>
                <Show when=move || refinable>
                    <button
                        type="button"
                        class="icon-btn capture-refine"
                        class:active=move || open_form.get()
                        title="Affiner cette capture, ou l'exporter"
                        aria-label=refine_label.clone()
                        aria-expanded=move || open_form.get().to_string()
                        on:click=toggle_form.clone()
                    >
                        <Icon name="sparkle" />
                    </button>
                </Show>
                <ConfirmDelete class="capture-delete" what=capture.title.clone() on_delete=Callback::new(move |()| with_core(|core| core.delete_capture(&delete_file))) />
            </div>
            <Show when=move || open_form.get()>
                <RefineForm file=form_file.clone() />
            </Show>
        </li>
    }
}

#[component]
fn RefinedRow(result: Refined) -> impl IntoView {
    let ui = use_ui();
    let file = result.file.clone();
    let loading = {
        let file = file.clone();
        Signal::derive(move || ui.refined_loading.with(|loading| loading.as_deref() == Some(file.as_str())))
    };
    let shown = {
        let file = file.clone();
        Signal::derive(move || ui.file.with(|shown| shown.as_ref().is_some_and(|shown| shown.name == file)))
    };
    let label = |action: &str| format!("{action} «\u{a0}{}\u{a0}»", result.title);
    let (view_label, download_label, share_label) = (label("Voir"), label("Télécharger"), label("Partager"));
    let (view_file, delete_file) = (file.clone(), file.clone());
    let (download_result, share_result) = (result.clone(), result.clone());
    view! {
        <li class="take refined" class:playing=move || shown.get() data-file=file data-preset=result.preset.clone()>
            <div class="take-text">
                <span class="take-title">{result.title.clone()}</span>
                <span class="take-meta">{result.meta()}</span>
            </div>
            <div class="take-actions">
                <button
                    type="button"
                    class="icon-btn refined-view"
                    title="Voir le résultat"
                    aria-label=view_label
                    disabled=move || ui.refined_loading.with(Option::is_some)
                    class:active=move || loading.get()
                    on:click=move |_| with_core(|core| core.view_refined(&view_file))
                >
                    <Icon name="eye" />
                </button>
                <button
                    type="button"
                    class="icon-btn refined-download"
                    title="Télécharger le fichier .ply"
                    aria-label=download_label
                    on:click=move |_| with_core(|core| core.download_refined(&download_result))
                >
                    <Icon name="download" />
                </button>
                <Show when=move || ui.lan_admin.get()>
                    <button
                        type="button"
                        class="icon-btn refined-share"
                        title="Partager"
                        aria-label=share_label.clone()
                        on:click={
                            let share_result = share_result.clone();
                            move |_| with_core(|core| core.share_refined(&share_result))
                        }
                    >
                        <Icon name="send" />
                    </button>
                    <ConfirmDelete
                        class="refined-delete"
                        what=result.title.clone()
                        on_delete={
                            let delete_file = delete_file.clone();
                            Callback::new(move |()| with_core(|core| core.delete_refined(&delete_file)))
                        }
                    />
                </Show>
            </div>
        </li>
    }
}

/// The job running on the server: what it does, how far it is, the way
/// to stop it.
#[component]
fn JobCard() -> impl IntoView {
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
fn LastJob() -> impl IntoView {
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

#[component]
pub fn CapturesSection() -> impl IntoView {
    let ui = use_ui();
    let server = move || ui.source.get() == Some(SourceKind::Server);
    let admin = move || server() && ui.lan_admin.get();
    let low = move || ui.storage.with(|storage| storage.as_ref().and_then(|storage| storage.low_text()));
    view! {
        <Section id="section-captures" title="Captures et affinage" icon="layers">
            <Show when=move || ui.source.get() == Some(SourceKind::Worker)>
                <p class="hint" id="captures-absent">{refine::ABSENT_HINT}</p>
            </Show>
            <Show when=admin>
                <Show when=move || low().is_some()>
                    <p class="warning" id="storage-low" role="status">{low}</p>
                </Show>
                <Show when=move || ui.job.with(Option::is_some)>
                    <JobCard />
                </Show>
                <Show when=move || ui.job.with(Option::is_none) && ui.job_end.with(Option::is_some)>
                    <LastJob />
                </Show>
                <p class="section-lead">"Captures conservées sur cet ordinateur"</p>
                <Show
                    when=move || !ui.captures.with(Vec::is_empty)
                    fallback=|| view! {
                        <p class="hint" id="captures-empty">
                            "Aucune capture pour l'instant. Celles d'un iPhone et les fichiers .gscap importés sont conservés ici, pour être rouverts ou affinés."
                        </p>
                    }
                >
                    <ul class="takes" id="captures" aria-label="Captures conservées">
                        <For each=move || ui.captures.get() key=|capture| (capture.file.clone(), capture.size, capture.capturing) let:capture>
                            <CaptureRow capture=capture />
                        </For>
                    </ul>
                </Show>
            </Show>
            <Show when=move || server() && !ui.lan_admin.get()>
                <p class="hint" id="captures-remote">
                    "Les captures se gèrent et s'affinent sur l'ordinateur qui exécute Splat Live. Les captures affinées s'ouvrent ici."
                </p>
            </Show>
            <Show when=move || server() && !ui.refined.with(Vec::is_empty)>
                <p class="section-lead">"Captures affinées"</p>
                <ul class="takes" id="refined" aria-label="Captures affinées">
                    <For each=move || ui.refined.get() key=|result| (result.file.clone(), result.size) let:result>
                        <RefinedRow result=result />
                    </For>
                </ul>
            </Show>
        </Section>
    }
}
