//! The capture frame: one WebSocket binary message of `/ingest`, one record
//! of a `.gscap`.
//!
//! Layout and rules are those of `docs/ARCHITECTURE.md` and of the reference
//! implementation `tools/gscap.py`; the validation order and the error codes
//! are shared with it and with the phone (see `ios/SplatCapture/Fixtures`).
//!
//! ```text
//! 0   4  magic "GSCF"            88  16 fx, fy, cx, cy (f32, colour pixels)
//! 4   1  version, 1              104 4  colour width, height (u16)
//! 5   1  flags                   108 4  colour byte length (a JPEG)
//! 6   1  tracking                112 4  depth width, height (u16)
//! 7   1  display orientation     116 4  depth byte length
//! 8   4  frame index             120 4  confidence byte length
//! 12  4  reserved                124 4  mask byte length
//! 16  8  timestamp (f64)         128 .. colour, depth, confidence, mask
//! 24  64 world_from_cam (16 f32, column-major)
//! ```

use std::fmt;

use miniz_oxide::inflate::core::{decompress, inflate_flags, DecompressorOxide};
use miniz_oxide::inflate::TINFLStatus;

/// First four bytes of a frame.
pub const FRAME_MAGIC: [u8; 4] = *b"GSCF";
/// The only version of the frame layout.
pub const FRAME_VERSION: u8 = 1;
/// Size of the fixed header that precedes the planes.
pub const FRAME_HEADER_SIZE: usize = 128;

/// Flag bit 0: a confidence plane follows the depth.
pub const FLAG_CONFIDENCE: u8 = 1;
/// Flag bit 1: depth, confidence and mask are raw-deflate streams.
pub const FLAG_DEFLATE: u8 = 2;
/// Flag bit 2: a person mask follows.
pub const FLAG_MASK: u8 = 4;
const KNOWN_FLAGS: u8 = FLAG_CONFIDENCE | FLAG_DEFLATE | FLAG_MASK;

/// Most pixels a depth map may have. A deflated frame of a few hundred
/// bytes can announce 65535 x 65535 pixels (8.6 GB once inflated); the
/// reference, the phone and this reader refuse anything above 2^22 from the
/// header alone. ARKit delivers 256 x 192.
pub const MAX_DEPTH_PIXELS: u32 = 1 << 22;
/// Longest frame a `.gscap` record may hold.
pub const MAX_FRAME_BYTES: usize = 64 << 20;

/// Stable identifiers of what can be wrong with a frame or a `.gscap`,
/// shared with the Python reference, the Swift code and the fixtures.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    /// Fewer than 128 bytes.
    TooShort,
    BadMagic,
    BadVersion,
    /// A flag bit above bit 2 is set.
    BadFlags,
    BadTracking,
    BadOrientation,
    /// Depth below 1x1 or above [`MAX_DEPTH_PIXELS`].
    BadDimensions,
    /// A length disagrees with the flags, the plane size or the frame size;
    /// in a file, a record below 128 bytes or above 64 MiB.
    BadLength,
    /// A plane does not inflate to exactly its size.
    BadDeflate,
    /// A value that the layout cannot hold (encoding only).
    BadValue,
    /// A `.gscap` that does not start with `GSCAP1\0\0`.
    BadFileMagic,
    /// The JSON header of a `.gscap` is too long, cut, not UTF-8, not JSON
    /// or not an object.
    BadHeader,
}

impl ErrorCode {
    /// The identifier as the fixtures and the other implementations spell it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TooShort => "too_short",
            Self::BadMagic => "bad_magic",
            Self::BadVersion => "bad_version",
            Self::BadFlags => "bad_flags",
            Self::BadTracking => "bad_tracking",
            Self::BadOrientation => "bad_orientation",
            Self::BadDimensions => "bad_dimensions",
            Self::BadLength => "bad_length",
            Self::BadDeflate => "bad_deflate",
            Self::BadValue => "bad_value",
            Self::BadFileMagic => "bad_file_magic",
            Self::BadHeader => "bad_header",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A frame or a file that does not follow the specification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FormatError {
    pub code: ErrorCode,
    /// What exactly is wrong, for logs (English).
    pub detail: String,
}

impl FormatError {
    pub(crate) fn new(code: ErrorCode, detail: impl Into<String>) -> Self {
        Self { code, detail: detail.into() }
    }
}

impl fmt::Display for FormatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.detail.is_empty() {
            write!(f, "{}", self.code)
        } else {
            write!(f, "{}: {}", self.code, self.detail)
        }
    }
}

