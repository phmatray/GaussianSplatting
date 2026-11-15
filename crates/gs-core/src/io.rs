//! Import/export of Gaussian splat files, as in-memory byte buffers (this
//! crate does no file I/O; the caller reads and writes the bytes).
//!
//! Two formats are supported:
//!
//! * **`.splat`** (antimatter15 web viewer): 32 bytes per splat, no header —
//!   position `3 × f32`, scale `3 × f32` (standard deviations, linear),
//!   color `4 × u8` RGBA, rotation `4 × u8` as `q·128 + 128` in
//!   `w, x, y, z` order.
//! * **`.ply`** (INRIA 3D Gaussian Splatting): `binary_little_endian`, one
//!   `vertex` element with float properties `x y z`, `nx ny nz` (unused),
//!   `f_dc_0..2` (degree-0 spherical harmonics: `color = 0.5 + 0.2820948·f_dc`),
//!   optional `f_rest_*` (higher SH bands, ignored on import), `opacity`
//!   (logit), `scale_0..2` (natural log), `rot_0..3` (`w, x, y, z`).
//!
//! Readers never panic and never allocate more than the input justifies, and
//! they drop splats whose position or scale is not finite.

use std::fmt;

use crate::types::Splat;

/// Size of one record of the `.splat` format in bytes.
pub const SPLAT_FILE_RECORD_SIZE: usize = 32;

/// Zeroth-order spherical-harmonics basis constant `1 / (2·√π)`.
const SH_C0: f32 = 0.282_094_79;

/// Why a file could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IoError {
    /// The file ends before the data its header announces.
    Truncated { needed: u64, available: usize },
    /// A `.splat` file whose length is not a multiple of 32 bytes.
    BadLength(usize),
    /// The PLY header is malformed.
    BadHeader(&'static str),
    /// A valid PLY file that this reader does not handle.
    Unsupported(&'static str),
}

impl fmt::Display for IoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated { needed, available } => {
                write!(f, "file truncated: {needed} bytes needed, {available} available")
            }
            Self::BadLength(len) => write!(f, ".splat file length {len} is not a multiple of 32 bytes"),
            Self::BadHeader(what) => write!(f, "malformed PLY header: {what}"),
            Self::Unsupported(what) => write!(f, "unsupported PLY file: {what}"),
        }
    }
}

impl std::error::Error for IoError {}

#[inline]
fn f32_at(bytes: &[u8], offset: usize) -> f32 {
    f32::from_le_bytes([bytes[offset], bytes[offset + 1], bytes[offset + 2], bytes[offset + 3]])
}

/// Normalizes an imported splat; `None` when it cannot be displayed.
fn sanitize(pos: [f32; 3], scale: [f32; 3], rot: [f32; 4], rgba: [u8; 4]) -> Option<Splat> {
    if !pos.iter().chain(&scale).all(|v| v.is_finite()) {
        return None;
    }
    Some(Splat { pos, scale: scale.map(f32::abs), rot: unit_rotation(rot), rgba })
}

/// Normalized quaternion; the identity when `rot` is zero or not finite.
fn unit_rotation(rot: [f32; 4]) -> [f32; 4] {
    let len2: f32 = rot.iter().map(|c| c * c).sum();
    if len2.is_finite() && len2 > 1e-24 {
        let inv = len2.sqrt().recip();
        rot.map(|c| c * inv)
    } else {
        [0.0, 0.0, 0.0, 1.0]
    }
}

// ---------------------------------------------------------------------------
// .splat
// ---------------------------------------------------------------------------

/// Serializes splats in the antimatter15 `.splat` format.
pub fn write_splat(splats: &[Splat]) -> Vec<u8> {
    let mut out = Vec::with_capacity(splats.len() * SPLAT_FILE_RECORD_SIZE);
    for s in splats {
        for v in s.pos.iter().chain(&s.scale) {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&s.rgba);
        let [x, y, z, w] = unit_rotation(s.rot);
        // Saturating cast: +1.0 maps to 255.
        out.extend([w, x, y, z].map(|c| (c * 128.0 + 128.0 + 0.5) as u8));
    }
    out
}

/// Parses an antimatter15 `.splat` file.
pub fn read_splat(bytes: &[u8]) -> Result<Vec<Splat>, IoError> {
    let records = bytes.chunks_exact(SPLAT_FILE_RECORD_SIZE);
    if !records.remainder().is_empty() {
        return Err(IoError::BadLength(bytes.len()));
    }
    let mut splats = Vec::with_capacity(records.len());
    for rec in records {
        let pos = [f32_at(rec, 0), f32_at(rec, 4), f32_at(rec, 8)];
        let scale = [f32_at(rec, 12), f32_at(rec, 16), f32_at(rec, 20)];
        let rgba = [rec[24], rec[25], rec[26], rec[27]];
        let q = |i: usize| (rec[28 + i] as f32 - 128.0) / 128.0;
        splats.extend(sanitize(pos, scale, [q(1), q(2), q(3), q(0)], rgba));
    }
    Ok(splats)
}

// ---------------------------------------------------------------------------
// .ply
// ---------------------------------------------------------------------------

