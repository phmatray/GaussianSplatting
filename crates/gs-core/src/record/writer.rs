//! Writing a recording from the messages of a session.

use std::convert::Infallible;
use std::fmt;
use std::io::{Seek, SeekFrom, Write};

use super::state::{scene_of, StreamState};
use super::wire::MessageClass;
use super::{
    write_record_header, RecordError, RecordKind, END_MAGIC, FLAG_DEFLATE, HEADER_JSON_SIZE, INDEX_ENTRY_SIZE, MAGIC,
    MAX_RECORD_LEN, RECORD_HEADER_SIZE, TRAILER_SIZE,
};
use crate::protocol::PROTOCOL_VERSION;

/// Where a [`Recorder`] puts the file: something that grows at its end and
/// lets a few bytes near its start be rewritten (the JSON header, once the
/// duration is known).
pub trait RecordSink {
    type Error;

    /// Appends `bytes` to the end of the file.
    fn append_bytes(&mut self, bytes: &[u8]) -> Result<(), Self::Error>;

    /// Overwrites `bytes.len()` bytes at `offset`, inside what has been
    /// appended so far. The next [`RecordSink::append_bytes`] still goes to the
    /// end.
    fn patch_bytes(&mut self, offset: u64, bytes: &[u8]) -> Result<(), Self::Error>;

    /// Hands everything written so far to the medium.
    fn flush(&mut self) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// A recording in memory.
impl RecordSink for Vec<u8> {
    type Error = Infallible;

    fn append_bytes(&mut self, bytes: &[u8]) -> Result<(), Infallible> {
        self.extend_from_slice(bytes);
        Ok(())
    }

    fn patch_bytes(&mut self, offset: u64, bytes: &[u8]) -> Result<(), Infallible> {
        // A recorder only patches what it wrote; anything else is ignored
        // rather than trusted.
        let target =
            usize::try_from(offset).ok().and_then(|start| self.get_mut(start..start.checked_add(bytes.len())?));
        if let Some(target) = target {
            target.copy_from_slice(bytes);
        }
        Ok(())
    }
}

/// A recording in a file, or in anything else that writes and seeks
/// (`std::fs::File`, `BufWriter<File>`, `Cursor<Vec<u8>>`). The writer must
/// be positioned at its end, which is where the recording starts.
#[derive(Debug)]
pub struct IoSink<W>(pub W);

impl<W: Write + Seek> RecordSink for IoSink<W> {
    type Error = std::io::Error;

    fn append_bytes(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.0.write_all(bytes)
    }

    fn patch_bytes(&mut self, offset: u64, bytes: &[u8]) -> std::io::Result<()> {
        let end = self.0.stream_position()?;
        self.0.seek(SeekFrom::Start(offset))?;
        self.0.write_all(bytes)?;
        self.0.seek(SeekFrom::Start(end))?;
        Ok(())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

/// How record payloads are stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Compression {
    /// As they are.
    None,
    /// Raw deflate at this level (1 to 10), per record, kept only when it
    /// saves at least a ninth of the payload.
    Deflate(u8),
}

impl Default for Compression {
    /// Level 1: see the module documentation for the measurements.
    fn default() -> Self {
        Self::Deflate(1)
    }
}

/// When a recorder writes a keyframe.
///
/// A keyframe is due once the records written since the last one weigh
/// `ratio` times what a keyframe would weigh now, but never before
/// `min_interval_s` seconds have passed. It is written at the end of a
/// tick (after a `Stats` message), or anywhere once it is overdue by a
/// factor of two.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KeyframePolicy {
    /// Shortest time between two keyframes in seconds.
    pub min_interval_s: f64,
    /// Bytes of records per byte of keyframe. A seek reads at most about
    /// `ratio + 1` snapshots' worth of bytes; keyframes add at most
    /// `1 / ratio` to the file.
    pub ratio: f64,
}

impl Default for KeyframePolicy {
    /// One second, ratio 8: one keyframe every 1.2 s of the default live
    /// stage (8 % of the file) and none during a scan.
    fn default() -> Self {
        Self { min_interval_s: 1.0, ratio: 8.0 }
    }
}

/// What a [`Recorder`] writes and how.
#[derive(Clone, Debug, PartialEq)]
pub struct RecorderOptions {
    /// Name of the recording, for the header.
    pub name: String,
    /// Start of the recording, RFC 3339, for the header.
    pub created: String,
    /// Scene identifier for the header. `None` takes the `scene` of the
    /// first `State` message.
    pub scene: Option<String>,
    pub compression: Compression,
    /// Record `Stats` messages (default: yes).
    pub stats: bool,
    /// Record `Preview` messages (default: yes).
    pub previews: bool,
    pub keyframes: KeyframePolicy,
    /// Largest file the recorder produces, index and trailer included.
    /// [`Recorder::push`] answers [`Pushed::Full`] once the next record
    /// would not fit. `None`: no limit.
    pub max_bytes: Option<u64>,
}

impl RecorderOptions {
    /// A bound for recordings kept in the memory of a browser tab: 256 MiB,
    /// 45 s of the default stage (5.8 MB/s) or two minutes of the light
    /// one (2 MB/s), and many complete house scans (17 MB each).
    pub const MEMORY_BOUND: u64 = 256 << 20;

