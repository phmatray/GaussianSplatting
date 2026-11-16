//! The captures the native server keeps, and what can be done with one:
//! open it again, refine it offline, export it as a dataset; and the
//! refined results (view, download, share, delete).
//!
//! All of it belongs to the native server: a refinement keeps every
//! processor core busy for minutes and its files live on the server's
//! disk. The page only asks, polls `GET /api/refine` and shows.

use leptos::prelude::*;
use leptos::task::spawn_local;

use super::{with_core, Core};
use crate::logic::files::MAX_IMPORT_BYTES;
use crate::logic::refine::{self, CaptureList, Preset, Refined, RefineAnswer, RefineState, RefinedList, Storage};
use crate::logic::share::encode_query_value;
use crate::logic::source::SourceKind;
use crate::net::{self, Body, DownloadError};
use crate::state::{ImportView, Shared, Ui};
use crate::{dom, tauri};

/// Time given to the short routes of the refinement.
const REFINE_TIMEOUT_MS: i32 = 15_000;
/// Time given to the download of a refined result (tens of megabytes from
/// the same machine or the same network).
const RESULT_TIMEOUT_MS: i32 = 10 * 60 * 1000;
/// Time given to the server to ingest a kept capture again.
const OPEN_TIMEOUT_MS: i32 = 30 * 60 * 1000;
/// Timer ticks (half seconds) between two polls of the job while one runs,
/// and while none does.
const POLL_RUNNING: u32 = 2;
const POLL_IDLE: u32 = 12;
/// The lists are read again every this many idle polls: a phone or another
/// window may have added a capture.
const LIST_EVERY: u32 = 5;

impl Core {
    /// The page talks to the native server from its own machine: the only
    /// place captures are managed from.
    fn manages_captures(&self) -> bool {
        self.ui.source.get_untracked() == Some(SourceKind::Server) && self.ui.lan_admin.get_untracked()
    }

    /// Reads the server's kept captures and refined results.
    pub fn refresh_captures(&self) {
        if self.ui.source.get_untracked() != Some(SourceKind::Server) {
            return;
        }
        // Refined results are listed for every client of the server; raw
        // captures only at the machine itself.
        if let Some(url) = self.server_url("/api/refined") {
            spawn_local(async move {
                let Ok(answer) = net::request(&url, None, 8000).await else { return };
                if let Some(list) = RefinedList::parse(&answer.body).filter(|_| answer.ok()) {
                    with_core(|core| Ui::set_if_changed(core.ui.refined, list.results));
                }
            });
        }
        if !self.manages_captures() {
            return;
        }
        let Some(url) = self.server_url("/api/captures") else { return };
        spawn_local(async move {
            let Ok(answer) = net::request(&url, None, 8000).await else { return };
            if let Some(list) = CaptureList::parse(&answer.body).filter(|_| answer.ok()) {
                with_core(|core| Ui::set_if_changed(core.ui.captures, list.captures));
            }
        });
    }

    /// Twice per second: follows the job of the server, often while one
    /// runs and now and then otherwise (another window may have started it).
    pub(super) fn tick_refine(&self) {
        if !self.manages_captures() {
            return;
        }
        let polls = self.refine_polls.get().wrapping_add(1);
        self.refine_polls.set(polls);
        let running = self.ui.job.with_untracked(Option::is_some);
        if !polls.is_multiple_of(if running { POLL_RUNNING } else { POLL_IDLE }) {
            return;
        }
        if !running && (polls / POLL_IDLE).is_multiple_of(LIST_EVERY) {
            self.refresh_captures();
        }
        self.poll_refine();
    }

    fn poll_refine(&self) {
        let Some(url) = self.server_url("/api/refine") else { return };
        spawn_local(async move {
            let Ok(answer) = net::request(&url, None, 5000).await else { return };
            let Some(state) = RefineState::parse(&answer.body).filter(|_| answer.ok()) else { return };
            with_core(|core| core.adopt_refine(state));
        });
    }

