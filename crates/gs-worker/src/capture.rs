//! A `.gscap` capture file opened without a server.
//!
//! The static web build has no `POST /api/captures` to send a capture to,
//! so the worker ingests the file itself with `gs_core::ingest`: the same
//! frames, the same fusion, the same messages for the viewer. Unlike the
//! server's import, which answers when everything is fused, the frames go
//! in one at a time and the viewer watches the capture build up, as it
//! would have live.
//!
//! [`LocalCapture`] holds the parser and the session; the `splat-worker`
//! binary reads the `Blob` piece by piece and decides how many frames fit
//! between two turns of its event loop.

use gs_core::ingest::{decode_frame, CaptureSession, FormatError, GscapParser, IngestParams, ParsedItem};
use gs_core::pipeline::Outgoing;
use serde::{Deserialize, Serialize};

/// A progress tick of the session (layer merge, refined cells re-sent) runs
/// after this many frames: about a second of capture, as on the server.
const FRAMES_PER_PROGRESS: u64 = 10;

/// Progress of a local import, sent to the page as the `capture` worker
/// event.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureProgress {
    /// Name of the capture, from the file header (empty until it is read).
    pub name: String,
    /// Bytes of the file read so far, and its size.
    pub bytes: u64,
    pub total: u64,
    /// Frames read and frames that went into the reconstruction.
    pub frames: u64,
    pub used: u64,
    /// The import is over (finished, failed or cancelled).
    pub done: bool,
    /// The file stops inside a record: a recording cut short.
    pub truncated: bool,
    /// Error code of `gs_core::ingest` when the file is not a valid capture.
    pub error: Option<String>,
}

/// What one step of the import produced.
#[derive(Debug, Default)]
pub struct CaptureStep {
    /// Messages for the viewer, in order.
    pub messages: Vec<Outgoing>,
    /// Frames fused or skipped by this step.
    pub frames: u32,
    /// The parser needs more bytes of the file before it can go on.
    pub starved: bool,
}

/// The import of one capture file, and the session it leaves behind.
pub struct LocalCapture {
    parser: GscapParser,
    params: IngestParams,
    session: Option<CaptureSession>,
    total: u64,
    bytes: u64,
    since_progress: u64,
    done: bool,
    truncated: bool,
    error: Option<String>,
    /// The last [`LocalCapture::step`] ran out of complete records.
    starved: bool,
}

impl LocalCapture {
    /// An import of a file of `total` bytes.
    pub fn new(total: u64) -> Self {
        Self {
            parser: GscapParser::new(),
            params: IngestParams::default(),
            session: None,
            total,
            bytes: 0,
            since_progress: 0,
            done: false,
            truncated: false,
            error: None,
            starved: false,
        }
    }

    /// Bytes the parser holds that it has not turned into frames yet.
    pub fn buffered(&self) -> usize {
        self.parser.buffered()
    }

    /// True when the host should read the next piece of the file: the
    /// parser holds less than `low_water` bytes, or what it holds is the
    /// beginning of a record whose end has not been read yet. A frame of a
    /// real phone is a few hundred kilobytes, far more than a sensible
    /// `low_water`, so the second condition is what keeps such a file
    /// moving. What is buffered stays bounded all the same: the parser
    /// refuses a record above 64 MiB before its bytes are asked for.
    pub fn wants_bytes(&self, low_water: usize) -> bool {
        !self.done && (self.starved || self.parser.buffered() < low_water)
    }

    pub fn is_done(&self) -> bool {
        self.done
    }

    /// True once the file header has been read and accepted: from here on
    /// the file is a capture, whatever its frames turn out to be.
    pub fn has_session(&self) -> bool {
        self.session.is_some()
    }

    /// Hands over the next bytes of the file.
    pub fn push(&mut self, chunk: &[u8]) {
        if !self.done {
            self.bytes += chunk.len() as u64;
            self.parser.push(chunk);
            // There may be a complete record now: the next step finds out.
            self.starved = false;
        }
    }

    fn fail(&mut self, error: &FormatError) {
        self.error = Some(error.code.as_str().to_string());
        self.done = true;
    }

