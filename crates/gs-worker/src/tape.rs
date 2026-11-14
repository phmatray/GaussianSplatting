//! In-memory recording of the stream the worker hands to the page.
//!
//! Without a native server nobody can write a file, so the worker records
//! into its own memory, up to a bound, and gives the finished `.gsrec` to
//! the page as pieces for a `Blob`. A [`Tape`] sees every message of the
//! session from the start: it keeps a [`StreamState`] (what a viewer that
//! joined now would be sent), so that a recording can start at any moment,
//! in relay mode too, where there is no pipeline to ask for a snapshot.

use gs_core::record::{ChunkedSink, Pushed, RecordError, Recorder, RecorderOptions, StreamState, Summary, WriteError};
use serde::{Deserialize, Serialize};

/// Progress of a recording, sent to the page as the `record` worker event
/// about twice per second while it runs.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecordStatus {
    pub active: bool,
    pub name: String,
    /// Bytes of the file so far.
    pub bytes: u64,
    /// Largest file the worker keeps.
    pub max_bytes: u64,
    pub duration_s: f64,
}

/// Description of a finished recording: the JSON that precedes its pieces
/// in the `recording` message.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RecordingMeta {
    pub name: String,
    pub bytes: u64,
    pub duration_s: f64,
    pub scene: String,
    pub keyframes: u64,
    /// The recording stopped by itself at the memory bound.
    pub full: bool,
    /// `RecordError::code()` when the session could not be recorded any
    /// further (a message too large for a record); the file is valid up to
    /// there.
    pub error: Option<String>,
}

/// A finished recording: the file in pieces, in order.
#[derive(Clone, Debug, PartialEq)]
pub struct Recording {
    pub meta: RecordingMeta,
    pub chunks: Vec<Vec<u8>>,
}

struct Running {
    recorder: Recorder<ChunkedSink>,
    name: String,
    max_bytes: u64,
}

/// The session as recorded so far, and the recording in progress, if any.
#[derive(Default)]
pub struct Tape {
    state: StreamState,
    running: Option<Running>,
}

impl Tape {
    pub fn new() -> Self {
        Self::default()
    }

    /// True once the session has said enough for a recording to start.
    pub fn can_record(&self) -> bool {
        self.state.has_state()
    }

    pub fn is_recording(&self) -> bool {
        self.running.is_some()
    }

    /// Forgets the session (a new source takes over). A recording in
    /// progress is abandoned.
    pub fn clear(&mut self) {
        self.state = StreamState::new();
        self.running = None;
    }

    /// Feeds one message of the stream, sent at clock value `now_ms`.
    /// Returns the finished recording when this message ended it (the
    /// memory bound was reached, or the session can no longer be recorded).
    pub fn observe(&mut self, now_ms: f64, message: &[u8]) -> Option<Recording> {
        // A message the state cannot hold is not part of what a late
        // joiner gets; the recorder below reports it.
        let _ = self.state.apply(message);
        let running = self.running.as_mut()?;
        match running.recorder.push(now_ms / 1000.0, message) {
            Ok(Pushed::Full) => self.finish(None),
            Ok(_) => None,
            Err(WriteError::Record(error)) => self.finish(Some(error)),
            // The sink is memory and cannot fail; `Stopped` cannot follow
            // because a failed recorder is finished at once.
            Err(WriteError::Sink(never)) => match never {},
            Err(WriteError::Stopped) => self.finish(Some(RecordError::TooLarge)),
        }
    }

