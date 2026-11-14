//! The `.gscap` capture file: what the phone records offline.
//!
//! 8 bytes `GSCAP1\0\0`, a `u32` length and that many bytes of JSON (the
//! `hello` fields plus `created`, optionally `region` and `floor`, possibly
//! padded with spaces), then for every frame a `u32` length and the frame
//! exactly as on the wire. A file that ends inside a record is a recording
//! cut short: the complete frames are kept.
//!
//! [`GscapParser`] is a push parser: the host feeds it the bytes as they
//! arrive (a request body, a file read in chunks) and takes the header and
//! the frames out as they complete, so a file of gigabytes is never held in
//! memory. [`split_gscap`] does the same for a file that is already in
//! memory, without copying.

use serde_json::{Map, Value};

use super::frame::{ErrorCode, FormatError, FRAME_HEADER_SIZE, MAX_FRAME_BYTES};
use super::message::CaptureRegion;
use crate::protocol::CaptureMode;

/// First eight bytes of a `.gscap`.
pub const FILE_MAGIC: [u8; 8] = *b"GSCAP1\0\0";
/// Longest JSON header.
pub const MAX_HEADER_JSON: usize = 1 << 20;
/// Bytes of JSON the phone reserves so that it can rewrite the header in
/// place when region and floor become known.
pub const HEADER_PAD: usize = 1024;

/// The JSON header of a `.gscap`. Unknown keys are ignored; a known key of
/// the wrong type counts as absent.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GscapHeader {
    /// Protocol version the file was written for, if stated.
    pub protocol: Option<u32>,
    pub device: String,
    pub app: String,
    /// `room`, `object`, `person` or `live`, as written (see
    /// [`GscapHeader::capture_mode`]).
    pub mode: String,
    pub name: String,
    /// RFC 3339 time of the recording.
    pub created: String,
    /// Region of interest in the world of the recording.
    pub region: Option<CaptureRegion>,
    /// World height of the floor ARKit detected.
    pub floor: Option<f32>,
}

impl GscapHeader {
    /// Reads the header from its JSON text (spaces around it are padding).
    pub fn from_json(text: &str) -> Result<Self, FormatError> {
        let value: Value =
            serde_json::from_str(text).map_err(|e| FormatError::new(ErrorCode::BadHeader, e.to_string()))?;
        let Value::Object(object) = value else {
            return Err(FormatError::new(ErrorCode::BadHeader, "the header is not a JSON object"));
        };
        Ok(Self::from_object(&object))
    }

    fn from_object(object: &Map<String, Value>) -> Self {
        let text = |key: &str| object.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
        let floor = match object.get("floor") {
            Some(Value::Object(floor)) => floor.get("y").and_then(json_f32),
            Some(other) => json_f32(other),
            None => None,
        };
        Self {
            protocol: object.get("protocol").and_then(Value::as_u64).and_then(|p| u32::try_from(p).ok()),
            device: text("device"),
            app: text("app"),
            mode: text("mode"),
            name: text("name"),
            created: text("created"),
            region: object.get("region").and_then(CaptureRegion::from_json),
            floor,
        }
    }

    /// The capture mode, when the header names one of the four.
    pub fn capture_mode(&self) -> Option<CaptureMode> {
        CaptureMode::from_name(&self.mode)
    }

    /// Compact JSON with sorted keys, as the reference writes it.
    pub fn to_json(&self) -> String {
        let mut object = Map::new();
        object.insert("protocol".into(), Value::from(self.protocol.unwrap_or(super::message::INGEST_PROTOCOL)));
        for (key, value) in
            [("device", &self.device), ("app", &self.app), ("mode", &self.mode), ("name", &self.name), ("created", &self.created)]
        {
            object.insert(key.into(), Value::from(value.as_str()));
        }
        if let Some(region) = &self.region {
            object.insert("region".into(), region.to_json());
        }
        if let Some(y) = self.floor.filter(|y| y.is_finite()) {
            let mut floor = Map::new();
            floor.insert("y".into(), Value::from(y as f64));
            object.insert("floor".into(), Value::Object(floor));
        }
        // `Map` keeps its keys sorted (serde_json without `preserve_order`).
        Value::Object(object).to_string()
    }
}

