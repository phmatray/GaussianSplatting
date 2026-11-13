//! Binary wire messages.
//!
//! One self-describing message per WebSocket frame or Worker `postMessage`;
//! the same bytes travel over both transports. All integers and floats are
//! little-endian.
//!
//! # Header (16 bytes)
//!
//! | Offset | Type | Field |
//! |---|---|---|
//! | 0 | `[u8; 4]` | magic `b"GSL1"` (the digit is the format version) |
//! | 4 | `u8` | kind: 1 = State, 2 = Layer, 3 = Preview, 4 = Stats |
//! | 5 | `u8` | flags (0; ignored when decoding) |
//! | 6 | `u16` | reserved (0; ignored when decoding) |
//! | 8 | `u32` | `seq`, sender's message counter |
//! | 12 | `u32` | `time_ms`, scene time in milliseconds |
//!
//! # State / Stats body
//!
//! UTF-8 JSON text filling the rest of the message
//! (`protocol::SessionState` and `protocol::Stats`).
//!
//! # Layer body
//!
//! | Offset | Type | Field |
//! |---|---|---|
//! | 0 | `u8` | `layer_id` (0 = static, 1 = live) |
//! | 1 | `u8` | mode: 0 = Replace, 1 = Append |
//! | 2 | `u16` | reserved |
//! | 4 | `u32` | `count` |
//! | 8 | `[f32; 3]` | `aabb_min` of the splat centers |
//! | 20 | `[f32; 3]` | `aabb_max` |
//! | 32 | `count × 16` | quantized splats |
//!
//! The message length must be exactly `16 + 32 + 16·count` bytes.
//!
//! ## Quantized splat ([`SPLAT_WIRE_SIZE`] = 16 bytes)
//!
//! | Offset | Type | Field |
//! |---|---|---|
//! | 0 | `3 × u16` | position `x, y, z`, unorm inside the AABB: `p = min·(1 − u/65535) + max·(u/65535)` |
//! | 6 | `3 × u8` | scale, log-quantized: `s = SCALE_MIN · exp(code · ln(SCALE_MAX/SCALE_MIN)/255)` |
//! | 9 | `4 × u8` | `r, g, b, a` (exact) |
//! | 13 | `3 × u8` | rotation, 24-bit little-endian word `w = b13 | b14 << 8 | b15 << 16` |
//!
//! Scales cover [`SCALE_MIN`] = 0.1 mm to [`SCALE_MAX`] = 10 m in steps of
//! 4.6 % (relative error ≤ 2.3 %); values outside are clamped.
//!
//! The rotation is a "smallest three" unit quaternion:
//!
//! * bits 22–23 of `w`: index `k` of the dropped component, the one with the
//!   largest magnitude, in `x, y, z, w` order (0 = x … 3 = w);
//! * bits 0–21: `v = l0 + 161·l1 + 161²·l2`, where `l0, l1, l2` are the three
//!   remaining components in `x, y, z, w` order, each quantized to
//!   [`ROT_LEVELS`] = 161 levels: `l = round((c·√2 + 1)/2 · 160)` after
//!   flipping the quaternion's sign so that the dropped component is positive
//!   (each remaining component then lies in `[−1/√2, 1/√2]`).
//!
//! Decoding: `c = (l/160·2 − 1)/√2`, dropped component `= √(1 − Σc²)`.
//! The worst-case rotation error is below 1.8°.

use std::fmt;

use glam::Vec3;
#[cfg(feature = "par")]
use rayon::prelude::*;

use crate::types::{Aabb, RgbdFrame, Splat};

/// First four bytes of every message; the digit is the format version.
pub const MAGIC: [u8; 4] = *b"GSL1";
/// Size of the message header in bytes.
pub const HEADER_SIZE: usize = 16;
/// Size of the fixed part of a layer body in bytes.
pub const LAYER_HEADER_SIZE: usize = 32;
/// Size of the fixed part of a preview body in bytes.
pub const PREVIEW_HEADER_SIZE: usize = 8;
/// Size of one quantized splat in bytes.
pub const SPLAT_WIRE_SIZE: usize = 16;

/// Smallest representable standard deviation (meters).
pub const SCALE_MIN: f32 = 1.0e-4;
/// Largest representable standard deviation (meters).
pub const SCALE_MAX: f32 = 10.0;
/// `ln(SCALE_MAX / SCALE_MIN) / 255`: one scale code step in log space.
const SCALE_LOG_STEP: f32 = 0.045_148_727;

/// Quantization levels per stored quaternion component (`161³ < 2²²`).
pub const ROT_LEVELS: u32 = 161;
const ROT_MAX_LEVEL: f32 = (ROT_LEVELS - 1) as f32;
/// Indices of the three stored quaternion components, by dropped component.
const ROT_KEPT: [[usize; 3]; 4] = [[1, 2, 3], [0, 2, 3], [0, 1, 3], [0, 1, 2]];

/// Message kind, byte 4 of the header.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Kind {
    /// JSON session state.
    State = 1,
    /// Quantized splats of one layer.
    Layer = 2,
    /// Downsampled RGB-D image of one camera.
    Preview = 3,
    /// JSON statistics.
    Stats = 4,
}

impl Kind {
    /// Parses the kind byte.
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::State),
            2 => Some(Self::Layer),
            3 => Some(Self::Preview),
            4 => Some(Self::Stats),
            _ => None,
        }
    }
}

/// What a layer message does to the receiver's copy of the layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum LayerMode {
    /// The splats replace the layer's content.
    Replace = 0,
    /// The splats are added to the layer.
    Append = 1,
}

/// Decoded message header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub kind: Kind,
    pub flags: u8,
    pub seq: u32,
    pub time_ms: u32,
}

/// Fixed part of a layer message, returned by [`decode_layer_into`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LayerHeader {
    pub seq: u32,
    pub time_ms: u32,
    pub layer_id: u8,
    pub mode: LayerMode,
    /// Number of splats in the message.
    pub count: u32,
    /// Bounds of the splat centers (all zero for an empty layer).
    pub aabb: Aabb,
}

/// A layer update.
#[derive(Clone, Debug, PartialEq)]
pub struct LayerMsg {
    pub layer_id: u8,
    pub mode: LayerMode,
    pub splats: Vec<Splat>,
}

/// A camera thumbnail: color plus depth in millimetres.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PreviewMsg {
    pub camera_id: u8,
    pub width: u16,
    pub height: u16,
    /// sRGB, `width·height·3` bytes, top row first.
    pub rgb: Vec<u8>,
    /// Forward depth in millimetres, `width·height` values. 0 = invalid,
    /// 65535 = 65.535 m or farther.
    pub depth_mm: Vec<u16>,
}

impl PreviewMsg {
    /// Converts a frame. Frames larger than 65535 pixels on a side are
    /// cropped (previews are thumbnails; this never happens in practice).
    pub fn from_frame(camera_id: u8, frame: &RgbdFrame) -> Self {
        let (width, height) = preview_size(frame);
        let mut msg = Self {
            camera_id,
            width,
            height,
            rgb: Vec::with_capacity(width as usize * height as usize * 3),
            depth_mm: Vec::with_capacity(width as usize * height as usize),
        };
        for_each_preview_row(frame, width, height, |rgb, depth| {
            msg.rgb.extend_from_slice(rgb);
            msg.depth_mm.extend(depth.iter().map(|&d| depth_to_mm(d)));
        });
        msg
    }

    /// Converts back to a frame with depth in meters.
    pub fn to_frame(&self) -> RgbdFrame {
        let n = self.width as usize * self.height as usize;
        let mut frame = RgbdFrame::new(self.width as u32, self.height as u32);
        let rgb_len = self.rgb.len().min(n * 3);
        frame.rgb[..rgb_len].copy_from_slice(&self.rgb[..rgb_len]);
        for (dst, &mm) in frame.depth.iter_mut().zip(&self.depth_mm) {
            *dst = mm as f32 * 1.0e-3;
        }
        frame
    }
}