    /// Takes the server's word on its job. A job this page saw running and
    /// that is no longer there has ended: say how.
    fn adopt_refine(&self, state: RefineState) {
        let watched = self.ui.job.with_untracked(|job| job.as_ref().map(|job| job.id));
        Ui::set_if_changed(self.ui.job, state.job);
        let ended = state.last;
        if let Some(last) = &ended {
            if watched == Some(last.id) {
                let (bad, text) = last.text();
                self.notify(bad, text);
                self.refresh_captures();
            }
        }
        // The latest end stays on screen until it is dismissed.
        if self.dismissed_job.get() != ended.as_ref().map(|last| last.id) {
            Ui::set_if_changed(self.ui.job_end, ended);
        }
    }

    fn post_refine(&self, body: serde_json::Value) {
        let Some(url) = self.server_url("/api/refine") else { return };
        if self.ui.refine_busy.get_untracked() {
            return;
        }
        self.ui.refine_busy.set(true);
        spawn_local(async move {
            let answer = net::send("POST", &url, Body::Json(&body.to_string()), REFINE_TIMEOUT_MS).await;
            with_core(|core| {
                core.ui.refine_busy.set(false);
                match answer {
                    Ok(answer) => {
                        let parsed = RefineAnswer::parse(&answer.body);
                        if answer.ok() {
                            // The form closes: the job card takes its place.
                            core.ui.refining.set(None);
                            core.ui.job_end.set(None);
                        } else {
                            core.notify(true, refine::refusal_text(answer.status, &parsed.code));
                        }
                        // Job ids start again when the server does: a
                        // summary closed before must not hide the end of
                        // a new job that bears the same number.
                        if let (Some(job), Some(dismissed)) = (&parsed.job, core.dismissed_job.get()) {
                            if job.id <= dismissed && answer.ok() {
                                core.dismissed_job.set(None);
                            }
                        }
                        // A refusal names the job that is in the way.
                        if parsed.job.is_some() {
                            Ui::set_if_changed(core.ui.job, parsed.job);
                        }
                        core.poll_refine();
                    }
                    Err(error) => core.notify(true, format!("Le serveur n'a pas répondu ({error}).")),
                }
            });
        });
    }

    /// "Affiner cette capture".
    pub fn start_refine(&self, capture: &str, preset: Preset, beyond_sensor: bool) {
        self.post_refine(serde_json::json!({ "action": "start", "capture": capture, "preset": preset.id(), "beyond_sensor": beyond_sensor }));
    }

    /// Writes a capture as a dataset (`colmap` or `nerfstudio`) on the
    /// server's disk.
    pub fn export_dataset(&self, capture: &str, format: &str) {
        self.post_refine(serde_json::json!({ "action": "export", "capture": capture, "format": format }));
    }

    pub fn cancel_refine(&self) {
        self.post_refine(serde_json::json!({ "action": "cancel" }));
    }

    /// Hides the summary of the latest job.
    pub fn dismiss_job_end(&self) {
        self.dismissed_job.set(self.ui.job_end.with_untracked(|last| last.as_ref().map(|last| last.id)));
        self.ui.job_end.set(None);
    }

    fn capture_url(&self, file: &str) -> Option<String> {
        self.server_url(&format!("/captures/{}", encode_query_value(file)))
    }

    fn refined_url(&self, file: &str) -> Option<String> {
        self.server_url(&format!("/refined/{}", encode_query_value(file)))
    }

    /// The photograph the gallery shows a kept capture by.
    pub fn capture_photo_url(&self, file: &str) -> Option<String> {
        // `v` changes when the server's choice of photograph does.
        self.server_url(&format!("/captures/{}/photo?v=2", encode_query_value(file)))
    }