/// Property names written by [`write_ply`], in file order.
const PLY_PROPERTIES: [&str; 17] = [
    "x", "y", "z", "nx", "ny", "nz", "f_dc_0", "f_dc_1", "f_dc_2", "opacity", "scale_0", "scale_1", "scale_2", "rot_0",
    "rot_1", "rot_2", "rot_3",
];

/// Serializes splats as an INRIA 3DGS `.ply` (binary little-endian, SH
/// degree 0), readable by the usual 3DGS viewers and editors.
pub fn write_ply(splats: &[Splat]) -> Vec<u8> {
    let mut header = format!("ply\nformat binary_little_endian 1.0\nelement vertex {}\n", splats.len());
    for name in PLY_PROPERTIES {
        header.push_str("property float ");
        header.push_str(name);
        header.push('\n');
    }
    header.push_str("end_header\n");

    let mut out = Vec::with_capacity(header.len() + splats.len() * PLY_PROPERTIES.len() * 4);
    out.extend_from_slice(header.as_bytes());
    for s in splats {
        let color = |c: u8| (c as f32 / 255.0 - 0.5) / SH_C0;
        // Keep the logit finite for fully transparent / opaque splats; the
        // margin is far below half an 8-bit step, so the round trip is exact.
        let alpha = (s.rgba[3] as f32 / 255.0).clamp(1.0e-4, 1.0 - 1.0e-4);
        let log_scale = |v: f32| v.abs().max(1.0e-9).ln();
        let [x, y, z, w] = unit_rotation(s.rot);
        let record: [f32; 17] = [
            s.pos[0],
            s.pos[1],
            s.pos[2],
            0.0,
            0.0,
            0.0,
            color(s.rgba[0]),
            color(s.rgba[1]),
            color(s.rgba[2]),
            (alpha / (1.0 - alpha)).ln(),
            log_scale(s.scale[0]),
            log_scale(s.scale[1]),
            log_scale(s.scale[2]),
            w,
            x,
            y,
            z,
        ];
        for v in record {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
    out
}

/// Scalar property types of the PLY format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PlyType {
    I8,
    U8,
    I16,
    U16,
    I32,
    U32,
    F32,
    F64,
}

impl PlyType {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "char" | "int8" => Self::I8,
            "uchar" | "uint8" => Self::U8,
            "short" | "int16" => Self::I16,
            "ushort" | "uint16" => Self::U16,
            "int" | "int32" => Self::I32,
            "uint" | "uint32" => Self::U32,
            "float" | "float32" => Self::F32,
            "double" | "float64" => Self::F64,
            _ => return None,
        })
    }

    fn size(self) -> usize {
        match self {
            Self::I8 | Self::U8 => 1,
            Self::I16 | Self::U16 => 2,
            Self::I32 | Self::U32 | Self::F32 => 4,
            Self::F64 => 8,
        }
    }

    /// Reads a value of this type at `offset`. The caller guarantees
    /// `offset + self.size() <= rec.len()`.
    fn read(self, rec: &[u8], offset: usize) -> f32 {
        let b = &rec[offset..offset + self.size()];
        match self {
            Self::I8 => b[0] as i8 as f32,
            Self::U8 => b[0] as f32,
            Self::I16 => i16::from_le_bytes([b[0], b[1]]) as f32,
            Self::U16 => u16::from_le_bytes([b[0], b[1]]) as f32,
            Self::I32 => i32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32,
            Self::U32 => u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f32,
            Self::F32 => f32::from_le_bytes([b[0], b[1], b[2], b[3]]),
            Self::F64 => f64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]) as f32,
        }
    }
}

/// Where to find one property inside a vertex record.
#[derive(Clone, Copy, Debug)]
struct Field {
    offset: usize,
    ty: PlyType,
}

/// One `element` declaration of the header.
#[derive(Debug, Default)]
struct Element<'a> {
    name: &'a str,
    count: u64,
    /// Scalar properties `(name, field)`.
    fields: Vec<(&'a str, Field)>,
    /// Bytes per record (meaningless when `has_list`).
    stride: usize,
    /// List properties make the record size data-dependent.
    has_list: bool,
}

impl Element<'_> {
    fn field(&self, name: &str) -> Option<Field> {
        self.fields.iter().find(|(n, _)| *n == name).map(|&(_, f)| f)
    }
}

/// Parsed PLY header.
struct PlyHeader<'a> {
    elements: Vec<Element<'a>>,
    /// Offset of the first data byte.
    data_start: usize,
}

