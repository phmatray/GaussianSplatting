//! Replay and recording as the interface presents them: times, speeds,
//! error messages, keyboard steps, the server's list of recordings.

use serde::Deserialize;

use super::format;

/// Playback speeds offered by the time line.
pub const SPEEDS: [f64; 6] = [0.25, 0.5, 1.0, 2.0, 4.0, 8.0];
/// Index of normal speed in [`SPEEDS`].
pub const NORMAL_SPEED: usize = 2;
/// Seconds the J and L keys move the play head.
pub const SKIP_S: f64 = 5.0;
/// Seconds the comma and period keys move the play head: one tick of the
/// light preset.
pub const STEP_S: f64 = 0.05;
/// A full deflection of the controller stick scrubs through the recording
/// in this many seconds, whatever its length (but never slower than real
/// time: see [`scrub_rate`]).
const SCRUB_FULL_S: f64 = 8.0;

/// Label of a speed: `× 0,5`.
pub fn speed_label(speed: f64) -> String {
    let decimals = if speed.fract() == 0.0 { 0 } else if (speed * 10.0).fract() == 0.0 { 1 } else { 2 };
    format!("×\u{a0}{}", format::decimal(speed as f32, decimals))
}

/// Index into [`SPEEDS`] of the speed closest to `speed`.
pub fn speed_index(speed: f64) -> usize {
    let mut best = NORMAL_SPEED;
    for (i, candidate) in SPEEDS.iter().enumerate() {
        if (candidate - speed).abs() < (SPEEDS[best] - speed).abs() {
            best = i;
        }
    }
    best
}

/// A time of the recording as `m:ss` (`h:mm:ss` from an hour up); with
/// `tenths`, as `m:ss,d`.
pub fn clock(seconds: f64, tenths: bool) -> String {
    let seconds = if seconds.is_finite() { seconds.max(0.0) } else { 0.0 };
    // Truncated, not rounded: 0:59,96 must not read 1:00 before the head is there.
    let total_tenths = (seconds * 10.0 + 1e-6).floor() as u64;
    let (whole, tenth) = (total_tenths / 10, total_tenths % 10);
    let (hours, minutes, secs) = (whole / 3600, whole % 3600 / 60, whole % 60);
    let mut out = if hours > 0 { format!("{hours}:{minutes:02}:{secs:02}") } else { format!("{minutes}:{secs:02}") };
    if tenths {
        out.push_str(&format!(",{tenth}"));
    }
    out
}

/// The same for a screen reader: `1 minute 5 secondes`.
pub fn spoken(seconds: f64) -> String {
    let whole = if seconds.is_finite() { seconds.max(0.0) as u64 } else { 0 };
    let (minutes, secs) = (whole / 60, whole % 60);
    let plural = |n: u64| if n > 1 { "s" } else { "" };
    match (minutes, secs) {
        (0, s) => format!("{s} seconde{}", plural(s)),
        (m, 0) => format!("{m} minute{}", plural(m)),
        (m, s) => format!("{m} minute{} {s} seconde{}", plural(m), plural(s)),
    }
}

/// A recording this short is a still: one moment, saved (a recording the
/// server starts and stops at once may hold a tick or two).
pub const STILL_S: f64 = 0.25;

/// Length of a recording for a list: `1:05`, or `image fixe` for a still.
pub fn length_label(duration_s: f64) -> String {
    if duration_s.is_finite() && duration_s >= STILL_S { clock(duration_s, false) } else { "image fixe".to_string() }
}

/// A size in bytes as `ko`, `Mo` or `Go`.
pub fn size(bytes: u64) -> String {
    let kilo = bytes as f32 / 1000.0;
    if kilo < 1000.0 {
        format!("{} ko", format::decimal(kilo, if kilo < 10.0 { 1 } else { 0 }))
    } else if kilo < 1.0e6 {
        format!("{} Mo", format::decimal(kilo / 1000.0, if kilo < 10_000.0 { 1 } else { 0 }))
    } else {
        format!("{} Go", format::decimal(kilo / 1.0e6, 1))
    }
}