    /// Fuses up to `max_frames` of the frames that are complete in the
    /// bytes pushed so far. An invalid file ends the import (see
    /// [`CaptureProgress::error`]); what was fused before stays on screen.
    pub fn step(&mut self, max_frames: u32) -> CaptureStep {
        let mut step = CaptureStep::default();
        while !self.done && step.frames < max_frames {
            let item = match self.parser.next_item() {
                Ok(Some(item)) => item,
                Ok(None) => {
                    step.starved = true;
                    self.starved = true;
                    break;
                }
                Err(error) => {
                    self.fail(&error);
                    break;
                }
            };
            match item {
                ParsedItem::Header(header) => match CaptureSession::from_header(&header, self.params) {
                    Ok(mut session) => {
                        step.messages.extend(session.snapshot());
                        self.session = Some(session);
                    }
                    Err(error) => self.fail(&error),
                },
                ParsedItem::Frame(bytes) => {
                    let frame = match decode_frame(bytes) {
                        Ok(frame) => frame,
                        Err(error) => {
                            self.fail(&error);
                            break;
                        }
                    };
                    step.frames += 1;
                    // The parser hands out the header first.
                    let Some(session) = &mut self.session else { continue };
                    step.messages.extend(session.handle_decoded(&frame).to_viewers);
                    self.since_progress += 1;
                    if self.since_progress >= FRAMES_PER_PROGRESS {
                        self.since_progress = 0;
                        step.messages.extend(session.progress().to_viewers);
                    }
                }
            }
        }
        step
    }

    /// The file has been read to its end: runs the final pass of the
    /// session. Call it once [`LocalCapture::step`] reports `starved` after
    /// the last bytes were pushed.
    pub fn finish(&mut self) -> Vec<Outgoing> {
        if self.done {
            return Vec::new();
        }
        self.done = true;
        match self.parser.finish() {
            Ok(end) => self.truncated = end.truncated,
            Err(error) => {
                self.fail(&error);
                return Vec::new();
            }
        }
        match &mut self.session {
            Some(session) => session.finish().to_viewers,
            None => {
                self.error = Some("bad_header".to_string());
                Vec::new()
            }
        }
    }

    /// What a viewer that joins now is sent (a resynchronisation); empty
    /// before the header has been read.
    pub fn snapshot(&mut self) -> Vec<Outgoing> {
        self.session.as_mut().map(CaptureSession::snapshot).unwrap_or_default()
    }

    pub fn progress(&self) -> CaptureProgress {
        CaptureProgress {
            name: self.session.as_ref().map(|s| s.name().to_string()).unwrap_or_default(),
            bytes: self.bytes,
            total: self.total,
            frames: self.parser.frames(),
            used: self.session.as_ref().map_or(0, |s| u64::from(s.frames_used())),
            done: self.done,
            truncated: self.truncated,
            error: self.error.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use gs_core::codec::{self, Kind};
    use gs_core::ingest::{encode_frame, file::encode_file_header, CaptureFrame, GscapHeader, Orientation, Tracking};
    use gs_core::protocol::{SessionState, Stats};

    use super::*;

    /// A frame looking at a wall 2 m ahead from `x` meters to the right,
    /// with a flat colour image.
    fn frame(index: u32, x: f32) -> Vec<u8> {
        sized_frame(index, x, 64, 48, true)
    }

    /// The same with a depth map of `w` x `h`, stored deflated or not.
    fn sized_frame(index: u32, x: f32, w: u16, h: u16, compressed: bool) -> Vec<u8> {
        let mut world_from_cam = [0.0f32; 16];
        for i in [0, 5, 10, 15] {
            world_from_cam[i] = 1.0;
        }
        world_from_cam[12] = x;
        world_from_cam[13] = 1.4;
        let frame = CaptureFrame {
            index,
            timestamp: f64::from(index) * 0.1,
            world_from_cam,
            intrinsics: [f32::from(w) * 0.94, f32::from(w) * 0.94, f32::from(w) / 2.0, f32::from(h) / 2.0],
            tracking: Tracking::Normal,
            orientation: Orientation::LandscapeNative,
            color_width: w,
            color_height: h,
            color: jpeg(w, h),
            depth_width: w,
            depth_height: h,
            depth_mm: vec![2000; usize::from(w) * usize::from(h)],
            confidence: Some(vec![2; usize::from(w) * usize::from(h)]),
            mask: None,
            compressed,
            reserved: 0,
        };
        encode_frame(&frame).unwrap()
    }

    /// A tiny grey baseline JPEG of the given size, written by hand: the
    /// library has no encoder and the fixtures live in another package.
    fn jpeg(width: u16, height: u16) -> Vec<u8> {
        let mut out = vec![0xFF, 0xD8];
        // Quantisation table: all ones.
        out.extend_from_slice(&[0xFF, 0xDB, 0x00, 0x43, 0x00]);
        out.extend_from_slice(&[1u8; 64]);
        // Baseline frame, one grey component.
        out.extend_from_slice(&[0xFF, 0xC0, 0x00, 0x0B, 0x08]);
        out.extend_from_slice(&height.to_be_bytes());
        out.extend_from_slice(&width.to_be_bytes());
        out.extend_from_slice(&[0x01, 0x01, 0x11, 0x00]);
        // DC table with the single code "0" for category 0, AC table with
        // the single code "0" for end of block.
        for class in [0x00u8, 0x10] {
            out.extend_from_slice(&[0xFF, 0xC4, 0x00, 0x14, class, 1]);
            out.extend_from_slice(&[0u8; 15]);
            out.push(0x00);
        }
        out.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00]);
        // Every block is "DC difference 0, end of block": two zero bits.
        let blocks = usize::from(width.div_ceil(8)) * usize::from(height.div_ceil(8));
        out.extend(std::iter::repeat_n(0u8, (blocks * 2).div_ceil(8)));
        out.extend_from_slice(&[0xFF, 0xD9]);
        out
    }