fn parse_ply_header(bytes: &[u8]) -> Result<PlyHeader<'_>, IoError> {
    let mut elements: Vec<Element> = Vec::new();
    let mut format_seen = false;
    let mut offset = 0;
    let mut first = true;
    loop {
        let rest = &bytes[offset..];
        let Some(newline) = rest.iter().position(|&b| b == b'\n') else {
            return Err(IoError::BadHeader("missing end_header"));
        };
        offset += newline + 1;
        let line = std::str::from_utf8(&rest[..newline]).map_err(|_| IoError::BadHeader("header is not text"))?;
        // `trim` also removes the '\r' of files written on Windows.
        let mut words = line.split_whitespace();
        let keyword = words.next().unwrap_or("");
        if first {
            if keyword != "ply" {
                return Err(IoError::BadHeader("missing ply magic"));
            }
            first = false;
            continue;
        }
        match keyword {
            "end_header" => break,
            "format" => {
                match words.next() {
                    Some("binary_little_endian") => {}
                    Some("ascii") => return Err(IoError::Unsupported("ascii PLY")),
                    Some("binary_big_endian") => return Err(IoError::Unsupported("big-endian PLY")),
                    _ => return Err(IoError::BadHeader("unknown format")),
                }
                format_seen = true;
            }
            "element" => {
                let name = words.next().ok_or(IoError::BadHeader("element without a name"))?;
                let count = words
                    .next()
                    .and_then(|w| w.parse::<u64>().ok())
                    .ok_or(IoError::BadHeader("element without a valid count"))?;
                elements.push(Element { name, count, ..Default::default() });
            }
            "property" => {
                let element = elements.last_mut().ok_or(IoError::BadHeader("property before any element"))?;
                let ty = words.next().ok_or(IoError::BadHeader("property without a type"))?;
                if ty == "list" {
                    element.has_list = true;
                } else {
                    let ty = PlyType::parse(ty).ok_or(IoError::BadHeader("unknown property type"))?;
                    let name = words.next().ok_or(IoError::BadHeader("property without a name"))?;
                    element.fields.push((name, Field { offset: element.stride, ty }));
                    element.stride += ty.size();
                }
            }
            // Comments, obj_info, blank lines and unknown keywords.
            _ => {}
        }
    }
    if !format_seen {
        return Err(IoError::BadHeader("missing format line"));
    }
    Ok(PlyHeader { elements, data_start: offset })
}

#[inline]
fn sigmoid(x: f32) -> f32 {
    1.0 / (1.0 + (-x).exp())
}

/// `[0, 1]` → `0..=255`, clamping (NaN gives 0).
#[inline]
fn unit_to_u8(v: f32) -> u8 {
    (v * 255.0 + 0.5) as u8
}

/// Parses an INRIA 3DGS `.ply`.
///
/// The header is parsed properly: properties may come in any order and with
/// any scalar type, unknown ones (normals, `f_rest_*`, …) are skipped, and
/// `\r\n` line ends are accepted. Only `x y z` are mandatory, so plain point
/// clouds load too: missing `scale_*` gives 1 cm splats, missing `rot_*` the
/// identity, missing `opacity` opaque splats, and colors fall back from
/// `f_dc_*` to `red green blue` (8-bit) to white.
pub fn read_ply(bytes: &[u8]) -> Result<Vec<Splat>, IoError> {
    let header = parse_ply_header(bytes)?;

    // Skip the elements stored before the vertices (unusual, but legal).
    let mut data_start = header.data_start as u64;
    let mut vertex = None;
    for element in &header.elements {
        if element.name == "vertex" {
            vertex = Some(element);
            break;
        }
        if element.has_list {
            return Err(IoError::Unsupported("list element before the vertices"));
        }
        data_start = element
            .count
            .checked_mul(element.stride as u64)
            .and_then(|size| size.checked_add(data_start))
            .ok_or(IoError::BadHeader("element size overflows"))?;
    }
    let vertex = vertex.ok_or(IoError::BadHeader("no vertex element"))?;
    if vertex.has_list {
        return Err(IoError::Unsupported("list property in the vertex element"));
    }
    let xyz = match [vertex.field("x"), vertex.field("y"), vertex.field("z")] {
        [Some(x), Some(y), Some(z)] => [x, y, z],
        _ => return Err(IoError::BadHeader("vertex element without x, y, z")),
    };

    // Validate the announced size against the bytes present before
    // allocating anything. `stride >= 3` here, so `count` is bounded by the
    // file size.
    let stride = vertex.stride;
    let needed = vertex
        .count
        .checked_mul(stride as u64)
        .and_then(|size| size.checked_add(data_start))
        .ok_or(IoError::BadHeader("element size overflows"))?;
    if needed > bytes.len() as u64 {
        return Err(IoError::Truncated { needed, available: bytes.len() });
    }
    let data = &bytes[data_start as usize..needed as usize];

    let triple = |names: [&str; 3]| match names.map(|n| vertex.field(n)) {
        [Some(a), Some(b), Some(c)] => Some([a, b, c]),
        _ => None,
    };
    let sh_dc = triple(["f_dc_0", "f_dc_1", "f_dc_2"]);
    let rgb = triple(["red", "green", "blue"]);
    let scale = triple(["scale_0", "scale_1", "scale_2"]);
    let opacity = vertex.field("opacity");
    let rot = match ["rot_0", "rot_1", "rot_2", "rot_3"].map(|n| vertex.field(n)) {
        [Some(w), Some(x), Some(y), Some(z)] => Some([w, x, y, z]),
        _ => None,
    };

    let mut splats = Vec::with_capacity(vertex.count as usize);
    for rec in data.chunks_exact(stride) {
        let get = |f: Field| f.ty.read(rec, f.offset);
        let pos = xyz.map(get);
        let color = if let Some(dc) = sh_dc {
            dc.map(|f| unit_to_u8(0.5 + SH_C0 * get(f)))
        } else if let Some(rgb) = rgb {
            // 8-bit colors as is; float colors are in [0, 1].
            rgb.map(|f| if f.ty == PlyType::U8 { rec[f.offset] } else { unit_to_u8(get(f)) })
        } else {
            [255; 3]
        };
        let alpha = opacity.map_or(255, |f| unit_to_u8(sigmoid(get(f))));
        let scale = scale.map_or([0.01; 3], |s| s.map(|f| get(f).exp()));
        let rot = rot.map_or([0.0, 0.0, 0.0, 1.0], |[w, x, y, z]| [get(x), get(y), get(z), get(w)]);
        splats.extend(sanitize(pos, scale, rot, [color[0], color[1], color[2], alpha]));
    }
    Ok(splats)
}