impl std::error::Error for FormatError {}

/// ARKit's tracking state when the frame was taken.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Tracking {
    /// The pose means nothing.
    NotAvailable = 0,
    /// The pose is a guess (initialising, fast motion, too few features).
    Limited = 1,
    #[default]
    Normal = 2,
}

impl Tracking {
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::NotAvailable),
            1 => Some(Self::Limited),
            2 => Some(Self::Normal),
            _ => None,
        }
    }
}

/// How the device was held: a hint for showing previews upright. The image
/// and the geometry are always in the sensor's native landscape orientation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Orientation {
    /// Sensor native: landscape, as the image is stored.
    #[default]
    LandscapeNative = 0,
    Portrait = 1,
    LandscapeFlipped = 2,
    PortraitUpsideDown = 3,
}

impl Orientation {
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::LandscapeNative),
            1 => Some(Self::Portrait),
            2 => Some(Self::LandscapeFlipped),
            3 => Some(Self::PortraitUpsideDown),
            _ => None,
        }
    }

    /// Quarter turns clockwise that bring the stored image upright on a
    /// screen held this way.
    pub fn quarter_turns_cw(self) -> u8 {
        match self {
            Self::LandscapeNative => 0,
            Self::Portrait => 1,
            Self::LandscapeFlipped => 2,
            Self::PortraitUpsideDown => 3,
        }
    }
}

/// The validated 128-byte header of a frame. Parsing it touches no payload
/// byte and allocates nothing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameHeader {
    pub flags: u8,
    pub tracking: Tracking,
    pub orientation: Orientation,
    pub index: u32,
    /// Written as 0, ignored when reading.
    pub reserved: u32,
    pub timestamp: f64,
    pub world_from_cam: [f32; 16],
    /// `fx, fy, cx, cy` in pixels of the colour image as sent.
    pub intrinsics: [f32; 4],
    pub color_width: u16,
    pub color_height: u16,
    pub color_length: u32,
    pub depth_width: u16,
    pub depth_height: u16,
    /// Stored (possibly deflated) sizes of the three planes.
    pub depth_length: u32,
    pub confidence_length: u32,
    pub mask_length: u32,
}

#[inline]
fn u16_at(data: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([data[at], data[at + 1]])
}

#[inline]
fn u32_at(data: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]])
}

#[inline]
fn f32_at(data: &[u8], at: usize) -> f32 {
    f32::from_bits(u32_at(data, at))
}