/// A finite JSON number as `f32`.
pub(crate) fn json_f32(value: &Value) -> Option<f32> {
    value.as_f64().map(|v| v as f32).filter(|v| v.is_finite())
}

/// File magic, length and JSON header; the JSON is padded with spaces to
/// `pad_to` bytes when it is shorter (see [`HEADER_PAD`]).
pub fn encode_file_header(header: &GscapHeader, pad_to: usize) -> Result<Vec<u8>, FormatError> {
    let mut body = header.to_json().into_bytes();
    if body.len() < pad_to.min(MAX_HEADER_JSON) {
        body.resize(pad_to.min(MAX_HEADER_JSON), b' ');
    }
    if body.len() > MAX_HEADER_JSON {
        return Err(FormatError::new(ErrorCode::BadHeader, "header JSON is too large"));
    }
    let mut out = Vec::with_capacity(FILE_MAGIC.len() + 4 + body.len());
    out.extend_from_slice(&FILE_MAGIC);
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

fn check_record_length(length: usize) -> Result<(), FormatError> {
    if (FRAME_HEADER_SIZE..=MAX_FRAME_BYTES).contains(&length) {
        Ok(())
    } else {
        Err(FormatError::new(ErrorCode::BadLength, format!("frame record of {length} bytes")))
    }
}

/// Writes a `.gscap` into anything that takes bytes.
#[derive(Debug)]
pub struct GscapWriter<W: std::io::Write> {
    out: W,
    frames: u64,
    bytes: u64,
}

impl<W: std::io::Write> GscapWriter<W> {
    /// Writes the file header. With `pad_to` (for example [`HEADER_PAD`])
    /// the header can later be replaced in place by the bytes of
    /// [`encode_file_header`] with the same padding.
    pub fn new(mut out: W, header: &GscapHeader, pad_to: usize) -> std::io::Result<Self> {
        let head = encode_file_header(header, pad_to).map_err(std::io::Error::other)?;
        out.write_all(&head)?;
        Ok(Self { out, frames: 0, bytes: head.len() as u64 })
    }

    /// Appends one frame, exactly as it travels on the wire.
    pub fn append(&mut self, frame_bytes: &[u8]) -> std::io::Result<()> {
        check_record_length(frame_bytes.len()).map_err(std::io::Error::other)?;
        self.out.write_all(&(frame_bytes.len() as u32).to_le_bytes())?;
        self.out.write_all(frame_bytes)?;
        self.frames += 1;
        self.bytes += 4 + frame_bytes.len() as u64;
        Ok(())
    }

    /// Frames written so far.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Bytes written so far, header included.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn into_inner(self) -> W {
        self.out
    }
}

/// What [`GscapParser::next_item`] hands out.
#[derive(Debug, PartialEq)]
pub enum ParsedItem<'a> {
    /// The file header; always the first item.
    Header(GscapHeader),
    /// The bytes of one frame record, to be given to
    /// [`super::frame::decode_frame`]. Valid until the parser is used again.
    Frame(&'a [u8]),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Magic,
    HeaderLength,
    Header(usize),
    RecordLength,
    Record(usize),
}

/// Incremental reader of a `.gscap`.
///
/// ```
/// use gs_core::ingest::{GscapParser, ParsedItem};
/// # let file = gs_core::ingest::file::encode_file_header(&Default::default(), 0).unwrap();
/// let mut parser = GscapParser::new();
/// for chunk in file.chunks(7) {
///     parser.push(chunk);
///     while let Some(item) = parser.next_item()? {
///         match item {
///             ParsedItem::Header(header) => assert_eq!(header.mode, ""),
///             ParsedItem::Frame(_bytes) => unreachable!(),
///         }
///     }
/// }
/// assert!(!parser.finish()?.truncated);
/// # Ok::<(), gs_core::ingest::FormatError>(())
/// ```
///
/// The parser buffers only what it has not handed out yet: at most one
/// record (64 MiB at the very most, a few hundred kB from the phone) plus
/// the chunk being pushed. It never reserves memory for a length the file
/// merely announces.
#[derive(Debug)]
pub struct GscapParser {
    buf: Vec<u8>,
    /// Start of the bytes not yet handed out.
    start: usize,
    stage: Stage,
    frames: u64,
    header_json_length: usize,
    error: Option<FormatError>,
}