/// Seconds of recording per second of full stick deflection in VR.
pub fn scrub_rate(duration: f64) -> f64 {
    (duration / SCRUB_FULL_S).max(1.0)
}

/// Where a seek of `delta` seconds from `time` lands in a recording of
/// `duration` seconds.
pub fn seek_target(time: f64, delta: f64, duration: Option<f64>) -> f64 {
    let end = duration.filter(|d| d.is_finite() && *d > 0.0).unwrap_or(f64::MAX);
    let target = time + delta;
    if target.is_finite() { target.clamp(0.0, end) } else { time.clamp(0.0, end) }
}

/// What to tell the user for an error code of the replay: a
/// `RecordError::code()`, or a code of the worker for a source that
/// failed (`network`, `read`, `too_large`, `http_404`).
pub fn error_text(code: &str) -> String {
    let known = match code {
        "too_short" => "Le fichier est trop court pour être un enregistrement.",
        "bad_magic" => "Ce fichier n'est pas un enregistrement Splat Live (.gsrec).",
        "bad_header" => "L'en-tête de l'enregistrement est illisible.",
        "empty" => "L'enregistrement ne contient aucune image complète.",
        "bad_length" | "bad_kind" | "bad_flags" | "bad_time" | "bad_deflate" | "bad_message" | "bad_keyframe" | "bad_index" | "trailing_data" => {
            "L'enregistrement est abîmé et ne peut pas être lu."
        }
        "too_large" => "L'enregistrement est trop volumineux pour être chargé d'un bloc, et son serveur ne permet pas de le lire par morceaux.",
        "network" => "Le téléchargement de l'enregistrement a échoué. Vérifiez l'adresse, et pour un autre site que celui-ci, qu'il autorise le partage (CORS).",
        "read" => "Le fichier n'a pas pu être lu.",
        "http_404" => "L'enregistrement est introuvable à cette adresse (erreur 404).",
        "http_403" | "http_401" => "L'accès à cet enregistrement est refusé.",
        _ => "",
    };
    if !known.is_empty() {
        return known.to_string();
    }
    match code.strip_prefix("http_") {
        Some(status) if status.chars().all(|c| c.is_ascii_digit()) => format!("Le serveur a répondu par une erreur {status}."),
        _ => format!("L'enregistrement ne peut pas être lu ({code})."),
    }
}

/// What to tell the user when the worker refused a request. Its reasons
/// are English sentences for a log; the ones a person can run into are
/// said in French, the others are shown as they are.
pub fn worker_refusal_text(reason: &str) -> String {
    match reason {
        "there is nothing to record yet" => "Il n'y a encore rien à enregistrer\u{a0}: la scène n'est pas arrivée.".to_string(),
        "a recording is already running" => "Un enregistrement est déjà en cours.".to_string(),
        "no recording is open" => "Aucun enregistrement n'est ouvert.".to_string(),
        other => format!("Commande refusée\u{a0}: {other}"),
    }
}

/// What to tell the user when the in-memory recording stopped by itself.
pub fn stop_text(full: bool, error: Option<&str>) -> Option<&'static str> {
    match (error, full) {
        (Some(_), _) => Some("L'enregistrement s'est arrêté\u{a0}: la scène est devenue trop volumineuse pour être enregistrée. Ce qui précède est conservé."),
        (None, true) => Some("L'enregistrement a atteint la taille maximale gardée en mémoire et s'est arrêté. Il est complet jusque-là."),
        (None, false) => None,
    }
}

/// A default name for a recording started now: `Scène 2026-11-14 16h05`.
/// `iso` is an RFC 3339 time (`Date.toISOString()` shifted to local time).
pub fn default_name(scene: &str, iso: &str) -> String {
    let label = match scene {
        "house" => "Maison",
        "capture" => "Capture",
        _ => "Scène",
    };
    match (iso.get(0..10), iso.get(11..13), iso.get(14..16)) {
        (Some(date), Some(hours), Some(minutes)) if iso.is_ascii() => format!("{label} {date} {hours}h{minutes}"),
        _ => label.to_string(),
    }
}

