//! Real capture, viewer side: pairing the phone companion with the native
//! server, importing a capture file, and opening whatever file the user
//! hands over.

use gs_worker::{CaptureProgress, WorkerRequest};
use leptos::prelude::*;
use leptos::task::spawn_local;

use super::{with_core, Core};
use crate::logic::capture::{self, UploadAnswer, MAX_CAPTURE_BYTES};
use crate::logic::qr;
use crate::logic::source::{ServerInfo, SourceKind};
use crate::net::{self, Body};
use crate::state::{ImportView, PairView, ReplayOrigin};

/// Time given to the server to ingest an uploaded capture: it answers
/// once every frame is fused.
const UPLOAD_TIMEOUT_MS: i32 = 30 * 60 * 1000;

/// What a file is, by its extension.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Opened {
    Recording,
    Capture,
    Splats,
}

fn kind_of(name: &str) -> Opened {
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".gsrec") {
        Opened::Recording
    } else if lower.ends_with(".gscap") {
        Opened::Capture
    } else {
        Opened::Splats
    }
}

impl Core {
    /// Opens a file the user picked or dropped: a recording is replayed, a
    /// capture is ingested, a `.ply` or `.splat` is shown.
    pub fn open_file(&self, file: web_sys::File) {
        match kind_of(&file.name()) {
            Opened::Recording => {
                if file.size() <= 0.0 {
                    self.notify(true, "Le fichier est vide.".to_string());
                    return;
                }
                let name = file.name();
                self.open_recording_blob(name.strip_suffix(".gsrec").unwrap_or(&name), file.into(), ReplayOrigin::File);
            }
            Opened::Capture => self.import_capture(file),
            Opened::Splats => self.import_file(file),
        }
    }

    // -------------------------------------------------------------- pairing