/// Reads a splat file of either supported format, sniffing the PLY magic
/// (`ply` followed by a line end); anything else is taken as `.splat`.
pub fn read_any(bytes: &[u8]) -> Result<Vec<Splat>, IoError> {
    if bytes.starts_with(b"ply\n") || bytes.starts_with(b"ply\r\n") {
        read_ply(bytes)
    } else {
        read_splat(bytes)
    }
}

#[cfg(test)]
mod tests {
    use glam::Quat;

    use super::*;
    use crate::splat::test_util::Rng;

    fn cloud(n: usize, seed: u64) -> Vec<Splat> {
        let mut rng = Rng::new(seed);
        (0..n).map(|_| rng.splat(5.0, 0.001, 0.5)).collect()
    }

    fn angle_deg(a: [f32; 4], b: [f32; 4]) -> f32 {
        // atan2 form: accurate for tiny angles, unlike acos of the dot product.
        let d = Quat::from_array(a).normalize().conjugate() * Quat::from_array(b).normalize();
        (2.0 * d.xyz().length().atan2(d.w.abs())).to_degrees()
    }

    #[test]
    fn splat_format_roundtrip() {
        let splats = cloud(2000, 1);
        let bytes = write_splat(&splats);
        assert_eq!(bytes.len(), 2000 * 32);
        let back = read_splat(&bytes).unwrap();
        assert_eq!(back.len(), splats.len());
        for (a, b) in splats.iter().zip(&back) {
            assert_eq!(a.pos, b.pos);
            assert_eq!(a.scale, b.scale);
            assert_eq!(a.rgba, b.rgba);
            // 8 bits per quaternion component.
            assert!(angle_deg(a.rot, b.rot) < 1.5, "{:?} -> {:?}", a.rot, b.rot);
            assert!((Quat::from_array(b.rot).length() - 1.0).abs() < 1e-5);
        }
        assert_eq!(read_any(&bytes).unwrap(), back);
    }

    #[test]
    fn splat_format_layout() {
        let s =
            Splat { pos: [1.0, 2.0, 3.0], scale: [0.1, 0.2, 0.3], rot: [0.0, 0.0, 0.0, 1.0], rgba: [11, 22, 33, 44] };
        let bytes = write_splat(&[s]);
        assert_eq!(&bytes[0..4], &1.0f32.to_le_bytes());
        assert_eq!(&bytes[20..24], &0.3f32.to_le_bytes());
        assert_eq!(&bytes[24..28], &[11, 22, 33, 44]);
        // w first; +1 saturates at 255, 0 is 128.
        assert_eq!(&bytes[28..32], &[255, 128, 128, 128]);
        let x_axis = Splat { rot: [-1.0, 0.0, 0.0, 0.0], ..s };
        assert_eq!(&write_splat(&[x_axis])[28..32], &[128, 0, 128, 128]);
    }

    #[test]
    fn splat_format_rejects_bad_lengths_and_filters_broken_records() {
        let bytes = write_splat(&cloud(3, 2));
        assert_eq!(read_splat(&bytes[..bytes.len() - 1]), Err(IoError::BadLength(95)));
        assert_eq!(read_splat(&bytes[..33]), Err(IoError::BadLength(33)));
        assert_eq!(read_splat(&[]), Ok(vec![]));

        let mut broken = cloud(4, 3);
        broken[1].pos[2] = f32::NAN;
        broken[2].scale[0] = f32::INFINITY;
        broken[3].scale[1] = -0.25;
        let mut bytes = write_splat(&broken);
        // Zero quaternion bytes (128) decode to the identity.
        bytes[28..32].copy_from_slice(&[128; 4]);
        let back = read_splat(&bytes).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].rot, [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(back[1].scale[1], 0.25);
    }

    #[test]
    fn ply_roundtrip() {
        let mut splats = cloud(2000, 4);
        // Extremes of color and opacity must survive exactly.
        splats[0].rgba = [0, 0, 0, 0];
        splats[1].rgba = [255, 255, 255, 255];
        splats[2].rgba = [1, 254, 128, 1];
        splats[3].rgba = [127, 3, 200, 254];
        let bytes = write_ply(&splats);
        assert!(bytes.starts_with(b"ply\nformat binary_little_endian 1.0\nelement vertex 2000\nproperty float x\n"));
        let header_len = bytes.windows(11).position(|w| w == b"end_header\n").unwrap() + 11;
        assert_eq!(bytes.len(), header_len + 2000 * 17 * 4);

        let back = read_ply(&bytes).unwrap();
        assert_eq!(back.len(), splats.len());
        for (a, b) in splats.iter().zip(&back) {
            assert_eq!(a.pos, b.pos);
            assert_eq!(a.rgba, b.rgba, "color/opacity");
            for k in 0..3 {
                assert!((a.scale[k] / b.scale[k] - 1.0).abs() < 1e-5);
            }
            assert!(angle_deg(a.rot, b.rot) < 1e-2);
        }
        assert_eq!(read_any(&bytes).unwrap(), back);
    }