    /// Default options for a recording called `name` that starts now.
    pub fn named(name: impl Into<String>) -> Self {
        let now = web_time::SystemTime::now().duration_since(web_time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
        Self {
            name: name.into(),
            created: rfc3339_utc(now),
            scene: None,
            compression: Compression::default(),
            stats: true,
            previews: true,
            keyframes: KeyframePolicy::default(),
            max_bytes: None,
        }
    }

    /// The same options with a size limit.
    pub fn with_max_bytes(mut self, max_bytes: u64) -> Self {
        self.max_bytes = Some(max_bytes);
        self
    }
}

/// `seconds` since the Unix epoch as an RFC 3339 time in UTC, to the
/// second: `2026-11-14T10:32:07Z`. Times before 1970 or not finite give
/// the epoch.
pub fn rfc3339_utc(seconds: f64) -> String {
    let total = if seconds.is_finite() && seconds > 0.0 { seconds.min(253_402_300_799.0) as u64 } else { 0 };
    let (days, rest) = (total / 86_400, total % 86_400);
    // Civil date from a day count (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let (era, doe) = (z / 146_097, z % 146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", rest / 3600, rest % 3600 / 60, rest % 60)
}

/// Why a recorder stopped.
#[derive(Debug, PartialEq, Eq)]
pub enum WriteError<E> {
    /// The sink failed. The file ends where it failed and still plays up
    /// to its last complete record.
    Sink(E),
    /// The session cannot be recorded any further (see [`RecordError`]).
    Record(RecordError),
    /// An earlier call failed; the recorder writes nothing more.
    Stopped,
}

impl<E: fmt::Display> fmt::Display for WriteError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sink(error) => write!(f, "writing the recording failed: {error}"),
            Self::Record(error) => write!(f, "{error}"),
            Self::Stopped => f.write_str("the recorder stopped after an earlier error"),
        }
    }
}

impl<E: fmt::Debug + fmt::Display> std::error::Error for WriteError<E> {}

/// What [`Recorder::push`] did with a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pushed {
    /// The message is part of the recording.
    Recorded,
    /// The message is part of the recording and a keyframe was written.
    Keyframe,
    /// The message is not recorded: a `Stats` or `Preview` the options
    /// leave out.
    Skipped,
    /// The message is not a valid wire message and was left out.
    Rejected,
    /// The recording reached [`RecorderOptions::max_bytes`]: nothing more
    /// is written. Call [`Recorder::finish`].
    Full,
}

/// What a finished recording holds.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Summary {
    /// Size of the file in bytes.
    pub bytes: u64,
    /// Length in seconds.
    pub duration_s: f64,
    /// Scene identifier written to the header.
    pub scene: String,
    /// Message records.
    pub messages: u64,
    /// Keyframe records.
    pub keyframes: u64,
    /// Bytes the payloads of all records had before compression.
    pub raw_bytes: u64,
    /// Messages left out because they were not valid wire messages.
    pub rejected: u64,
    /// The recording stopped at [`RecorderOptions::max_bytes`].
    pub full: bool,
}

/// Writes a `.gsrec` recording from the stream a viewer receives.
///
/// Feed it every message of the session, starting with what a viewer that
/// connects now gets (`Pipeline::snapshot()`, or a [`StreamState`] kept
/// since the start of the session through [`Recorder::with_state`]), each
/// with the time it was sent on any clock that counts seconds. Then call
/// [`Recorder::finish`]. A recorder that is dropped, or whose sink fails,
/// leaves a file without index that still plays.
///
/// Nothing is written before the first `State` message is known: an
/// empty file is not a recording.
pub struct Recorder<S: RecordSink> {
    sink: S,
    options: RecorderOptions,
    state: StreamState,
    /// Bytes handed to the sink.
    written: u64,
    /// Time and offset of every keyframe.
    index: Vec<(f64, u64)>,
    /// The caller's time of the first record.
    origin: Option<f64>,
    /// The caller's time of the first message that waits for the first
    /// keyframe.
    waiting_since: Option<f64>,
    /// Time of the last record, rebased.
    time: f64,
    /// Stored bytes of the records since the last keyframe.
    since_keyframe: u64,
    keyframe_time: f64,
    /// Stored size over raw size of the last keyframe.
    keyframe_ratio: f64,
    scene: String,
    messages: u64,
    raw_bytes: u64,
    rejected: u64,
    full: bool,
    stopped: bool,
    scratch: Vec<u8>,
}