/// How a `.gscap` ended.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FileEnd {
    /// Complete frames read.
    pub frames: u64,
    /// The file stops inside a record or inside a record length.
    pub truncated: bool,
    /// Bytes of that incomplete record.
    pub trailing_bytes: usize,
}

impl Default for GscapParser {
    fn default() -> Self {
        Self::new()
    }
}

impl GscapParser {
    pub fn new() -> Self {
        Self { buf: Vec::new(), start: 0, stage: Stage::Magic, frames: 0, header_json_length: 0, error: None }
    }

    /// Adds the next bytes of the file. Take the items out with
    /// [`GscapParser::next_item`] before pushing more, or the buffer grows.
    pub fn push(&mut self, chunk: &[u8]) {
        if self.error.is_some() {
            return;
        }
        // Forget what was handed out, so that the buffer stays as small as
        // the unfinished record.
        if self.start > 0 {
            self.buf.drain(..self.start);
            self.start = 0;
        }
        self.buf.extend_from_slice(chunk);
    }

    /// Bytes pushed and not yet handed out.
    pub fn buffered(&self) -> usize {
        self.buf.len() - self.start
    }

    /// Complete frames handed out so far.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Length of the JSON header as declared in the file, padding included
    /// (0 until the header has been read).
    pub fn header_json_length(&self) -> usize {
        self.header_json_length
    }

    fn fail<T>(&mut self, error: FormatError) -> Result<T, FormatError> {
        self.error = Some(error.clone());
        Err(error)
    }

    fn take(&mut self, n: usize) -> Option<std::ops::Range<usize>> {
        (self.buffered() >= n).then(|| {
            let range = self.start..self.start + n;
            self.start += n;
            range
        })
    }

    fn take_u32(&mut self) -> Option<usize> {
        let range = self.take(4)?;
        let b = &self.buf[range];
        Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
    }