    #[test]
    fn ply_roundtrip_of_every_color_and_opacity_level() {
        let base = Splat { pos: [0.0; 3], scale: [0.1; 3], rot: [0.0, 0.0, 0.0, 1.0], rgba: [0; 4] };
        let splats: Vec<Splat> = (0..=255u8).map(|v| Splat { rgba: [v, 255 - v, v ^ 0x55, v], ..base }).collect();
        let back = read_ply(&write_ply(&splats)).unwrap();
        for (a, b) in splats.iter().zip(&back) {
            assert_eq!(a.rgba, b.rgba);
        }
    }

    #[test]
    fn ply_write_handles_degenerate_splats() {
        let s = Splat { pos: [1.0, 2.0, 3.0], scale: [0.0, -0.5, 1e-20], rot: [0.0; 4], rgba: [5, 6, 7, 8] };
        let back = read_ply(&write_ply(&[s])).unwrap();
        assert_eq!(back.len(), 1);
        assert!(back[0].scale[0] > 0.0 && back[0].scale[0] <= 1.1e-9);
        assert!((back[0].scale[1] - 0.5).abs() < 1e-6);
        assert_eq!(back[0].rot, [0.0, 0.0, 0.0, 1.0]);
        assert_eq!(read_ply(&write_ply(&[])).unwrap(), vec![]);
    }

    /// Builds a little-endian PLY with arbitrary properties.
    fn build_ply(line_end: &str, props: &[(&str, &str)], rows: &[Vec<f64>]) -> Vec<u8> {
        let mut header = format!("ply{e}format binary_little_endian 1.0{e}comment hand-built{e}", e = line_end);
        header += &format!("element vertex {}{}", rows.len(), line_end);
        for (ty, name) in props {
            header += &format!("property {ty} {name}{line_end}");
        }
        header += &format!("end_header{line_end}");
        let mut out = header.into_bytes();
        for row in rows {
            assert_eq!(row.len(), props.len());
            for (&(ty, _), &v) in props.iter().zip(row) {
                match ty {
                    "float" | "float32" => out.extend_from_slice(&(v as f32).to_le_bytes()),
                    "double" => out.extend_from_slice(&v.to_le_bytes()),
                    "uchar" => out.push(v as u8),
                    "short" => out.extend_from_slice(&(v as i16).to_le_bytes()),
                    "int" => out.extend_from_slice(&(v as i32).to_le_bytes()),
                    other => panic!("test does not write {other}"),
                }
            }
        }
        out
    }

    fn logit(p: f64) -> f64 {
        (p / (1.0 - p)).ln()
    }

    #[test]
    fn hand_built_ply_with_shuffled_and_extra_properties() {
        let props = [
            ("float", "rot_3"),
            ("uchar", "label"),
            ("float", "scale_1"),
            ("float", "f_rest_0"),
            ("double", "y"),
            ("float", "f_dc_2"),
            ("float", "opacity"),
            ("short", "x"),
            ("float", "f_rest_1"),
            ("float", "rot_0"),
            ("float", "scale_0"),
            ("int", "frame"),
            ("float", "f_dc_0"),
            ("float", "nx"),
            ("float", "rot_1"),
            ("float32", "z"),
            ("float", "scale_2"),
            ("float", "f_dc_1"),
            ("float", "rot_2"),
            ("float", "f_rest_44"),
        ];
        let dc = |c: f64| (c - 0.5) / 0.282_094_79;
        // rot (w, x, y, z) = (2, 0, 0, 2)/… → 90° about z after normalization.
        let rows = vec![
            vec![
                2.0,
                7.0,
                (0.2f64).ln(),
                9.9,
                1.5,
                dc(0.75),
                logit(0.5),
                -3.0,
                9.9,
                2.0,
                (0.1f64).ln(),
                12345.0,
                dc(0.25),
                1.0,
                0.0,
                2.25,
                (0.3f64).ln(),
                dc(1.0),
                0.0,
                9.9,
            ],
            // Out-of-gamut color, huge opacity, zero quaternion.
            vec![
                0.0,
                1.0,
                0.0,
                0.0,
                -1.0,
                dc(-3.0),
                50.0,
                4.0,
                0.0,
                0.0,
                0.0,
                -1.0,
                dc(7.0),
                0.0,
                0.0,
                0.5,
                0.0,
                dc(0.5),
                0.0,
                0.0,
            ],
        ];
        for line_end in ["\n", "\r\n"] {
            let bytes = build_ply(line_end, &props, &rows);
            let splats = read_any(&bytes).unwrap();
            assert_eq!(splats.len(), 2, "line end {line_end:?}");
            let s = &splats[0];
            assert_eq!(s.pos, [-3.0, 1.5, 2.25]);
            for (got, want) in s.scale.iter().zip([0.1, 0.2, 0.3]) {
                assert!((got - want).abs() < 1e-6);
            }
            assert_eq!(s.rgba, [64, 255, 191, 128]);
            let h = std::f32::consts::FRAC_1_SQRT_2;
            for (got, want) in s.rot.iter().zip([0.0, 0.0, h, h]) {
                assert!((got - want).abs() < 1e-6, "{:?}", s.rot);
            }
            let t = &splats[1];
            assert_eq!(t.pos, [4.0, -1.0, 0.5]);
            assert_eq!(t.rgba, [255, 128, 0, 255]);
            assert_eq!(t.scale, [1.0, 1.0, 1.0]);
            assert_eq!(t.rot, [0.0, 0.0, 0.0, 1.0]);
        }
    }

