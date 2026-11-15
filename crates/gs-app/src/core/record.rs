//! Recording: the record button, the list of finished recordings, and
//! what can be done with one (replay, download, delete, share).
//!
//! With a native server reached over loopback the server records to its
//! disk (`POST /api/record`) and keeps the files. Everywhere else the
//! worker behind the transport records in its memory and hands the file
//! over as a `Blob`, which lives as long as the page.


use gs_worker::{RecordingMeta, WorkerEvent, WorkerRequest};
use leptos::prelude::*;
use leptos::task::spawn_local;

use super::{with_core, Core};
use crate::dom;
use crate::logic::replay::{self, RecorderAnswer, RecordingList, SavedRecording, ServerRecorder};
use crate::logic::share::encode_query_value;
use crate::logic::source::SourceKind;
use crate::net::{self, Body};
use crate::state::{ImportView, RecordingView, ReplayOrigin, Shared, Take, TakePlace, Ui};
use crate::tauri;

/// Where a recording started now would be written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordTarget {
    /// The native server's recordings directory.
    Server,
    /// The memory of the worker behind the transport.
    Memory,
}

/// Time given to the recorder routes of the server.
const RECORD_TIMEOUT_MS: i32 = 15_000;

impl Core {
    /// Where recording is possible right now, if anywhere.
    pub fn record_target(&self) -> Option<RecordTarget> {
        if self.is_replay() || self.is_file_mode() {
            return None;
        }
        match self.source()? {
            // The server only takes recorder commands over loopback.
            SourceKind::Server if self.ui.lan_admin.get_untracked() => Some(RecordTarget::Server),
            _ => Some(RecordTarget::Memory),
        }
    }

    fn default_take_name(&self) -> String {
        let scene = self.session.borrow().as_ref().map(|s| s.scene.clone()).unwrap_or_default();
        replay::default_name(&scene, &dom::local_iso_time())
    }

    /// The record button: starts a recording, or ends the one in progress.
    pub fn toggle_record(&self) {
        if self.ui.record_busy.get_untracked() {
            return;
        }
        // A recording in progress is stopped where it runs, whatever is on
        // screen by now (a server recording goes on during a replay).
        let running = self.ui.recording.with_untracked(|r| r.as_ref().map(|r| if r.max_bytes.is_some() { RecordTarget::Memory } else { RecordTarget::Server }));
        let recording = running.is_some();
        match running.or_else(|| self.record_target()) {
            Some(RecordTarget::Server) => self.server_record(!recording, self.default_take_name()),
            Some(RecordTarget::Memory) => {
                self.worker_record(&if recording { WorkerRequest::RecordStop } else { WorkerRequest::RecordStart { name: self.default_take_name() } });
            }
            None => {}
        }
    }

