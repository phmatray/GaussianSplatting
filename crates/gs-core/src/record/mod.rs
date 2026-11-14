//! 4D recording and replay: the `.gsrec` format.
//!
//! A recording is the stream a viewer receives, with time stamps, so that it
//! replays in any viewer without the pipeline. See "4D recording and
//! sharing" in `docs/ARCHITECTURE.md`; `tools/gsrec.py` is an independent
//! implementation of the same format.
//!
//! * [`Recorder`] writes one, to a file ([`IoSink`]) or to memory
//!   (`Vec<u8>`), from the wire messages of a session.
//! * [`Player`] replays one against a clock: from the bytes of a whole
//!   file, from a download in progress, or from HTTP `Range` requests.
//! * [`verify`] checks a whole file and describes it.
//! * [`StreamState`] is what both of them know about a session: the
//!   snapshot a viewer would need to join now.
//!
//! Nothing here does I/O or reads a clock; the module builds for
//! `wasm32-unknown-unknown`.
//!
//! # File layout (little-endian)
//!
//! | Size | Field |
//! |---|---|
//! | 8 | magic `GSREC1\0\0` |
//! | 4 | `u32` length of the JSON header |
//! | … | JSON object: `name`, `created` (RFC 3339), `scene`, `protocol`, `duration_s` (0 while unknown). May be padded with spaces inside its length; unknown keys are ignored |
//! | … | records |
//! | 8 | `u64` file offset of the index record |
//! | 8 | `GSRECEND` |
//!
//! A record is a `u32` length of what follows, then `u8` kind, `u8` flags
//! (bit 0: the payload is raw-deflate compressed, RFC 1951), `u16` reserved,
//! `f64` time in seconds since the start, then the payload.
//!
//! * kind 0, *message*: one wire message ([`crate::codec`]) exactly as sent
//!   to viewers.
//! * kind 1, *keyframe*: a complete snapshot, each message prefixed by its
//!   `u32` length: `State`, layer 0 `Replace`, layer 1 `Replace`, then
//!   optionally the latest `Stats` and the latest `Preview` of every camera.
//! * kind 2, *index*: `(f64 time, u64 file offset)` of every keyframe. It
//!   is the last record; its own time is the duration of the recording.
//!
//! # Decisions where the specification is silent
//!
//! * **A keyframe is pure redundancy.** It holds the state reached by the
//!   records before it and adds nothing to it, so forward playback skips
//!   keyframes and a seek starts at the last one whose time is not after
//!   the wanted time. Records are in time order; a keyframe carries the
//!   time of the record before it.
//! * **Keyframes may carry more than three messages.** A viewer that joins
//!   a live session also gets the latest `Stats`; without them a seek on a
//!   paused player would leave the statistics, the sensor gizmo and the
//!   thumbnails of another moment on screen. Readers accept any `Stats` and
//!   `Preview` messages after the three required ones.
//! * **Layer 0 of a keyframe is one `Replace`.** When the session built the
//!   layer from `Append` chunks (a scan), they are merged: every chunk
//!   quantizes positions inside its own box, so the positions are
//!   requantized into the union of the boxes (at most half a step of that
//!   box away, 0.1 mm in a house) and everything else is copied bit for
//!   bit. A layer that is a single `Replace` is stored verbatim.
//! * **When keyframes are written** ([`KeyframePolicy`]): once the records
//!   since the last keyframe weigh eight times what a keyframe would, and
//!   never more often than once a second. A seek therefore never reads more
//!   than about nine snapshots' worth of bytes, and keyframes add at most
//!   an eighth to the file. A live stage gets one about every 1.2 s; a
//!   scan, whose snapshot grows as fast as its stream, keeps the one it
//!   started with ("a still scan is a recording with one keyframe").
//! * **What is recorded**: every `State` and `Layer` message, and by
//!   default every `Stats` and `Preview` too, because the replay shows the
//!   same panels as the live view. Measured on the default stage they are
//!   0.1 % and 1 % of the file; in a house scan the previews are a third
//!   of it (17 MB with, 11 MB without) and [`RecorderOptions::previews`]
//!   turns them off.
//! * **Compression**: deflate level 1, per record, kept only when it saves
//!   at least a ninth. Measured on one native core
//!   (`examples/gsrec_bench.rs`): the live layer of the stage shrinks to
//!   76 % at 175 MB/s and inflates at 300 MB/s, scan layers shrink to
//!   41 %, previews to 11 % (stage) and 43 % (house). The default stage
//!   records at 5.8 MB/s instead of 8.5 for 5 % of a core, a house scan
//!   takes 17 MB instead of 41. Level 6 gains three to five points for
//!   four times the work; transposing the 16-byte splat records before
//!   deflate gains ten points on the live layer, not enough to justify a
//!   second flag bit.
//! * **Header**: the writer pads the JSON to [`HEADER_JSON_SIZE`] bytes, so
//!   the first record starts at offset 1024 and `duration_s` and `scene`
//!   are patched in place when the recording ends.
//! * **Times** are rebased: the first record is at 0 whatever clock the
//!   caller uses; a time that goes backwards or is not a number takes the
//!   time of the record before it.
//! * **Damage**: a file that ends inside a record, or without the index, is
//!   a recording cut short and plays up to its last complete record. A
//!   record whose framing breaks the rules (see [`RecordError`] for the
//!   codes and their order) ends the playable part in the same way, since
//!   nothing behind it can be found; a record whose payload is bad (it
//!   does not inflate, it is not a message, it is not a snapshot) is
//!   skipped. A wrong index costs a player that depends on it (`Range`
//!   requests) a detour, never a wrong picture: a keyframe is only used
//!   if it is where the index says, with the time the index says. Nothing
//!   is allocated from an announced length before the bytes are there, a
//!   record is at most [`MAX_RECORD_LEN`] bytes and inflates to at most
//!   [`MAX_INFLATED`].
//! * **Statistics and thumbnails across a seek**: a keyframe carries the
//!   latest of them, so [`Update::reset`] tells the host to forget the
//!   ones on screen. A `State` of another scene drops them from the
//!   snapshot, as the app drops them from the screen.