impl<S: RecordSink> Recorder<S> {
    /// A recorder that has seen nothing of the session yet.
    pub fn new(sink: S, options: RecorderOptions) -> Self {
        Self::with_state(sink, options, StreamState::new())
    }

    /// A recorder that starts in the middle of a session: `state` is what
    /// the stream has said so far, and becomes the first keyframe.
    pub fn with_state(sink: S, options: RecorderOptions, mut state: StreamState) -> Self {
        state.forget(!options.stats, !options.previews);
        Self {
            sink,
            options,
            state,
            written: 0,
            index: Vec::new(),
            origin: None,
            waiting_since: None,
            time: 0.0,
            since_keyframe: 0,
            keyframe_time: 0.0,
            keyframe_ratio: 1.0,
            scene: String::new(),
            messages: 0,
            raw_bytes: 0,
            rejected: 0,
            full: false,
            stopped: false,
            scratch: Vec::new(),
        }
    }

    /// Records one wire message sent at `time_s` (seconds on any clock;
    /// the recording starts at 0 with its first record).
    pub fn push(&mut self, time_s: f64, message: &[u8]) -> Result<Pushed, WriteError<S::Error>> {
        if self.stopped {
            return Err(WriteError::Stopped);
        }
        if self.full {
            return Ok(Pushed::Full);
        }
        let Some(class) = super::classify(message) else {
            self.rejected += 1;
            return Ok(Pushed::Rejected);
        };
        let recorded = match class {
            MessageClass::Stats => self.options.stats,
            MessageClass::Preview(_) => self.options.previews,
            MessageClass::State | MessageClass::Layer { .. } => true,
        };
        if recorded {
            if let Err(error) = self.state.apply(message) {
                self.full = true;
                return Err(WriteError::Record(error));
            }
        }
        // The end of a tick: the state is whole, a good place for a keyframe.
        let boundary = class == MessageClass::Stats;

        if self.index.is_empty() {
            // The first keyframe waits for the end of the burst that
            // describes the session (State, layers, Stats: one instant).
            if !self.state.has_state() {
                return Ok(if recorded { Pushed::Recorded } else { Pushed::Skipped });
            }
            let since = *self.waiting_since.get_or_insert(time_s);
            if !(boundary || time_s > since) {
                return Ok(if recorded { Pushed::Recorded } else { Pushed::Skipped });
            }
            self.origin = Some(if time_s.is_finite() { time_s } else { since });
            self.write_keyframe(0.0)?;
            return Ok(if self.full { Pushed::Full } else { Pushed::Keyframe });
        }

        let time = self.rebase(time_s);
        if recorded {
            if self.write_record(RecordKind::Message, time, message)? == 0 {
                return Ok(Pushed::Full);
            }
            self.messages += 1;
        }
        if self.keyframe_due(time, if boundary { 1.0 } else { 2.0 }) {
            self.write_keyframe(time)?;
            // A keyframe that did not fit ends the recording, but the
            // message itself is in it.
            if !self.full {
                return Ok(Pushed::Keyframe);
            }
        }
        Ok(if recorded { Pushed::Recorded } else { Pushed::Skipped })
    }

