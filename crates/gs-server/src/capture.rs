//! Raw captures on disk: the stream of a phone kept as a `.gscap`, and the
//! import of such a file.
//!
//! Both block on the disk and on the reconstruction: they run on the
//! pipeline thread ([`RawCapture`]) or on a blocking task
//! ([`import_capture`]), never on the async threads.

use std::fs::{self, File};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Arc;

use gs_core::ingest::file::{encode_file_header, HEADER_PAD};
use gs_core::ingest::frame::{ErrorCode, FRAME_HEADER_SIZE, MAX_FRAME_BYTES};
use gs_core::ingest::{
    decode_frame, CaptureRegion, CaptureSession, FormatError, GscapHeader, GscapParser, Hello, ImportError,
    ImportSummary, IngestParams, ParsedItem,
};
use gs_core::protocol::CaptureMode;
use gs_core::record;

use crate::recording::{save_still, EndReason, FileRecording, Saved};
use crate::store::{self, Hold, Kind, Store};

/// Buffer between the frames and the file: about one frame of the phone.
const WRITE_BUFFER: usize = 1 << 20;
/// How much of a file is read at a time during an import.
const READ_CHUNK: usize = 256 << 10;
/// A gap between two frames of an imported `live` capture is replayed as
/// this many seconds at most (the phone was paused, or its clock jumped).
const MAX_FRAME_GAP_S: f64 = 2.0;

/// The stem of the files a capture leaves behind: its name, then the
/// moment it started.
pub(crate) fn capture_stem(name: &str) -> String {
    format!("{}-{}", store::slug(name, "capture"), store::timestamp())
}

/// The title of a capture for people: its name, or a default.
pub(crate) fn capture_title(name: &str) -> &str {
    if name.trim().is_empty() {
        "Capture"
    } else {
        name
    }
}

/// The stream of a phone, written as it arrives.
///
/// The header is padded so that it can be rewritten in place when the
/// phone tells its region of interest and its floor height, which it does
/// after the hello. A write error stops the saving, not the capture.
pub(crate) struct RawCapture {
    out: BufWriter<File>,
    /// Keeps the file from being deleted or imported while it is written.
    hold: Hold,
    path: PathBuf,
    header: GscapHeader,
    /// The header on disk is older than `header`.
    header_dirty: bool,
    frames: u64,
    failed: bool,
}

impl RawCapture {
    /// Creates `<stem>.gscap` (or the next free name) with the header of
    /// `hello`.
    pub fn create(store: &Arc<Store>, stem: &str, hello: &Hello) -> io::Result<Self> {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
        let header = GscapHeader {
            protocol: Some(hello.protocol),
            device: hello.device.clone(),
            app: hello.app.clone(),
            mode: hello.mode.name().to_string(),
            name: hello.name.clone(),
            created: record::rfc3339_utc(now),
            region: None,
            floor: None,
        };
        let head = encode_file_header(&header, HEADER_PAD).map_err(io::Error::other)?;
        let (hold, path, handle) = store.create(Kind::Capture, stem)?;
        let mut out = BufWriter::with_capacity(WRITE_BUFFER, handle);
        // On the disk at once: the capture is listed with its name and
        // mode while it is still being filmed.
        if let Err(err) = out.write_all(&head).and_then(|()| out.flush()) {
            drop(out);
            let _ = fs::remove_file(&path);
            return Err(err);
        }
        log::info!("the capture is kept as {}", hold.file());
        Ok(Self { out, hold, path, header, header_dirty: false, frames: 0, failed: false })
    }

    /// The file name without `.gscap`.
    pub fn stem(&self) -> &str {
        let file = self.hold.file();
        file.strip_suffix(store::CAPTURE_EXT).unwrap_or(file)
    }

    fn fail(&mut self, err: &io::Error) {
        if !self.failed {
            self.failed = true;
            log::error!("la capture n'est plus enregistrée dans {} : {err}", self.hold.file());
        }
    }

    /// Appends one frame exactly as it came off the wire. The caller has
    /// decoded it: nothing unreadable goes into the file.
    pub fn append(&mut self, frame: &[u8]) {
        if self.failed || !(FRAME_HEADER_SIZE..=MAX_FRAME_BYTES).contains(&frame.len()) {
            return;
        }
        let written = self.out.write_all(&(frame.len() as u32).to_le_bytes()).and_then(|()| self.out.write_all(frame));
        match written {
            Ok(()) => self.frames += 1,
            Err(err) => self.fail(&err),
        }
    }

    pub fn set_region(&mut self, region: CaptureRegion) {
        self.header.region = Some(region);
        self.header_dirty = true;
    }

    pub fn set_floor(&mut self, y: f32) {
        self.header.floor = Some(y);
        self.header_dirty = true;
    }

    /// Brings the file on disk up to date: everything received so far, and
    /// the header with the latest region and floor.
    pub fn checkpoint(&mut self) {
        if self.failed {
            return;
        }
        let result = (|| {
            self.out.flush()?;
            if self.header_dirty {
                // Same padding, same length: the frames stay where they are.
                let head = encode_file_header(&self.header, HEADER_PAD).map_err(io::Error::other)?;
                if head.len() == store_header_len() {
                    let file = self.out.get_mut();
                    file.seek(SeekFrom::Start(0))?;
                    file.write_all(&head)?;
                    file.seek(SeekFrom::End(0))?;
                }
                self.header_dirty = false;
            }
            Ok(())
        })();
        if let Err(err) = result {
            self.fail(&err);
        }
    }

