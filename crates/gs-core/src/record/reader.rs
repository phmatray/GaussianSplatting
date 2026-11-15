//! Checking a whole recording.

use super::wire::{classify, MessageClass};
use super::{
    parse_file_header, parse_index, payload_of, peek_record, split_keyframe, u64_at, HeaderParse, Peek, RecordError,
    RecordKind, RecordingInfo, END_MAGIC, RECORD_HEADER_SIZE, TRAILER_SIZE,
};

/// Something wrong with a recording, and where.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Problem {
    /// File offset of the record (or of the trailer) it was found in.
    pub offset: u64,
    pub error: RecordError,
}

/// What [`verify`] found in a recording.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Report {
    pub info: RecordingInfo,
    /// Size of the file.
    pub bytes: u64,
    /// The index and the trailer are there and describe the keyframes.
    pub complete: bool,
    /// The file ends inside a record or without an index: a recording cut
    /// short. It plays up to its last complete record.
    pub truncated: bool,
    /// Complete records, the index included.
    pub records: u64,
    /// Message records by content.
    pub states: u64,
    pub layers: u64,
    pub stats: u64,
    pub previews: u64,
    /// Time and file offset of every valid keyframe.
    pub keyframes: Vec<(f64, u64)>,
    /// Time of the last record in seconds.
    pub duration_s: f64,
    /// Bytes of all payloads as stored, and after inflation.
    pub stored_bytes: u64,
    pub raw_bytes: u64,
    /// Everything that breaks the format, in file order.
    pub problems: Vec<Problem>,
}

impl Report {
    /// A finished recording without a flaw.
    pub fn is_valid(&self) -> bool {
        self.complete && self.problems.is_empty()
    }

    /// Message records.
    pub fn messages(&self) -> u64 {
        self.states + self.layers + self.stats + self.previews
    }
}

/// Reads a whole recording and checks everything in it: every payload is
/// inflated, every message validated as a viewer would, every keyframe
/// taken apart, the index compared with the keyframes found.
///
/// `Err` when the file header is unusable (`too_short`, `bad_magic`,
/// `bad_header`). Anything else is listed in [`Report::problems`]: damage
/// to the framing of a record (`bad_length`, `bad_kind`, `bad_flags`,
/// `bad_time`) ends the scan there; a record whose payload is bad
/// (`bad_deflate`, `bad_message`, `bad_keyframe`) is reported and skipped.
/// `tools/gsrec.py verify` reports the same problems at the same offsets.
pub fn verify(bytes: &[u8]) -> Result<Report, RecordError> {
    let (info, first_record) = match parse_file_header(bytes)? {
        HeaderParse::Done { info, first_record } => (info, first_record),
        HeaderParse::NeedMore(_) => return Err(RecordError::TooShort),
    };
    let mut report = Report { info, bytes: bytes.len() as u64, ..Report::default() };
    // Offsets of all keyframe records, valid or not: what an index lists.
    let mut keyframe_records: Vec<(f64, u64)> = Vec::new();
    let mut pos = first_record;
    let mut last_time = 0.0f64;
    loop {
        let rest = &bytes[pos..];
        let header = match peek_record(rest) {
            Peek::Record(header) if rest.len() >= header.total_len() => header,
            Peek::Record(_) | Peek::NeedMore => {
                report.truncated = true;
                break;
            }
            Peek::Damaged(error) => {
                report.problems.push(Problem { offset: pos as u64, error });
                break;
            }
        };
        if header.time < last_time {
            report.problems.push(Problem { offset: pos as u64, error: RecordError::BadTime });
            break;
        }
        last_time = header.time;
        report.duration_s = header.time;
        if report.records == 0 && header.kind != RecordKind::Keyframe {
            report.problems.push(Problem { offset: pos as u64, error: RecordError::BadKeyframe });
        }
        report.records += 1;
        let stored = &rest[RECORD_HEADER_SIZE..header.total_len()];
        report.stored_bytes += stored.len() as u64;
        let mut problem = |error| report.problems.push(Problem { offset: pos as u64, error });
        if header.kind == RecordKind::Keyframe {
            keyframe_records.push((header.time, pos as u64));
        }
        match payload_of(stored, header.deflated()) {
            Err(error) => problem(error),
            Ok(payload) => {
                report.raw_bytes += payload.len() as u64;
                match header.kind {
                    RecordKind::Message => match classify(&payload) {
                        Some(MessageClass::State) => report.states += 1,
                        Some(MessageClass::Layer { .. }) => report.layers += 1,
                        Some(MessageClass::Stats) => report.stats += 1,
                        Some(MessageClass::Preview(_)) => report.previews += 1,
                        None => problem(RecordError::BadMessage),
                    },
                    RecordKind::Keyframe => match split_keyframe(&payload) {
                        Ok(_) => report.keyframes.push((header.time, pos as u64)),
                        Err(error) => problem(error),
                    },
                    RecordKind::Index => {
                        let same = |a: &[(f64, u64)], b: &[(f64, u64)]| {
                            a.len() == b.len()
                                && a.iter().zip(b).all(|(x, y)| x.0.to_bits() == y.0.to_bits() && x.1 == y.1)
                        };
                        let listed = parse_index(&payload);
                        if !listed.is_ok_and(|entries| same(&entries, &keyframe_records)) {
                            problem(RecordError::BadIndex);
                        }
                    }
                }
            }
        }
        pos += header.total_len();
        if header.kind == RecordKind::Index {
            // The index is the last record: the trailer follows.
            let trailer = &bytes[pos..];
            if trailer.len() < TRAILER_SIZE {
                report.truncated = true;
            } else {
                let index_at = (pos - header.total_len()) as u64;
                let found = report.problems.len();
                if u64_at(trailer, 0) != index_at || trailer[8..TRAILER_SIZE] != END_MAGIC {
                    report.problems.push(Problem { offset: pos as u64, error: RecordError::BadIndex });
                }
                if trailer.len() > TRAILER_SIZE {
                    report
                        .problems
                        .push(Problem { offset: (pos + TRAILER_SIZE) as u64, error: RecordError::TrailingData });
                }
                let index_ok = !report.problems.iter().any(|p| p.offset == index_at);
                report.complete = index_ok && report.problems.len() == found;
            }
            break;
        }
    }
    if report.keyframes.is_empty() {
        report.problems.push(Problem { offset: first_record as u64, error: RecordError::Empty });
    }
    Ok(report)
}