    /// Starts a recording of at most `max_bytes` from the present state of
    /// the session.
    pub fn start(&mut self, name: &str, max_bytes: u64) -> Result<(), &'static str> {
        if self.running.is_some() {
            return Err("a recording is already running");
        }
        if !self.state.has_state() {
            return Err("there is nothing to record yet");
        }
        let options = RecorderOptions::named(name).with_max_bytes(max_bytes);
        let recorder = Recorder::with_state(ChunkedSink::new(), options, self.state.clone());
        self.running = Some(Running { recorder, name: name.to_string(), max_bytes });
        Ok(())
    }

    /// Ends the recording in progress and returns it; `None` when none is
    /// running or nothing was written.
    pub fn stop(&mut self) -> Option<Recording> {
        self.finish(None)
    }

    /// A recording of the present moment alone: one keyframe, a still.
    /// This is how a finished scan is saved.
    pub fn still(&self, name: &str) -> Option<Recording> {
        if !self.state.has_state() {
            return None;
        }
        let recorder = Recorder::with_state(ChunkedSink::new(), RecorderOptions::named(name), self.state.clone());
        Self::close(recorder, name, None)
    }

    pub fn status(&self) -> RecordStatus {
        match &self.running {
            Some(running) => RecordStatus {
                active: true,
                name: running.name.clone(),
                bytes: running.recorder.bytes_written(),
                max_bytes: running.max_bytes,
                duration_s: running.recorder.duration_s(),
            },
            None => RecordStatus::default(),
        }
    }

    fn finish(&mut self, error: Option<RecordError>) -> Option<Recording> {
        let running = self.running.take()?;
        Self::close(running.recorder, &running.name, error)
    }

    fn close(recorder: Recorder<ChunkedSink>, name: &str, error: Option<RecordError>) -> Option<Recording> {
        // A recorder that failed has still written a file that plays to
        // its last record: hand that out rather than nothing.
        let (sink, summary) = match recorder.finish() {
            Ok(done) => done,
            Err(WriteError::Sink(never)) => match never {},
            Err(_) => return None,
        };
        if summary.bytes == 0 {
            return None;
        }
        let Summary { bytes, duration_s, scene, keyframes, full, .. } = summary;
        let meta = RecordingMeta { name: name.to_string(), bytes, duration_s, scene, keyframes, full, error: error.map(|e| e.code().to_string()) };
        Some(Recording { meta, chunks: sink.into_chunks() })
    }
}

/// The unaccented letters of an accented Latin letter, for the common ones
/// (French names are the usual case here).
fn unaccented(c: char) -> Option<&'static str> {
    Some(match c {
        'à' | 'â' | 'ä' | 'á' | 'ã' | 'å' => "a",
        'À' | 'Â' | 'Ä' | 'Á' | 'Ã' | 'Å' => "A",
        'ç' => "c",
        'Ç' => "C",
        'é' | 'è' | 'ê' | 'ë' => "e",
        'É' | 'È' | 'Ê' | 'Ë' => "E",
        'î' | 'ï' | 'í' | 'ì' => "i",
        'Î' | 'Ï' | 'Í' | 'Ì' => "I",
        'ñ' => "n",
        'Ñ' => "N",
        'ô' | 'ö' | 'ó' | 'ò' | 'õ' => "o",
        'Ô' | 'Ö' | 'Ó' | 'Ò' | 'Õ' => "O",
        'ù' | 'û' | 'ü' | 'ú' => "u",
        'Ù' | 'Û' | 'Ü' | 'Ú' => "U",
        'ÿ' => "y",
        'œ' => "oe",
        'Œ' => "OE",
        'æ' => "ae",
        'Æ' => "AE",
        _ => return None,
    })
}

/// File name of a recording: its name reduced to characters every file
/// system and URL accepts, with the `.gsrec` extension.
pub fn file_name(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().chars() {
        match c {
            c if c.is_ascii_alphanumeric() || c == '-' || c == '_' => out.push(c),
            ' ' | '.' | ':' | '/' | '\\' if !out.ends_with('-') => out.push('-'),
            c => out.push_str(unaccented(c).unwrap_or_default()),
        }
        if out.len() >= 64 {
            break;
        }
    }
    let stem = out.trim_matches('-');
    format!("{}.gsrec", if stem.is_empty() { "enregistrement" } else { stem })
}

#[cfg(test)]
mod tests {
    use gs_core::pipeline::Pipeline;
    use gs_core::protocol::PipelineConfig;
    use gs_core::record::verify;
    use gs_core::scene::SceneKind;

    use super::*;

    fn small_stage() -> Pipeline {
        let mut config = PipelineConfig::light(SceneKind::Stage);
        config.rig.count = 3;
        config.rig.width = 48;
        config.rig.height = 36;
        Pipeline::new(config)
    }

    fn file(recording: &Recording) -> Vec<u8> {
        recording.chunks.concat()
    }