mod player;
mod reader;
mod state;
mod wire;
mod writer;

use std::fmt;

pub use player::{ByteRange, Player, PlayerOptions, PlayerStats, Update};
pub use reader::{verify, Problem, Report};
pub use state::StreamState;
pub use wire::{classify, MessageClass};
pub use writer::{
    rfc3339_utc, Compression, IoSink, KeyframePolicy, Pushed, RecordSink, Recorder, RecorderOptions, Summary,
    WriteError,
};

/// First eight bytes of a recording.
pub const MAGIC: [u8; 8] = *b"GSREC1\0\0";
/// Last eight bytes of a complete recording.
pub const END_MAGIC: [u8; 8] = *b"GSRECEND";
/// Size of the trailer: the offset of the index record and [`END_MAGIC`].
pub const TRAILER_SIZE: usize = 16;
/// Bytes of a record before its payload: the length field and the fixed
/// fields it covers.
pub const RECORD_HEADER_SIZE: usize = 16;
/// Bytes of a record that its length field counts before the payload.
const RECORD_FIXED: u32 = 12;
/// Longest accepted JSON header.
pub const MAX_HEADER_JSON: usize = 1 << 20;
/// Size the writer pads its JSON header to; the first record of its files
/// starts at offset `12 + HEADER_JSON_SIZE` = 1024.
pub const HEADER_JSON_SIZE: usize = 1012;
/// Largest accepted record length field (128 MiB: eight million splats).
pub const MAX_RECORD_LEN: u32 = 128 << 20;
/// Largest size a compressed payload may inflate to.
pub const MAX_INFLATED: usize = 128 << 20;
/// Record flag: the payload is raw-deflate compressed.
pub const FLAG_DEFLATE: u8 = 1;
/// Size of one index entry: `f64` time, `u64` offset.
pub const INDEX_ENTRY_SIZE: usize = 16;