/// One recording of the server's recordings directory, as `GET
/// /api/recordings` lists it.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ServerRecording {
    /// File name without directory (`danse-20261114-160509.gsrec`).
    pub file: String,
    /// The file name without its extension: what the routes take.
    pub name: String,
    /// Name given when it was recorded.
    pub title: String,
    pub size: u64,
    pub duration_s: f64,
    pub scene: String,
    pub created: String,
    /// The file is still being written.
    pub recording: bool,
}

impl ServerRecording {
    /// File name under `/recordings/`.
    pub fn file_name(&self) -> String {
        let base = if self.file.is_empty() { &self.name } else { &self.file };
        if base.to_ascii_lowercase().ends_with(".gsrec") { base.clone() } else { format!("{base}.gsrec") }
    }

    /// Name shown in the list.
    pub fn title(&self) -> String {
        let title = [&self.title, &self.name, &self.file].into_iter().find(|t| !t.is_empty()).cloned().unwrap_or_default();
        title.strip_suffix(".gsrec").unwrap_or(&title).to_string()
    }
}

/// The recording the server is writing: the `recording` member of the
/// answers of `/api/record` and `/api/recordings` (`null` when it is not
/// recording).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct ServerRecorder {
    pub name: String,
    pub file: String,
    pub title: String,
    pub size: u64,
    pub duration_s: f64,
}

impl ServerRecorder {
    pub fn title(&self) -> &str {
        if self.title.is_empty() { &self.name } else { &self.title }
    }
}

/// How a recording of the server ended: the `saved` and `last` members
/// of `/api/record`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct SavedRecording {
    pub name: String,
    pub file: String,
    pub title: String,
    pub size: u64,
    pub duration_s: f64,
    /// `stopped`, `full`, `disk_error` or `too_large`.
    pub reason: String,
}

impl SavedRecording {
    /// What to tell the user when the recording did not simply stop on
    /// request.
    pub fn problem(&self) -> Option<&'static str> {
        match self.reason.as_str() {
            "full" => Some("L'enregistrement a atteint la taille maximale et s'est arrêté. Il est complet jusque-là."),
            "disk_error" => Some("L'enregistrement s'est arrêté\u{a0}: le disque n'a pas pu être écrit. Ce qui précède est conservé."),
            "too_large" => Some("L'enregistrement s'est arrêté\u{a0}: la scène est devenue trop volumineuse pour être enregistrée. Ce qui précède est conservé."),
            _ => None,
        }
    }
}

/// Answer of `GET /api/record` and of `POST /api/record`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct RecorderAnswer {
    /// The recording in progress, if any.
    pub recording: Option<ServerRecorder>,
    /// The recording a `stop` just ended.
    pub saved: Option<SavedRecording>,
    /// The last recording that ended, however it did.
    pub last: Option<SavedRecording>,
}

impl RecorderAnswer {
    pub fn parse(json: &str) -> Option<Self> {
        serde_json::from_str(json).ok()
    }
}

/// Answer of `GET /api/recordings`: the list, newest first, and the
/// recording in progress.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct RecordingList {
    pub recordings: Vec<ServerRecording>,
    pub recording: Option<ServerRecorder>,
}

/// File names are used in URLs and as DOM text: anything odd is left out.
fn plain_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 255 && !name.contains(['/', '\\']) && !name.chars().any(char::is_control) && name != "." && name != ".."
}