    #[test]
    fn inria_ply_with_higher_order_harmonics() {
        // The property list of a real 3DGS training output, for SH degrees
        // 0 to 3 (0, 9, 24 or 45 `f_rest_*`), with unnormalized quaternions.
        let mut rng = Rng::new(7);
        for rest in [0usize, 9, 24, 45] {
            let mut names: Vec<String> =
                ["x", "y", "z", "nx", "ny", "nz", "f_dc_0", "f_dc_1", "f_dc_2"].map(String::from).to_vec();
            names.extend((0..rest).map(|i| format!("f_rest_{i}")));
            names.extend(
                ["opacity", "scale_0", "scale_1", "scale_2", "rot_0", "rot_1", "rot_2", "rot_3"].map(String::from),
            );
            let props: Vec<(&str, &str)> = names.iter().map(|n| ("float", n.as_str())).collect();
            let expected = cloud(50, 70 + rest as u64);
            let rows: Vec<Vec<f64>> = expected
                .iter()
                .map(|s| {
                    let mut row: Vec<f64> = s.pos.iter().map(|&v| v as f64).collect();
                    row.extend([0.0; 3]);
                    row.extend(s.rgba[..3].iter().map(|&c| (c as f64 / 255.0 - 0.5) / 0.282_094_791_773_878_14));
                    row.extend((0..rest).map(|_| rng.range(-1.0, 1.0) as f64));
                    row.push(logit((s.rgba[3] as f64 / 255.0).clamp(1e-4, 1.0 - 1e-4)));
                    row.extend(s.scale.iter().map(|&v| (v as f64).ln()));
                    let k = rng.range(0.5, 3.0) as f64;
                    row.extend([s.rot[3], s.rot[0], s.rot[1], s.rot[2]].map(|c| c as f64 * k));
                    row
                })
                .collect();
            let bytes = build_ply("\n", &props, &rows);
            assert_eq!(
                bytes.len() - bytes.windows(11).position(|w| w == b"end_header\n").unwrap() - 11,
                50 * (17 + rest) * 4
            );
            let got = read_any(&bytes).unwrap();
            assert_eq!(got.len(), expected.len());
            for (a, b) in expected.iter().zip(&got) {
                assert_eq!((a.pos, a.rgba), (b.pos, b.rgba), "{rest} f_rest properties");
                for k in 0..3 {
                    assert!((a.scale[k] / b.scale[k] - 1.0).abs() < 1e-5);
                }
                assert!(angle_deg(a.rot, b.rot) < 0.05);
            }
        }
    }

    #[test]
    fn splat_file_follows_the_antimatter15_conventions() {
        // A record built the way antimatter15's converter does it: linear
        // scales, 8-bit color and opacity, rotation bytes w, x, y, z.
        let q = Quat::from_euler(glam::EulerRot::XYZ, 0.3, -0.7, 1.9);
        let mut record = Vec::new();
        for v in [1.0f32, 2.0, 3.0, 0.1, 0.2, 0.3] {
            record.extend_from_slice(&v.to_le_bytes());
        }
        record.extend_from_slice(&[10, 20, 30, 200]);
        record.extend([q.w, q.x, q.y, q.z].map(|c| (c * 128.0 + 128.0).clamp(0.0, 255.0) as u8));
        let splats = read_splat(&record).unwrap();
        assert_eq!(splats.len(), 1);
        assert_eq!(
            (splats[0].pos, splats[0].scale, splats[0].rgba),
            ([1.0, 2.0, 3.0], [0.1, 0.2, 0.3], [10, 20, 30, 200])
        );
        assert!(angle_deg(splats[0].rot, q.to_array()) < 1.5, "{:?} vs {q:?}", splats[0].rot);
        // Writing it back changes no byte by more than one level.
        for (a, b) in write_splat(&splats).iter().zip(&record) {
            assert!(a.abs_diff(*b) <= 1);
        }
    }