/// Why a recording, or a part of it, cannot be read. [`RecordError::code`]
/// is the stable name shared with `tools/gsrec.py`.
///
/// A record is checked in this order: `bad_length`, `bad_kind`,
/// `bad_flags`, `bad_time`, then its payload: `bad_deflate`, and
/// `bad_message`, `bad_keyframe` or `bad_index`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum RecordError {
    /// The data ends inside the magic or the JSON header.
    TooShort,
    /// The data does not start with [`MAGIC`].
    BadMagic,
    /// The JSON header is longer than [`MAX_HEADER_JSON`], not UTF-8 or
    /// not a JSON object.
    BadHeader,
    /// A record length below 12 or above [`MAX_RECORD_LEN`].
    BadLength,
    /// A record kind other than 0, 1 or 2.
    BadKind,
    /// Record flag bits 1 to 7 are set.
    BadFlags,
    /// A record time that is negative, not finite or earlier than the
    /// time of the record before it.
    BadTime,
    /// A compressed payload that does not inflate, or inflates to more
    /// than [`MAX_INFLATED`].
    BadDeflate,
    /// A message that a viewer would refuse: unknown kind, a layer whose
    /// size does not match its count, a layer id above 1, text that is not
    /// UTF-8.
    BadMessage,
    /// A keyframe whose payload is not a list of length-prefixed messages
    /// starting with `State`, layer 0 `Replace`, layer 1 `Replace` and
    /// continuing with `Stats` and `Preview` messages only; or a recording
    /// whose first record is not a keyframe.
    BadKeyframe,
    /// An index or a trailer that does not describe the keyframes of the
    /// file.
    BadIndex,
    /// Bytes after the trailer.
    TrailingData,
    /// The recording holds no keyframe: there is nothing to show.
    Empty,
    /// The session state outgrew what a recorder keeps in memory.
    TooLarge,
}

impl RecordError {
    /// Stable snake_case name of the error.
    pub fn code(self) -> &'static str {
        match self {
            Self::TooShort => "too_short",
            Self::BadMagic => "bad_magic",
            Self::BadHeader => "bad_header",
            Self::BadLength => "bad_length",
            Self::BadKind => "bad_kind",
            Self::BadFlags => "bad_flags",
            Self::BadTime => "bad_time",
            Self::BadDeflate => "bad_deflate",
            Self::BadMessage => "bad_message",
            Self::BadKeyframe => "bad_keyframe",
            Self::BadIndex => "bad_index",
            Self::TrailingData => "trailing_data",
            Self::Empty => "empty",
            Self::TooLarge => "too_large",
        }
    }
}

impl fmt::Display for RecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            Self::TooShort => "the data ends inside the file header",
            Self::BadMagic => "not a GSREC1 recording (bad magic)",
            Self::BadHeader => "the JSON header is not a JSON object of at most 1 MiB",
            Self::BadLength => "a record has an impossible length",
            Self::BadKind => "a record has an unknown kind",
            Self::BadFlags => "a record has unknown flag bits",
            Self::BadTime => "a record has a time that is not finite or goes backwards",
            Self::BadDeflate => "a compressed payload does not inflate",
            Self::BadMessage => "a recorded message is not a valid wire message",
            Self::BadKeyframe => "a keyframe is not a complete snapshot",
            Self::BadIndex => "the index does not describe the keyframes",
            Self::TrailingData => "there is data after the end marker",
            Self::Empty => "the recording holds no keyframe",
            Self::TooLarge => "the session state is too large to record",
        };
        f.write_str(text)
    }
}

impl std::error::Error for RecordError {}

/// The JSON header of a recording.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RecordingInfo {
    /// Name given by whoever recorded it.
    pub name: String,
    /// When the recording started, RFC 3339.
    pub created: String,
    /// Scene identifier of the session (`"stage"`, `"house"`, ...).
    pub scene: String,
    /// [`crate::protocol::PROTOCOL_VERSION`] of the recorded messages.
    pub protocol: u32,
    /// Length of the recording in seconds; 0 while it is not known (a
    /// recording that was never finished).
    pub duration_s: f64,
}