/// Payload of a [`Message`].
#[derive(Clone, Debug, PartialEq)]
pub enum Body {
    /// JSON `SessionState`.
    State(String),
    Layer(LayerMsg),
    Preview(PreviewMsg),
    /// JSON `Stats`.
    Stats(String),
}

impl Body {
    pub fn kind(&self) -> Kind {
        match self {
            Body::State(_) => Kind::State,
            Body::Layer(_) => Kind::Layer,
            Body::Preview(_) => Kind::Preview,
            Body::Stats(_) => Kind::Stats,
        }
    }
}

/// One wire message.
#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    /// Sender's message counter.
    pub seq: u32,
    /// Scene time in milliseconds.
    pub time_ms: u32,
    pub body: Body,
}

/// Why a buffer could not be decoded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// The buffer ends before the data it announces.
    Truncated { needed: u64, available: usize },
    /// The buffer is longer than the message it contains.
    TrailingBytes { expected: u64, actual: usize },
    /// The buffer does not start with [`MAGIC`].
    BadMagic,
    /// Unknown message kind byte.
    UnknownKind(u8),
    /// The message is valid but not of the expected kind.
    UnexpectedKind { expected: Kind, found: Kind },
    /// Unknown layer mode byte.
    BadLayerMode(u8),
    /// The layer bounding box is not finite or has `min > max`.
    BadAabb,
    /// A JSON body is not valid UTF-8.
    InvalidUtf8,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Truncated { needed, available } => {
                write!(f, "message truncated: {needed} bytes needed, {available} available")
            }
            Self::TrailingBytes { expected, actual } => {
                write!(f, "message too long: {expected} bytes expected, {actual} received")
            }
            Self::BadMagic => write!(f, "not a GSL1 message (bad magic)"),
            Self::UnknownKind(kind) => write!(f, "unknown message kind {kind}"),
            Self::UnexpectedKind { expected, found } => {
                write!(f, "expected a {expected:?} message, found {found:?}")
            }
            Self::BadLayerMode(mode) => write!(f, "unknown layer mode {mode}"),
            Self::BadAabb => write!(f, "invalid layer bounding box"),
            Self::InvalidUtf8 => write!(f, "text body is not valid UTF-8"),
        }
    }
}

impl std::error::Error for DecodeError {}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

fn write_header(out: &mut Vec<u8>, kind: Kind, seq: u32, time_ms: u32) {
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&[kind as u8, 0, 0, 0]);
    out.extend_from_slice(&seq.to_le_bytes());
    out.extend_from_slice(&time_ms.to_le_bytes());
}

/// Scale → 8-bit log code, i.e. `round(ln(|scale| / SCALE_MIN) / SCALE_LOG_STEP)`
/// clamped to `0..=255`, without calling `ln` per value (three logarithms per
/// splat would dominate the encoder, especially on wasm).
struct ScaleQuantizer {
    /// `lower[c]`: smallest scale that rounds to code `c` (geometric mean of
    /// the values of codes `c − 1` and `c`). `lower[0] = 0` and
    /// `lower[256] = NaN` are sentinels that never trigger a correction.
    lower: [f32; 257],
}

impl ScaleQuantizer {
    fn new() -> Self {
        let mut lower = [0.0; 257];
        for (code, bound) in lower.iter_mut().enumerate().take(256).skip(1) {
            *bound = SCALE_MIN * ((code as f32 - 0.5) * SCALE_LOG_STEP).exp();
        }
        // NaN: `s >= lower[256]` is false even for an infinite scale.
        lower[256] = f32::NAN;
        Self { lower }
    }

    /// Negative scales count by magnitude; NaN and values below
    /// [`SCALE_MIN`] give 0, values above [`SCALE_MAX`] give 255.
    #[inline]
    fn quantize(&self, scale: f32) -> u8 {
        /// `1 / (SCALE_LOG_STEP · log2(e))`: codes per octave.
        const CODES_PER_OCTAVE: f32 = std::f32::consts::LN_2 / SCALE_LOG_STEP;
        /// `log2(SCALE_MIN)`.
        const LOG2_SCALE_MIN: f32 = -13.287_712;

        let s = scale.abs();
        // Estimate log2(s) from the float's exponent and a quadratic fit of
        // log2(1 + m) on the mantissa. The error is below 0.01 octave (0.15
        // code), so the rounded estimate is at most one code off; the two
        // branch-free corrections against the exact bounds settle it.
        let bits = s.to_bits();
        let exponent = (bits >> 23) as i32 - 127;
        let m = (bits & 0x7F_FFFF) as f32 * (1.0 / (1 << 23) as f32);
        let log2 = exponent as f32 + m * (1.346_5 - 0.346_5 * m);
        // NaN: the comparisons below are false and the cast gives 0.
        let estimate = ((log2 - LOG2_SCALE_MIN) * CODES_PER_OCTAVE + 0.5).clamp(0.0, 255.0);
        let mut code = if s.is_nan() { 0 } else { estimate as usize };
        code += (s >= self.lower[code + 1]) as usize;
        code -= (s < self.lower[code]) as usize;
        code as u8
    }
}

/// The 256 scale values, indexed by code.
fn scale_table() -> [f32; 256] {
    let mut table = [0.0; 256];
    for (code, value) in table.iter_mut().enumerate() {
        *value = SCALE_MIN * (code as f32 * SCALE_LOG_STEP).exp();
    }
    table[255] = SCALE_MAX;
    table
}

/// Unit quaternion `[x, y, z, w]` → 24-bit smallest-three word (see the
/// module documentation). Degenerate input encodes the identity.
#[inline]
fn quantize_rotation(rot: [f32; 4]) -> u32 {
    let len2 = rot[0] * rot[0] + rot[1] * rot[1] + rot[2] * rot[2] + rot[3] * rot[3];
    let q = if len2.is_finite() && len2 > 1e-24 {
        let inv = len2.sqrt().recip();
        [rot[0] * inv, rot[1] * inv, rot[2] * inv, rot[3] * inv]
    } else {
        [0.0, 0.0, 0.0, 1.0]
    };
    // Branch-free argmax (first wins on ties): random rotations would
    // otherwise mispredict constantly.
    let mut largest = 0;
    let mut best = q[0].abs();
    for (i, c) in q.iter().enumerate().skip(1) {
        let better = c.abs() > best;
        largest = if better { i } else { largest };
        best = if better { c.abs() } else { best };
    }
    // q and −q are the same rotation: make the dropped component positive.
    let sign = if q[largest] < 0.0 { -1.0 } else { 1.0 };
    let level = |i: usize| {
        let unorm = q[i] * sign * std::f32::consts::FRAC_1_SQRT_2 + 0.5;
        ((unorm * ROT_MAX_LEVEL + 0.5) as u32).min(ROT_LEVELS - 1)
    };
    let [a, b, c] = ROT_KEPT[largest];
    (largest as u32) << 22 | (level(a) + ROT_LEVELS * (level(b) + ROT_LEVELS * level(c)))
}