    fn capture_file(frames: u32) -> Vec<u8> {
        let header = GscapHeader { mode: "room".into(), name: "Salon".into(), device: "iPhone17,2".into(), ..Default::default() };
        let mut file = encode_file_header(&header, 0).unwrap();
        for i in 0..frames {
            let bytes = frame(i, i as f32 * 0.05);
            file.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            file.extend_from_slice(&bytes);
        }
        file
    }

    /// Runs an import the way the worker does: small pieces, a few frames
    /// per step.
    fn import(file: &[u8], piece: usize) -> (LocalCapture, Vec<Outgoing>) {
        let mut capture = LocalCapture::new(file.len() as u64);
        let mut messages = Vec::new();
        for chunk in file.chunks(piece) {
            capture.push(chunk);
            loop {
                let step = capture.step(2);
                messages.extend(step.messages);
                if step.starved || capture.is_done() {
                    break;
                }
            }
        }
        messages.extend(capture.finish());
        (capture, messages)
    }

    fn last_json<T: serde::de::DeserializeOwned>(messages: &[Outgoing], kind: Kind) -> T {
        let bytes = &messages.iter().rev().find(|m| codec::peek_kind(&m.bytes) == Ok(kind)).expect("message").bytes;
        match codec::decode(bytes).unwrap().body {
            codec::Body::State(json) | codec::Body::Stats(json) => serde_json::from_str(&json).unwrap(),
            _ => unreachable!(),
        }
    }

    #[test]
    fn a_capture_file_becomes_a_capture_session_step_by_step() {
        let file = capture_file(24);
        let (capture, messages) = import(&file, 5000);
        let progress = capture.progress();
        assert_eq!((progress.frames, progress.done, progress.truncated, &progress.error), (24, true, false, &None));
        assert_eq!((progress.name.as_str(), progress.bytes, progress.total), ("Salon", file.len() as u64, file.len() as u64));
        assert!(progress.used >= 20, "{} frames used", progress.used);

        // The viewer first gets the empty session, then sees it grow.
        assert_eq!(codec::peek_kind(&messages[0].bytes), Ok(Kind::State));
        let state: SessionState = last_json(&messages, Kind::State);
        let capture_info = state.capture.expect("a capture session");
        assert_eq!((state.scene.as_str(), capture_info.name.as_str(), capture_info.imported), ("capture", "Salon", true));
        let stats: Stats = last_json(&messages, Kind::Stats);
        let capture_stats = stats.capture.expect("capture statistics");
        assert!(capture_stats.finished && capture_stats.frames_received == 24 && stats.splats_static > 500, "{stats:?}");
        let appends = messages.iter().filter(|m| codec::peek_kind(&m.bytes) == Ok(Kind::Layer)).count();
        assert!(appends > 10, "{appends} layer messages");
    }

    #[test]
    fn the_result_does_not_depend_on_how_the_file_is_cut() {
        let file = capture_file(12);
        let count = |piece: usize| {
            let (mut capture, _) = import(&file, piece);
            let snapshot = capture.snapshot();
            let stats: Stats = last_json(&snapshot, Kind::Stats);
            (capture.progress().frames, capture.progress().used, stats.splats_static)
        };
        let whole = count(file.len());
        assert_eq!(count(1), whole);
        assert_eq!(count(777), whole);
        assert!(whole.2 > 0);
    }

    /// Runs an import with the reading policy of the worker's glue: one
    /// frame per step, and a piece of the file whenever
    /// [`LocalCapture::wants_bytes`] says so. Panics if it stops advancing.
    fn host_import(file: &[u8], piece: usize, low_water: usize) -> LocalCapture {
        let mut capture = LocalCapture::new(file.len() as u64);
        let mut offset = 0;
        let mut turns = 0;
        while !capture.is_done() {
            turns += 1;
            assert!(turns < 100_000, "the import stalls at byte {offset} of {}, {} frames in", file.len(), capture.progress().frames);
            let step = capture.step(1);
            if step.starved && offset >= file.len() {
                capture.finish();
            } else if capture.wants_bytes(low_water) && offset < file.len() {
                let end = (offset + piece).min(file.len());
                capture.push(&file[offset..end]);
                offset = end;
            }
        }
        capture
    }