impl RecordingList {
    pub fn parse(json: &str) -> Result<Self, String> {
        let mut list: Self = serde_json::from_str(json).map_err(|e| format!("réponse illisible\u{a0}: {e}"))?;
        list.recordings.retain(|r| plain_name(&r.file_name()) && r.file_name() != ".gsrec");
        Ok(list)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clocks() {
        assert_eq!(clock(0.0, false), "0:00");
        assert_eq!(clock(59.96, false), "0:59");
        assert_eq!(clock(59.96, true), "0:59,9");
        assert_eq!(clock(65.25, true), "1:05,2");
        assert_eq!(clock(3725.0, false), "1:02:05");
        assert_eq!(clock(f64::NAN, true), "0:00,0");
        assert_eq!(clock(-3.0, false), "0:00");
        assert_eq!(clock(0.3, true), "0:00,3");
        assert_eq!(length_label(0.0), "image fixe");
        assert_eq!(length_label(0.06), "image fixe");
        assert_eq!(length_label(f64::NAN), "image fixe");
        assert_eq!(length_label(65.0), "1:05");
        assert_eq!(spoken(0.4), "0 seconde");
        assert_eq!(spoken(1.0), "1 seconde");
        assert_eq!(spoken(65.0), "1 minute 5 secondes");
        assert_eq!(spoken(120.0), "2 minutes");
    }

    #[test]
    fn speeds_and_sizes() {
        assert_eq!(speed_label(1.0), "×\u{a0}1");
        assert_eq!(speed_label(0.5), "×\u{a0}0,5");
        assert_eq!(speed_label(0.25), "×\u{a0}0,25");
        assert_eq!(SPEEDS[NORMAL_SPEED], 1.0);
        assert_eq!(speed_index(1.0), NORMAL_SPEED);
        assert_eq!(speed_index(0.3), 0);
        assert_eq!(speed_index(100.0), SPEEDS.len() - 1);
        assert_eq!(size(900), "0,9 ko");
        assert_eq!(size(250_000), "250 ko");
        assert_eq!(size(8_642_638), "8,6 Mo");
        assert_eq!(size(256 << 20), "268 Mo");
        assert_eq!(size(3_500_000_000), "3,5 Go");
    }

    #[test]
    fn seeks_stay_inside_the_recording() {
        assert_eq!(seek_target(3.0, SKIP_S, Some(10.0)), 8.0);
        assert_eq!(seek_target(8.0, SKIP_S, Some(10.0)), 10.0);
        assert_eq!(seek_target(3.0, -SKIP_S, Some(10.0)), 0.0);
        assert_eq!(seek_target(3.0, 2.0, None), 5.0);
        assert_eq!(seek_target(3.0, f64::NAN, Some(10.0)), 3.0);
        assert_eq!(scrub_rate(4.0), 1.0);
        assert_eq!(scrub_rate(80.0), 10.0);
    }

    #[test]
    fn every_error_has_a_french_sentence() {
        for code in [
            "too_short", "bad_magic", "bad_header", "bad_length", "bad_kind", "bad_flags", "bad_time", "bad_deflate", "bad_message",
            "bad_keyframe", "bad_index", "trailing_data", "empty", "too_large", "network", "read", "http_404", "http_403", "http_500", "weird",
        ] {
            let text = error_text(code);
            assert!(text.ends_with('.') && text.len() > 20 && !text.contains('\u{2014}'), "{code}: {text}");
        }
        assert!(error_text("http_500").contains("500"));
        assert!(error_text("weird").contains("weird"));
        assert!(worker_refusal_text("there is nothing to record yet").starts_with("Il n'y a encore rien"));
        assert!(worker_refusal_text("a recording is already running").contains("déjà en cours"));
        assert_eq!(worker_refusal_text("unknown message type"), "Commande refusée\u{a0}: unknown message type");
        assert!(stop_text(false, None).is_none());
        assert!(stop_text(true, None).unwrap().contains("taille maximale"));
        assert!(stop_text(true, Some("too_large")).unwrap().contains("trop volumineuse"));
    }

    #[test]
    fn default_names() {
        assert_eq!(default_name("stage", "2026-11-14T16:05:09.123Z"), "Scène 2026-11-14 16h05");
        assert_eq!(default_name("house", "2026-11-14T16:05:09Z"), "Maison 2026-11-14 16h05");
        assert_eq!(default_name("capture", "bad"), "Capture");
        assert_eq!(default_name("x", "é026-11-14T16:05:09Z"), "Scène");
    }

    #[test]
    fn the_recording_list_of_the_server_is_read() {
        let json = r#"{"recordings":[
            {"name":"danse-20261114-160509","file":"danse-20261114-160509.gsrec","title":"Danse du soir","scene":"stage","created":"2026-11-14T16:05:09Z","duration_s":5.5,"size":1234,"url":"recordings/danse-20261114-160509.gsrec","recording":false},
            {"name":"en-cours","file":"en-cours.gsrec","title":"En cours","scene":"stage","created":"","duration_s":0,"size":10,"url":"recordings/en-cours.gsrec","recording":true}],
            "recording":{"name":"en-cours","file":"en-cours.gsrec","title":"En cours","size":10,"duration_s":1.5},
            "dir":"/home/x/recordings"}"#;
        let list = RecordingList::parse(json).unwrap();
        assert_eq!(list.recordings.len(), 2);
        let first = &list.recordings[0];
        assert_eq!((first.file_name().as_str(), first.title().as_str(), first.size, first.duration_s), ("danse-20261114-160509.gsrec", "Danse du soir", 1234, 5.5));
        assert!(!first.recording && list.recordings[1].recording);
        assert_eq!(list.recording.as_ref().map(|r| (r.title(), r.size)), Some(("En cours", 10)));