    /// Shows a kept capture again: the server ingests its file once more
    /// and every viewer is switched to it.
    pub fn open_capture(&self, file: &str) {
        let Some(url) = self.server_url("/api/captures/open") else { return };
        if self.ui.import.with_untracked(|i| i.as_ref().is_some_and(|i| !i.progress.done)) {
            self.notify(true, "Un import est déjà en cours.".to_string());
            return;
        }
        if self.is_replay() {
            self.close_replay();
        }
        if self.is_file_mode() {
            self.return_to_live();
        }
        let size = self.ui.captures.with_untracked(|list| list.iter().find(|c| c.file == file).map_or(0, |c| c.size));
        let progress = gs_worker::CaptureProgress { total: size, ..gs_worker::CaptureProgress::default() };
        self.ui.import.set(Some(ImportView { file: file.to_string(), upload: true, progress }));
        let body = serde_json::json!({ "file": file }).to_string();
        spawn_local(async move {
            let answer = net::send("POST", &url, Body::Json(&body), OPEN_TIMEOUT_MS).await;
            with_core(|core| core.on_upload_answer(size, answer));
        });
    }

    /// Deletes a kept capture from the server's disk.
    pub fn delete_capture(&self, file: &str) {
        let Some(url) = self.capture_url(file) else { return };
        let file = file.to_string();
        spawn_local(async move {
            let answer = net::send("DELETE", &url, Body::None, REFINE_TIMEOUT_MS).await;
            with_core(|core| {
                match answer {
                    Ok(answer) if answer.ok() || answer.status == 404 => {
                        core.ui.captures.update(|list| list.retain(|c| c.file != file));
                        if core.ui.refining.with_untracked(|open| open.as_deref() == Some(file.as_str())) {
                            core.ui.refining.set(None);
                        }
                    }
                    // A capture stays the server's for as long as it is on
                    // show, also after its phone has finished.
                    Ok(answer) if answer.status == 409 => core.notify(
                        true,
                        "Cette capture est encore ouverte sur le serveur (en cours, ou affichée après sa fin). Revenez à une démonstration avec les boutons du haut, puis supprimez-la.".to_string(),
                    ),
                    Ok(answer) if answer.status == 403 => core.notify(true, "Les captures se suppriment depuis l'ordinateur qui exécute Splat Live.".to_string()),
                    Ok(answer) => core.notify(true, format!("Le serveur a refusé la suppression (HTTP {}).", answer.status)),
                    Err(error) => core.notify(true, format!("Le serveur n'a pas répondu ({error}).")),
                }
                core.refresh_captures();
            });
        });
    }

    /// "Voir le résultat": downloads a refined result and shows it as an
    /// imported file (drawn sorted, as the optimiser composed it).
    pub fn view_refined(&self, file: &str) {
        let Some(url) = self.refined_url(file) else { return };
        let listed = self.ui.refined.with_untracked(|list| list.iter().find(|r| r.file == file).cloned());
        let (room, start) = listed.map_or((false, None), |result| (result.is_room(), result.start_view()));
        if self.ui.refined_loading.get_untracked().is_some() {
            return;
        }
        let name = file.to_string();
        self.ui.refined_loading.set(Some(name.clone()));
        spawn_local(async move {
            let bytes = net::download(&url, MAX_IMPORT_BYTES, RESULT_TIMEOUT_MS).await;
            with_core(|core| {
                core.ui.refined_loading.set(None);
                match bytes {
                    Ok(bytes) => {
                        core.import_bytes(&name, &bytes);
                        // A room is looked at from inside, from where the
                        // phone started when the server says where that
                        // was; an object or a person from around.
                        if room {
                            core.walk_into_file();
                            if let Some((eye, target)) = start {
                                core.viewer.set_view(glam::Vec3::from_array(eye), glam::Vec3::from_array(target));
                            }
                        }
                    }
                    Err(DownloadError::Status(404)) => {
                        core.notify(true, "Ce résultat n'existe plus sur le serveur.".to_string());
                        core.refresh_captures();
                    }
                    Err(DownloadError::Status(status)) => core.notify(true, format!("Le serveur n'a pas donné le résultat (HTTP {status}).")),
                    Err(DownloadError::TooLarge) => core.notify(true, "Ce résultat est trop volumineux pour être affiché dans cette page.".to_string()),
                    Err(DownloadError::Network(reason)) => core.notify(true, format!("Le résultat n'a pas pu être téléchargé ({reason}).")),
                }
            });
        });
    }