    /// The next complete item, `None` when more bytes are needed. After an
    /// error every call returns that error.
    pub fn next_item(&mut self) -> Result<Option<ParsedItem<'_>>, FormatError> {
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        loop {
            match self.stage {
                Stage::Magic => {
                    // A wrong magic is reported as soon as it shows.
                    let seen = self.buffered().min(FILE_MAGIC.len());
                    if self.buf[self.start..self.start + seen] != FILE_MAGIC[..seen] {
                        let found = format!("{:02x?}", &self.buf[self.start..self.start + seen]);
                        return self.fail(FormatError::new(ErrorCode::BadFileMagic, found));
                    }
                    if self.take(FILE_MAGIC.len()).is_none() {
                        return Ok(None);
                    }
                    self.stage = Stage::HeaderLength;
                }
                Stage::HeaderLength => {
                    let Some(length) = self.take_u32() else { return Ok(None) };
                    if length > MAX_HEADER_JSON {
                        return self.fail(FormatError::new(ErrorCode::BadHeader, format!("header JSON of {length} bytes")));
                    }
                    self.stage = Stage::Header(length);
                }
                Stage::Header(length) => {
                    let Some(range) = self.take(length) else { return Ok(None) };
                    let parsed = std::str::from_utf8(&self.buf[range])
                        .map_err(|e| FormatError::new(ErrorCode::BadHeader, e.to_string()))
                        .and_then(GscapHeader::from_json);
                    let header = match parsed {
                        Ok(header) => header,
                        Err(error) => return self.fail(error),
                    };
                    self.header_json_length = length;
                    self.stage = Stage::RecordLength;
                    return Ok(Some(ParsedItem::Header(header)));
                }
                Stage::RecordLength => {
                    if self.buffered() < 4 {
                        return Ok(None);
                    }
                    let b = &self.buf[self.start..self.start + 4];
                    let length = u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize;
                    if let Err(error) = check_record_length(length) {
                        return self.fail(error);
                    }
                    // The length stays in the buffer until the record is
                    // complete, so that `finish` can count what is left.
                    self.stage = Stage::Record(length);
                }
                Stage::Record(length) => {
                    if self.buffered() < 4 + length {
                        return Ok(None);
                    }
                    self.start += 4;
                    let range = self.start..self.start + length;
                    self.start += length;
                    self.frames += 1;
                    self.stage = Stage::RecordLength;
                    return Ok(Some(ParsedItem::Frame(&self.buf[range])));
                }
            }
        }
    }

    /// Call when the file has ended and [`GscapParser::next_item`] returned
    /// `None`: tells whether it ended cleanly, was cut short inside a frame
    /// (not an error) or inside its header (an error).
    pub fn finish(&mut self) -> Result<FileEnd, FormatError> {
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        let left = self.buffered();
        match self.stage {
            Stage::Magic => self.fail(FormatError::new(ErrorCode::BadFileMagic, format!("the file has {left} bytes"))),
            Stage::HeaderLength => self.fail(FormatError::new(ErrorCode::BadHeader, "file ends inside the header length")),
            Stage::Header(_) => self.fail(FormatError::new(ErrorCode::BadHeader, "file ends inside the header")),
            Stage::RecordLength | Stage::Record(_) => {
                Ok(FileEnd { frames: self.frames, truncated: left > 0, trailing_bytes: left })
            }
        }
    }
}

/// A `.gscap` that is in memory, cut into its parts without copying.
#[derive(Debug, PartialEq)]
pub struct GscapContents<'a> {
    pub header: GscapHeader,
    /// Length of the JSON header as declared, padding included.
    pub header_json_length: usize,
    /// The complete frame records.
    pub frames: Vec<&'a [u8]>,
    pub truncated: bool,
    pub trailing_bytes: usize,
}

