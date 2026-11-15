//! GPU representation of a splat and the layout of the data texture.
//!
//! # Why an `RGBA32UI` texture
//!
//! The vertex shader needs, per splat: the center (3 × f32), the six unique
//! terms of the world covariance (6 × f32) and the color with opacity
//! (4 × u8). The normalised blending mode also needs the plane of a surfel:
//! its normal and the reach of its disc. That is twelve 32-bit words, stored
//! in three texels of an unsigned-integer texture and read back with
//! `texelFetch` + `uintBitsToFloat`:
//!
//! ```text
//! texel 0: pos.x      pos.y      pos.z      r | g<<8 | b<<16 | a<<24
//! texel 1: Σxx        Σxy        Σxz        Σyy
//! texel 2: Σyz        Σzz        normal     reach
//! ```
//!
//! `normal` is the third local axis of the splat in octahedral encoding,
//! 16 bits per coordinate ([`encode_normal`]; 3·10⁻⁵ rad, far below what the
//! 1 % depth tolerance of the normalised mode can see). `reach` is the larger
//! of the two tangential standard deviations, or `0.0` for a splat that has
//! no plane (a zero tangential scale): its fragments then all take the depth
//! of its center, like in the CPU reference (`gs_core::experimental::wblend`).
//!
//! * `RGBA32UI` textures, `texelFetch` and `uintBitsToFloat` are core WebGL2 /
//!   GLSL ES 3.00: no extension is involved, unlike rendering to or filtering
//!   float textures.
//! * Floats travel as raw bits, so the shader sees exactly the numbers the CPU
//!   reference rasterizer uses (`gs_core::splat::cov6`). Half-float covariances
//!   (2 texels per splat) would underflow for millimetre-sized surfels
//!   (σ² ≈ 10⁻⁶ m² is below the half-float normal range).
//! * The color needs no second texture or float conversion: one word.
//!
//! Splat `i` of a layer occupies texels `3·(i mod S) .. +3` of row `i / S`,
//! with `S` splats per row, so a contiguous run of splats is at most three
//! rectangles (`texSubImage2D` calls): a partial first row, full rows, a
//! partial last row.

use glam::Vec3;
use gs_core::splat::{cov6, surfel_normal};
use gs_core::Splat;

/// Texels per splat.
pub(crate) const TEXELS_PER_SPLAT: u32 = 3;
/// 32-bit words per splat (`TEXELS_PER_SPLAT` RGBA texels).
pub(crate) const WORDS_PER_SPLAT: usize = 12;
/// Preferred number of splats per texture row (3072 texels).
pub(crate) const MAX_SPLATS_PER_ROW: u32 = 1024;

/// Writes the twelve words of `splat` into `out`.
#[inline]
pub(crate) fn pack_splat(splat: &Splat, out: &mut [u32]) {
    let c = cov6(splat);
    out[0] = splat.pos[0].to_bits();
    out[1] = splat.pos[1].to_bits();
    out[2] = splat.pos[2].to_bits();
    out[3] = u32::from_le_bytes(splat.rgba);
    out[4] = c[0].to_bits();
    out[5] = c[1].to_bits();
    out[6] = c[2].to_bits();
    out[7] = c[3].to_bits();
    out[8] = c[4].to_bits();
    out[9] = c[5].to_bits();
    out[10] = encode_normal(surfel_normal(splat));
    let [su, sv, _] = splat.scale;
    // The reference builds the plane from the two scaled tangent axes: it
    // exists when neither of them vanishes.
    let has_plane = su != 0.0 && sv != 0.0 && su.is_finite() && sv.is_finite();
    out[11] = if has_plane { su.max(sv) } else { 0.0 }.to_bits();
}

/// Quantization steps of a normal coordinate on either side of zero.
///
/// An even number of steps in all (`0..=65534`), so that `-1`, `0` and `1`
/// are on the grid: the normals of floors and walls, which are exactly
/// axis-aligned in every synthetic scene, survive unchanged. With `65535`
/// steps a zero decodes to `1.5·10⁻⁵`, which tilts such a normal by that
/// much and culls a floor seen from exactly inside its plane on one side.
const NORMAL_STEPS: f32 = 32767.0;