    /// Saves the `.ply` of a refined result as a file of the user's.
    pub fn download_refined(&self, result: &Refined) {
        if tauri::is_tauri() {
            self.desktop_export(result.stem().to_string(), "refined");
            return;
        }
        let Some(url) = self.refined_url(&result.file) else { return };
        if let Err(error) = dom::download_url(&result.file, &format!("{url}?download")) {
            self.notify(true, format!("Le téléchargement a échoué\u{a0}: {error}."));
        }
    }

    pub fn delete_refined(&self, file: &str) {
        let Some(url) = self.refined_url(file) else { return };
        let file = file.to_string();
        spawn_local(async move {
            let answer = net::send("DELETE", &url, Body::None, REFINE_TIMEOUT_MS).await;
            with_core(|core| {
                match answer {
                    Ok(answer) if answer.ok() || answer.status == 404 => {
                        core.ui.refined.update(|list| list.retain(|r| r.file != file));
                        // Its summary and its share dialog go with it.
                        if core.ui.job_end.with_untracked(|last| last.as_ref().and_then(|last| last.result.as_ref()).is_some_and(|r| r.file == file)) {
                            core.ui.job_end.set(None);
                        }
                        if core.ui.share.with_untracked(|shared| matches!(shared, Some(Shared::Refined(r)) if r.file == file)) {
                            core.ui.share.set(None);
                        }
                    }
                    Ok(answer) if answer.status == 403 => core.notify(true, "Les résultats se suppriment depuis l'ordinateur qui exécute Splat Live.".to_string()),
                    Ok(answer) => core.notify(true, format!("Le serveur a refusé la suppression (HTTP {}).", answer.status)),
                    Err(error) => core.notify(true, format!("Le serveur n'a pas répondu ({error}).")),
                }
                core.refresh_captures();
            });
        });
    }

    /// Opens the share dialog of a refined result.
    pub fn share_refined(&self, result: &Refined) {
        self.ui.share.set(Some(Shared::Refined(result.clone())));
    }

    /// Address of the share archive of a refined result.
    pub fn refined_zip_url(&self, result: &Refined) -> Option<String> {
        self.server_url(&format!("/api/share/refined/{}.zip", encode_query_value(result.stem())))
    }

    /// In the desktop shell a web view cannot download: the shell writes
    /// the file into the download folder and shows it in the file manager.
    /// `kind` is `recording`, `share`, `refined` or `refined_share`.
    pub fn desktop_export(&self, name: String, kind: &'static str) {
        self.notify(false, "Enregistrement du fichier dans le dossier Téléchargements…".to_string());
        spawn_local(async move {
            let written = tauri::export(&name, kind).await;
            match written {
                Ok(path) => {
                    with_core(|core| core.notify(false, format!("Fichier enregistré\u{a0}: {path}")));
                    // Best effort: the path is on screen either way.
                    let _ = tauri::reveal(&path).await;
                }
                Err(error) => with_core(|core| core.notify(true, error)),
            }
        });
    }

    /// Shows a folder of the server in the file manager (desktop shell).
    pub fn reveal_folder(&self, target: &'static str) {
        spawn_local(async move {
            if let Err(error) = tauri::reveal(target).await {
                with_core(|core| core.notify(true, error));
            }
        });
    }

    /// Takes the server's word on its disk: a warning while it is nearly
    /// full, a notice each time it refused something.
    pub(super) fn adopt_storage(&self, storage: Option<Storage>) {
        if let Some(problem) = storage.as_ref().and_then(|storage| storage.problem.as_ref()) {
            if self.problem_seen.replace(Some(problem.serial)) != Some(problem.serial) {
                self.notify(true, problem.text());
            }
        }
        Ui::set_if_changed(self.ui.storage, storage);
    }
}