    /// Closes the file. A capture without a single frame leaves no file.
    pub fn close(mut self) {
        self.checkpoint();
        let Self { out, hold, path, frames, .. } = self;
        drop(out);
        if frames == 0 {
            let _ = fs::remove_file(path);
        }
        drop(hold);
    }

    /// Closes and deletes the file (the phone threw the capture away).
    pub fn discard(self) {
        let Self { out, hold, path, .. } = self;
        drop(out);
        let _ = fs::remove_file(path);
        drop(hold);
    }
}

/// Size of the padded header [`RawCapture`] writes: magic, length, JSON.
fn store_header_len() -> usize {
    gs_core::ingest::file::FILE_MAGIC.len() + 4 + HEADER_PAD
}

/// What [`import_capture`] produced.
pub(crate) struct Imported {
    /// The finished capture, ready to be shown.
    pub session: CaptureSession,
    pub summary: ImportSummary,
    /// The recording it was saved as, when there is a recordings directory
    /// and the disk took it.
    pub saved: Option<Saved>,
}

/// Ingests a whole `.gscap` from `source`, one frame in memory at a time,
/// and saves the result as a recording named after `stem`: a still for the
/// fusing modes, the whole stream at the pace of the frames' time stamps
/// for a `live` capture.
pub(crate) fn import_capture(
    mut source: impl Read,
    params: IngestParams,
    store: &Arc<Store>,
    stem: &str,
    max_recording_bytes: u64,
) -> Result<Imported, ImportError> {
    let records = store.dir(Kind::Recording).is_some();
    let mut parser = GscapParser::new();
    let mut session: Option<CaptureSession> = None;
    let mut stream: Option<FileRecording> = None;
    // Time line of a `live` recording: seconds since the first frame.
    let (mut clock, mut last_stamp) = (0.0_f64, None::<f64>);
    let mut bytes = 0u64;
    let mut buffer = vec![0u8; READ_CHUNK];
    let outcome = (|| -> Result<_, ImportError> {
        loop {
            let read = match source.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => read,
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err.into()),
            };
            bytes += read as u64;
            parser.push(&buffer[..read]);
            while let Some(item) = parser.next_item()? {
                match item {
                    ParsedItem::Header(header) => {
                        let mut fresh = CaptureSession::from_header(&header, params)?;
                        if records && fresh.mode() == CaptureMode::Live {
                            let title = capture_title(fresh.name()).to_string();
                            match FileRecording::start(store, stem, &title, max_recording_bytes, &fresh.snapshot()) {
                                Ok(recording) => stream = Some(recording),
                                Err(err) => log::error!("la capture importée ne sera pas enregistrée : {err}"),
                            }
                        }
                        session = Some(fresh);
                    }
                    ParsedItem::Frame(frame_bytes) => {
                        let frame = decode_frame(frame_bytes)?;
                        // The parser hands out the header first.
                        let Some(session) = session.as_mut() else { continue };
                        let reply = session.handle_decoded(&frame);
                        if let Some(recording) = stream.as_mut() {
                            let step = last_stamp.map_or(0.0, |last| frame.timestamp - last);
                            clock += if step.is_finite() { step.clamp(0.0, MAX_FRAME_GAP_S) } else { 0.0 };
                            last_stamp = Some(frame.timestamp);
                            let ended =
                                reply.to_viewers.iter().find_map(|message| recording.push_at(clock, &message.bytes));
                            if let Some(reason) = ended {
                                // Full or failed: what is written stays.
                                if let Some(recording) = stream.take() {
                                    recording.finish(reason);
                                }
                            }
                        }
                    }
                }
            }
        }
        let end = parser.finish()?;
        let Some(mut session) = session.take() else {
            return Err(FormatError { code: ErrorCode::BadHeader, detail: "file ends inside the header".into() }.into());
        };
        let closing = session.finish();
        let summary = ImportSummary {
            frames: end.frames,
            frames_used: u64::from(session.frames_used()),
            truncated: end.truncated,
            name: session.name().to_string(),
            bytes,
        };
        Ok((session, summary, closing))
    })();
    let (mut session, summary, closing) = match outcome {
        Ok(done) => done,
        Err(err) => {
            // A damaged file leaves no half recording behind.
            if let Some(recording) = stream.take() {
                let saved = recording.finish(EndReason::Stopped);
                let _ = store.delete(Kind::Recording, &saved.file);
            }
            return Err(err);
        }
    };

    let saved = if let Some(mut recording) = stream.take() {
        let ended = closing.to_viewers.iter().find_map(|message| recording.push_at(clock, &message.bytes));
        Some(recording.finish(ended.unwrap_or(EndReason::Stopped)))
    } else if records && session.mode().fuses() {
        let snapshot = session.snapshot();
        match save_still(store, stem, None, capture_title(session.name()), &snapshot) {
            Ok(saved) => Some(saved),
            Err(err) => {
                log::error!("la capture importée n'a pas pu être enregistrée : {err}");
                None
            }
        }
    } else {
        None
    };
    Ok(Imported { session, summary, saved })
}
