//! Whole-file import: a `.gscap` in, a finished [`CaptureSession`] out.
//!
//! [`GscapImporter`] takes the file in chunks of any size, as a request
//! body or a file read delivers them, and fuses every frame as soon as it
//! is complete: at no time is more than one frame record buffered, whatever
//! the size of the file. [`import_gscap`] and [`import_gscap_reader`] are
//! the same for a file in memory and for anything that implements
//! [`std::io::Read`].
//!
//! A damaged frame (a record that is not a valid capture frame) makes the
//! file invalid, as for the reference tools; a file that merely stops in
//! the middle of a record is a recording cut short and keeps its complete
//! frames.

use std::fmt;

use super::file::{GscapParser, ParsedItem};
use super::frame::{ErrorCode, FormatError};
use super::prepare::IngestParams;
use super::session::CaptureSession;

/// Why an import failed. `POST /api/captures` answers 400 for either.
#[derive(Debug)]
pub enum ImportError {
    /// The file is not a valid `.gscap` (wrong magic, bad header, damaged
    /// frame, no bytes at all).
    Format(FormatError),
    /// Reading the source failed.
    Io(std::io::Error),
}

impl fmt::Display for ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Format(e) => write!(f, "{e}"),
            Self::Io(e) => write!(f, "read error: {e}"),
        }
    }
}

impl std::error::Error for ImportError {}

impl From<FormatError> for ImportError {
    fn from(e: FormatError) -> Self {
        Self::Format(e)
    }
}

impl From<std::io::Error> for ImportError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// What an import read: the body of the answer of `POST /api/captures`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImportSummary {
    /// Complete frames in the file.
    pub frames: u64,
    /// How many of them went into the reconstruction.
    pub frames_used: u64,
    /// The file stops inside a record: a recording cut short.
    pub truncated: bool,
    /// The capture's name, from the header.
    pub name: String,
    /// Bytes of the file.
    pub bytes: u64,
}

/// Imports a `.gscap` that arrives in pieces.
///
/// ```
/// use gs_core::ingest::{GscapImporter, IngestParams};
/// # use gs_core::ingest::{file, GscapHeader};
/// # let header = GscapHeader { mode: "room".into(), name: "Salon".into(), ..Default::default() };
/// # let file = file::encode_file_header(&header, 0).unwrap();
/// let mut importer = GscapImporter::new(IngestParams::default());
/// for chunk in file.chunks(64 * 1024) {
///     importer.push(chunk)?;
/// }
/// let (mut session, summary) = importer.finish()?;
/// assert_eq!((summary.frames, summary.name.as_str()), (0, "Salon"));
/// let for_viewers = session.snapshot();
/// # assert!(for_viewers.len() >= 3);
/// # Ok::<(), gs_core::ingest::ImportError>(())
/// ```
#[derive(Debug)]
pub struct GscapImporter {
    parser: GscapParser,
    params: IngestParams,
    session: Option<CaptureSession>,
    bytes: u64,
}

impl GscapImporter {
    pub fn new(params: IngestParams) -> Self {
        Self { parser: GscapParser::new(), params, session: None, bytes: 0 }
    }

    /// Feeds the next bytes of the file and fuses the frames they complete.
    /// After an error the import is over.
    pub fn push(&mut self, chunk: &[u8]) -> Result<(), FormatError> {
        self.bytes += chunk.len() as u64;
        self.parser.push(chunk);
        while let Some(item) = self.parser.next_item()? {
            match item {
                ParsedItem::Header(header) => {
                    self.session = Some(CaptureSession::from_header(&header, self.params)?);
                }
                ParsedItem::Frame(bytes) => {
                    let frame = super::frame::decode_frame(bytes)?;
                    // The parser hands out the header first.
                    if let Some(session) = &mut self.session {
                        session.handle_decoded(&frame);
                    }
                }
            }
        }
        Ok(())
    }

    /// Frames read so far.
    pub fn frames(&self) -> u64 {
        self.parser.frames()
    }

    /// The session being built, once the header has been read (to show
    /// progress while a large file is imported).
    pub fn session(&self) -> Option<&CaptureSession> {
        self.session.as_ref()
    }

    /// The file has ended: runs the final pass and returns the finished
    /// session, ready for [`CaptureSession::snapshot`].
    pub fn finish(mut self) -> Result<(CaptureSession, ImportSummary), FormatError> {
        let end = self.parser.finish()?;
        let Some(mut session) = self.session else {
            return Err(FormatError { code: ErrorCode::BadHeader, detail: "file ends inside the header".into() });
        };
        session.finish();
        let summary = ImportSummary {
            frames: end.frames,
            frames_used: session.frames_used() as u64,
            truncated: end.truncated,
            name: session.name().to_string(),
            bytes: self.bytes,
        };
        Ok((session, summary))
    }
}

/// Imports a `.gscap` held in memory.
pub fn import_gscap(data: &[u8], params: IngestParams) -> Result<(CaptureSession, ImportSummary), FormatError> {
    let mut importer = GscapImporter::new(params);
    // Bounded pieces: the importer's buffer stays as small as one frame.
    for chunk in data.chunks(1 << 20) {
        importer.push(chunk)?;
    }
    importer.finish()
}

/// Imports a `.gscap` from a reader, 256 kB at a time.
pub fn import_gscap_reader(
    mut reader: impl std::io::Read,
    params: IngestParams,
) -> Result<(CaptureSession, ImportSummary), ImportError> {
    let mut importer = GscapImporter::new(params);
    let mut buffer = vec![0u8; 256 * 1024];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => importer.push(&buffer[..n])?,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(importer.finish()?)
}