/// Inverse of [`quantize_rotation`]. Any 24-bit word decodes to a unit
/// quaternion.
#[inline]
fn dequantize_rotation(word: u32) -> [f32; 4] {
    let largest = ((word >> 22) & 3) as usize;
    let v = word & 0x3F_FFFF;
    // Words written by other software may exceed 161³ − 1: clamp the last digit.
    let levels = [v % ROT_LEVELS, v / ROT_LEVELS % ROT_LEVELS, (v / (ROT_LEVELS * ROT_LEVELS)).min(ROT_LEVELS - 1)];
    let mut q = [0.0f32; 4];
    let mut sum = 0.0;
    for (&i, &level) in ROT_KEPT[largest].iter().zip(&levels) {
        let c = (level as f32 * (2.0 / ROT_MAX_LEVEL) - 1.0) * std::f32::consts::FRAC_1_SQRT_2;
        q[i] = c;
        sum += c * c;
    }
    if sum <= 1.0 {
        q[largest] = (1.0 - sum).sqrt();
    } else {
        // Out-of-range combination from a non-conforming encoder.
        let inv = sum.sqrt().recip();
        q.iter_mut().for_each(|c| *c *= inv);
    }
    q
}

/// Bounds of the finite splat centers; all zero when there is none.
fn center_bounds(splats: &[Splat]) -> ([f32; 3], [f32; 3]) {
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    for s in splats {
        for k in 0..3 {
            // NaN fails both comparisons; infinities are excluded explicitly.
            let p = s.pos[k];
            if p < min[k] && p > f32::NEG_INFINITY {
                min[k] = p;
            }
            if p > max[k] && p < f32::INFINITY {
                max[k] = p;
            }
        }
    }
    for k in 0..3 {
        if min[k] > max[k] {
            min[k] = 0.0;
            max[k] = 0.0;
        }
    }
    (min, max)
}

/// Encodes a layer message into `out` (cleared first). This is the hot path
/// of the pipeline: apart from growing `out` it does not allocate. With the
/// `par` feature large layers are quantized on the rayon pool; the bytes are
/// identical either way.
///
/// Quantization (see the module documentation): positions to 16 bits inside
/// the bounding box of the centers, scales to 8 bits (log), rotation to
/// 24 bits; color and opacity are exact. Non-finite position coordinates are
/// clamped into the box. At most `u32::MAX` splats are written.
pub fn encode_layer(seq: u32, time_ms: u32, layer_id: u8, mode: LayerMode, splats: &[Splat], out: &mut Vec<u8>) {
    let count = u32::try_from(splats.len()).unwrap_or(u32::MAX);
    let splats = &splats[..count as usize];
    out.clear();
    out.reserve(HEADER_SIZE + LAYER_HEADER_SIZE + splats.len().saturating_mul(SPLAT_WIRE_SIZE));

    write_header(out, Kind::Layer, seq, time_ms);
    let (min, max) = center_bounds(splats);
    out.extend_from_slice(&[layer_id, mode as u8, 0, 0]);
    out.extend_from_slice(&count.to_le_bytes());
    for v in min.iter().chain(&max) {
        out.extend_from_slice(&v.to_le_bytes());
    }

    let quantizer = LayerQuantizer::new(min, max);
    #[cfg(feature = "par")]
    if splats.len() >= PAR_MIN_SPLATS {
        let start = out.len();
        out.resize(start + splats.len() * SPLAT_WIRE_SIZE, 0);
        out[start..]
            .par_chunks_mut(PAR_CHUNK_SPLATS * SPLAT_WIRE_SIZE)
            .zip(splats.par_chunks(PAR_CHUNK_SPLATS))
            .for_each(|(wire, chunk)| {
                for (w, s) in wire.chunks_exact_mut(SPLAT_WIRE_SIZE).zip(chunk) {
                    w.copy_from_slice(&quantizer.encode(s));
                }
            });
        return;
    }
    for s in splats {
        out.extend_from_slice(&quantizer.encode(s));
    }
}

/// Layers smaller than this are encoded on the calling thread.
#[cfg(feature = "par")]
const PAR_MIN_SPLATS: usize = 32_768;
/// Splats per parallel work item.
#[cfg(feature = "par")]
const PAR_CHUNK_SPLATS: usize = 8_192;

/// Quantization parameters of one layer message.
struct LayerQuantizer {
    min: [f32; 3],
    /// `65535 / extent` per axis; 0 for a degenerate (or overflowing) extent.
    to_unorm: [f32; 3],
    scales: ScaleQuantizer,
}

impl LayerQuantizer {
    fn new(min: [f32; 3], max: [f32; 3]) -> Self {
        let mut to_unorm = [0.0f32; 3];
        for k in 0..3 {
            let extent = max[k] - min[k];
            if extent > 0.0 && extent.is_finite() {
                to_unorm[k] = 65535.0 / extent;
            }
        }
        Self { min, to_unorm, scales: ScaleQuantizer::new() }
    }

    /// One splat in wire format (layout in the module documentation).
    #[inline]
    fn encode(&self, s: &Splat) -> [u8; SPLAT_WIRE_SIZE] {
        let mut wire = [0u8; SPLAT_WIRE_SIZE];
        for k in 0..3 {
            // The saturating cast clamps (and sends NaN to 0).
            let u = (((s.pos[k] - self.min[k]) * self.to_unorm[k] + 0.5) as u32).min(65535) as u16;
            wire[2 * k..2 * k + 2].copy_from_slice(&u.to_le_bytes());
            wire[6 + k] = self.scales.quantize(s.scale[k]);
        }
        wire[9..13].copy_from_slice(&s.rgba);
        let rot = quantize_rotation(s.rot);
        wire[13..16].copy_from_slice(&rot.to_le_bytes()[..3]);
        wire
    }
}

/// Saturating conversion of a forward depth in meters to millimetres.
/// Invalid (`≤ 0`, NaN) depths give 0; valid ones at least 1.
#[inline]
fn depth_to_mm(depth: f32) -> u16 {
    if depth > 0.0 {
        ((depth * 1000.0 + 0.5) as u32).clamp(1, u16::MAX as u32) as u16
    } else {
        0
    }
}

/// Preview size of a frame: its size, cropped to what the format can hold
/// and to the data actually present.
fn preview_size(frame: &RgbdFrame) -> (u16, u16) {
    let width = frame.width.min(u16::MAX as u32);
    let mut height = frame.height.min(u16::MAX as u32);
    if width > 0 {
        let stride = frame.width as usize;
        let rows = (frame.rgb.len() / 3).min(frame.depth.len()) / stride;
        height = height.min(rows.min(u16::MAX as usize) as u32);
    }
    if width == 0 || height == 0 {
        return (0, 0);
    }
    (width as u16, height as u16)
}

/// Calls `f(rgb_row, depth_row)` for every row of the `width × height`
/// top-left region of `frame`.
fn for_each_preview_row(frame: &RgbdFrame, width: u16, height: u16, mut f: impl FnMut(&[u8], &[f32])) {
    let stride = frame.width as usize;
    let w = width as usize;
    for y in 0..height as usize {
        f(&frame.rgb[y * stride * 3..][..w * 3], &frame.depth[y * stride..][..w]);
    }
}

/// Encodes a preview message for one camera frame straight into `out`
/// (cleared first), without building a [`PreviewMsg`].
pub fn encode_preview(seq: u32, time_ms: u32, camera_id: u8, frame: &RgbdFrame, out: &mut Vec<u8>) {
    let (width, height) = preview_size(frame);
    let n = width as usize * height as usize;
    out.clear();
    out.reserve(HEADER_SIZE + PREVIEW_HEADER_SIZE + n * 5);
    write_header(out, Kind::Preview, seq, time_ms);
    write_preview_header(out, camera_id, width, height);
    for_each_preview_row(frame, width, height, |rgb, _| out.extend_from_slice(rgb));
    for_each_preview_row(frame, width, height, |_, depth| {
        out.extend(depth.iter().flat_map(|&d| depth_to_mm(d).to_le_bytes()));
    });
}