/// Octahedral encoding of a unit vector: `x | y << 16`, each coordinate of
/// the unfolded octahedron mapped from `-1..=1` to `0..=65534`
/// ([`NORMAL_STEPS`]). The sign of every component survives (back-face
/// culling depends on it). The inverse is `decode_normal` in the vertex
/// shader (and [`decode_normal`] here).
#[inline]
pub(crate) fn encode_normal(n: Vec3) -> u32 {
    let l1 = n.x.abs() + n.y.abs() + n.z.abs();
    // Not finite or zero: any direction will do; +Z is what an identity
    // rotation gives.
    let n = if l1.is_finite() && l1 > 0.0 { n / l1 } else { Vec3::Z };
    let (x, y) = if n.z >= 0.0 { (n.x, n.y) } else { ((1.0 - n.y.abs()).copysign(n.x), (1.0 - n.x.abs()).copysign(n.y)) };
    let quantize = |v: f32| (v * NORMAL_STEPS + NORMAL_STEPS).round().clamp(0.0, 2.0 * NORMAL_STEPS) as u32;
    quantize(x) | quantize(y) << 16
}

/// Inverse of [`encode_normal`], written like the GLSL version.
#[cfg(test)]
pub(crate) fn decode_normal(word: u32) -> Vec3 {
    let f = |v: u32| (v as f32 - NORMAL_STEPS) / NORMAL_STEPS;
    let (x, y) = (f(word & 0xffff), f(word >> 16));
    let z = 1.0 - x.abs() - y.abs();
    let t = (-z).max(0.0);
    Vec3::new(x + if x >= 0.0 { -t } else { t }, y + if y >= 0.0 { -t } else { t }, z).normalize()
}

/// Packs `splats` into `out` (cleared first).
pub(crate) fn pack_splats(splats: &[Splat], out: &mut Vec<u32>) {
    out.clear();
    out.resize(splats.len() * WORDS_PER_SPLAT, 0);
    for (splat, words) in splats.iter().zip(out.chunks_exact_mut(WORDS_PER_SPLAT)) {
        pack_splat(splat, words);
    }
}

/// Shape of a layer's data texture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TextureLayout {
    /// Splats per row.
    pub splats_per_row: u32,
    /// Largest number of rows the GPU allows.
    pub max_rows: u32,
}

/// One `texSubImage2D` rectangle covering a contiguous run of splats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct UploadRect {
    /// Texel offset and size.
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    /// The run of splats it holds, as indices into the layer.
    pub first_splat: usize,
    pub splat_count: usize,
}

impl TextureLayout {
    /// Layout for a GPU whose `MAX_TEXTURE_SIZE` is `max_texture_size`.
    pub fn new(max_texture_size: u32) -> Self {
        let max = max_texture_size.max(TEXELS_PER_SPLAT);
        Self { splats_per_row: (max / TEXELS_PER_SPLAT).min(MAX_SPLATS_PER_ROW), max_rows: max }
    }

    /// Width of the texture in texels.
    pub fn width(&self) -> u32 {
        self.splats_per_row * TEXELS_PER_SPLAT
    }

    /// Most splats a layer can hold.
    pub fn max_splats(&self) -> usize {
        self.splats_per_row as usize * self.max_rows as usize
    }

    /// Rows needed for `count` splats.
    pub fn rows_for(&self, count: usize) -> u32 {
        count.div_ceil(self.splats_per_row as usize) as u32
    }

    /// Number of rows to allocate when `needed` rows no longer fit into
    /// `current`: at least double, so that appending is amortized O(1).
    pub fn grown_rows(&self, current: u32, needed: u32) -> u32 {
        needed.max(current.saturating_mul(2)).max(MIN_ROWS).min(self.max_rows)
    }