impl FrameHeader {
    /// Decodes and validates the header at the start of `data`, in the
    /// order of the reference: size, magic, version, flags, tracking,
    /// orientation, depth dimensions, then the agreement of flags, plane
    /// sizes and lengths. The total size is checked by [`decode_frame`].
    pub fn parse(data: &[u8]) -> Result<Self, FormatError> {
        use ErrorCode::*;
        if data.len() < FRAME_HEADER_SIZE {
            return Err(FormatError::new(TooShort, format!("{} bytes, a header is {FRAME_HEADER_SIZE}", data.len())));
        }
        if data[..4] != FRAME_MAGIC {
            return Err(FormatError::new(BadMagic, format!("{:02x?}", &data[..4])));
        }
        if data[4] != FRAME_VERSION {
            return Err(FormatError::new(BadVersion, data[4].to_string()));
        }
        let flags = data[5];
        if flags & !KNOWN_FLAGS != 0 {
            return Err(FormatError::new(BadFlags, format!("0x{flags:02x}")));
        }
        let tracking = Tracking::from_u8(data[6]).ok_or_else(|| FormatError::new(BadTracking, data[6].to_string()))?;
        let orientation =
            Orientation::from_u8(data[7]).ok_or_else(|| FormatError::new(BadOrientation, data[7].to_string()))?;
        let mut world_from_cam = [0.0f32; 16];
        for (i, value) in world_from_cam.iter_mut().enumerate() {
            *value = f32_at(data, 24 + 4 * i);
        }
        let mut intrinsics = [0.0f32; 4];
        for (i, value) in intrinsics.iter_mut().enumerate() {
            *value = f32_at(data, 88 + 4 * i);
        }
        let mut timestamp = [0u8; 8];
        timestamp.copy_from_slice(&data[16..24]);
        let header = Self {
            flags,
            tracking,
            orientation,
            index: u32_at(data, 8),
            reserved: u32_at(data, 12),
            timestamp: f64::from_le_bytes(timestamp),
            world_from_cam,
            intrinsics,
            color_width: u16_at(data, 104),
            color_height: u16_at(data, 106),
            color_length: u32_at(data, 108),
            depth_width: u16_at(data, 112),
            depth_height: u16_at(data, 114),
            depth_length: u32_at(data, 116),
            confidence_length: u32_at(data, 120),
            mask_length: u32_at(data, 124),
        };
        let (dw, dh) = (header.depth_width, header.depth_height);
        if dw < 1 || dh < 1 {
            return Err(FormatError::new(BadDimensions, format!("depth {dw}x{dh}")));
        }
        // At most 2^32, no overflow.
        let pixels = dw as u32 * dh as u32;
        if pixels > MAX_DEPTH_PIXELS {
            return Err(FormatError::new(BadDimensions, format!("depth {dw}x{dh} is more than {MAX_DEPTH_PIXELS} pixels")));
        }
        let (has_conf, has_mask) = (header.has_confidence(), header.has_mask());
        if has_conf != (header.confidence_length != 0) {
            return Err(FormatError::new(BadLength, "confidence flag and length disagree"));
        }
        if has_mask != (header.mask_length != 0) {
            return Err(FormatError::new(BadLength, "mask flag and length disagree"));
        }
        if !header.is_deflated() {
            if header.depth_length != 2 * pixels {
                return Err(FormatError::new(
                    BadLength,
                    format!("depth is {} bytes, expected {}", header.depth_length, 2 * pixels),
                ));
            }
            if has_conf && header.confidence_length != pixels {
                return Err(FormatError::new(
                    BadLength,
                    format!("confidence is {} bytes, expected {pixels}", header.confidence_length),
                ));
            }
            if has_mask && header.mask_length != pixels {
                return Err(FormatError::new(BadLength, format!("mask is {} bytes, expected {pixels}", header.mask_length)));
            }
        } else if header.depth_length == 0 {
            return Err(FormatError::new(BadLength, "compressed depth is empty"));
        }
        Ok(header)
    }

    pub fn has_confidence(&self) -> bool {
        self.flags & FLAG_CONFIDENCE != 0
    }

    pub fn has_mask(&self) -> bool {
        self.flags & FLAG_MASK != 0
    }

    pub fn is_deflated(&self) -> bool {
        self.flags & FLAG_DEFLATE != 0
    }

    /// Pixels of the depth map (and of the confidence and the mask).
    pub fn depth_pixels(&self) -> usize {
        self.depth_width as usize * self.depth_height as usize
    }

    /// Size of the whole frame: the header plus the four lengths, added in
    /// 64 bits (four `u32` can exceed 2^32).
    pub fn total_length(&self) -> u64 {
        FRAME_HEADER_SIZE as u64
            + self.color_length as u64
            + self.depth_length as u64
            + self.confidence_length as u64
            + self.mask_length as u64
    }
}

/// A decoded capture frame; the planes are held uncompressed.
#[derive(Clone, Debug, PartialEq)]
pub struct CaptureFrame {
    /// Grows from frame to frame, with gaps; never reset.
    pub index: u32,
    /// Seconds since the device booted: only differences mean something.
    pub timestamp: f64,
    /// Camera-to-world, column-major. ARKit's conventions are ours.
    pub world_from_cam: [f32; 16],
    /// `fx, fy, cx, cy` in pixels of the colour image as sent.
    pub intrinsics: [f32; 4],
    pub tracking: Tracking,
    pub orientation: Orientation,
    /// The pixel grid the intrinsics refer to (also without a colour image).
    pub color_width: u16,
    pub color_height: u16,
    /// The colour image, a JPEG; empty when the frame carries none.
    pub color: Vec<u8>,
    pub depth_width: u16,
    pub depth_height: u16,
    /// Distance along the camera axis in millimeters, row-major from the
    /// top-left; `0` = no value.
    pub depth_mm: Vec<u16>,
    /// One byte per depth pixel: 0 low, 1 medium, 2 high.
    pub confidence: Option<Vec<u8>>,
    /// One byte per depth pixel: 0 background, 255 person.
    pub mask: Option<Vec<u8>>,
    /// The planes travelled deflated.
    pub compressed: bool,
    pub reserved: u32,
}

