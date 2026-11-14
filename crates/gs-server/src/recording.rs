//! Recordings written to the recordings directory.
//!
//! Two shapes, both `.gsrec` files of `gs_core::record`:
//!
//! * a [`FileRecording`] follows a stream message by message, for as long
//!   as it lasts (the record button, a `live` capture);
//! * [`save_still`] writes what a viewer holds at one moment as a recording
//!   with a single keyframe (a finished scan).
//!
//! Everything here blocks on the disk: it runs on the pipeline thread or on
//! a blocking task, never on the async threads.

use std::fs::{self, File};
use std::io::{self, BufWriter};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use gs_core::pipeline::Outgoing;
use gs_core::record::{IoSink, Pushed, Recorder, RecorderOptions, WriteError};
use serde::Serialize;

use crate::store::{self, Hold, Kind, Store};

/// Buffer between the recorder and the file: one live layer of the default
/// stage is about 250 kB, so most ticks cost one write.
const WRITE_BUFFER: usize = 256 << 10;

/// A recording in progress, as `GET /api/record` reports it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RecordingStatus {
    /// File name without `.gsrec`.
    pub name: String,
    /// File name.
    pub file: String,
    /// The name the recording was started with.
    pub title: String,
    /// Bytes written so far.
    pub size: u64,
    /// Seconds recorded so far.
    pub duration_s: f64,
}

/// Why a recording ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    /// Somebody stopped it.
    Stopped,
    /// It reached the size limit.
    Full,
    /// The disk refused a write.
    DiskError,
    /// The session holds more than a recording can (`too_large`).
    TooLarge,
}

/// A recording that ended, as the answers of `POST /api/record` and
/// `GET /api/record` report it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Saved {
    /// File name without `.gsrec`.
    pub name: String,
    /// File name.
    pub file: String,
    pub title: String,
    pub scene: String,
    pub size: u64,
    pub duration_s: f64,
    /// `recordings/<file>`, percent-encoded.
    pub url: String,
    pub reason: EndReason,
}

/// What the request handlers know about the record button: updated by the
/// pipeline thread, read by `GET /api/record`.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct RecordState {
    /// The recording in progress.
    pub recording: Option<RecordingStatus>,
    /// The latest recording that ended since the server started.
    pub last: Option<Saved>,
}

pub(crate) type SharedRecordState = Arc<Mutex<RecordState>>;

/// A recording that follows a stream.
pub(crate) struct FileRecording {
    recorder: Recorder<IoSink<BufWriter<File>>>,
    /// Keeps the file from being deleted or shared while it is written.
    hold: Hold,
    title: String,
    path: PathBuf,
    started: Instant,
}

fn stem_of(file: &str) -> String {
    file.strip_suffix(store::RECORDING_EXT).unwrap_or(file).to_string()
}

impl FileRecording {
    /// Creates `<stem>.gsrec` (or the next free name) and starts recording
    /// with `snapshot`, what a viewer joining now would receive.
    pub fn start(
        store: &Arc<Store>,
        stem: &str,
        title: &str,
        max_bytes: u64,
        snapshot: &[Outgoing],
    ) -> io::Result<Self> {
        let (hold, path, handle) = store.create(Kind::Recording, stem)?;
        let options = RecorderOptions::named(title).with_max_bytes(max_bytes);
        let recorder = Recorder::new(IoSink(BufWriter::with_capacity(WRITE_BUFFER, handle)), options);
        let mut recording = Self { recorder, hold, title: title.to_string(), path, started: Instant::now() };
        for message in snapshot {
            if let Some(reason) = recording.push(&message.bytes) {
                let error = io::Error::other(format!("the recording could not start: {reason:?}"));
                recording.discard();
                return Err(error);
            }
        }
        Ok(recording)
    }

    pub fn status(&self) -> RecordingStatus {
        RecordingStatus {
            name: stem_of(self.hold.file()),
            file: self.hold.file().to_string(),
            title: self.title.clone(),
            size: self.recorder.bytes_written(),
            duration_s: self.recorder.duration_s(),
        }
    }

    /// Records one message at the present time. `Some(reason)` when the
    /// recording cannot go on: call [`FileRecording::finish`].
    pub fn push(&mut self, message: &[u8]) -> Option<EndReason> {
        self.push_at(self.started.elapsed().as_secs_f64(), message)
    }