    /// Splits the splat range `start..end` into the rectangles to upload.
    pub fn rects(&self, start: usize, end: usize) -> impl Iterator<Item = UploadRect> {
        let spr = self.splats_per_row as usize;
        let mut i = start;
        std::iter::from_fn(move || {
            if i >= end {
                return None;
            }
            let (row, col) = (i / spr, i % spr);
            let remaining = end - i;
            let (count, height) = if col != 0 || remaining < spr {
                (remaining.min(spr - col), 1)
            } else {
                let rows = remaining / spr;
                (rows * spr, rows)
            };
            let rect = UploadRect {
                x: col as u32 * TEXELS_PER_SPLAT,
                y: row as u32,
                width: (count / height) as u32 * TEXELS_PER_SPLAT,
                height: height as u32,
                first_splat: i,
                splat_count: count,
            };
            i += count;
            Some(rect)
        })
    }
}

/// Smallest texture height ever allocated.
pub(crate) const MIN_ROWS: u32 = 16;

#[cfg(test)]
mod tests {
    use glam::{Quat, Vec3};
    use gs_core::splat::cov6;

    use super::*;

    #[test]
    fn packing_is_bit_exact() {
        let splat = Splat {
            pos: [1.5, -2.25, 3.125],
            scale: [0.01, 0.2, 0.003],
            rot: Quat::from_euler(glam::EulerRot::XYZ, 0.3, -1.1, 2.0).to_array(),
            rgba: [10, 20, 30, 200],
        };
        let mut words = Vec::new();
        pack_splats(&[splat, splat], &mut words);
        assert_eq!(words.len(), 2 * WORDS_PER_SPLAT);
        let w = &words[WORDS_PER_SPLAT..];
        assert_eq!([f32::from_bits(w[0]), f32::from_bits(w[1]), f32::from_bits(w[2])], splat.pos);
        // Little-endian packing: the shader extracts r from the low byte.
        assert_eq!((w[3] & 0xff, (w[3] >> 8) & 0xff, (w[3] >> 16) & 0xff, w[3] >> 24), (10, 20, 30, 200));
        let cov: Vec<f32> = w[4..10].iter().map(|&b| f32::from_bits(b)).collect();
        assert_eq!(cov, cov6(&splat));
        // The plane: normal within a tenth of a millirad, reach = larger
        // tangential scale.
        let normal = splat.rotation() * Vec3::Z;
        assert!(angle(decode_normal(w[10]), normal) < 1.0e-4);
        assert_eq!(f32::from_bits(w[11]), 0.2);
    }

    /// Angle between two unit vectors that are almost equal, radians. (The
    /// arc cosine of their dot product cannot resolve less than 5·10⁻⁴ rad
    /// in 32-bit floats.)
    fn angle(a: Vec3, b: Vec3) -> f32 {
        a.cross(b).length().atan2(a.dot(b))
    }

    #[test]
    fn normals_survive_the_octahedral_encoding() {
        let mut worst = 0.0f32;
        let mut state = 0x1234_5678_9abc_def0u64;
        let mut unit = || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (state >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0
        };
        for i in 0..20_000 {
            let n = match i {
                0 => Vec3::X,
                1 => Vec3::NEG_X,
                2 => Vec3::Y,
                3 => Vec3::NEG_Y,
                4 => Vec3::Z,
                5 => Vec3::NEG_Z,
                _ => Vec3::new(unit(), unit(), unit()).normalize_or(Vec3::Z),
            };
            let decoded = decode_normal(encode_normal(n));
            worst = worst.max(angle(decoded, n));
            // The side a surfel faces must never flip.
            for eye in [Vec3::X, Vec3::Y, Vec3::Z, Vec3::new(0.3, -0.5, 0.8)] {
                if n.dot(eye).abs() > 1.0e-3 {
                    assert_eq!(n.dot(eye) > 0.0, decoded.dot(eye) > 0.0, "{n:?} -> {decoded:?}");
                }
            }
        }
        assert!(worst < 1.0e-4, "worst error {worst} rad");
        // The six axes are exact: a floor seen from inside its plane is
        // neither culled nor tilted.
        for axis in [Vec3::X, Vec3::NEG_X, Vec3::Y, Vec3::NEG_Y, Vec3::Z, Vec3::NEG_Z] {
            assert_eq!(decode_normal(encode_normal(axis)), axis);
        }
        // Garbage in: a valid direction out.
        for bad in [Vec3::ZERO, Vec3::splat(f32::NAN), Vec3::new(f32::INFINITY, 0.0, 0.0)] {
            assert_eq!(decode_normal(encode_normal(bad)), Vec3::Z);
        }
    }