impl Default for CaptureFrame {
    /// A 1x1 frame without depth at the origin.
    fn default() -> Self {
        Self {
            index: 0,
            timestamp: 0.0,
            world_from_cam: glam::Mat4::IDENTITY.to_cols_array(),
            intrinsics: [1.0, 1.0, 0.0, 0.0],
            tracking: Tracking::Normal,
            orientation: Orientation::LandscapeNative,
            color_width: 0,
            color_height: 0,
            color: Vec::new(),
            depth_width: 1,
            depth_height: 1,
            depth_mm: vec![0],
            confidence: None,
            mask: None,
            compressed: false,
            reserved: 0,
        }
    }
}

impl CaptureFrame {
    /// The flags byte this frame is written with.
    pub fn flags(&self) -> u8 {
        let mut flags = 0;
        if self.confidence.is_some() {
            flags |= FLAG_CONFIDENCE;
        }
        if self.compressed {
            flags |= FLAG_DEFLATE;
        }
        if self.mask.is_some() {
            flags |= FLAG_MASK;
        }
        flags
    }

    /// Pixels of the depth map.
    pub fn depth_pixels(&self) -> usize {
        self.depth_width as usize * self.depth_height as usize
    }
}

/// One depth sample as the phone converts it: `floor(m · 1000 + 0.5)` in
/// double precision; not positive, not finite or beyond 65.535 m gives 0.
pub fn depth_meters_to_mm(meters: f32) -> u16 {
    if meters.is_nan() || meters <= 0.0 || meters.is_infinite() {
        return 0;
    }
    let mm = (meters as f64 * 1000.0 + 0.5).floor();
    if mm > 65535.0 {
        0
    } else {
        mm as u16
    }
}

/// Inflates one raw deflate stream that must yield exactly `expected` bytes
/// and end exactly at the end of `data`.
///
/// The output buffer grows with what the stream really produces, up to one
/// byte past `expected`: a small stream cannot make the reader reserve the
/// megabytes its header announces, and a stream that inflates to gigabytes
/// is stopped at once.
fn inflate_exact(data: &[u8], expected: usize, plane: &str) -> Result<Vec<u8>, FormatError> {
    let bad = |what: String| FormatError::new(ErrorCode::BadDeflate, format!("{plane}: {what}"));
    let limit = expected + 1;
    let mut out = vec![0u8; limit.min((data.len().saturating_mul(4)).max(1024))];
    let mut state = DecompressorOxide::new();
    let (mut in_pos, mut out_pos) = (0usize, 0usize);
    loop {
        let flags = inflate_flags::TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF;
        let (status, consumed, written) = decompress(&mut state, &data[in_pos..], &mut out, out_pos, flags);
        in_pos += consumed;
        out_pos += written;
        match status {
            TINFLStatus::Done => break,
            TINFLStatus::HasMoreOutput => {
                if out.len() >= limit {
                    return Err(bad(format!("inflates to more than {expected} bytes")));
                }
                let grown = out.len().saturating_mul(2).min(limit);
                out.resize(grown, 0);
            }
            // The input is all there: asking for more means it is cut.
            TINFLStatus::NeedsMoreInput | TINFLStatus::FailedCannotMakeProgress => {
                return Err(bad("stream is not terminated".into()));
            }
            _ => return Err(bad("invalid deflate stream".into())),
        }
    }
    if out_pos != expected {
        let more = if out_pos > expected { "more than " } else { "" };
        return Err(bad(format!("inflates to {more}{} bytes, expected {expected}", out_pos.min(expected))));
    }
    if in_pos != data.len() {
        return Err(bad("bytes after the end of the stream".into()));
    }
    out.truncate(expected);
    Ok(out)
}