    /// Saves what is on screen now as a recording of a single moment: a
    /// finished scan, kept as a file that replays anywhere.
    pub fn save_still(&self) {
        let name = self.default_take_name();
        match self.record_target() {
            Some(RecordTarget::Server) => {
                if self.ui.recording.with_untracked(Option::is_some) || self.ui.record_busy.get_untracked() {
                    self.notify(true, "Un enregistrement est déjà en cours.".to_string());
                    return;
                }
                // A recording that is stopped right away holds the snapshot
                // it starts with, and nothing else.
                let Some(url) = self.server_url("/api/record") else { return };
                self.ui.record_busy.set(true);
                spawn_local(async move {
                    let start = serde_json::json!({ "action": "start", "name": name }).to_string();
                    let started = net::send("POST", &url, Body::Json(&start), RECORD_TIMEOUT_MS).await;
                    let stopped = match &started {
                        Ok(answer) if answer.ok() => net::send("POST", &url, Body::Json(r#"{"action":"stop"}"#), RECORD_TIMEOUT_MS).await.ok(),
                        _ => None,
                    };
                    with_core(|core| {
                        core.ui.record_busy.set(false);
                        match stopped.filter(net::Answer::ok).and_then(|answer| RecorderAnswer::parse(&answer.body)).and_then(|parsed| parsed.saved) {
                            Some(saved) => core.notify_saved(&saved),
                            None => core.notify(true, "Le serveur n'a pas pu enregistrer le scan.".to_string()),
                        }
                        core.refresh_takes();
                    });
                });
            }
            Some(RecordTarget::Memory) => self.worker_record(&WorkerRequest::RecordStill { name }),
            None => {}
        }
    }


    /// Sends a recorder request to the worker behind the transport. There
    /// is none when the page reads the server's WebSocket itself (the
    /// fallback when no worker can start): recording is then impossible.
    fn worker_record(&self, request: &WorkerRequest) {
        let sent = self.transport.borrow().as_ref().is_some_and(|transport| transport.request(request));
        if !sent {
            self.notify(true, "L'enregistrement n'est pas possible ici\u{a0}: le calcul d'arrière-plan n'a pas démarré.".to_string());
        }
    }

    // --------------------------------------------------------------- server

    fn server_record(&self, start: bool, name: String) {
        let Some(url) = self.server_url("/api/record") else { return };
        self.ui.record_busy.set(true);
        spawn_local(async move {
            let body = if start { serde_json::json!({ "action": "start", "name": name }) } else { serde_json::json!({ "action": "stop" }) }.to_string();
            let answer = net::send("POST", &url, Body::Json(&body), RECORD_TIMEOUT_MS).await;
            with_core(|core| {
                core.ui.record_busy.set(false);
                match answer {
                    // 409: it was already recording (start) or no longer
                    // recording (stop); the answer says what is true.
                    Ok(answer) if answer.ok() || answer.status == 409 => {
                        let parsed = RecorderAnswer::parse(&answer.body).unwrap_or_default();
                        core.adopt_recorder(parsed.recording, None);
                        if let Some(saved) = parsed.saved.filter(|_| answer.ok()) {
                            core.notify_saved(&saved);
                        }
                        core.refresh_takes();
                    }
                    Ok(answer) if answer.status == 403 => core.notify(true, "L'enregistrement se commande depuis l'ordinateur qui exécute Splat Live.".to_string()),
                    Ok(answer) if answer.status == 503 => core.notify(true, "Ce serveur n'a pas de dossier où écrire ses enregistrements.".to_string()),
                    Ok(answer) => core.notify(true, format!("Le serveur a refusé la demande (HTTP {}).", answer.status)),
                    Err(error) => core.notify(true, format!("Le serveur n'a pas répondu ({error}).")),
                }
            });
        });
    }

    fn notify_saved(&self, saved: &SavedRecording) {
        let title = if saved.title.is_empty() { &saved.name } else { &saved.title };
        match saved.problem() {
            Some(problem) => self.notify(true, problem.to_string()),
            None => {
                let length = replay::length_label(saved.duration_s);
                self.notify(false, format!("«\u{a0}{title}\u{a0}» est enregistré\u{a0}: {length}, {}.", replay::size(saved.size)));
            }
        }
    }

    /// Reads the server's list of recordings and its recorder state.
    pub fn refresh_takes(&self) {
        if self.ui.source.get_untracked() != Some(SourceKind::Server) {
            return;
        }
        let Some(url) = self.server_url("/api/recordings") else { return };
        spawn_local(async move {
            let Ok(answer) = net::request(&url, None, 8000).await else { return };
            if !answer.ok() {
                return;
            }
            let Ok(list) = RecordingList::parse(&answer.body) else { return };
            with_core(|core| core.set_server_takes(list));
        });
    }

    fn set_server_takes(&self, list: RecordingList) {
        let known = self.ui.takes.get_untracked();
        let mut takes: Vec<Take> = known.iter().filter(|t| t.place == TakePlace::Memory).cloned().collect();
        // The file being written is not a finished recording yet.
        for recording in list.recordings.into_iter().filter(|r| !r.recording) {
            let file = recording.file_name();
            let id = known.iter().find(|t| t.place == TakePlace::Server && t.file == file).map(|t| t.id).unwrap_or_else(|| self.next_take_id());
            takes.push(Take { id, place: TakePlace::Server, title: recording.title(), file, bytes: recording.size, duration_s: recording.duration_s, scene: recording.scene });
        }
        Ui::set_if_changed(self.ui.takes, takes);
        self.adopt_recorder(list.recording, None);
    }

    /// Takes the server's word on whether it records: this window, another
    /// window or the server itself (a full disk) may have changed it.
    /// `last` is the recording that ended most recently.
    fn adopt_recorder(&self, recorder: Option<ServerRecorder>, last: Option<SavedRecording>) {
        if self.source() != Some(SourceKind::Server) || !self.ui.lan_admin.get_untracked() {
            // Elsewhere the record button belongs to the worker's recorder.
            return;
        }
        let was_recording = self.ui.recording.with_untracked(Option::is_some);
        match recorder {
            Some(recorder) => {
                let view = RecordingView { name: recorder.title().to_string(), bytes: recorder.size, max_bytes: None, duration_s: recorder.duration_s };
                Ui::set_if_changed(self.ui.recording, Some(view));
            }
            None if was_recording => {
                self.ui.recording.set(None);
                // It ended without this window asking: say why, if it matters.
                if let Some(problem) = last.as_ref().and_then(SavedRecording::problem) {
                    self.notify(true, problem.to_string());
                }
                self.refresh_takes();
            }
            None => {}
        }
    }

    /// Twice per second: asks the server how its recorder is doing, every
    /// second while it records and now and then otherwise (another window
    /// may have started it).
    pub(super) fn tick_recording(&self) {
        if self.record_target() != Some(RecordTarget::Server) || self.ui.record_busy.get_untracked() {
            return;
        }
        let polls = self.record_polls.get().wrapping_add(1);
        self.record_polls.set(polls);
        let every = if self.ui.recording.with_untracked(Option::is_some) { 2 } else { 12 };
        if !polls.is_multiple_of(every) {
            return;
        }
        let Some(url) = self.server_url("/api/record") else { return };
        spawn_local(async move {
            let Ok(answer) = net::request(&url, None, 4000).await else { return };
            let Some(parsed) = RecorderAnswer::parse(&answer.body).filter(|_| answer.ok()) else { return };
            // The same answer tells the state of the server's disk.
            let storage = crate::logic::refine::Storage::parse(&answer.body);
            with_core(|core| {
                core.adopt_storage(storage);
                if !core.ui.record_busy.get_untracked() {
                    core.adopt_recorder(parsed.recording, parsed.last);
                }
            });
        });
    }

    // --------------------------------------------------------------- worker

    pub(super) fn on_worker_event(&self, event: WorkerEvent) {
        match event {
            WorkerEvent::Record(status) => {
                let view = status.active.then(|| RecordingView {
                    name: status.name,
                    bytes: status.bytes,
                    max_bytes: Some(status.max_bytes).filter(|max| *max > 0),
                    duration_s: status.duration_s,
                });
                Ui::set_if_changed(self.ui.recording, view);
            }
            WorkerEvent::Capture(progress) => {
                let failed = progress.error.clone();
                let finished = progress.done && self.ui.import.with_untracked(|i| i.as_ref().is_some_and(|i| !i.progress.done));
                self.ui.import.update(|import| {
                    let file = import.as_ref().map(|i| i.file.clone()).unwrap_or_default();
                    *import = Some(ImportView { file, upload: false, progress: progress.clone() });
                });
                if finished {
                    match failed {
                        Some(code) => self.notify(true, crate::logic::capture::import_error_text(&code)),
                        None => self.notify(false, crate::logic::capture::import_done_text(progress.frames, progress.used, progress.truncated)),
                    }
                }
            }
            _ => {}
        }
    }

    /// A finished in-memory recording arrived from the worker.
    pub(super) fn on_recording(&self, meta: RecordingMeta, parts: js_sys::Array) {
        let blob = match dom::blob_of(&parts) {
            Ok(blob) => blob,
            Err(error) => {
                self.notify(true, format!("L'enregistrement n'a pas pu être conservé ({error})."));
                return;
            }
        };
        let id = self.next_take_id();
        self.blobs.borrow_mut().insert(id, blob);
        let take = Take {
            id,
            place: TakePlace::Memory,
            title: meta.name.clone(),
            file: gs_worker::tape::file_name(&meta.name),
            bytes: meta.bytes,
            duration_s: meta.duration_s,
            scene: meta.scene.clone(),
        };
        self.ui.takes.update(|takes| takes.insert(0, take));
        match replay::stop_text(meta.full, meta.error.as_deref()) {
            Some(text) => self.notify(meta.error.is_some(), text.to_string()),
            None => self.notify(
                false,
                format!("«\u{a0}{}\u{a0}» est prêt\u{a0}: {}, {}. Il reste en mémoire tant que cette page est ouverte.", meta.name, replay::clock(meta.duration_s, false), replay::size(meta.bytes)),
            ),
        }
    }

    fn next_take_id(&self) -> u32 {
        let id = self.next_take.get();
        self.next_take.set(id.wrapping_add(1));
        id
    }

    // ---------------------------------------------------------------- takes

    fn take(&self, id: u32) -> Option<Take> {
        self.ui.takes.with_untracked(|takes| takes.iter().find(|t| t.id == id).cloned())
    }

    fn take_url(&self, take: &Take) -> Option<String> {
        self.server_url(&format!("/recordings/{}", encode_query_value(&take.file)))
    }

    pub fn replay_take(&self, id: u32) {
        let Some(take) = self.take(id) else { return };
        match take.place {
            TakePlace::Memory => {
                let blob = self.blobs.borrow().get(&id).cloned();
                if let Some(blob) = blob {
                    self.open_recording_blob(&take.title, blob, ReplayOrigin::Memory(id));
                }
            }
            TakePlace::Server => self.open_server_recording(&take.title, &take.file),
        }
    }

    pub fn download_take(&self, id: u32) {
        let Some(take) = self.take(id) else { return };
        let result = match take.place {
            TakePlace::Memory => match self.blobs.borrow().get(&id) {
                Some(blob) => dom::download_blob(&take.file, blob),
                None => return,
            },
            // A web view has no download manager: the desktop shell writes
            // the file into the download folder itself.
            TakePlace::Server if tauri::is_tauri() => {
                self.desktop_export(take.file.trim_end_matches(".gsrec").to_string(), "recording");
                return;
            }
            // `?download` makes the server send the file as an attachment.
            TakePlace::Server => match self.take_url(&take) {
                Some(url) => dom::download_url(&take.file, &format!("{url}?download")),
                None => return,
            },
        };
        if let Err(error) = result {
            self.notify(true, format!("Le téléchargement a échoué\u{a0}: {error}."));
        }
    }

    pub fn delete_take(&self, id: u32) {
        let Some(take) = self.take(id) else { return };
        // The recording on screen goes with its file.
        let playing = self.ui.replay.with_untracked(|view| {
            view.as_ref().is_some_and(|v| match &v.origin {
                ReplayOrigin::Memory(playing) => *playing == id,
                ReplayOrigin::Url(url) => take.place == TakePlace::Server && *url == format!("recordings/{}", take.file),
                ReplayOrigin::File => false,
            })
        });
        if playing {
            self.close_replay();
        }
        match take.place {
            TakePlace::Memory => {
                self.blobs.borrow_mut().remove(&id);
                self.ui.takes.update(|takes| takes.retain(|t| t.id != id));
            }
            TakePlace::Server => {
                let Some(url) = self.take_url(&take) else { return };
                spawn_local(async move {
                    let answer = net::send("DELETE", &url, Body::None, RECORD_TIMEOUT_MS).await;
                    with_core(|core| {
                        match answer {
                            Ok(answer) if answer.ok() || answer.status == 404 => core.ui.takes.update(|takes| takes.retain(|t| t.id != id)),
                            Ok(answer) if answer.status == 403 => {
                                core.notify(true, "Les enregistrements se suppriment depuis l'ordinateur qui exécute Splat Live.".to_string())
                            }
                            Ok(answer) if answer.status == 409 => core.notify(true, "Cet enregistrement est encore en cours d'écriture.".to_string()),
                            Ok(answer) => core.notify(true, format!("Le serveur a refusé la suppression (HTTP {}).", answer.status)),
                            Err(error) => core.notify(true, format!("Le serveur n'a pas répondu ({error}).")),
                        }
                        core.refresh_takes();
                    });
                });
            }
        }
        if self.ui.share.with_untracked(|shared| matches!(shared, Some(Shared::Take(take)) if take.id == id)) {
            self.ui.share.set(None);
        }
    }

    /// Opens the share dialog of a recording of the server.
    pub fn share_take(&self, id: u32) {
        if let Some(take) = self.take(id).filter(|t| t.place == TakePlace::Server) {
            self.ui.share.set(Some(Shared::Take(take)));
        }
    }

    /// Address of the share archive of a recording: the web build with the
    /// recording inside, to unpack onto any static host.
    pub fn share_zip_url(&self, take: &Take) -> Option<String> {
        let stem = take.file.strip_suffix(".gsrec").unwrap_or(&take.file);
        self.server_url(&format!("/api/share/{}.zip", encode_query_value(stem)))
    }
}