    /// Shows the pairing code for the phone companion, switching the LAN
    /// listener on first if needed (the phone reaches the server there).
    pub fn pair_phone(&self) {
        if self.ui.pair.with_untracked(|p| p.busy) {
            return;
        }
        let lan_on = self.ui.server.with_untracked(|s| s.as_ref().is_some_and(|s| s.lan.enabled));
        let (Some(lan_url), Some(pair_url)) = (self.server_url("/api/lan"), self.server_url("/api/pair.svg")) else { return };
        self.ui.pair.set(PairView { qr: None, busy: true, error: None });
        spawn_local(async move {
            let mut failure = None;
            if !lan_on {
                match net::request(&lan_url, Some(r#"{"enabled":true}"#), 8000).await {
                    Ok(answer) if answer.ok() => {
                        if let Ok(info) = ServerInfo::parse(&answer.body) {
                            with_core(|core| core.set_server_info(info));
                        }
                    }
                    Ok(answer) if answer.status == 403 => failure = Some("L'appairage se fait depuis l'ordinateur qui exécute Splat Live.".to_string()),
                    Ok(answer) => failure = Some(format!("Le partage sur le réseau local n'a pas pu être activé (HTTP {}).", answer.status)),
                    Err(error) => failure = Some(format!("Le serveur n'a pas répondu ({error}).")),
                }
            }
            let mut code = None;
            if failure.is_none() {
                match net::request(&pair_url, None, 8000).await {
                    Ok(answer) if answer.ok() && answer.body.contains("<svg") => code = Some(qr::svg_data_url(&answer.body)),
                    Ok(answer) if answer.status == 403 => failure = Some("L'appairage se fait depuis l'ordinateur qui exécute Splat Live.".to_string()),
                    Ok(answer) if answer.status == 404 => {
                        failure = Some("Ce serveur ne propose pas l'appairage, ou le partage sur le réseau local est coupé.".to_string())
                    }
                    Ok(answer) => failure = Some(format!("Le serveur n'a pas donné de code d'appairage (HTTP {}).", answer.status)),
                    Err(error) => failure = Some(format!("Le serveur n'a pas répondu ({error}).")),
                }
            }
            with_core(|core| core.ui.pair.set(PairView { qr: code, busy: false, error: failure }));
        });
    }

    /// Hides the pairing code (it gives access to the ingest route).
    pub fn hide_pairing(&self) {
        self.ui.pair.set(PairView::default());
    }

    // --------------------------------------------------------------- import

    /// Imports a `.gscap` capture file: the native server ingests it when
    /// there is one, otherwise the local worker does.
    pub fn import_capture(&self, file: web_sys::File) {
        let name = file.name();
        let size = file.size().max(0.0) as u64;
        if size == 0 {
            self.notify(true, "Le fichier est vide.".to_string());
            return;
        }
        if self.ui.import.with_untracked(|i| i.as_ref().is_some_and(|i| !i.progress.done)) {
            self.notify(true, "Un import est déjà en cours.".to_string());
            return;
        }
        // The capture takes the place of whatever else is shown.
        if self.is_replay() {
            self.close_replay();
        }
        if self.is_file_mode() {
            self.return_to_live();
        }
        let progress = CaptureProgress { total: size, ..CaptureProgress::default() };
        match self.source() {
            Some(SourceKind::Server) => {
                let Some(url) = self.server_url("/api/captures") else { return };
                self.ui.import.set(Some(ImportView { file: name, upload: true, progress }));
                spawn_local(async move {
                    let answer = net::send("POST", &url, Body::File(&file), UPLOAD_TIMEOUT_MS).await;
                    with_core(|core| core.on_upload_answer(size, answer));
                });
            }
            Some(SourceKind::Worker) => {
                if size > MAX_CAPTURE_BYTES {
                    self.notify(true, "Cette capture est trop volumineuse pour être importée dans le navigateur.".to_string());
                    return;
                }
                let sent = self.transport.borrow().as_ref().is_some_and(|t| t.request_with_file(&WorkerRequest::CaptureFile { name: name.clone() }, &file));
                if sent {
                    self.ui.import.set(Some(ImportView { file: name, upload: false, progress }));
                } else {
                    self.notify(true, "Le calcul local n'est pas prêt\u{a0}: réessayez dans un instant.".to_string());
                }
            }
            None => self.notify(true, "Aucune source n'est prête pour importer cette capture.".to_string()),
        }
    }

    fn on_upload_answer(&self, size: u64, answer: Result<net::Answer, String>) {
        let mut progress = CaptureProgress { total: size, bytes: size, done: true, ..CaptureProgress::default() };
        let outcome = match answer {
            Ok(answer) => {
                let parsed = serde_json::from_str::<UploadAnswer>(&answer.body).unwrap_or_default();
                if answer.ok() && parsed.ok {
                    progress.frames = parsed.frames;
                    progress.used = parsed.frames_used;
                    progress.truncated = parsed.truncated;
                    progress.name = parsed.name;
                    Ok(capture::import_done_text(parsed.frames, parsed.frames_used, parsed.truncated))
                } else {
                    Err(capture::upload_error_text(answer.status, &parsed.code))
                }
            }
            Err(error) => Err(format!("L'envoi de la capture a échoué ({error}).")),
        };
        if let Err(text) = &outcome {
            progress.error = Some(text.clone());
        }
        self.ui.import.update(|import| {
            if let Some(import) = import {
                import.progress = progress;
            }
        });
        match outcome {
            Ok(text) => self.notify(false, text),
            Err(text) => self.notify(true, text),
        }
    }

    /// Stops a local import; what was fused stays on screen.
    pub fn cancel_import(&self) {
        if let Some(transport) = self.transport.borrow().as_ref() {
            transport.request(&WorkerRequest::CaptureCancel);
        }
    }

    pub fn dismiss_import(&self) {
        if self.ui.import.with_untracked(|i| i.as_ref().is_none_or(|i| i.progress.done)) {
            self.ui.import.set(None);
        }
    }
}