    #[test]
    fn frames_larger_than_the_read_buffer_are_read_to_their_end() {
        // What a phone sends: a frame record of a few hundred kilobytes,
        // several times the piece the host reads and its low-water mark.
        let header = GscapHeader { mode: "room".into(), name: "Salon".into(), ..Default::default() };
        let mut file = encode_file_header(&header, 0).unwrap();
        let mut largest = 0;
        for i in 0..6 {
            let bytes = sized_frame(i, i as f32 * 0.05, 256, 192, false);
            largest = largest.max(bytes.len());
            file.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            file.extend_from_slice(&bytes);
        }
        let (piece, low_water) = (16 * 1024, 4 * 1024);
        assert!(largest > 4 * (piece + low_water), "{largest} bytes per frame");
        let capture = host_import(&file, piece, low_water);
        let progress = capture.progress();
        assert_eq!((progress.frames, progress.bytes, &progress.error, progress.truncated), (6, file.len() as u64, &None, false));
        assert!(progress.used >= 5, "{} frames used", progress.used);
        // Never much more than one record is buffered on the way.
        let mut capture = LocalCapture::new(file.len() as u64);
        let (mut offset, mut peak) = (0, 0);
        while !capture.is_done() {
            let step = capture.step(1);
            if step.starved && offset >= file.len() {
                capture.finish();
            } else if capture.wants_bytes(low_water) && offset < file.len() {
                let end = (offset + piece).min(file.len());
                capture.push(&file[offset..end]);
                offset = end;
                peak = peak.max(capture.buffered());
            }
        }
        assert!(peak <= largest + 4 + piece + low_water, "{peak} bytes buffered, frames of {largest}");
        // The small files of the other tests behave the same under this policy.
        assert_eq!(host_import(&capture_file(5), 1024, 64 * 1024).progress().frames, 5);
        let file = capture_file(3);
        assert!(host_import(&file[..file.len() - 100], 512, 64).progress().truncated);
    }

    #[test]
    fn a_file_cut_short_keeps_its_complete_frames() {
        let file = capture_file(6);
        let (capture, messages) = import(&file[..file.len() - 100], 4096);
        let progress = capture.progress();
        assert_eq!((progress.frames, progress.truncated, progress.done, &progress.error), (5, true, true, &None));
        let stats: Stats = last_json(&messages, Kind::Stats);
        assert!(stats.capture.unwrap().finished);
    }

    #[test]
    fn damaged_files_end_the_import_with_a_code_and_never_panic() {
        // Not a capture at all.
        let (capture, messages) = import(b"GSREC1\0\0 this is another format", 7);
        assert_eq!(capture.progress().error.as_deref(), Some("bad_file_magic"));
        assert!(messages.is_empty());
        // Empty.
        let (capture, _) = import(b"", 1);
        assert!(capture.progress().error.is_some() && capture.progress().done);
        // A damaged frame record after three good ones: what was fused stays.
        let mut file = capture_file(4);
        let good = capture_file(3).len();
        // First byte of the fourth frame's magic, after its length field.
        file[good + 4] ^= 0xFF;
        let (mut capture, _) = import(&file, 1000);
        let progress = capture.progress();
        assert_eq!((progress.error.as_deref(), progress.done), (Some("bad_magic"), true));
        assert!(!capture.snapshot().is_empty());
        // Every single-byte truncation is survived.
        let file = capture_file(2);
        for cut in (0..file.len()).step_by(97) {
            let (capture, _) = import(&file[..cut], 64);
            assert!(capture.progress().done);
        }
        // A file that is not a capture never gets a session; a capture
        // has one as soon as its header is read, before any frame.
        let mut capture = LocalCapture::new(32);
        capture.push(b"GSREC1\0\0 this is another format");
        capture.step(1);
        assert!(capture.is_done() && !capture.has_session());
        let file = capture_file(2);
        let mut capture = LocalCapture::new(file.len() as u64);
        capture.push(&file[..600]);
        assert!(!capture.has_session());
        let step = capture.step(1);
        assert!(capture.has_session() && step.starved && step.frames == 0 && !step.messages.is_empty());
        // Nothing happens after the end.
        let (mut capture, _) = import(&capture_file(1), 64);
        capture.push(b"more");
        assert!(capture.step(10).messages.is_empty() && capture.finish().is_empty());
    }
}