    #[test]
    fn unusual_but_legal_ply_headers_are_read() {
        let xyz = "property float x\nproperty float y\nproperty float z\n";
        let data: Vec<u8> = [1.0f32, 2.0, 3.0].iter().flat_map(|v| v.to_le_bytes()).collect();
        let with_data = |header: String| [header.into_bytes(), data.clone()].concat();
        let f = "ply\nformat binary_little_endian 1.0\n";
        // An element without properties before the vertices occupies no bytes.
        let empty_element = with_data(format!("{f}element junk 99999999999\nelement vertex 1\n{xyz}end_header\n"));
        // Blank lines, tabs and repeated spaces.
        let spaced = with_data(
            "ply\n\n   \nformat    binary_little_endian   1.0   \n  element   vertex   1  \n\tproperty\tfloat\tx\n\
             property float y\nproperty float z\nend_header\n"
                .to_string(),
        );
        // A comment that mentions end_header does not end the header.
        let comment = with_data(format!("{f}comment end_header\nelement vertex 1\n{xyz}end_header\n"));
        for bytes in [empty_element, spaced, comment] {
            assert_eq!(read_any(&bytes).unwrap()[0].pos, [1.0, 2.0, 3.0]);
        }
        // A duplicated property name: the first one is used, offsets still count both.
        let mut dup = format!("{f}element vertex 1\nproperty float x\n{xyz}end_header\n").into_bytes();
        dup.extend([5.0f32, 6.0, 7.0, 8.0].iter().flat_map(|v| v.to_le_bytes()));
        assert_eq!(read_ply(&dup).unwrap()[0].pos, [5.0, 7.0, 8.0]);
        // Scales whose exponential overflows drop the splat instead of producing infinities.
        let mut big = format!("{f}element vertex 1\n{xyz}property double scale_0\nproperty double scale_1\nproperty double scale_2\nend_header\n").into_bytes();
        big.extend(data.iter());
        big.extend([1e300f64, -1e300, 0.0].iter().flat_map(|v| v.to_le_bytes()));
        assert_eq!(read_ply(&big), Ok(vec![]));
    }

    #[test]
    fn plain_point_cloud_ply_gets_defaults() {
        let props =
            [("float", "x"), ("float", "y"), ("float", "z"), ("uchar", "red"), ("uchar", "green"), ("uchar", "blue")];
        let rows = vec![vec![1.0, 2.0, 3.0, 10.0, 20.0, 30.0], vec![f64::NAN, 0.0, 0.0, 1.0, 2.0, 3.0]];
        let splats = read_ply(&build_ply("\n", &props, &rows)).unwrap();
        // The NaN point is dropped.
        assert_eq!(
            splats,
            vec![Splat { pos: [1.0, 2.0, 3.0], scale: [0.01; 3], rot: [0.0, 0.0, 0.0, 1.0], rgba: [10, 20, 30, 255] }]
        );
        let xyz_only = build_ply("\n", &props[..3], &[vec![0.0, 0.0, 0.0]]);
        assert_eq!(read_ply(&xyz_only).unwrap()[0].rgba, [255; 4]);
    }