    #[test]
    fn splats_without_a_plane_are_marked() {
        let ok = Splat::surfel(Vec3::ZERO, Vec3::Y, 0.05, 0.005, [255; 4]);
        let reach = |splat: Splat| {
            let mut words = Vec::new();
            pack_splats(&[splat], &mut words);
            f32::from_bits(words[11])
        };
        assert_eq!(reach(ok), 0.05);
        assert_eq!(reach(Splat { scale: [0.02, 0.07, 0.3], ..ok }), 0.07);
        assert_eq!(reach(Splat { scale: [0.0, 0.07, 0.3], ..ok }), 0.0);
        assert_eq!(reach(Splat { scale: [0.0; 3], ..ok }), 0.0);
        assert_eq!(reach(Splat { scale: [f32::NAN, 0.05, 0.05], ..ok }), 0.0);
        assert_eq!(reach(Splat { scale: [f32::INFINITY, 0.05, 0.05], ..ok }), 0.0);
        // Negative scales square to the same covariance; the reference takes
        // the larger signed value.
        assert_eq!(reach(Splat { scale: [-0.05, 0.03, 0.05], ..ok }), 0.03);
    }

    #[test]
    fn layout_adapts_to_the_texture_limit() {
        let big = TextureLayout::new(8192);
        assert_eq!((big.splats_per_row, big.width(), big.max_rows), (1024, 3072, 8192));
        assert!(big.max_splats() >= 8_000_000);
        // The WebGL2 minimum.
        let small = TextureLayout::new(2048);
        assert_eq!((small.splats_per_row, small.width()), (682, 2046));
        assert!(small.max_splats() >= 1_000_000);
        assert_eq!(big.rows_for(0), 0);
        assert_eq!(big.rows_for(1), 1);
        assert_eq!(big.rows_for(1024), 1);
        assert_eq!(big.rows_for(1025), 2);
    }

    #[test]
    fn growth_is_geometric_and_capped() {
        let layout = TextureLayout::new(4096);
        assert_eq!(layout.grown_rows(0, 1), MIN_ROWS);
        assert_eq!(layout.grown_rows(16, 17), 32);
        assert_eq!(layout.grown_rows(16, 100), 100);
        assert_eq!(layout.grown_rows(3000, 3001), 4096);
    }

    /// Every splat of the range is covered exactly once, by rectangles that
    /// stay inside one row unless they are full-width.
    fn check(layout: &TextureLayout, start: usize, end: usize) -> usize {
        let spr = layout.splats_per_row as usize;
        let mut next = start;
        let mut n = 0;
        for r in layout.rects(start, end) {
            assert_eq!(r.first_splat, next);
            assert_eq!(r.x as usize, (next % spr) * 3);
            assert_eq!(r.y as usize, next / spr);
            assert_eq!((r.width * r.height) as usize, r.splat_count * 3);
            assert!(r.x + r.width <= layout.width());
            if r.height > 1 {
                assert_eq!((r.x, r.width), (0, layout.width()));
            }
            next += r.splat_count;
            n += 1;
        }
        assert_eq!(next, end.max(start));
        n
    }

    #[test]
    fn upload_rectangles_cover_the_range() {
        let layout = TextureLayout { splats_per_row: 8, max_rows: 64 };
        assert_eq!(check(&layout, 0, 0), 0);
        assert_eq!(check(&layout, 0, 5), 1);
        assert_eq!(check(&layout, 0, 8), 1);
        assert_eq!(check(&layout, 0, 24), 1);
        assert_eq!(check(&layout, 0, 27), 2);
        assert_eq!(check(&layout, 3, 5), 1);
        assert_eq!(check(&layout, 3, 8), 1);
        assert_eq!(check(&layout, 3, 16), 2);
        assert_eq!(check(&layout, 3, 29), 3);
        assert_eq!(check(&layout, 16, 17), 1);
        for start in 0..20 {
            for end in start..60 {
                assert!(check(&layout, start, end) <= 3);
            }
        }
    }
}
