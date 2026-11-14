//! A capture of the phone companion as the interface presents it: mode
//! names, the state of the session, the outcome of an import.

use gs_core::protocol::{CaptureMode, CaptureStats};

use super::format;

/// Largest capture file opened without a server, bytes. The file is read
/// piece by piece, so the limit is about patience, not memory.
pub const MAX_CAPTURE_BYTES: u64 = 8 * 1024 * 1024 * 1024;

/// Name of a capture mode.
pub fn mode_label(mode: CaptureMode) -> &'static str {
    match mode {
        CaptureMode::Room => "Pièce",
        CaptureMode::Object => "Objet",
        CaptureMode::Person => "Personne",
        CaptureMode::Live => "Direct",
    }
}

/// One line on what a mode does.
pub fn mode_hint(mode: CaptureMode) -> &'static str {
    match mode {
        CaptureMode::Room => "Les pièces parcourues s'assemblent en une maquette.",
        CaptureMode::Object => "L'objet est reconstruit finement, le reste est écarté.",
        CaptureMode::Person => "La personne, immobile, est reconstruite seule.",
        CaptureMode::Live => "Chaque image remplace la précédente\u{a0}: un relief vu du téléphone.",
    }
}

/// State of a capture session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureState {
    /// No frame has arrived yet.
    Waiting,
    Running,
    Paused,
    Finished,
}

impl CaptureState {
    pub fn of(stats: Option<&CaptureStats>) -> Self {
        match stats {
            Some(s) if s.finished => Self::Finished,
            Some(s) if s.paused => Self::Paused,
            Some(s) if s.frames_received > 0 => Self::Running,
            _ => Self::Waiting,
        }
    }

    pub fn label(self, imported: bool) -> &'static str {
        match (self, imported) {
            (Self::Waiting, true) => "Lecture du fichier…",
            (Self::Waiting, false) => "En attente des images",
            (Self::Running, true) => "Import en cours",
            (Self::Running, false) => "Capture en cours",
            (Self::Paused, _) => "Capture en pause",
            (Self::Finished, _) => "Capture terminée",
        }
    }

    /// Identifier for styling and the debug hook.
    pub fn id(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Running => "running",
            Self::Paused => "paused",
            Self::Finished => "finished",
        }
    }
}

/// What ARKit's tracking state means for the user; `None` when all is well.
pub fn tracking_text(stats: &CaptureStats) -> Option<&'static str> {
    if stats.finished || stats.paused || stats.frames_received == 0 {
        return None;
    }
    match stats.tracking {
        0 => Some("Le téléphone a perdu le suivi de sa position. Ralentissez et visez une zone détaillée."),
        1 => Some("Le suivi de position est limité\u{a0}: ces images ne sont pas utilisées. Bougez plus lentement."),
        _ => None,
    }
}

/// `123 images reçues, 118 utilisées`.
pub fn frames_text(received: u64, used: u64) -> String {
    let plural = |n: u64| if n > 1 { "s" } else { "" };
    format!("{} image{} reçue{}, {} utilisée{}", format::count(received), plural(received), plural(received), format::count(used), plural(used))
}

/// The notice at the end of a local import.
pub fn import_done_text(frames: u64, used: u64, truncated: bool) -> String {
    let mut text = format!("Capture importée\u{a0}: {}.", frames_text(frames, used).replace("reçue", "lue"));
    if truncated {
        text.push_str(" Le fichier est incomplet (enregistrement interrompu)\u{a0}: ses dernières images manquent.");
    }
    text
}

/// What to tell the user for an error code of `gs_core::ingest`, or for a
/// failure of the upload.
pub fn import_error_text(code: &str) -> String {
    match code {
        "bad_file_magic" => "Ce fichier n'est pas une capture Splat Capture (.gscap).".to_string(),
        "bad_header" => "L'en-tête de la capture est illisible, ou son mode de capture est inconnu.".to_string(),
        "too_short" => "Le fichier est vide ou trop court pour être une capture.".to_string(),
        "bad_magic" | "bad_version" | "bad_flags" | "bad_tracking" | "bad_orientation" | "bad_dimensions" | "bad_length" | "bad_deflate" | "bad_value" => {
            "La capture contient une image abîmée\u{a0}: l'import s'est arrêté là. Ce qui précède reste affiché.".to_string()
        }
        other => format!("La capture ne peut pas être importée ({other})."),
    }
}

/// Answer of `POST /api/captures`.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(default)]
pub struct UploadAnswer {
    pub ok: bool,
    pub frames: u64,
    pub frames_used: u64,
    pub truncated: bool,
    pub name: String,
    /// Stable code of a refusal: a format code of the capture file, or
    /// `busy`, `import_in_progress`, `too_large`, `timeout`.
    pub code: String,
}