/// Reads the JSON header from the first bytes of a recording, for example
/// to list the recordings of a directory without reading them: the first
/// 1024 bytes suffice for files of this module's writer (the header may be
/// as long as `12 + MAX_HEADER_JSON` bytes in general).
///
/// `duration_s` is 0 for a recording that was never finished; [`verify`]
/// finds out how long it plays.
///
/// [`RecordError::TooShort`] when `prefix` ends inside the header.
pub fn read_info(prefix: &[u8]) -> Result<RecordingInfo, RecordError> {
    match parse_file_header(prefix)? {
        HeaderParse::Done { info, .. } => Ok(info),
        HeaderParse::NeedMore(_) => Err(RecordError::TooShort),
    }
}

/// Kind of a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum RecordKind {
    /// One wire message.
    Message = 0,
    /// A complete snapshot.
    Keyframe = 1,
    /// Times and offsets of the keyframes; the last record.
    Index = 2,
}

/// The fixed fields of a record.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct RecordHeader {
    pub kind: RecordKind,
    pub flags: u8,
    pub time: f64,
    /// Stored size of the payload in bytes.
    pub payload_len: u32,
}

impl RecordHeader {
    /// Size of the whole record, length field included.
    pub fn total_len(&self) -> usize {
        RECORD_HEADER_SIZE + self.payload_len as usize
    }

    pub fn deflated(&self) -> bool {
        self.flags & FLAG_DEFLATE != 0
    }
}

/// Result of looking for a record at the start of a buffer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Peek {
    /// The fixed fields are there. The payload may not be: compare
    /// [`RecordHeader::total_len`] with the buffer.
    Record(RecordHeader),
    /// Fewer than [`RECORD_HEADER_SIZE`] bytes.
    NeedMore,
    Damaged(RecordError),
}

#[inline]
pub(crate) fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([bytes[offset], bytes[offset + 1], bytes[offset + 2], bytes[offset + 3]])
}

#[inline]
pub(crate) fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    let mut raw = [0u8; 8];
    raw.copy_from_slice(&bytes[offset..offset + 8]);
    u64::from_le_bytes(raw)
}

/// Reads the fixed fields of the record that starts at `bytes[0]`.
pub(crate) fn peek_record(bytes: &[u8]) -> Peek {
    if bytes.len() < 4 {
        return Peek::NeedMore;
    }
    // The length is judged as soon as it is visible: a stream reader must
    // not wait for the gigabytes a damaged length announces.
    let len = u32_at(bytes, 0);
    if !(RECORD_FIXED..=MAX_RECORD_LEN).contains(&len) {
        return Peek::Damaged(RecordError::BadLength);
    }
    if bytes.len() < RECORD_HEADER_SIZE {
        return Peek::NeedMore;
    }
    let kind = match bytes[4] {
        0 => RecordKind::Message,
        1 => RecordKind::Keyframe,
        2 => RecordKind::Index,
        _ => return Peek::Damaged(RecordError::BadKind),
    };
    let flags = bytes[5];
    if flags & !FLAG_DEFLATE != 0 {
        return Peek::Damaged(RecordError::BadFlags);
    }
    let time = f64::from_bits(u64_at(bytes, 8));
    if !time.is_finite() || time < 0.0 {
        return Peek::Damaged(RecordError::BadTime);
    }
    Peek::Record(RecordHeader { kind, flags, time, payload_len: len - RECORD_FIXED })
}

/// Appends the 16 bytes that precede the payload of a record.
pub(crate) fn write_record_header(out: &mut Vec<u8>, kind: RecordKind, flags: u8, time: f64, payload_len: u32) {
    out.extend_from_slice(&(payload_len + RECORD_FIXED).to_le_bytes());
    out.extend_from_slice(&[kind as u8, flags, 0, 0]);
    out.extend_from_slice(&time.to_le_bytes());
}

/// Result of parsing the start of a file.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum HeaderParse {
    /// The header needs this many bytes from the start of the file.
    NeedMore(usize),
    Done {
        info: RecordingInfo,
        /// Offset of the first record.
        first_record: usize,
    },
}