    #[test]
    fn ply_skips_elements_before_the_vertices_and_ignores_later_ones() {
        let mut bytes = b"ply\nformat binary_little_endian 1.0\nobj_info test\n\
            element camera 2\nproperty uchar id\nproperty float focal\n\
            element vertex 1\nproperty float x\nproperty float y\nproperty float z\n\
            element face 5\nproperty list uchar int vertex_indices\nend_header\n"
            .to_vec();
        bytes.extend_from_slice(&[0xEE; 10]); // two camera records of 5 bytes
        for v in [7.0f32, 8.0, 9.0] {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        bytes.extend_from_slice(&[1, 2, 3]); // (truncated) faces: not read
        let splats = read_ply(&bytes).unwrap();
        assert_eq!(splats.len(), 1);
        assert_eq!(splats[0].pos, [7.0, 8.0, 9.0]);
    }

    #[test]
    fn truncated_ply_is_an_error() {
        let bytes = write_ply(&cloud(10, 5));
        let header_len = bytes.windows(11).position(|w| w == b"end_header\n").unwrap() + 11;
        for len in [bytes.len() - 1, bytes.len() - 68, header_len + 1, header_len] {
            assert_eq!(
                read_ply(&bytes[..len]),
                Err(IoError::Truncated { needed: bytes.len() as u64, available: len }),
                "len {len}"
            );
        }
        // Cut inside the header.
        for len in [header_len - 1, 40, 4] {
            assert_eq!(read_ply(&bytes[..len]), Err(IoError::BadHeader("missing end_header")), "len {len}");
        }
        // Trailing bytes after the vertices are tolerated.
        let mut longer = bytes.clone();
        longer.extend_from_slice(b"junk");
        assert_eq!(read_ply(&longer).unwrap().len(), 10);
    }

    #[test]
    fn malformed_ply_headers_are_rejected() {
        let ply = |header: &str| read_ply(header.as_bytes());
        let v = "element vertex 0\nproperty float x\nproperty float y\nproperty float z\nend_header\n";
        assert_eq!(ply(&format!("ply\nformat binary_little_endian 1.0\n{v}")), Ok(vec![]));
        assert_eq!(
            ply(&format!("plx\nformat binary_little_endian 1.0\n{v}")),
            Err(IoError::BadHeader("missing ply magic"))
        );
        assert_eq!(ply(&format!("ply\n{v}")), Err(IoError::BadHeader("missing format line")));
        assert_eq!(ply(&format!("ply\nformat ascii 1.0\n{v}")), Err(IoError::Unsupported("ascii PLY")));
        assert_eq!(
            ply(&format!("ply\nformat binary_big_endian 1.0\n{v}")),
            Err(IoError::Unsupported("big-endian PLY"))
        );
        assert_eq!(ply(&format!("ply\nformat weird 1.0\n{v}")), Err(IoError::BadHeader("unknown format")));
        let f = "ply\nformat binary_little_endian 1.0\n";
        assert_eq!(ply(&format!("{f}end_header\n")), Err(IoError::BadHeader("no vertex element")));
        assert_eq!(
            ply(&format!("{f}property float x\nend_header\n")),
            Err(IoError::BadHeader("property before any element"))
        );
        assert_eq!(
            ply(&format!("{f}element vertex\nend_header\n")),
            Err(IoError::BadHeader("element without a valid count"))
        );
        assert_eq!(
            ply(&format!("{f}element vertex -3\nend_header\n")),
            Err(IoError::BadHeader("element without a valid count"))
        );
        assert_eq!(
            ply(&format!("{f}element vertex 1\nproperty float x\nproperty float y\nend_header\n")),
            Err(IoError::BadHeader("vertex element without x, y, z"))
        );
        assert_eq!(
            ply(&format!("{f}element vertex 1\nproperty quaternion x\nend_header\n")),
            Err(IoError::BadHeader("unknown property type"))
        );
        assert_eq!(
            ply(&format!("{f}element vertex 1\nproperty float\nend_header\n")),
            Err(IoError::BadHeader("property without a name"))
        );
        assert_eq!(
            ply(&format!("{f}element vertex 1\nproperty list uchar float x\n{}", &v[17..])),
            Err(IoError::Unsupported("list property in the vertex element"))
        );
        assert_eq!(
            ply(&format!("{f}element face 1\nproperty list uchar int i\n{v}")),
            Err(IoError::Unsupported("list element before the vertices"))
        );
        assert_eq!(read_ply(b"ply\n\xFF\xFE\nend_header\n"), Err(IoError::BadHeader("header is not text")));
        assert_eq!(read_ply(b""), Err(IoError::BadHeader("missing end_header")));
    }

    #[test]
    fn hostile_counts_do_not_allocate() {
        let f = "ply\nformat binary_little_endian 1.0\n";
        let xyz = "property float x\nproperty float y\nproperty float z\n";
        // 2^60 vertices announced, no data.
        let huge = format!("{f}element vertex 1152921504606846976\n{xyz}end_header\n");
        assert!(matches!(read_ply(huge.as_bytes()), Err(IoError::Truncated { .. })));
        // Size computations that overflow u64.
        let overflow = format!("{f}element vertex 18446744073709551615\n{xyz}end_header\n");
        assert_eq!(read_ply(overflow.as_bytes()), Err(IoError::BadHeader("element size overflows")));
        let before =
            format!("{f}element junk 18446744073709551615\nproperty double a\nelement vertex 1\n{xyz}end_header\n");
        assert_eq!(read_ply(before.as_bytes()), Err(IoError::BadHeader("element size overflows")));
        let skip = format!("{f}element junk 4000000000\nproperty double a\nelement vertex 1\n{xyz}end_header\n");
        assert!(matches!(read_ply(skip.as_bytes()), Err(IoError::Truncated { .. })));
    }

    #[test]
    fn fuzz_never_panics() {
        let mut rng = Rng::new(9);
        let splats = cloud(20, 6);
        let seeds = [write_ply(&splats), write_splat(&splats), write_ply(&[])];
        for seed in &seeds {
            for len in 0..=seed.len().min(700) {
                let _ = read_any(&seed[..len]);
            }
            for _ in 0..4000 {
                let mut bytes = seed.clone();
                match rng.below(3) {
                    0 => {
                        // Corrupt the header region.
                        for _ in 0..1 + rng.below(6) {
                            let i = rng.below(bytes.len().clamp(1, 400));
                            if i < bytes.len() {
                                bytes[i] = rng.next_u64() as u8;
                            }
                        }
                    }
                    1 => {
                        let keep = rng.below(bytes.len() + 1);
                        bytes.truncate(keep);
                    }
                    _ => {
                        if !bytes.is_empty() {
                            let i = rng.below(bytes.len());
                            bytes[i..].iter_mut().for_each(|b| *b = rng.next_u64() as u8);
                        }
                    }
                }
                if let Ok(out) = read_any(&bytes) {
                    assert!(out.len() <= bytes.len());
                    assert!(out.iter().all(|s| s.pos.iter().chain(&s.scale).chain(&s.rot).all(|v| v.is_finite())));
                }
            }
        }
        for _ in 0..5000 {
            let mut bytes: Vec<u8> = (0..rng.below(300)).map(|_| rng.next_u64() as u8).collect();
            if rng.below(2) == 0 {
                bytes.splice(0..0, *b"ply\nformat binary_little_endian 1.0\nelement vertex 3\nproperty float x\n");
            }
            let _ = read_any(&bytes);
            let _ = read_ply(&bytes);
            let _ = read_splat(&bytes);
        }
    }

    #[test]
    fn errors_are_printable() {
        for e in [
            IoError::Truncated { needed: 100, available: 10 },
            IoError::BadLength(33),
            IoError::BadHeader("x"),
            IoError::Unsupported("y"),
        ] {
            assert!(!(Box::new(e) as Box<dyn std::error::Error>).to_string().is_empty());
        }
    }
}