    /// [`FileRecording::push`] with a time line of the caller's own
    /// (seconds, never decreasing): an imported capture is recorded at the
    /// pace it was filmed, not at the pace it is read.
    pub fn push_at(&mut self, time_s: f64, message: &[u8]) -> Option<EndReason> {
        match self.recorder.push(time_s, message) {
            Ok(Pushed::Full) => Some(EndReason::Full),
            Ok(_) => None,
            Err(WriteError::Sink(err)) => {
                log::error!("écriture de l'enregistrement {} impossible : {err}", self.hold.file());
                self.hold.report_problem("recording_not_saved", err);
                Some(EndReason::DiskError)
            }
            Err(WriteError::Record(err)) => {
                log::error!("l'enregistrement {} s'arrête : {err}", self.hold.file());
                Some(EndReason::TooLarge)
            }
            Err(WriteError::Stopped) => Some(EndReason::DiskError),
        }
    }

    /// Writes the index and closes the file. The file stays, whatever
    /// happens: a recording cut short still plays.
    pub fn finish(self, reason: EndReason) -> Saved {
        let Self { recorder, hold, title, path, .. } = self;
        let file = hold.file().to_string();
        let mut saved = Saved {
            name: stem_of(&file),
            url: store::url_of(Kind::Recording, &file),
            title,
            scene: String::new(),
            size: 0,
            duration_s: 0.0,
            reason,
            file,
        };
        match recorder.finish() {
            Ok((sink, summary)) => {
                // `finish` flushed; taking the buffer apart here reports a
                // late write error instead of losing it in a drop. No
                // `fsync`: it can take seconds on a busy disk, and this is
                // the thread every viewer waits on.
                if let Err(err) = sink.0.into_inner().map_err(|e| e.into_error()) {
                    log::error!("fermeture de l'enregistrement {} : {err}", saved.file);
                    saved.reason = EndReason::DiskError;
                }
                saved.scene = summary.scene;
                saved.duration_s = summary.duration_s;
                saved.size = summary.bytes;
            }
            Err(err) => {
                log::error!("l'enregistrement {} est resté incomplet : {err}", saved.file);
                if saved.reason == EndReason::Stopped {
                    saved.reason = EndReason::DiskError;
                }
                saved.size = fs::metadata(&path).map_or(0, |meta| meta.len());
            }
        }
        // Only now may the file be deleted or shared.
        drop(hold);
        saved
    }

    /// Stops and deletes the file.
    pub fn discard(self) {
        let Self { recorder, hold, path, .. } = self;
        drop(recorder);
        let _ = fs::remove_file(path);
        drop(hold);
    }
}

/// Writes `snapshot` (what a viewer joining now would receive) as a
/// recording with one keyframe. With `replace`, the file of that name is
/// replaced; otherwise `<stem>.gsrec` or the next free name is created.
/// The new file appears under its name only once it is complete.
pub(crate) fn save_still(
    store: &Arc<Store>,
    stem: &str,
    replace: Option<&str>,
    title: &str,
    snapshot: &[Outgoing],
) -> io::Result<Saved> {
    let (part, handle) = store.create_part(Kind::Recording)?;
    let written = (|| {
        let mut recorder = Recorder::new(IoSink(BufWriter::with_capacity(WRITE_BUFFER, handle)), RecorderOptions::named(title));
        for message in snapshot {
            recorder.push(0.0, &message.bytes).map_err(io::Error::other)?;
        }
        let (sink, summary) = recorder.finish().map_err(io::Error::other)?;
        sink.0.into_inner().map_err(|e| e.into_error())?;
        if summary.keyframes == 0 {
            return Err(io::Error::other("nothing to record"));
        }
        Ok(summary)
    })();
    let summary = match written {
        Ok(summary) => summary,
        Err(err) => {
            let _ = fs::remove_file(&part);
            return Err(err);
        }
    };
    let file = match replace.filter(|file| store::is_safe_name(file, store::RECORDING_EXT)) {
        Some(file) => {
            let Some(dir) = store.dir(Kind::Recording) else {
                let _ = fs::remove_file(&part);
                return Err(io::Error::new(io::ErrorKind::NotFound, "no recordings directory"));
            };
            fs::rename(&part, dir.join(file)).inspect_err(|_| {
                let _ = fs::remove_file(&part);
            })?;
            file.to_string()
        }
        None => store.adopt(Kind::Recording, &part, stem).inspect_err(|_| {
            let _ = fs::remove_file(&part);
        })?,
    };
    Ok(Saved {
        name: stem_of(&file),
        url: store::url_of(Kind::Recording, &file),
        title: title.to_string(),
        scene: summary.scene,
        size: summary.bytes,
        duration_s: summary.duration_s,
        reason: EndReason::Stopped,
        file,
    })
}