/// Parses the magic and the JSON header at the start of `bytes`.
pub(crate) fn parse_file_header(bytes: &[u8]) -> Result<HeaderParse, RecordError> {
    let seen = bytes.len().min(MAGIC.len());
    if bytes[..seen] != MAGIC[..seen] {
        return Err(RecordError::BadMagic);
    }
    if bytes.len() < 12 {
        return Ok(HeaderParse::NeedMore(12));
    }
    let len = u32_at(bytes, 8) as usize;
    if len > MAX_HEADER_JSON {
        return Err(RecordError::BadHeader);
    }
    if bytes.len() < 12 + len {
        return Ok(HeaderParse::NeedMore(12 + len));
    }
    let text = std::str::from_utf8(&bytes[12..12 + len]).map_err(|_| RecordError::BadHeader)?;
    // serde_json refuses NaN, Infinity, numbers out of range and nesting
    // deeper than 128 levels.
    let value: serde_json::Value = serde_json::from_str(text).map_err(|_| RecordError::BadHeader)?;
    let object = value.as_object().ok_or(RecordError::BadHeader)?;
    let text_of = |key: &str| object.get(key).and_then(|v| v.as_str()).unwrap_or_default().to_owned();
    let info = RecordingInfo {
        name: text_of("name"),
        created: text_of("created"),
        scene: text_of("scene"),
        protocol: object.get("protocol").and_then(|v| v.as_u64()).map_or(0, |p| p.min(u32::MAX as u64) as u32),
        duration_s: object
            .get("duration_s")
            .and_then(|v| v.as_f64())
            .filter(|d| d.is_finite() && *d >= 0.0)
            .unwrap_or(0.0),
    };
    Ok(HeaderParse::Done { info, first_record: 12 + len })
}

/// Inflates a raw-deflate payload, refusing more than [`MAX_INFLATED`].
pub(crate) fn inflate(data: &[u8]) -> Result<Vec<u8>, RecordError> {
    miniz_oxide::inflate::decompress_to_vec_with_limit(data, MAX_INFLATED).map_err(|_| RecordError::BadDeflate)
}

/// The payload of a record as stored in `stored`, inflated if `deflated`.
pub(crate) fn payload_of(stored: &[u8], deflated: bool) -> Result<std::borrow::Cow<'_, [u8]>, RecordError> {
    if deflated {
        inflate(stored).map(std::borrow::Cow::Owned)
    } else {
        Ok(std::borrow::Cow::Borrowed(stored))
    }
}

/// Splits a keyframe payload into its messages and checks that it is a
/// snapshot: `State`, layer 0 `Replace`, layer 1 `Replace`, then `Stats`
/// and `Preview` messages only.
pub(crate) fn split_keyframe(payload: &[u8]) -> Result<Vec<&[u8]>, RecordError> {
    let mut messages = Vec::new();
    let mut rest = payload;
    while !rest.is_empty() {
        if rest.len() < 4 {
            return Err(RecordError::BadKeyframe);
        }
        let len = u32_at(rest, 0) as usize;
        let body = rest.get(4..4 + len).ok_or(RecordError::BadKeyframe)?;
        let class = classify(body).ok_or(RecordError::BadKeyframe)?;
        let expected = match messages.len() {
            0 => class == MessageClass::State,
            1 => class == MessageClass::Layer { layer: 0, replace: true },
            2 => class == MessageClass::Layer { layer: 1, replace: true },
            _ => matches!(class, MessageClass::Stats | MessageClass::Preview(_)),
        };
        if !expected {
            return Err(RecordError::BadKeyframe);
        }
        messages.push(body);
        rest = &rest[4 + len..];
    }
    if messages.len() < 3 {
        return Err(RecordError::BadKeyframe);
    }
    Ok(messages)
}