/// Parses one capture frame.
///
/// Checks are made in the order of the reference: the header fields, the
/// total length (before any slice is taken by an announced length), then
/// the inflation of depth, confidence and mask.
pub fn decode_frame(data: &[u8]) -> Result<CaptureFrame, FormatError> {
    let h = FrameHeader::parse(data)?;
    if data.len() as u64 != h.total_length() {
        return Err(FormatError::new(
            ErrorCode::BadLength,
            format!("frame is {} bytes, header announces {}", data.len(), h.total_length()),
        ));
    }
    // The four lengths add up to the size of `data`: none of them wraps.
    let mut pos = FRAME_HEADER_SIZE;
    let mut take = |length: u32| {
        let part = &data[pos..pos + length as usize];
        pos += length as usize;
        part
    };
    let color = take(h.color_length);
    let depth = take(h.depth_length);
    let confidence = take(h.confidence_length);
    let mask = take(h.mask_length);
    let pixels = h.depth_pixels();
    let plane = |stored: &[u8], size: usize, name: &str| -> Result<Vec<u8>, FormatError> {
        if h.is_deflated() {
            inflate_exact(stored, size, name)
        } else {
            Ok(stored.to_vec())
        }
    };
    let depth = plane(depth, 2 * pixels, "depth")?;
    let confidence = if h.has_confidence() { Some(plane(confidence, pixels, "confidence")?) } else { None };
    let mask = if h.has_mask() { Some(plane(mask, pixels, "mask")?) } else { None };
    Ok(CaptureFrame {
        index: h.index,
        timestamp: h.timestamp,
        world_from_cam: h.world_from_cam,
        intrinsics: h.intrinsics,
        tracking: h.tracking,
        orientation: h.orientation,
        color_width: h.color_width,
        color_height: h.color_height,
        color: color.to_vec(),
        depth_width: h.depth_width,
        depth_height: h.depth_height,
        depth_mm: depth.chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect(),
        confidence,
        mask,
        compressed: h.is_deflated(),
        reserved: h.reserved,
    })
}