fn write_preview_header(out: &mut Vec<u8>, camera_id: u8, width: u16, height: u16) {
    out.extend_from_slice(&[camera_id, 0]);
    out.extend_from_slice(&width.to_le_bytes());
    out.extend_from_slice(&height.to_le_bytes());
    out.extend_from_slice(&[0, 0]);
}

/// Encodes a message.
///
/// A [`PreviewMsg`] whose buffers do not match its declared size is padded
/// with zeros / cut so that the output is always well-formed.
pub fn encode(msg: &Message) -> Vec<u8> {
    let mut out = Vec::new();
    match &msg.body {
        Body::State(json) | Body::Stats(json) => {
            out.reserve(HEADER_SIZE + json.len());
            write_header(&mut out, msg.body.kind(), msg.seq, msg.time_ms);
            out.extend_from_slice(json.as_bytes());
        }
        Body::Layer(layer) => {
            encode_layer(msg.seq, msg.time_ms, layer.layer_id, layer.mode, &layer.splats, &mut out);
        }
        Body::Preview(preview) => {
            let n = preview.width as usize * preview.height as usize;
            out.reserve(HEADER_SIZE + PREVIEW_HEADER_SIZE + n * 5);
            write_header(&mut out, Kind::Preview, msg.seq, msg.time_ms);
            write_preview_header(&mut out, preview.camera_id, preview.width, preview.height);
            let rgb_start = out.len();
            out.extend_from_slice(&preview.rgb[..preview.rgb.len().min(n * 3)]);
            out.resize(rgb_start + n * 3, 0);
            out.extend(preview.depth_mm.iter().take(n).flat_map(|d| d.to_le_bytes()));
            out.resize(rgb_start + n * 5, 0);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

#[inline]
fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([bytes[offset], bytes[offset + 1]])
}

#[inline]
fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([bytes[offset], bytes[offset + 1], bytes[offset + 2], bytes[offset + 3]])
}

#[inline]
fn f32_at(bytes: &[u8], offset: usize) -> f32 {
    f32::from_bits(u32_at(bytes, offset))
}

/// Checks that `bytes` is exactly `expected` bytes long.
fn check_len(bytes: &[u8], expected: u64) -> Result<(), DecodeError> {
    let actual = bytes.len();
    match (actual as u64).cmp(&expected) {
        std::cmp::Ordering::Less => Err(DecodeError::Truncated { needed: expected, available: actual }),
        std::cmp::Ordering::Greater => Err(DecodeError::TrailingBytes { expected, actual }),
        std::cmp::Ordering::Equal => Ok(()),
    }
}

/// Decodes the 16-byte header without touching the body.
pub fn peek_header(bytes: &[u8]) -> Result<Header, DecodeError> {
    if bytes.len() < HEADER_SIZE {
        // Report a wrong magic even on a short buffer when it is visible.
        if !MAGIC.starts_with(&bytes[..bytes.len().min(4)]) {
            return Err(DecodeError::BadMagic);
        }
        return Err(DecodeError::Truncated { needed: HEADER_SIZE as u64, available: bytes.len() });
    }
    if bytes[..4] != MAGIC {
        return Err(DecodeError::BadMagic);
    }
    let kind = Kind::from_u8(bytes[4]).ok_or(DecodeError::UnknownKind(bytes[4]))?;
    Ok(Header { kind, flags: bytes[5], seq: u32_at(bytes, 8), time_ms: u32_at(bytes, 12) })
}

/// Kind of the message in `bytes`, to dispatch without decoding the body.
pub fn peek_kind(bytes: &[u8]) -> Result<Kind, DecodeError> {
    peek_header(bytes).map(|h| h.kind)
}

/// Validates a layer message and returns its header and the splat bytes.
fn split_layer(bytes: &[u8]) -> Result<(LayerHeader, &[u8]), DecodeError> {
    let header = peek_header(bytes)?;
    if header.kind != Kind::Layer {
        return Err(DecodeError::UnexpectedKind { expected: Kind::Layer, found: header.kind });
    }
    let fixed = HEADER_SIZE + LAYER_HEADER_SIZE;
    if bytes.len() < fixed {
        return Err(DecodeError::Truncated { needed: fixed as u64, available: bytes.len() });
    }
    let body = &bytes[HEADER_SIZE..];
    let mode = match body[1] {
        0 => LayerMode::Replace,
        1 => LayerMode::Append,
        other => return Err(DecodeError::BadLayerMode(other)),
    };
    let count = u32_at(body, 4);
    // The announced count must match the bytes actually received before
    // anything is allocated; u64 so that it cannot wrap on 32-bit targets.
    check_len(bytes, fixed as u64 + count as u64 * SPLAT_WIRE_SIZE as u64)?;
    let min = Vec3::new(f32_at(body, 8), f32_at(body, 12), f32_at(body, 16));
    let max = Vec3::new(f32_at(body, 20), f32_at(body, 24), f32_at(body, 28));
    if !(min.is_finite() && max.is_finite() && min.cmple(max).all()) {
        return Err(DecodeError::BadAabb);
    }
    let layer = LayerHeader {
        seq: header.seq,
        time_ms: header.time_ms,
        layer_id: body[0],
        mode,
        count,
        aabb: Aabb { min, max },
    };
    Ok((layer, &bytes[fixed..]))
}

/// Decodes a layer message, **appending** its splats to `out` (the caller
/// clears `out` first when the returned mode is [`LayerMode::Replace`] and it
/// holds the previous content of the layer).
///
/// On error `out` is left untouched.
pub fn decode_layer_into(bytes: &[u8], out: &mut Vec<Splat>) -> Result<LayerHeader, DecodeError> {
    let (layer, data) = split_layer(bytes)?;
    let (min, max) = (layer.aabb.min.to_array(), layer.aabb.max.to_array());
    let scales = scale_table();
    out.reserve(layer.count as usize);
    out.extend(data.chunks_exact(SPLAT_WIRE_SIZE).map(|wire| {
        let mut pos = [0.0f32; 3];
        let mut scale = [0.0f32; 3];
        for k in 0..3 {
            let t = u16_at(wire, 2 * k) as f32 / 65535.0;
            // Lerp in this form cannot overflow, whatever the box.
            pos[k] = min[k] * (1.0 - t) + max[k] * t;
            scale[k] = scales[wire[6 + k] as usize];
        }
        let rot = dequantize_rotation(wire[13] as u32 | (wire[14] as u32) << 8 | (wire[15] as u32) << 16);
        Splat { pos, scale, rot, rgba: [wire[9], wire[10], wire[11], wire[12]] }
    }));
    Ok(layer)
}

fn decode_text(bytes: &[u8]) -> Result<String, DecodeError> {
    std::str::from_utf8(&bytes[HEADER_SIZE..]).map(str::to_owned).map_err(|_| DecodeError::InvalidUtf8)
}

fn decode_preview(bytes: &[u8]) -> Result<PreviewMsg, DecodeError> {
    let fixed = HEADER_SIZE + PREVIEW_HEADER_SIZE;
    if bytes.len() < fixed {
        return Err(DecodeError::Truncated { needed: fixed as u64, available: bytes.len() });
    }
    let body = &bytes[HEADER_SIZE..];
    let (width, height) = (u16_at(body, 2), u16_at(body, 4));
    let pixels = width as u64 * height as u64;
    check_len(bytes, fixed as u64 + pixels * 5)?;
    // The length check above bounds `pixels` by the buffer size.
    let n = pixels as usize;
    let data = &bytes[fixed..];
    Ok(PreviewMsg {
        camera_id: body[0],
        width,
        height,
        rgb: data[..n * 3].to_vec(),
        depth_mm: data[n * 3..].chunks_exact(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect(),
    })
}

/// Decodes any message. Never panics and never allocates more than a small
/// multiple of `bytes.len()`, whatever the input.
pub fn decode(bytes: &[u8]) -> Result<Message, DecodeError> {
    let header = peek_header(bytes)?;
    let body = match header.kind {
        Kind::State => Body::State(decode_text(bytes)?),
        Kind::Stats => Body::Stats(decode_text(bytes)?),
        Kind::Layer => {
            let mut splats = Vec::new();
            let layer = decode_layer_into(bytes, &mut splats)?;
            Body::Layer(LayerMsg { layer_id: layer.layer_id, mode: layer.mode, splats })
        }
        Kind::Preview => Body::Preview(decode_preview(bytes)?),
    };
    Ok(Message { seq: header.seq, time_ms: header.time_ms, body })
}

#[cfg(test)]
mod tests {
    use glam::Quat;

    use super::*;
    use crate::splat::test_util::Rng;

    fn layer_bytes(splats: &[Splat]) -> Vec<u8> {
        let mut out = Vec::new();
        encode_layer(7, 1234, 1, LayerMode::Replace, splats, &mut out);
        out
    }

    fn roundtrip(splats: &[Splat]) -> Vec<Splat> {
        let mut out = Vec::new();
        let header = decode_layer_into(&layer_bytes(splats), &mut out).unwrap();
        assert_eq!(header.count as usize, splats.len());
        assert_eq!(out.len(), splats.len());
        out
    }

    /// Angle in degrees between two rotations.
    fn angle_deg(a: [f32; 4], b: [f32; 4]) -> f32 {
        // atan2 form: accurate for tiny angles, unlike acos of the dot product.
        let d = Quat::from_array(a).normalize().conjugate() * Quat::from_array(b).normalize();
        (2.0 * d.xyz().length().atan2(d.w.abs())).to_degrees()
    }

    #[test]
    fn constants_are_consistent() {
        assert_eq!(SPLAT_WIRE_SIZE, 16);
        assert!((SCALE_LOG_STEP - (SCALE_MAX / SCALE_MIN).ln() / 255.0).abs() < 1e-8);
        assert!(ROT_LEVELS.pow(3) <= 1 << 22);
        let table = scale_table();
        assert!((table[0] - SCALE_MIN).abs() < 1e-10);
        assert_eq!(table[255], SCALE_MAX);
        assert!(table.windows(2).all(|w| w[1] > w[0]));
    }

    #[test]
    fn header_layout_is_as_documented() {
        let bytes = encode(&Message { seq: 0x0403_0201, time_ms: 0x0807_0605, body: Body::State("{}".into()) });
        assert_eq!(&bytes[..4], b"GSL1");
        assert_eq!(bytes[4], 1);
        assert_eq!(&bytes[5..8], &[0, 0, 0]);
        assert_eq!(&bytes[8..12], &[1, 2, 3, 4]);
        assert_eq!(&bytes[12..16], &[5, 6, 7, 8]);
        assert_eq!(&bytes[16..], b"{}");
        assert_eq!(bytes.len(), HEADER_SIZE + 2);
    }

    #[test]
    fn layer_layout_is_as_documented() {
        let s = Splat {
            pos: [1.0, 2.0, 3.0],
            scale: [SCALE_MIN, 0.01, SCALE_MAX],
            rot: [0.0, 0.0, 0.0, 1.0],
            rgba: [10, 20, 30, 40],
        };
        let t = Splat { pos: [3.0, 2.0, -1.0], ..s };
        let mut bytes = Vec::new();
        encode_layer(9, 99, 5, LayerMode::Append, &[s, t], &mut bytes);
        assert_eq!(bytes.len(), 16 + 32 + 2 * 16);
        assert_eq!(bytes[4], 2);
        let body = &bytes[16..];
        assert_eq!(&body[..4], &[5, 1, 0, 0]);
        assert_eq!(u32_at(body, 4), 2);
        assert_eq!([f32_at(body, 8), f32_at(body, 12), f32_at(body, 16)], [1.0, 2.0, -1.0]);
        assert_eq!([f32_at(body, 20), f32_at(body, 24), f32_at(body, 28)], [3.0, 2.0, 3.0]);
        let w = &body[32..48];
        // x at min, y degenerate, z at max.
        assert_eq!([u16_at(w, 0), u16_at(w, 2), u16_at(w, 4)], [0, 0, 65535]);
        // 0.01 m is 2/5 of the log range: code 102.
        assert_eq!(&w[6..9], &[0, 102, 255]);
        assert_eq!(&w[9..13], &[10, 20, 30, 40]);
        // Identity: dropped component w (index 3), three mid-range levels (80).
        let word = w[13] as u32 | (w[14] as u32) << 8 | (w[15] as u32) << 16;
        assert_eq!(word, (3 << 22) | (80 + 161 * 80 + 161 * 161 * 80));
        let w2 = &body[48..64];
        assert_eq!([u16_at(w2, 0), u16_at(w2, 2), u16_at(w2, 4)], [65535, 0, 0]);
    }

    #[test]
    fn roundtrip_error_bounds() {
        let mut rng = Rng::new(1);
        let splats: Vec<Splat> = (0..20_000)
            .map(|_| {
                let mut s = rng.splat(1.0, 0.0, 1.0);
                s.pos = [rng.range(-3.0, 7.0), rng.range(0.0, 2.5), rng.range(-40.0, 40.0)];
                // Log-uniform scales over the whole representable range.
                s.scale = [0; 3].map(|_| SCALE_MIN * (rng.unit() * (SCALE_MAX / SCALE_MIN).ln()).exp());
                s
            })
            .collect();
        let bytes = layer_bytes(&splats);
        assert_eq!(bytes.len(), HEADER_SIZE + LAYER_HEADER_SIZE + splats.len() * SPLAT_WIRE_SIZE);
        let mut decoded = Vec::new();
        let header = decode_layer_into(&bytes, &mut decoded).unwrap();
        assert_eq!((header.seq, header.time_ms, header.layer_id, header.mode), (7, 1234, 1, LayerMode::Replace));
        let aabb = Aabb::from_points(splats.iter().map(Splat::position));
        assert_eq!(header.aabb, aabb);
        let step = aabb.size() / 65535.0;

        let (mut max_scale_err, mut max_angle) = (0.0f32, 0.0f32);
        for (a, b) in splats.iter().zip(&decoded) {
            assert_eq!(a.rgba, b.rgba);
            for k in 0..3 {
                let tol = 0.51 * step[k] + 2e-5;
                assert!((a.pos[k] - b.pos[k]).abs() <= tol, "pos {:?} -> {:?}", a.pos, b.pos);
                max_scale_err = max_scale_err.max((b.scale[k] / a.scale[k] - 1.0).abs());
            }
            assert!((Quat::from_array(b.rot).length() - 1.0).abs() < 1e-5);
            max_angle = max_angle.max(angle_deg(a.rot, b.rot));
        }
        assert!(max_scale_err < 0.024, "scale error {max_scale_err}");
        assert!(max_angle < 1.8, "rotation error {max_angle}°");
        // The bounds are meaningful, not vacuous.
        assert!(max_scale_err > 0.015 && max_angle > 0.5);
    }

    #[test]
    fn rotation_worst_cases_stay_under_two_degrees() {
        // Near-ties of the largest component are the hardest inputs.
        let mut rng = Rng::new(2);
        let mut worst = 0.0f32;
        let mut check = |q: [f32; 4]| {
            let d = dequantize_rotation(quantize_rotation(q));
            assert!((Quat::from_array(d).length() - 1.0).abs() < 1e-5);
            worst = worst.max(angle_deg(q, d));
        };
        for signs in 0..16 {
            let s = |bit: u32| if signs >> bit & 1 == 1 { -0.5 } else { 0.5 };
            check([s(0), s(1), s(2), s(3)]);
        }
        for _ in 0..200_000 {
            let jitter = [0; 4].map(|_| 0.5 + rng.range(-0.02, 0.02));
            let q = Quat::from_array(jitter).normalize().to_array();
            check([q[0], -q[1], q[2], -q[3]]);
            check(rng.quat().to_array());
        }
        for q in [[1.0, 0.0, 0.0, 0.0], [0.0, -1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0], [0.0, 0.0, 0.0, -1.0]] {
            assert!(angle_deg(q, dequantize_rotation(quantize_rotation(q))) < 1e-2);
        }
        assert!(worst < 1.8, "worst rotation error {worst}°");
    }

    #[test]
    fn rotation_handles_unnormalized_and_degenerate_input() {
        let q = Quat::from_rotation_y(0.9).to_array();
        let scaled = q.map(|c| c * 5.0);
        assert_eq!(quantize_rotation(q), quantize_rotation(scaled));
        let identity = quantize_rotation([0.0, 0.0, 0.0, 1.0]);
        for bad in [[0.0; 4], [f32::NAN; 4], [f32::INFINITY, 0.0, 0.0, 0.0]] {
            assert_eq!(quantize_rotation(bad), identity);
        }
        // Every 24-bit word decodes to a unit quaternion.
        for word in (0..1u32 << 24).step_by(997).chain([0xFF_FFFF, 0x3F_FFFF]) {
            let q = Quat::from_array(dequantize_rotation(word));
            assert!((q.length() - 1.0).abs() < 1e-4, "{word:#x} -> {q:?}");
        }
    }

    #[test]
    fn scale_quantization_clamps_and_is_monotonic() {
        let q = ScaleQuantizer::new();
        assert_eq!(q.quantize(0.0), 0);
        assert_eq!(q.quantize(-0.0), 0);
        assert_eq!(q.quantize(1e-9), 0);
        assert_eq!(q.quantize(f32::MIN_POSITIVE / 4.0), 0);
        assert_eq!(q.quantize(f32::NAN), 0);
        assert_eq!(q.quantize(SCALE_MIN), 0);
        assert_eq!(q.quantize(SCALE_MAX), 255);
        assert_eq!(q.quantize(1e9), 255);
        assert_eq!(q.quantize(f32::MAX), 255);
        assert_eq!(q.quantize(f32::INFINITY), 255);
        assert_eq!(q.quantize(-0.01), q.quantize(0.01));
        // Decoded values are fixed points.
        for (code, &value) in scale_table().iter().enumerate() {
            assert_eq!(q.quantize(value) as usize, code);
        }
        let mut prev = 0;
        for i in 0..20_000 {
            let code = q.quantize(SCALE_MIN * 0.9 * 1.000_6f32.powi(i));
            assert!(code >= prev);
            prev = code;
        }
        assert_eq!(prev, 255);
    }

    #[test]
    fn fast_scale_quantizer_matches_the_logarithm() {
        let q = ScaleQuantizer::new();
        let reference = |s: f32| ((s.max(SCALE_MIN) / SCALE_MIN).ln() / SCALE_LOG_STEP).clamp(0.0, 255.0);
        let mut rng = Rng::new(10);
        for _ in 0..200_000 {
            // Log-uniform over a range wider than the representable one.
            let s = 1.0e-6 * (rng.unit() * (1.0e9f32).ln()).exp();
            let exact = reference(s);
            let code = q.quantize(s) as f32;
            // Nearest code, except within float noise of a rounding boundary.
            assert!((code - exact).abs() <= 0.5 + 2e-3, "scale {s}: code {code}, exact {exact}");
        }
    }

    #[test]
    fn empty_layer_roundtrips() {
        let bytes = layer_bytes(&[]);
        assert_eq!(bytes.len(), HEADER_SIZE + LAYER_HEADER_SIZE);
        let mut out = vec![Splat { pos: [1.0; 3], scale: [1.0; 3], rot: [0.0, 0.0, 0.0, 1.0], rgba: [1; 4] }];
        let header = decode_layer_into(&bytes, &mut out).unwrap();
        assert_eq!(header.count, 0);
        assert_eq!(header.aabb, Aabb::new(Vec3::ZERO, Vec3::ZERO));
        assert_eq!(out.len(), 1, "decode appends");
        let msg = decode(&bytes).unwrap();
        assert_eq!(msg.body, Body::Layer(LayerMsg { layer_id: 1, mode: LayerMode::Replace, splats: vec![] }));
    }

    #[test]
    fn single_point_layer_is_exact_in_position() {
        let s = Splat {
            pos: [1.25, -3.5, 100.125],
            scale: [0.02, 0.02, 0.002],
            rot: Quat::from_rotation_x(0.3).to_array(),
            rgba: [1, 2, 3, 4],
        };
        let out = roundtrip(&[s]);
        assert_eq!(out[0].pos, s.pos);
        assert_eq!(out[0].rgba, s.rgba);
        // Several splats sharing one coordinate (a plane): that axis is exact.
        let plane: Vec<Splat> = (0..10).map(|i| Splat { pos: [i as f32, 0.7, -(i as f32)], ..s }).collect();
        for (a, b) in plane.iter().zip(roundtrip(&plane)) {
            assert_eq!(a.pos[1], b.pos[1]);
            assert!((a.pos[0] - b.pos[0]).abs() < 1e-4);
        }
    }

    #[test]
    fn decode_appends_to_existing_splats() {
        let mut rng = Rng::new(3);
        let a: Vec<Splat> = (0..10).map(|_| rng.splat(1.0, 0.01, 0.1)).collect();
        let b: Vec<Splat> = (0..5).map(|_| rng.splat(1.0, 0.01, 0.1)).collect();
        let mut out = Vec::new();
        decode_layer_into(&layer_bytes(&a), &mut out).unwrap();
        let mut bytes = Vec::new();
        encode_layer(1, 2, 0, LayerMode::Append, &b, &mut bytes);
        let header = decode_layer_into(&bytes, &mut out).unwrap();
        assert_eq!((header.mode, header.layer_id, header.count), (LayerMode::Append, 0, 5));
        assert_eq!(out.len(), 15);
        assert_eq!(out[10..], roundtrip(&b)[..]);
    }

    #[test]
    fn encode_layer_reuses_and_clears_the_buffer() {
        let mut rng = Rng::new(4);
        let splats: Vec<Splat> = (0..100).map(|_| rng.splat(1.0, 0.01, 0.1)).collect();
        let mut buf = vec![0xAB; 5];
        encode_layer(1, 2, 0, LayerMode::Replace, &splats, &mut buf);
        let first = buf.clone();
        let capacity = buf.capacity();
        encode_layer(1, 2, 0, LayerMode::Replace, &splats, &mut buf);
        assert_eq!(buf, first);
        assert_eq!(buf.capacity(), capacity);
        assert_eq!(
            encode(&Message {
                seq: 1,
                time_ms: 2,
                body: Body::Layer(LayerMsg { layer_id: 0, mode: LayerMode::Replace, splats })
            }),
            first
        );
    }

    /// The multi-threaded path must produce the same bytes as the serial one.
    #[cfg(feature = "par")]
    #[test]
    fn parallel_encoding_is_byte_identical_to_serial() {
        let mut rng = Rng::new(40);
        let splats: Vec<Splat> = (0..PAR_MIN_SPLATS + 12_345).map(|_| rng.splat(3.0, 0.001, 0.3)).collect();
        let parallel = layer_bytes(&splats);
        let (min, max) = center_bounds(&splats);
        let quantizer = LayerQuantizer::new(min, max);
        let serial: Vec<u8> = splats.iter().flat_map(|s| quantizer.encode(s)).collect();
        assert_eq!(parallel.len(), HEADER_SIZE + LAYER_HEADER_SIZE + serial.len());
        assert!(parallel[HEADER_SIZE + LAYER_HEADER_SIZE..] == serial[..], "parallel and serial encodings differ");
        // And both agree with a small (serial-path) message over the same box.
        let head = layer_bytes(&splats[..100]);
        let mut decoded = Vec::new();
        decode_layer_into(&head, &mut decoded).unwrap();
        assert_eq!(decoded.len(), 100);
    }

    #[test]
    fn re_encoding_decoded_splats_is_stable() {
        // Quantized positions, scales and colors are fixed points of the
        // codec (the AABB is unchanged: its corners are representable
        // exactly). A rotation can move once more when its two largest
        // components are nearly tied and swap roles.
        let mut rng = Rng::new(5);
        let splats: Vec<Splat> = (0..2000).map(|_| rng.splat(2.0, 0.001, 0.5)).collect();
        let once = roundtrip(&splats);
        let twice = roundtrip(&once);
        for (a, b) in once.iter().zip(&twice) {
            assert_eq!(a.rgba, b.rgba);
            assert_eq!(a.scale, b.scale);
            for k in 0..3 {
                assert!((a.pos[k] - b.pos[k]).abs() < 1e-5);
            }
            assert!(angle_deg(a.rot, b.rot) < 1.8);
        }
    }

    #[test]
    fn non_finite_input_is_encoded_without_panicking() {
        let good = Splat { pos: [1.0, 2.0, 3.0], scale: [0.1; 3], rot: [0.0, 0.0, 0.0, 1.0], rgba: [9; 4] };
        let splats = [
            good,
            Splat { pos: [f32::NAN, f32::INFINITY, f32::NEG_INFINITY], ..good },
            Splat { pos: [-1.0, 0.0, 5.0], scale: [f32::NAN, f32::INFINITY, -1.0], rot: [f32::NAN; 4], ..good },
            Splat { pos: [f32::MAX, -f32::MAX, 0.0], ..good },
        ];
        let out = roundtrip(&splats);
        for s in &out {
            assert!(s.pos.iter().chain(&s.scale).chain(&s.rot).all(|v| v.is_finite()), "{s:?}");
        }
        // Only non-finite positions: the box collapses to the origin.
        let out = roundtrip(&[Splat { pos: [f32::NAN; 3], ..good }]);
        assert_eq!(out[0].pos, [0.0; 3]);
    }

    #[test]
    fn text_messages_roundtrip() {
        for (body, kind) in [
            (Body::State(r#"{"scene":"stage","é":"ü"}"#.to_string()), Kind::State),
            (Body::Stats(String::new()), Kind::Stats),
        ] {
            let msg = Message { seq: 42, time_ms: 1000, body };
            let bytes = encode(&msg);
            assert_eq!(peek_kind(&bytes), Ok(kind));
            assert_eq!(peek_header(&bytes), Ok(Header { kind, flags: 0, seq: 42, time_ms: 1000 }));
            assert_eq!(decode(&bytes).unwrap(), msg);
        }
        let mut bytes = encode(&Message { seq: 0, time_ms: 0, body: Body::State("ok".into()) });
        bytes.extend_from_slice(&[0xFF, 0xFE]);
        assert_eq!(decode(&bytes), Err(DecodeError::InvalidUtf8));
    }

    fn test_frame(width: u32, height: u32) -> RgbdFrame {
        let mut frame = RgbdFrame::new(width, height);
        for (i, v) in frame.rgb.iter_mut().enumerate() {
            *v = (i * 7 % 256) as u8;
        }
        for (i, d) in frame.depth.iter_mut().enumerate() {
            *d = i as f32 * 0.137;
        }
        frame
    }

    #[test]
    fn preview_roundtrips_and_matches_the_direct_encoder() {
        let mut frame = test_frame(7, 5);
        frame.depth[1] = 0.0004; // valid but below 1 mm
        frame.depth[2] = 1.2346;
        frame.depth[3] = 500.0; // saturates
        frame.depth[4] = -1.0;
        frame.depth[5] = f32::NAN;
        frame.depth[6] = f32::INFINITY;
        let preview = PreviewMsg::from_frame(3, &frame);
        assert_eq!((preview.camera_id, preview.width, preview.height), (3, 7, 5));
        assert_eq!(preview.rgb, frame.rgb);
        assert_eq!(&preview.depth_mm[..7], &[0, 1, 1235, 65535, 0, 0, 65535]);

        let msg = Message { seq: 5, time_ms: 6, body: Body::Preview(preview.clone()) };
        let bytes = encode(&msg);
        assert_eq!(bytes.len(), HEADER_SIZE + PREVIEW_HEADER_SIZE + 7 * 5 * 5);
        assert_eq!(peek_kind(&bytes), Ok(Kind::Preview));
        assert_eq!(&bytes[16..24], &[3, 0, 7, 0, 5, 0, 0, 0]);
        assert_eq!(decode(&bytes).unwrap(), msg);

        let mut direct = vec![1, 2, 3];
        encode_preview(5, 6, 3, &frame, &mut direct);
        assert_eq!(direct, bytes);

        let back = preview.to_frame();
        assert_eq!((back.width, back.height), (7, 5));
        assert_eq!(back.rgb, frame.rgb);
        assert!((back.depth[2] - 1.235).abs() < 1e-6);
        assert_eq!(back.depth[4], 0.0);
        for (a, b) in frame.depth.iter().zip(&back.depth).skip(7) {
            assert!((a - b).abs() <= 0.0005 + 1e-6);
        }
    }

    #[test]
    fn empty_and_inconsistent_previews_are_well_formed() {
        let empty = PreviewMsg::from_frame(0, &RgbdFrame::default());
        assert_eq!((empty.width, empty.height), (0, 0));
        let msg = Message { seq: 0, time_ms: 0, body: Body::Preview(empty) };
        assert_eq!(decode(&encode(&msg)).unwrap(), msg);

        // Buffers shorter / longer than declared are padded / cut.
        let odd = PreviewMsg { camera_id: 1, width: 4, height: 2, rgb: vec![9; 5], depth_mm: vec![7; 20] };
        let bytes = encode(&Message { seq: 0, time_ms: 0, body: Body::Preview(odd) });
        let Body::Preview(decoded) = decode(&bytes).unwrap().body else { panic!() };
        assert_eq!(decoded.rgb.len(), 24);
        assert_eq!(&decoded.rgb[..6], &[9, 9, 9, 9, 9, 0]);
        assert_eq!(decoded.depth_mm, vec![7; 8]);

        // A frame whose buffers are shorter than width × height is cropped.
        let mut short = test_frame(4, 4);
        short.depth.truncate(9);
        let p = PreviewMsg::from_frame(0, &short);
        assert_eq!((p.width, p.height), (4, 2));
        assert_eq!(p.rgb.len(), 24);
        let mut out = Vec::new();
        encode_preview(0, 0, 0, &short, &mut out);
        assert!(decode(&out).is_ok());
    }

    #[test]
    fn malformed_messages_are_rejected() {
        let mut rng = Rng::new(6);
        let splats: Vec<Splat> = (0..3).map(|_| rng.splat(1.0, 0.01, 0.1)).collect();
        let good = layer_bytes(&splats);
        let mut out = Vec::new();

        assert_eq!(decode(&[]), Err(DecodeError::Truncated { needed: 16, available: 0 }));
        assert_eq!(decode(b"GS"), Err(DecodeError::Truncated { needed: 16, available: 2 }));
        assert_eq!(decode(b"PNG"), Err(DecodeError::BadMagic));
        assert_eq!(decode(b"GSL2............"), Err(DecodeError::BadMagic));

        let mut bad = good.clone();
        bad[4] = 0;
        assert_eq!(decode(&bad), Err(DecodeError::UnknownKind(0)));
        bad[4] = 200;
        assert_eq!(peek_kind(&bad), Err(DecodeError::UnknownKind(200)));

        let mut bad = good.clone();
        bad[HEADER_SIZE + 1] = 2;
        assert_eq!(decode(&bad), Err(DecodeError::BadLayerMode(2)));

        assert_eq!(
            decode(&good[..good.len() - 1]),
            Err(DecodeError::Truncated { needed: good.len() as u64, available: good.len() - 1 })
        );
        assert!(matches!(decode(&good[..40]), Err(DecodeError::Truncated { needed: 48, available: 40 })));
        let mut long = good.clone();
        long.push(0);
        assert_eq!(
            decode(&long),
            Err(DecodeError::TrailingBytes { expected: good.len() as u64, actual: good.len() + 1 })
        );

        for (offset, value) in [(8, f32::NAN), (12, f32::INFINITY), (28, f32::NEG_INFINITY), (20, -1.0e9)] {
            let mut bad = good.clone();
            bad[HEADER_SIZE + offset..HEADER_SIZE + offset + 4].copy_from_slice(&value.to_le_bytes());
            assert_eq!(decode_layer_into(&bad, &mut out), Err(DecodeError::BadAabb), "offset {offset}");
        }

        let state = encode(&Message { seq: 0, time_ms: 0, body: Body::State("{}".into()) });
        assert_eq!(
            decode_layer_into(&state, &mut out),
            Err(DecodeError::UnexpectedKind { expected: Kind::Layer, found: Kind::State })
        );
        assert!(out.is_empty(), "failed decodes leave the output untouched");

        // Errors are printable.
        for e in [
            DecodeError::Truncated { needed: 48, available: 3 },
            DecodeError::TrailingBytes { expected: 48, actual: 50 },
            DecodeError::BadMagic,
            DecodeError::UnknownKind(9),
            DecodeError::UnexpectedKind { expected: Kind::Layer, found: Kind::Stats },
            DecodeError::BadLayerMode(7),
            DecodeError::BadAabb,
            DecodeError::InvalidUtf8,
        ] {
            let text = (Box::new(e) as Box<dyn std::error::Error>).to_string();
            assert!(!text.is_empty());
        }
    }

    #[test]
    fn huge_counts_are_rejected_before_allocating() {
        // A 48-byte message announcing 4 billion splats (64 GiB).
        let mut bytes = layer_bytes(&[]);
        bytes[HEADER_SIZE + 4..HEADER_SIZE + 8].copy_from_slice(&u32::MAX.to_le_bytes());
        let mut out = Vec::new();
        let needed = 48 + u32::MAX as u64 * 16;
        assert_eq!(decode_layer_into(&bytes, &mut out), Err(DecodeError::Truncated { needed, available: 48 }));
        assert_eq!(out.capacity(), 0);
        assert!(decode(&bytes).is_err());

        // Same for a preview announcing 65535 × 65535 pixels.
        let mut bytes = encode(&Message { seq: 0, time_ms: 0, body: Body::Preview(PreviewMsg::default()) });
        bytes[HEADER_SIZE + 2..HEADER_SIZE + 6].copy_from_slice(&[0xFF; 4]);
        let needed = 24 + 65535u64 * 65535 * 5;
        assert_eq!(decode(&bytes), Err(DecodeError::Truncated { needed, available: 24 }));
    }

    #[test]
    fn fuzz_never_panics() {
        let mut rng = Rng::new(7);
        let splats: Vec<Splat> = (0..40).map(|_| rng.splat(1.0, 0.01, 0.1)).collect();
        let valid = [
            layer_bytes(&splats),
            layer_bytes(&[]),
            encode(&Message { seq: 1, time_ms: 2, body: Body::State(r#"{"a":1}"#.into()) }),
            encode(&Message { seq: 1, time_ms: 2, body: Body::Stats("{}".into()) }),
            encode(&Message { seq: 1, time_ms: 2, body: Body::Preview(PreviewMsg::from_frame(1, &test_frame(6, 4))) }),
        ];
        let exercise = |bytes: &[u8]| {
            let _ = peek_kind(bytes);
            let _ = peek_header(bytes);
            if let Ok(msg) = decode(bytes) {
                // Whatever decodes must re-encode to something decodable.
                assert!(decode(&encode(&msg)).is_ok());
            }
            let mut sink = Vec::new();
            if let Ok(header) = decode_layer_into(bytes, &mut sink) {
                assert_eq!(sink.len(), header.count as usize);
                assert!(sink.iter().all(|s| s.pos.iter().chain(&s.scale).chain(&s.rot).all(|v| v.is_finite())));
            }
            assert!(sink.capacity() * SPLAT_WIRE_SIZE <= bytes.len(), "over-allocation");
        };

        for message in &valid {
            // Every truncation.
            for len in 0..=message.len() {
                exercise(&message[..len]);
            }
            // Random corruptions: byte flips, overwritten ranges, extensions.
            for _ in 0..3000 {
                let mut bytes = message.clone();
                match rng.below(4) {
                    0 => {
                        let i = rng.below(bytes.len());
                        bytes[i] ^= 1 << rng.below(8);
                    }
                    1 => {
                        for _ in 0..1 + rng.below(8) {
                            let i = rng.below(bytes.len().min(64));
                            bytes[i] = rng.next_u64() as u8;
                        }
                    }
                    2 => {
                        let keep = rng.below(bytes.len() + 1);
                        bytes.truncate(keep);
                        for _ in 0..rng.below(40) {
                            bytes.push(rng.next_u64() as u8);
                        }
                    }
                    _ => {
                        let i = rng.below(bytes.len());
                        bytes[i..].iter_mut().for_each(|b| *b = rng.next_u64() as u8);
                    }
                }
                exercise(&bytes);
            }
        }
        // Pure noise, with and without a valid magic and kind.
        for _ in 0..20_000 {
            let mut bytes: Vec<u8> = (0..rng.below(200)).map(|_| rng.next_u64() as u8).collect();
            if rng.below(4) != 0 && bytes.len() >= 5 {
                bytes[..4].copy_from_slice(&MAGIC);
                bytes[4] = 1 + rng.below(4) as u8;
            }
            exercise(&bytes);
        }
    }

    /// Timing report. Run with
    /// `cargo test -p gs-core --release codec::tests::timing -- --ignored --nocapture`.
    #[test]
    #[ignore = "timing report, run in --release"]
    fn timing() {
        let mut rng = Rng::new(8);
        let splats: Vec<Splat> = (0..1_000_000).map(|_| rng.splat(4.0, 0.001, 0.05)).collect();
        let mut bytes = Vec::new();
        let mut decoded = Vec::new();
        encode_layer(0, 0, 0, LayerMode::Replace, &splats, &mut bytes);
        decode_layer_into(&bytes, &mut decoded).unwrap();
        let (mut encode_ms, mut decode_ms) = (f64::MAX, f64::MAX);
        for _ in 0..15 {
            let start = web_time::Instant::now();
            encode_layer(0, 0, 0, LayerMode::Replace, &splats, &mut bytes);
            encode_ms = encode_ms.min(start.elapsed().as_secs_f64() * 1e3);
            std::hint::black_box(&bytes);
            decoded.clear();
            let start = web_time::Instant::now();
            decode_layer_into(&bytes, &mut decoded).unwrap();
            decode_ms = decode_ms.min(start.elapsed().as_secs_f64() * 1e3);
            std::hint::black_box(&decoded);
        }
        println!(
            "codec 1M splats: encode {encode_ms:.1} ms, decode {decode_ms:.1} ms (best of 15), {} bytes",
            bytes.len()
        );
    }
}