    #[test]
    fn a_recording_can_start_in_the_middle_of_a_session() {
        let mut pipeline = small_stage();
        let mut tape = Tape::new();
        assert!(!tape.can_record() && tape.start("x", 1 << 20).is_err() && tape.still("x").is_none());
        for message in pipeline.snapshot() {
            assert!(tape.observe(0.0, &message.bytes).is_none());
        }
        let mut now = 0.0;
        for _ in 0..10 {
            now += 50.0;
            for message in pipeline.tick(0.05) {
                tape.observe(now, &message.bytes);
            }
        }
        assert!(tape.can_record() && !tape.is_recording());
        tape.start("Danse du soir", 64 << 20).unwrap();
        assert!(tape.start("again", 64 << 20).is_err());
        for _ in 0..40 {
            now += 50.0;
            for message in pipeline.tick(0.05) {
                assert!(tape.observe(now, &message.bytes).is_none());
            }
        }
        let status = tape.status();
        assert!(status.active && status.bytes > 1024 && (status.duration_s - 1.95).abs() < 0.11, "{status:?}");
        let recording = tape.stop().unwrap();
        assert!(!tape.is_recording() && tape.stop().is_none() && !tape.status().active);
        let report = verify(&file(&recording)).unwrap();
        assert!(report.is_valid() && report.complete, "{:?}", report.problems);
        assert_eq!(report.info.name, "Danse du soir");
        assert_eq!((recording.meta.scene.as_str(), recording.meta.full, &recording.meta.error), ("stage", false, &None));
        assert_eq!(recording.meta.bytes, file(&recording).len() as u64);
        assert!((report.duration_s - recording.meta.duration_s).abs() < 1e-9 && report.duration_s > 1.8);
    }

    #[test]
    fn the_memory_bound_ends_the_recording_with_a_valid_file() {
        let mut pipeline = small_stage();
        let mut tape = Tape::new();
        for message in pipeline.snapshot() {
            tape.observe(0.0, &message.bytes);
        }
        let bound = 300 * 1024;
        tape.start("borne", bound).unwrap();
        let mut finished = None;
        let mut now = 0.0;
        'session: for _ in 0..2000 {
            now += 50.0;
            for message in pipeline.tick(0.05) {
                if let Some(recording) = tape.observe(now, &message.bytes) {
                    finished = Some(recording);
                    break 'session;
                }
            }
        }
        let recording = finished.expect("the bound is reached");
        assert!(recording.meta.full && !tape.is_recording());
        assert!(recording.meta.bytes <= bound && recording.meta.bytes > bound / 2, "{}", recording.meta.bytes);
        let report = verify(&file(&recording)).unwrap();
        assert!(report.is_valid() && report.complete, "{:?}", report.problems);
    }

    #[test]
    fn a_still_is_a_recording_with_one_keyframe() {
        let pipeline = small_stage();
        let mut tape = Tape::new();
        for message in pipeline.snapshot() {
            tape.observe(0.0, &message.bytes);
        }
        let still = tape.still("Scan du salon").unwrap();
        let report = verify(&file(&still)).unwrap();
        assert!(report.is_valid() && report.complete);
        assert_eq!((report.keyframes.len(), still.meta.keyframes, report.duration_s), (1, 1, 0.0));
        // Taking a still does not disturb a recording in progress.
        tape.start("suite", 64 << 20).unwrap();
        assert!(tape.still("autre").is_some() && tape.is_recording());
        tape.clear();
        assert!(!tape.is_recording() && !tape.can_record());
    }

    #[test]
    fn garbage_in_the_stream_is_left_out() {
        let mut pipeline = small_stage();
        let mut tape = Tape::new();
        assert!(tape.observe(0.0, b"junk").is_none());
        for message in pipeline.snapshot() {
            tape.observe(0.0, &message.bytes);
        }
        tape.start("x", 64 << 20).unwrap();
        assert!(tape.observe(10.0, b"junk").is_none() && tape.observe(20.0, &[]).is_none());
        for message in pipeline.tick(0.05) {
            tape.observe(50.0, &message.bytes);
        }
        let report = verify(&file(&tape.stop().unwrap())).unwrap();
        assert!(report.is_valid());
    }

    #[test]
    fn file_names_are_safe() {
        assert_eq!(file_name("Danse du soir"), "Danse-du-soir.gsrec");
        assert_eq!(file_name("  ../../etc/passwd  "), "etc-passwd.gsrec");
        assert_eq!(file_name("été à la plage"), "ete-a-la-plage.gsrec");
        assert_eq!(file_name("Scène 2026-11-14 16h05"), "Scene-2026-11-14-16h05.gsrec");
        assert_eq!(file_name("Cœur de forêt, façade n°3"), "Coeur-de-foret-facade-n3.gsrec");
        assert_eq!(file_name("日本語"), "enregistrement.gsrec");
        assert_eq!(file_name(""), "enregistrement.gsrec");
        assert_eq!(file_name("///"), "enregistrement.gsrec");
        assert!(file_name(&"a".repeat(500)).len() <= 64 + 6);
    }
}