/// Serialises a frame; the planes are deflated when `frame.compressed` is
/// set. Fails when the planes do not have the size of the depth map or a
/// length does not fit its field.
pub fn encode_frame(frame: &CaptureFrame) -> Result<Vec<u8>, FormatError> {
    use ErrorCode::*;
    let (dw, dh) = (frame.depth_width, frame.depth_height);
    if dw < 1 || dh < 1 {
        return Err(FormatError::new(BadDimensions, "depth must be at least 1x1"));
    }
    let pixels = frame.depth_pixels();
    if pixels > MAX_DEPTH_PIXELS as usize {
        return Err(FormatError::new(BadDimensions, format!("depth of more than {MAX_DEPTH_PIXELS} pixels")));
    }
    if frame.depth_mm.len() != pixels {
        return Err(FormatError::new(BadLength, format!("depth plane must hold {pixels} values")));
    }
    for (plane, name) in [(&frame.confidence, "confidence"), (&frame.mask, "mask")] {
        if plane.as_ref().is_some_and(|p| p.len() != pixels) {
            return Err(FormatError::new(BadLength, format!("{name} plane must be {pixels} bytes")));
        }
    }
    let mut depth: Vec<u8> = Vec::with_capacity(2 * pixels);
    for mm in &frame.depth_mm {
        depth.extend_from_slice(&mm.to_le_bytes());
    }
    let mut confidence = frame.confidence.clone().unwrap_or_default();
    let mut mask = frame.mask.clone().unwrap_or_default();
    if frame.compressed {
        let deflate = |plane: &[u8]| miniz_oxide::deflate::compress_to_vec(plane, 6);
        depth = deflate(&depth);
        if frame.confidence.is_some() {
            confidence = deflate(&confidence);
        }
        if frame.mask.is_some() {
            mask = deflate(&mask);
        }
    }
    let length = |bytes: &[u8], name: &str| {
        u32::try_from(bytes.len()).map_err(|_| FormatError::new(BadValue, format!("{name} does not fit in 32 bits")))
    };
    let lengths = [
        length(&frame.color, "color length")?,
        length(&depth, "depth length")?,
        length(&confidence, "confidence length")?,
        length(&mask, "mask length")?,
    ];
    let mut out = Vec::with_capacity(FRAME_HEADER_SIZE + frame.color.len() + depth.len() + confidence.len() + mask.len());
    out.extend_from_slice(&FRAME_MAGIC);
    out.extend_from_slice(&[FRAME_VERSION, frame.flags(), frame.tracking as u8, frame.orientation as u8]);
    out.extend_from_slice(&frame.index.to_le_bytes());
    out.extend_from_slice(&frame.reserved.to_le_bytes());
    out.extend_from_slice(&frame.timestamp.to_le_bytes());
    for value in frame.world_from_cam.iter().chain(&frame.intrinsics) {
        out.extend_from_slice(&value.to_le_bytes());
    }
    out.extend_from_slice(&frame.color_width.to_le_bytes());
    out.extend_from_slice(&frame.color_height.to_le_bytes());
    out.extend_from_slice(&lengths[0].to_le_bytes());
    out.extend_from_slice(&dw.to_le_bytes());
    out.extend_from_slice(&dh.to_le_bytes());
    for length in &lengths[1..] {
        out.extend_from_slice(&length.to_le_bytes());
    }
    debug_assert_eq!(out.len(), FRAME_HEADER_SIZE);
    out.extend_from_slice(&frame.color);
    out.extend_from_slice(&depth);
    out.extend_from_slice(&confidence);
    out.extend_from_slice(&mask);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> CaptureFrame {
        CaptureFrame {
            index: 7,
            timestamp: 12.5,
            color_width: 16,
            color_height: 12,
            color: vec![0xFF, 0xD8, 0xFF, 0xD9],
            depth_width: 4,
            depth_height: 3,
            depth_mm: (0..12).map(|i| i * 400).collect(),
            confidence: Some(vec![2; 12]),
            mask: Some((0..12).map(|i| if i % 2 == 0 { 255 } else { 0 }).collect()),
            ..CaptureFrame::default()
        }
    }

    #[test]
    fn frames_round_trip_plain_and_deflated() {
        for compressed in [false, true] {
            let frame = CaptureFrame { compressed, ..sample() };
            let bytes = encode_frame(&frame).unwrap();
            assert_eq!(decode_frame(&bytes).unwrap(), frame);
            let header = FrameHeader::parse(&bytes).unwrap();
            assert_eq!(header.total_length(), bytes.len() as u64);
            assert_eq!(header.flags, if compressed { 7 } else { 5 });
        }
        let bare = CaptureFrame::default();
        assert_eq!(decode_frame(&encode_frame(&bare).unwrap()).unwrap(), bare);
    }

    #[test]
    fn encoding_refuses_planes_of_the_wrong_size() {
        let code = |frame: CaptureFrame| encode_frame(&frame).unwrap_err().code;
        assert_eq!(code(CaptureFrame { depth_width: 0, ..sample() }), ErrorCode::BadDimensions);
        assert_eq!(code(CaptureFrame { depth_width: 4096, depth_height: 4096, ..sample() }), ErrorCode::BadDimensions);
        assert_eq!(code(CaptureFrame { depth_mm: vec![0; 11], ..sample() }), ErrorCode::BadLength);
        assert_eq!(code(CaptureFrame { confidence: Some(vec![0; 13]), ..sample() }), ErrorCode::BadLength);
        assert_eq!(code(CaptureFrame { mask: Some(Vec::new()), ..sample() }), ErrorCode::BadLength);
    }

    #[test]
    fn inflation_stops_one_byte_past_the_plane() {
        // 64 MiB of zeros deflate to about 64 kB: a decoder that trusts the
        // stream would allocate it all.
        let bomb = miniz_oxide::deflate::compress_to_vec(&vec![0u8; 64 << 20], 6);
        assert!(bomb.len() < 100_000);
        let err = inflate_exact(&bomb, 24, "depth").unwrap_err();
        assert_eq!(err.code, ErrorCode::BadDeflate);
        assert!(err.detail.contains("more than 24"), "{err}");

        let good = miniz_oxide::deflate::compress_to_vec(&[1, 2, 3, 4], 6);
        assert_eq!(inflate_exact(&good, 4, "p").unwrap(), [1, 2, 3, 4]);
        assert_eq!(inflate_exact(&good, 5, "p").unwrap_err().code, ErrorCode::BadDeflate);
        assert_eq!(inflate_exact(&good, 3, "p").unwrap_err().code, ErrorCode::BadDeflate);
        assert_eq!(inflate_exact(&good[..good.len() - 1], 4, "p").unwrap_err().code, ErrorCode::BadDeflate);
        let mut trailing = good.clone();
        trailing.push(0);
        assert!(inflate_exact(&trailing, 4, "p").unwrap_err().detail.contains("after the end"));
        assert_eq!(inflate_exact(&[], 0, "p").unwrap_err().code, ErrorCode::BadDeflate);
        // An empty plane is a valid, terminated stream of nothing.
        let empty = miniz_oxide::deflate::compress_to_vec(&[], 6);
        assert_eq!(inflate_exact(&empty, 0, "p").unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn depth_conversion_rounds_half_up_and_drops_what_is_not_a_depth() {
        assert_eq!(depth_meters_to_mm(1.0), 1000);
        assert_eq!(depth_meters_to_mm(0.0005), 1);
        assert_eq!(depth_meters_to_mm(0.00049), 0);
        assert_eq!(depth_meters_to_mm(65.535), 65535);
        for bad in [0.0, -0.0, -1.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 65.536, 1.0e30] {
            assert_eq!(depth_meters_to_mm(bad), 0, "{bad}");
        }
    }
}