/// Cuts a whole `.gscap` into header and frame records.
pub fn split_gscap(data: &[u8]) -> Result<GscapContents<'_>, FormatError> {
    use ErrorCode::*;
    if data.len() < FILE_MAGIC.len() || data[..FILE_MAGIC.len()] != FILE_MAGIC {
        let seen = &data[..data.len().min(FILE_MAGIC.len())];
        return Err(FormatError::new(BadFileMagic, format!("{seen:02x?}")));
    }
    let mut rest = &data[FILE_MAGIC.len()..];
    let u32_of = |b: &[u8]| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize;
    if rest.len() < 4 {
        return Err(FormatError::new(BadHeader, "file ends inside the header length"));
    }
    let header_json_length = u32_of(rest);
    rest = &rest[4..];
    if header_json_length > MAX_HEADER_JSON {
        return Err(FormatError::new(BadHeader, format!("header JSON of {header_json_length} bytes")));
    }
    if rest.len() < header_json_length {
        return Err(FormatError::new(BadHeader, "file ends inside the header"));
    }
    let text =
        std::str::from_utf8(&rest[..header_json_length]).map_err(|e| FormatError::new(BadHeader, e.to_string()))?;
    let header = GscapHeader::from_json(text)?;
    rest = &rest[header_json_length..];
    let mut frames = Vec::new();
    let (mut truncated, mut trailing_bytes) = (false, 0);
    while !rest.is_empty() {
        if rest.len() < 4 {
            (truncated, trailing_bytes) = (true, rest.len());
            break;
        }
        let length = u32_of(rest);
        check_record_length(length)?;
        if rest.len() - 4 < length {
            (truncated, trailing_bytes) = (true, rest.len());
            break;
        }
        frames.push(&rest[4..4 + length]);
        rest = &rest[4 + length..];
    }
    Ok(GscapContents { header, header_json_length, frames, truncated, trailing_bytes })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> GscapHeader {
        GscapHeader {
            protocol: Some(1),
            device: "iPhone17,2".into(),
            app: "0.1.0".into(),
            mode: "object".into(),
            name: "Théière".into(),
            created: "2026-11-14T09:30:00Z".into(),
            region: Some(CaptureRegion { center: [0.5, -0.25, -1.5], radius: 0.75 }),
            floor: Some(-1.25),
        }
    }

    fn file(frames: &[Vec<u8>], pad: usize) -> Vec<u8> {
        let mut writer = GscapWriter::new(Vec::new(), &header(), pad).unwrap();
        for frame in frames {
            writer.append(frame).unwrap();
        }
        assert_eq!(writer.frames(), frames.len() as u64);
        let bytes = writer.bytes();
        let out = writer.into_inner();
        assert_eq!(out.len() as u64, bytes);
        out
    }

    fn records() -> Vec<Vec<u8>> {
        (0..3u8).map(|i| vec![i; 128 + 50 * i as usize]).collect()
    }

    #[test]
    fn header_json_is_compact_sorted_and_round_trips() {
        let json = header().to_json();
        assert_eq!(
            json,
            r#"{"app":"0.1.0","created":"2026-11-14T09:30:00Z","device":"iPhone17,2","floor":{"y":-1.25},"mode":"object","name":"Théière","protocol":1,"region":{"center":[0.5,-0.25,-1.5],"radius":0.75}}"#
        );
        assert_eq!(GscapHeader::from_json(&json).unwrap(), header());
        assert_eq!(header().capture_mode(), Some(CaptureMode::Object));
        // Padding, a bare floor number, unknown keys and wrong types.
        let odd = r#"  {"mode":"room","floor":-0.5,"name":7,"region":{"center":[1,2],"radius":1},"token":"x","protocol":"1"}   "#;
        let odd = GscapHeader::from_json(odd).unwrap();
        assert_eq!((odd.mode.as_str(), odd.floor, odd.name.as_str(), odd.region, odd.protocol), ("room", Some(-0.5), "", None, None));
        for bad in ["", "[]", "7", "null", "{", r#"{"floor":NaN}"#, r#"{"a":Infinity}"#] {
            assert_eq!(GscapHeader::from_json(bad).unwrap_err().code, ErrorCode::BadHeader, "{bad}");
        }
        let deep = format!("{}{}", "[".repeat(100_000), "]".repeat(100_000));
        assert_eq!(GscapHeader::from_json(&format!(r#"{{"x":{deep}}}"#)).unwrap_err().code, ErrorCode::BadHeader);
    }

    #[test]
    fn parser_gives_the_same_items_whatever_the_chunking() {
        let records = records();
        for pad in [0, HEADER_PAD] {
            let bytes = file(&records, pad);
            let whole = split_gscap(&bytes).unwrap();
            assert_eq!(whole.header, header());
            assert_eq!(whole.frames, records.iter().map(Vec::as_slice).collect::<Vec<_>>());
            assert!(!whole.truncated);
            if pad > 0 {
                assert_eq!(whole.header_json_length, pad);
            }
            for chunk in [1, 3, 127, 128, 4096, bytes.len()] {
                let mut parser = GscapParser::new();
                let mut frames = Vec::new();
                let mut headers = 0;
                let mut most_buffered = 0;
                for part in bytes.chunks(chunk) {
                    parser.push(part);
                    while let Some(item) = parser.next_item().unwrap() {
                        match item {
                            ParsedItem::Header(h) => {
                                assert_eq!(h, header());
                                headers += 1;
                            }
                            ParsedItem::Frame(frame) => frames.push(frame.to_vec()),
                        }
                    }
                    most_buffered = most_buffered.max(parser.buffered());
                }
                assert_eq!((headers, &frames), (1, &records), "chunks of {chunk}");
                assert_eq!(parser.finish().unwrap(), FileEnd { frames: 3, truncated: false, trailing_bytes: 0 });
                // Never more than the largest unfinished item.
                assert!(most_buffered <= whole.header_json_length.max(4 + 228), "{most_buffered}");
            }
        }
    }

    #[test]
    fn every_cut_keeps_the_complete_frames_or_fails_in_the_header() {
        let bytes = file(&records(), 0);
        let head = split_gscap(&bytes).unwrap().header_json_length + 12;
        let ends: Vec<usize> = records().iter().scan(head, |end, r| { *end += 4 + r.len(); Some(*end) }).collect();
        for cut in 0..=bytes.len() {
            let mut parser = GscapParser::new();
            parser.push(&bytes[..cut]);
            let mut frames = 0;
            let outcome = loop {
                match parser.next_item() {
                    Ok(Some(ParsedItem::Frame(_))) => frames += 1,
                    Ok(Some(ParsedItem::Header(_))) => {}
                    Ok(None) => break parser.finish(),
                    Err(e) => break Err(e),
                }
            };
            let whole = split_gscap(&bytes[..cut]);
            if cut < 8 {
                assert_eq!(outcome.unwrap_err().code, ErrorCode::BadFileMagic, "cut {cut}");
                assert_eq!(whole.unwrap_err().code, ErrorCode::BadFileMagic);
            } else if cut < head {
                assert_eq!(outcome.unwrap_err().code, ErrorCode::BadHeader, "cut {cut}");
                assert_eq!(whole.unwrap_err().code, ErrorCode::BadHeader);
            } else {
                let complete = ends.iter().filter(|&&e| e <= cut).count();
                let last = ends[..complete].last().copied().unwrap_or(head);
                let end = outcome.unwrap();
                assert_eq!((frames, end.frames, end.truncated, end.trailing_bytes), (complete, complete as u64, cut > last, cut - last), "cut {cut}");
                let whole = whole.unwrap();
                assert_eq!((whole.frames.len(), whole.truncated, whole.trailing_bytes), (complete, cut > last, cut - last));
            }
        }
    }

    #[test]
    fn hostile_lengths_are_refused_before_anything_is_buffered() {
        let mut bytes = FILE_MAGIC.to_vec();
        bytes.extend_from_slice(&u32::MAX.to_le_bytes());
        let mut parser = GscapParser::new();
        parser.push(&bytes);
        assert_eq!(parser.next_item().unwrap_err().code, ErrorCode::BadHeader);
        // The error sticks.
        parser.push(b"more");
        assert_eq!(parser.next_item().unwrap_err().code, ErrorCode::BadHeader);
        assert_eq!(parser.finish().unwrap_err().code, ErrorCode::BadHeader);

        for (length, ok) in [(127u32, false), (128, true), (64 << 20, true), ((64 << 20) + 1, false), (u32::MAX, false)] {
            let mut bytes = encode_file_header(&header(), 0).unwrap();
            bytes.extend_from_slice(&length.to_le_bytes());
            let mut parser = GscapParser::new();
            parser.push(&bytes);
            assert!(matches!(parser.next_item(), Ok(Some(ParsedItem::Header(_)))));
            let next = parser.next_item();
            assert_eq!(next.is_ok(), ok, "record of {length}");
            assert_eq!(split_gscap(&bytes).is_ok(), ok);
            if !ok {
                assert_eq!(next.unwrap_err().code, ErrorCode::BadLength);
            }
            assert!(parser.buf.capacity() < 4096, "nothing reserved for the announced {length} bytes");
        }
        let mut writer = GscapWriter::new(Vec::new(), &header(), 0).unwrap();
        assert!(writer.append(&[0; 127]).is_err());
        assert_eq!(writer.frames(), 0);
        assert_eq!(GscapParser::new().finish().unwrap_err().code, ErrorCode::BadFileMagic);
        let mut wrong = GscapParser::new();
        wrong.push(b"GSCAP2");
        assert_eq!(wrong.next_item().unwrap_err().code, ErrorCode::BadFileMagic);
    }
}