/// What to tell the user when the server refused an uploaded capture.
pub fn upload_error_text(status: u16, code: &str) -> String {
    match (status, code) {
        (_, "busy") => "Un téléphone est en train de capturer\u{a0}: l'import attendra la fin de sa capture.".to_string(),
        (_, "import_in_progress") => "Un autre import est déjà en cours sur le serveur.".to_string(),
        (413, _) | (_, "too_large") => "La capture est trop volumineuse pour ce serveur.".to_string(),
        (408, _) | (_, "timeout") => "L'envoi de la capture a pris trop de temps.".to_string(),
        (401 | 403, _) => "Le serveur a refusé l'import depuis cet appareil.".to_string(),
        (400, "") => "Le serveur a refusé ce fichier\u{a0}: ce n'est pas une capture valide.".to_string(),
        (400, code) => import_error_text(code),
        (status, _) => format!("Le serveur n'a pas pu importer la capture (HTTP {status})."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states_follow_the_statistics() {
        let stats = |frames_received, paused, finished| CaptureStats { frames_received, paused, finished, tracking: 2, ..CaptureStats::default() };
        assert_eq!(CaptureState::of(None), CaptureState::Waiting);
        assert_eq!(CaptureState::of(Some(&stats(0, false, false))), CaptureState::Waiting);
        assert_eq!(CaptureState::of(Some(&stats(3, false, false))), CaptureState::Running);
        assert_eq!(CaptureState::of(Some(&stats(3, true, false))), CaptureState::Paused);
        assert_eq!(CaptureState::of(Some(&stats(3, true, true))), CaptureState::Finished);
        for state in [CaptureState::Waiting, CaptureState::Running, CaptureState::Paused, CaptureState::Finished] {
            for imported in [false, true] {
                assert!(!state.label(imported).is_empty() && !state.label(imported).contains('\u{2014}'));
            }
            assert!(!state.id().is_empty());
        }
    }

    #[test]
    fn tracking_trouble_is_only_reported_while_capturing() {
        let running = CaptureStats { frames_received: 5, tracking: 1, ..CaptureStats::default() };
        assert!(tracking_text(&running).unwrap().contains("limité"));
        assert!(tracking_text(&CaptureStats { tracking: 0, ..running }).unwrap().contains("perdu"));
        assert!(tracking_text(&CaptureStats { tracking: 2, ..running }).is_none());
        assert!(tracking_text(&CaptureStats { finished: true, ..running }).is_none());
        assert!(tracking_text(&CaptureStats { paused: true, ..running }).is_none());
        assert!(tracking_text(&CaptureStats::default()).is_none());
    }

    #[test]
    fn texts() {
        assert_eq!(frames_text(1, 0), "1 image reçue, 0 utilisée");
        assert_eq!(frames_text(1234, 1200), "1\u{202f}234 images reçues, 1\u{202f}200 utilisées");
        assert_eq!(import_done_text(24, 22, false), "Capture importée\u{a0}: 24 images lues, 22 utilisées.");
        assert!(import_done_text(5, 5, true).contains("incomplet"));
        for code in ["bad_file_magic", "bad_header", "too_short", "bad_magic", "bad_deflate", "weird"] {
            let text = import_error_text(code);
            assert!(text.ends_with('.') && !text.contains('\u{2014}'), "{text}");
        }
        for mode in CaptureMode::ALL {
            assert!(!mode_label(mode).is_empty() && mode_hint(mode).ends_with('.'));
        }
    }

    #[test]
    fn the_upload_answer_of_the_spec_is_read() {
        let answer: UploadAnswer = serde_json::from_str(
            r#"{"ok":true,"frames":24,"frames_used":22,"truncated":false,"name":"Salon","file":"salon.gscap","recording":"salon-1","shown":true}"#,
        )
        .unwrap();
        assert_eq!(answer, UploadAnswer { ok: true, frames: 24, frames_used: 22, truncated: false, name: "Salon".into(), code: String::new() });
        let refused: UploadAnswer = serde_json::from_str(r#"{"ok":false,"error":"the header is not valid","code":"bad_header"}"#).unwrap();
        assert!(!refused.ok && refused.code == "bad_header");
        assert!(upload_error_text(400, "bad_header").contains("en-tête"));
        assert!(upload_error_text(409, "busy").contains("téléphone"));
        assert!(upload_error_text(409, "import_in_progress").contains("import"));
        assert!(upload_error_text(413, "").contains("volumineuse"));
        assert!(upload_error_text(500, "internal").contains("500"));
        assert!(upload_error_text(400, "").contains("capture valide"));
    }
}
