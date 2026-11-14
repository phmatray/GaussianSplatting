//! Real capture: frames from the phone companion turned into splats.
//!
//! See "Real capture: the phone companion" in `docs/ARCHITECTURE.md`. The
//! module is runtime-agnostic: no sockets, no files, no clock. A host (the
//! server's `/ingest` WebSocket and `POST /api/captures`, or the web worker
//! opening a `.gscap`) hands it bytes and broadcasts what it returns.
//!
//! | Module | Contents |
//! |---|---|
//! | [`frame`] | The capture frame (`GSCF`): header, planes, raw deflate |
//! | [`file`] | The `.gscap` container: header JSON, push parser, writer |
//! | [`message`] | JSON text messages of `/ingest`, both directions |
//! | [`pairing`] | The `splatlive://pair` URI |
//! | [`image`] | JPEG decoding and resampling of the colour image |
//! | [`prepare`] | A decoded frame turned into the RGB-D image the reconstruction consumes |
//! | [`session`] | [`CaptureSession`]: the state of one capture, viewer messages |
//! | [`import`] | Whole-file import, streaming |
//!
//! Everything that parses bytes is written for hostile input: errors instead
//! of panics, checked arithmetic, nothing allocated from a length that the
//! data does not back.

pub mod file;
pub mod frame;
pub mod image;
pub mod import;
pub mod message;
pub mod pairing;
pub mod prepare;
pub mod session;

pub use file::{GscapHeader, GscapParser, GscapWriter, ParsedItem};
pub use frame::{decode_frame, encode_frame, CaptureFrame, ErrorCode, FormatError, FrameHeader, Orientation, Tracking};
pub use import::{import_gscap, import_gscap_reader, GscapImporter, ImportError, ImportSummary};
pub use message::{CaptureRegion, ClientMessage, Hello, ServerMessage};
pub use pairing::{pairing_uri, parse_pairing_uri, Pairing, PairingError};
pub use prepare::IngestParams;
pub use session::{CaptureSession, Reply, SessionError};

/// Recommended cap on a WebSocket message of `/ingest`, in bytes.
///
/// A frame of the phone is a JPEG of at most 1920x1440 at quality 0.8 (about
/// 0.3 to 0.9 MB, 1.5 MB for a very busy picture) plus a few kB of deflated
/// planes. 8 MiB leaves room for an uncompressed fallback frame and a
/// better JPEG quality, and bounds what one connection can make the server
/// buffer (`max_inflight` messages).
pub const MAX_INGEST_MESSAGE_BYTES: usize = 8 << 20;

/// `max_inflight` the server should announce in its `welcome`.
pub const RECOMMENDED_MAX_INFLIGHT: u32 = 4;