/// Parses an index payload. The entries must be in strictly increasing
/// offset order and in time order.
pub(crate) fn parse_index(payload: &[u8]) -> Result<Vec<(f64, u64)>, RecordError> {
    let chunks = payload.chunks_exact(INDEX_ENTRY_SIZE);
    if !chunks.remainder().is_empty() {
        return Err(RecordError::BadIndex);
    }
    let mut entries: Vec<(f64, u64)> = Vec::with_capacity(chunks.len());
    for entry in chunks {
        let time = f64::from_bits(u64_at(entry, 0));
        let offset = u64_at(entry, 8);
        let ordered = entries.last().is_none_or(|&(t, o)| time >= t && offset > o);
        if !time.is_finite() || time < 0.0 || !ordered {
            return Err(RecordError::BadIndex);
        }
        entries.push((time, offset));
    }
    Ok(entries)
}

/// A fast 64-bit fingerprint of a message, to notice that two keyframes
/// hold the very same bytes. Not a cryptographic hash.
pub(crate) fn fingerprint(bytes: &[u8]) -> u64 {
    const K: u64 = 0x9E37_79B9_7F4A_7C15;
    let mut h = (bytes.len() as u64).wrapping_mul(K) ^ 0x243F_6A88_85A3_08D3;
    let mut words = bytes.chunks_exact(8);
    for word in &mut words {
        h = (h ^ u64_at(word, 0)).wrapping_mul(K);
        h ^= h >> 29;
    }
    for &byte in words.remainder() {
        h = (h ^ byte as u64).wrapping_mul(K);
    }
    h ^ (h >> 32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_headers_are_checked_in_order() {
        let mut record = Vec::new();
        write_record_header(&mut record, RecordKind::Keyframe, FLAG_DEFLATE, 1.5, 7);
        assert_eq!(record.len(), RECORD_HEADER_SIZE);
        let header = RecordHeader { kind: RecordKind::Keyframe, flags: 1, time: 1.5, payload_len: 7 };
        assert_eq!(peek_record(&record), Peek::Record(header));
        assert_eq!((header.total_len(), header.deflated()), (23, true));
        assert_eq!(peek_record(&record[..15]), Peek::NeedMore);
        assert_eq!(peek_record(&[]), Peek::NeedMore);

        let patched = |offset: usize, bytes: &[u8]| {
            let mut r = record.clone();
            r[offset..offset + bytes.len()].copy_from_slice(bytes);
            peek_record(&r)
        };
        assert_eq!(patched(0, &11u32.to_le_bytes()), Peek::Damaged(RecordError::BadLength));
        assert_eq!(patched(0, &(MAX_RECORD_LEN + 1).to_le_bytes()), Peek::Damaged(RecordError::BadLength));
        // A bad length is reported from four bytes, before anything else.
        assert_eq!(peek_record(&u32::MAX.to_le_bytes()), Peek::Damaged(RecordError::BadLength));
        assert_eq!(patched(4, &[3]), Peek::Damaged(RecordError::BadKind));
        assert_eq!(patched(5, &[2]), Peek::Damaged(RecordError::BadFlags));
        assert_eq!(patched(8, &f64::NAN.to_le_bytes()), Peek::Damaged(RecordError::BadTime));
        assert_eq!(patched(8, &(-0.5f64).to_le_bytes()), Peek::Damaged(RecordError::BadTime));
        assert_eq!(patched(8, &f64::INFINITY.to_le_bytes()), Peek::Damaged(RecordError::BadTime));
        // Kind before flags before time; the reserved field is ignored.
        let mut all = record.clone();
        all[4..8].copy_from_slice(&[9, 0xff, 0xab, 0xcd]);
        assert_eq!(peek_record(&all), Peek::Damaged(RecordError::BadKind));
        assert!(matches!(patched(6, &[0xab, 0xcd]), Peek::Record(_)));
    }

    fn header_bytes(json: &str) -> Vec<u8> {
        let mut out = MAGIC.to_vec();
        out.extend_from_slice(&(json.len() as u32).to_le_bytes());
        out.extend_from_slice(json.as_bytes());
        out
    }

    #[test]
    fn file_headers_are_lenient_about_fields_and_strict_about_shape() {
        let json = r#"{"name":"Salon","created":"2026-11-14T10:00:00Z","scene":"house","protocol":1,"duration_s":12.5,"extra":[1,2]}   "#;
        let bytes = header_bytes(json);
        let HeaderParse::Done { info, first_record } = parse_file_header(&bytes).unwrap() else { panic!() };
        assert_eq!(first_record, bytes.len());
        assert_eq!(
            (info.name.as_str(), info.scene.as_str(), info.protocol, info.duration_s),
            ("Salon", "house", 1, 12.5)
        );
        // Every prefix asks for more, never errs, never claims to be done.
        for cut in 0..bytes.len() {
            let needed = if cut < 12 { 12 } else { bytes.len() };
            assert_eq!(parse_file_header(&bytes[..cut]), Ok(HeaderParse::NeedMore(needed)), "{cut}");
        }
        // Missing or mistyped fields take neutral values.
        let HeaderParse::Done { info, .. } =
            parse_file_header(&header_bytes(r#"{"name":7,"protocol":-1,"duration_s":"long"}"#)).unwrap()
        else {
            panic!()
        };
        assert_eq!(info, RecordingInfo::default());
        let HeaderParse::Done { info, .. } =
            parse_file_header(&header_bytes(r#"{"protocol":99999999999,"duration_s":-3}"#)).unwrap()
        else {
            panic!()
        };
        assert_eq!((info.protocol, info.duration_s), (u32::MAX, 0.0));

        for bad in ["[]", "7", "null", "{", r#"{"duration_s":NaN}"#, r#"{"duration_s":1e999}"#, "", "\u{0}{}"] {
            assert_eq!(parse_file_header(&header_bytes(bad)), Err(RecordError::BadHeader), "{bad:?}");
        }
        let deep = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
        assert_eq!(parse_file_header(&header_bytes(&deep)), Err(RecordError::BadHeader));
        let mut not_utf8 = header_bytes("{}  ");
        not_utf8[14] = 0xff;
        assert_eq!(parse_file_header(&not_utf8), Err(RecordError::BadHeader));
        let mut huge = MAGIC.to_vec();
        huge.extend_from_slice(&(MAX_HEADER_JSON as u32 + 1).to_le_bytes());
        assert_eq!(parse_file_header(&huge), Err(RecordError::BadHeader));
        assert_eq!(parse_file_header(b"GSCAP1\0\0"), Err(RecordError::BadMagic));
        assert_eq!(parse_file_header(b"GSX"), Err(RecordError::BadMagic));
        assert_eq!(parse_file_header(b""), Ok(HeaderParse::NeedMore(12)));
    }

    #[test]
    fn index_entries_must_be_ordered() {
        let entry = |time: f64, offset: u64| [time.to_le_bytes(), offset.to_le_bytes()].concat();
        let good = [entry(0.0, 1024), entry(1.5, 5000), entry(1.5, 9000)].concat();
        assert_eq!(parse_index(&good), Ok(vec![(0.0, 1024), (1.5, 5000), (1.5, 9000)]));
        assert_eq!(parse_index(&[]), Ok(vec![]));
        assert_eq!(parse_index(&good[..17]), Err(RecordError::BadIndex));
        for bad in [
            [entry(1.0, 1024), entry(0.5, 5000)].concat(),
            [entry(0.0, 5000), entry(1.0, 5000)].concat(),
            [entry(0.0, 5000), entry(1.0, 1024)].concat(),
            entry(f64::NAN, 1024),
            entry(-1.0, 1024),
        ] {
            assert_eq!(parse_index(&bad), Err(RecordError::BadIndex));
        }
    }

    #[test]
    fn fingerprints_tell_messages_apart() {
        let a: Vec<u8> = (0..1000u32).map(|i| (i * 7) as u8).collect();
        let mut b = a.clone();
        assert_eq!(fingerprint(&a), fingerprint(&b));
        b[999] ^= 1;
        assert_ne!(fingerprint(&a), fingerprint(&b));
        b[999] ^= 1;
        b[3] ^= 0x80;
        assert_ne!(fingerprint(&a), fingerprint(&b));
        assert_ne!(fingerprint(&a[..999]), fingerprint(&a));
        assert_ne!(fingerprint(&[]), fingerprint(&[0]));
    }
}