        // Without titles the names serve; nothing recording.
        let bare = RecordingList::parse(r#"{"recordings":[{"name":"a"},{"file":"b.gsrec"}],"recording":null}"#).unwrap();
        assert_eq!(bare.recordings.iter().map(|r| (r.file_name(), r.title())).collect::<Vec<_>>(), [("a.gsrec".to_string(), "a".to_string()), ("b.gsrec".to_string(), "b".to_string())]);
        assert!(bare.recording.is_none());
        assert_eq!(RecordingList::parse("{}").unwrap(), RecordingList::default());

        assert!(RecordingList::parse("<!doctype html>").is_err());
        assert!(RecordingList::parse("7").is_err());
        assert!(RecordingList::parse(r#"{"recordings":[{"name":7}]}"#).is_err());
        // Names that could not be a file of the recordings directory are left out.
        let odd = RecordingList::parse(r#"{"recordings":[{"name":"../etc/passwd"},{"name":""},{"name":"a\u0000b"},{"name":"ok"}]}"#).unwrap();
        assert_eq!(odd.recordings.iter().map(ServerRecording::file_name).collect::<Vec<_>>(), ["ok.gsrec"]);
    }

    #[test]
    fn the_recorder_answers_of_the_server_are_read() {
        let started = RecorderAnswer::parse(r#"{"recording":{"name":"a-1","file":"a-1.gsrec","title":"A","size":2048,"duration_s":0.0}}"#).unwrap();
        assert_eq!(started.recording.as_ref().map(|r| (r.title(), r.size)), Some(("A", 2048)));
        assert!(started.saved.is_none());
        let stopped = RecorderAnswer::parse(
            r#"{"recording":null,"saved":{"name":"a-1","file":"a-1.gsrec","title":"A","scene":"stage","size":9000,"duration_s":4.2,"url":"recordings/a-1.gsrec","reason":"stopped"}}"#,
        )
        .unwrap();
        assert!(stopped.recording.is_none());
        let saved = stopped.saved.unwrap();
        assert_eq!((saved.title.as_str(), saved.size, saved.duration_s), ("A", 9000, 4.2));
        assert!(saved.problem().is_none());
        let polled = RecorderAnswer::parse(r#"{"recording":null,"last":{"name":"a-1","reason":"full"}}"#).unwrap();
        assert!(polled.last.unwrap().problem().unwrap().contains("taille maximale"));
        for reason in ["disk_error", "too_large"] {
            let saved = SavedRecording { reason: reason.to_string(), ..SavedRecording::default() };
            assert!(saved.problem().is_some_and(|text| !text.contains('\u{2014}')));
        }
        assert!(RecorderAnswer::parse("nope").is_none());
        assert_eq!(RecorderAnswer::parse(r#"{"error":"x","code":"not_recording"}"#), Some(RecorderAnswer::default()));
    }
}