    /// Writes the index, the trailer and the final header, and returns the
    /// sink with a description of the file.
    ///
    /// A recorder that never saw a `State` message has written nothing:
    /// the summary reports zero bytes and the sink is as it was.
    pub fn finish(mut self) -> Result<(S, Summary), WriteError<S::Error>> {
        if self.stopped {
            return Err(WriteError::Stopped);
        }
        if self.index.is_empty() && self.state.has_state() && !self.full {
            self.origin = self.waiting_since.or(Some(0.0));
            self.write_keyframe(0.0)?;
        }
        if self.written > 0 {
            let offset = self.written;
            let mut record =
                Vec::with_capacity(RECORD_HEADER_SIZE + self.index.len() * INDEX_ENTRY_SIZE + TRAILER_SIZE);
            write_record_header(
                &mut record,
                RecordKind::Index,
                0,
                self.time,
                (self.index.len() * INDEX_ENTRY_SIZE) as u32,
            );
            for (time, at) in &self.index {
                record.extend_from_slice(&time.to_le_bytes());
                record.extend_from_slice(&at.to_le_bytes());
            }
            record.extend_from_slice(&offset.to_le_bytes());
            record.extend_from_slice(&END_MAGIC);
            self.append(&record)?;
            let header = self.header_json(self.time);
            self.sink.patch_bytes(12, &header).map_err(WriteError::Sink)?;
        }
        self.sink.flush().map_err(WriteError::Sink)?;
        let summary = Summary {
            bytes: self.written,
            duration_s: self.time,
            scene: self.scene.clone(),
            messages: self.messages,
            keyframes: self.index.len() as u64,
            raw_bytes: self.raw_bytes,
            rejected: self.rejected,
            full: self.full,
        };
        Ok((self.sink, summary))
    }

    /// Bytes written so far.
    pub fn bytes_written(&self) -> u64 {
        self.written
    }

    /// Time of the last record in seconds: the length of the recording so
    /// far.
    pub fn duration_s(&self) -> f64 {
        self.time
    }

    /// Keyframes written so far.
    pub fn keyframes(&self) -> usize {
        self.index.len()
    }

    /// True once [`RecorderOptions::max_bytes`] stopped the recording.
    pub fn is_full(&self) -> bool {
        self.full
    }

    /// The session as the recorder knows it.
    pub fn state(&self) -> &StreamState {
        &self.state
    }

    /// The sink, for example to look at a recording in memory while it
    /// grows.
    pub fn sink(&self) -> &S {
        &self.sink
    }

    /// The time of a record on the recording's own clock: it starts at 0
    /// and never runs backwards.
    fn rebase(&mut self, time_s: f64) -> f64 {
        let origin = self.origin.unwrap_or(time_s);
        let time = time_s - origin;
        if time.is_finite() && time > self.time {
            self.time = time;
        }
        self.time
    }

    fn keyframe_due(&self, time: f64, overdue: f64) -> bool {
        let policy = &self.options.keyframes;
        let ratio = if policy.ratio.is_finite() { policy.ratio.max(0.0) } else { f64::INFINITY };
        let estimate = self.state.snapshot_bytes() as f64 * self.keyframe_ratio;
        time - self.keyframe_time >= policy.min_interval_s && self.since_keyframe as f64 >= ratio * overdue * estimate
    }

    fn write_keyframe(&mut self, time: f64) -> Result<(), WriteError<S::Error>> {
        let Some(snapshot) = self.state.snapshot() else { return Ok(()) };
        if self.written == 0 && self.scene.is_empty() {
            self.scene = self.options.scene.clone().unwrap_or_else(|| scene_of(&snapshot[0]));
        }
        let mut payload = Vec::with_capacity(snapshot.iter().map(|m| m.len() + 4).sum());
        for message in &snapshot {
            payload.extend_from_slice(&(message.len() as u32).to_le_bytes());
            payload.extend_from_slice(message);
        }
        let stored = self.write_record(RecordKind::Keyframe, time, &payload)?;
        if stored > 0 {
            self.index.push((time, self.written - stored));
            self.keyframe_time = time;
            self.keyframe_ratio = stored as f64 / (RECORD_HEADER_SIZE + payload.len()) as f64;
            self.since_keyframe = 0;
        }
        Ok(())
    }

