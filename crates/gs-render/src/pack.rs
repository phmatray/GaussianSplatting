//! GPU representation of a splat and the layout of the data texture.
//!
//! # Why an `RGBA32UI` texture
//!
//! The vertex shader needs, per splat: the center (3 × f32), the six unique
//! terms of the world covariance (6 × f32) and the color with opacity
//! (4 × u8). That is ten 32-bit words; they are stored in three texels of an
//! unsigned-integer texture (twelve words, two spare) and read back with
//! `texelFetch` + `uintBitsToFloat`:
//!
//! ```text
//! texel 0: pos.x      pos.y      pos.z      r | g<<8 | b<<16 | a<<24
//! texel 1: Σxx        Σxy        Σxz        Σyy
//! texel 2: Σyz        Σzz        0          0
//! ```
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

use gs_core::splat::cov6;
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
    out[10] = 0;
    out[11] = 0;
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
        assert_eq!(&w[10..], &[0, 0]);
        let _ = Vec3::ZERO;
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