    /// Appends one record, compressed if that pays, and returns its size
    /// in the file. When it does not fit below the size limit, nothing is
    /// written, `full` is set and the size is 0.
    fn write_record(&mut self, kind: RecordKind, time: f64, payload: &[u8]) -> Result<u64, WriteError<S::Error>> {
        let mut record = std::mem::take(&mut self.scratch);
        record.clear();
        if self.written == 0 {
            // The file header goes out with the first record, or not at
            // all: a header alone is not a recording.
            record.extend_from_slice(&MAGIC);
            record.extend_from_slice(&(HEADER_JSON_SIZE as u32).to_le_bytes());
            record.extend_from_slice(&self.header_json(0.0));
        }
        let header_len = record.len();
        let packed = match self.options.compression {
            // Tiny payloads cannot win back the cost of a deflate block.
            Compression::Deflate(level) if payload.len() >= 64 => {
                let packed = miniz_oxide::deflate::compress_to_vec(payload, level.clamp(1, 10));
                (packed.len() + packed.len() / 8 <= payload.len()).then_some(packed)
            }
            _ => None,
        };
        let (flags, stored) = match &packed {
            Some(packed) => (FLAG_DEFLATE, packed.as_slice()),
            None => (0, payload),
        };
        let size = (RECORD_HEADER_SIZE + stored.len()) as u64;
        let result = if stored.len() as u64 + 12 > MAX_RECORD_LEN as u64 {
            self.full = true;
            Err(WriteError::Record(RecordError::TooLarge))
        } else if !self.has_room(header_len as u64 + size) {
            self.full = true;
            Ok(0)
        } else {
            write_record_header(&mut record, kind, flags, time, stored.len() as u32);
            record.extend_from_slice(stored);
            self.raw_bytes += payload.len() as u64;
            if kind == RecordKind::Message {
                self.since_keyframe += size;
            }
            self.append(&record).map(|()| size)
        };
        self.scratch = record;
        result
    }

    /// True when `bytes` more fit below the size limit together with the
    /// index and the trailer that must follow them.
    fn has_room(&self, bytes: u64) -> bool {
        let tail = (RECORD_HEADER_SIZE + (self.index.len() + 1) * INDEX_ENTRY_SIZE + TRAILER_SIZE) as u64;
        self.options.max_bytes.is_none_or(|max| self.written + bytes + tail <= max)
    }

    fn append(&mut self, bytes: &[u8]) -> Result<(), WriteError<S::Error>> {
        match self.sink.append_bytes(bytes) {
            Ok(()) => {
                self.written += bytes.len() as u64;
                Ok(())
            }
            Err(error) => {
                self.stopped = true;
                Err(WriteError::Sink(error))
            }
        }
    }

    /// The JSON header, padded with spaces to [`HEADER_JSON_SIZE`] bytes.
    fn header_json(&self, duration_s: f64) -> Vec<u8> {
        let mut name: &str = &self.options.name;
        let mut scene: &str = &self.scene;
        loop {
            let json = serde_json::json!({
                "name": name,
                "created": self.options.created,
                "scene": scene,
                "protocol": PROTOCOL_VERSION,
                "duration_s": duration_s,
            })
            .to_string();
            if json.len() <= HEADER_JSON_SIZE {
                let mut bytes = json.into_bytes();
                bytes.resize(HEADER_JSON_SIZE, b' ');
                return bytes;
            }
            // Too long for the fixed header: cut the name, then the scene,
            // then (a creation time of hundreds of bytes) give up on both.
            let cut = |text: &str| {
                let mut end = text.len() / 2;
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                end
            };
            if !name.is_empty() {
                name = &name[..cut(name)];
            } else if !scene.is_empty() {
                scene = &scene[..cut(scene)];
            } else {
                let mut bytes = format!(r#"{{"protocol":{PROTOCOL_VERSION},"duration_s":{duration_s}}}"#).into_bytes();
                bytes.resize(HEADER_JSON_SIZE, b' ');
                return bytes;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_matches_known_dates() {
        assert_eq!(rfc3339_utc(0.0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(951_782_399.0), "2000-02-28T23:59:59Z");
        assert_eq!(rfc3339_utc(951_782_400.9), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339_utc(1_794_652_327.0), "2026-11-14T10:32:07Z");
        assert_eq!(rfc3339_utc(4_102_444_800.0), "2100-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(f64::NAN), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(-5.0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339_utc(1e30), "9999-12-31T23:59:59Z");
        let named = RecorderOptions::named("x");
        assert_eq!(named.created.len(), 20);
        assert!(named.created.as_str() > "2025" && named.stats && named.previews && named.max_bytes.is_none());
    }

    #[test]
    fn memory_sinks_patch_in_place_and_ignore_what_is_outside() {
        let mut sink = b"0123456789".to_vec();
        sink.patch_bytes(2, b"ab").unwrap();
        sink.patch_bytes(9, b"xy").unwrap();
        sink.patch_bytes(u64::MAX, b"z").unwrap();
        sink.append_bytes(b"!").unwrap();
        assert_eq!(sink, b"01ab456789!");

        let mut io = IoSink(std::io::Cursor::new(Vec::new()));
        io.append_bytes(b"0123456789").unwrap();
        io.patch_bytes(2, b"ab").unwrap();
        io.append_bytes(b"!").unwrap();
        io.flush().unwrap();
        assert_eq!(io.0.into_inner(), b"01ab456789!");
    }
}
