//! CPU reference Gaussian splat rasterizer.
//!
//! Implements exactly the algorithm documented in [`crate::splat`]: every
//! splat is projected with [`crate::splat::project_splat_with_axes`], the
//! visible ones are sorted back to front with [`DepthSorter`], and composited
//! with "over" directly on sRGB values. It is the ground truth the WebGL
//! renderer is compared against in its *sorted* blending mode, and it
//! produces the images for the tests (PSNR against the ray-cast scene) — CI
//! has no GPU. The reference of the *normalised* blending mode of
//! `docs/ARCHITECTURE.md` is [`crate::experimental::wblend`].
//!
//! The screen-space low-pass conserves the energy of a splat (step 6 of
//! [`crate::splat`]); [`RasterOptions::classic_dilation`] switches that off
//! for splat files trained with the plain dilation of 3DGS.
//!
//! Like the shader, it evaluates the Gaussian exponent from the quad
//! coordinate (`m = 9·|q|²`, steps 8–9) rather than from the conic: the two
//! are the same number, but the quad form is a sum of squares and stays
//! accurate for needle-like splats of any size.
//!
//! With the `par` feature the work is spread over image bands with rayon; the
//! output is bit-identical to the single-threaded path.

#[cfg(feature = "par")]
use rayon::prelude::*;

use crate::sort::DepthSorter;
use crate::splat::{cov6, project_splat_with_axes, ViewParams, CUTOFF_MAHALANOBIS2, DEFAULT_NEAR, MIN_ALPHA};
use crate::types::{Camera, RgbdFrame, Splat};

/// 8-bit sRGB image, row-major, top row first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    /// 3 bytes per pixel.
    pub rgb: Vec<u8>,
}

impl Image {
    /// Image filled with one color.
    pub fn new(width: u32, height: u32, fill: [u8; 3]) -> Self {
        let n = width as usize * height as usize;
        let mut rgb = Vec::with_capacity(n * 3);
        for _ in 0..n {
            rgb.extend_from_slice(&fill);
        }
        Self { width, height, rgb }
    }

    /// Color image of an RGB-D frame.
    pub fn from_frame(frame: &RgbdFrame) -> Self {
        Self { width: frame.width, height: frame.height, rgb: frame.rgb.clone() }
    }

    /// Color of pixel `(x, y)`.
    ///
    /// # Panics
    /// When the pixel is outside the image.
    #[inline]
    pub fn pixel(&self, x: u32, y: u32) -> [u8; 3] {
        assert!(x < self.width && y < self.height, "pixel ({x}, {y}) outside {}x{}", self.width, self.height);
        let i = (y as usize * self.width as usize + x as usize) * 3;
        [self.rgb[i], self.rgb[i + 1], self.rgb[i + 2]]
    }
}

impl From<&RgbdFrame> for Image {
    fn from(frame: &RgbdFrame) -> Self {
        Self::from_frame(frame)
    }
}

/// Sum over all channels of `f(|a − b|)`, and the channel count.
fn accumulate_diff(a: &Image, b: &Image, f: impl Fn(u32) -> u64) -> (u64, usize) {
    assert!(
        a.width == b.width && a.height == b.height && a.rgb.len() == b.rgb.len(),
        "image sizes differ: {}x{} vs {}x{}",
        a.width,
        a.height,
        b.width,
        b.height
    );
    let sum = a.rgb.iter().zip(&b.rgb).map(|(&x, &y)| f(x.abs_diff(y) as u32)).sum();
    (sum, a.rgb.len())
}

/// Peak signal-to-noise ratio between two images in dB (peak = 255).
/// Identical images give `f32::INFINITY`; two empty images too.
///
/// # Panics
/// When the image sizes differ.
pub fn psnr(a: &Image, b: &Image) -> f32 {
    let (sum, n) = accumulate_diff(a, b, |d| (d * d) as u64);
    if sum == 0 {
        return f32::INFINITY;
    }
    let mse = sum as f64 / n as f64;
    (10.0 * (255.0 * 255.0 / mse).log10()) as f32
}

/// Mean absolute difference per channel, in 8-bit levels (`0..=255`).
///
/// # Panics
/// When the image sizes differ.
pub fn mean_abs_error(a: &Image, b: &Image) -> f32 {
    let (sum, n) = accumulate_diff(a, b, |d| d as u64);
    if n == 0 {
        return 0.0;
    }
    (sum as f64 / n as f64) as f32
}

/// Options of [`rasterize`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RasterOptions {
    /// Color of pixels no splat covers.
    pub background: [u8; 3],
    /// Multiplies every standard deviation (see [`ViewParams::splat_scale`]).
    pub splat_scale: f32,
    /// Draw every splat as a small dot of fixed size on screen instead of its
    /// real footprint (point-cloud view).
    pub point_mode: bool,
    /// Standard deviation of the dots of `point_mode`, in pixels.
    pub point_sigma_px: f32,
    /// Splats whose center is closer than this are not drawn (meters).
    pub near: f32,
    /// Leave the opacity alone when the low-pass dilates a splat, as the
    /// reference 3DGS implementation does (and as this rasterizer did before
    /// the filter was made energy conserving). Right for imported splat files,
    /// which were trained that way; wrong for sensor surfels, whose far and
    /// miniature views it turns to mush.
    pub classic_dilation: bool,
}

impl Default for RasterOptions {
    fn default() -> Self {
        Self {
            background: [0, 0, 0],
            splat_scale: 1.0,
            point_mode: false,
            point_sigma_px: 0.8,
            near: DEFAULT_NEAR,
            classic_dilation: false,
        }
    }
}

/// One visible splat, ready to be composited.
#[derive(Clone, Copy)]
struct Draw {
    px: f32,
    py: f32,
    /// Rows of the pixel-offset → quad-coordinate map: `q = (u1·δ, u2·δ)`,
    /// with `u = axis / |axis|²` for the major and minor half-axes. `|q| = 1`
    /// is the 3σ contour (step 8 of the algorithm in `crate::splat`).
    u1: [f32; 2],
    u2: [f32; 2],
    /// sRGB color in `0..=255`.
    color: [f32; 3],
    opacity: f32,
    depth: f32,
    /// Covered pixel rectangle, end-exclusive.
    x0: u32,
    x1: u32,
    y0: u32,
    y1: u32,
}

/// Projects one splat and computes the pixel rectangle it can touch.
fn prepare(splat: &Splat, vp: &ViewParams, opts: &RasterOptions) -> Option<Draw> {
    let mut opacity = splat.rgba[3] as f32 / 255.0;
    if opacity < MIN_ALPHA {
        return None;
    }
    let (p, mut major, mut minor) = project_splat_with_axes(splat.position(), &cov6(splat), vp)?;
    if opts.point_mode {
        let r = p.with_point_footprint(opts.point_sigma_px).radius;
        major = [r, 0.0];
        minor = [0.0, r];
    } else if !opts.classic_dilation {
        // Step 6: the low-pass spreads the splat without adding energy.
        opacity *= p.gain;
        if opacity < MIN_ALPHA {
            return None;
        }
    }
    // Alpha drops below 1/255 where the Mahalanobis distance² exceeds
    // 2·ln(255·opacity), so faint splats need less than the full 3σ box:
    // `shrink2` is the squared fraction of it that can contribute.
    let shrink2 = (2.0 * (opacity / MIN_ALPHA).ln()).min(CUTOFF_MAHALANOBIS2) / CUTOFF_MAHALANOBIS2;
    // Axis-aligned half-extents of the 3σ ellipse: √(9·Σ'ₖₖ) with
    // 9·Σ'ₖₖ = majorₖ² + minorₖ².
    let ex = (shrink2 * (major[0] * major[0] + minor[0] * minor[0])).sqrt();
    let ey = (shrink2 * (major[1] * major[1] + minor[1] * minor[1])).sqrt();
    // Pixel i has its center at i + 0.5. Saturating float → int casts clip
    // the rectangle to the image (and send NaN to an empty one).
    let x0 = (p.px - ex - 0.5).ceil().max(0.0) as u32;
    let y0 = (p.py - ey - 0.5).ceil().max(0.0) as u32;
    let x1 = ((p.px + ex - 0.5).floor() + 1.0).clamp(0.0, vp.width) as u32;
    let y1 = ((p.py + ey - 0.5).floor() + 1.0).clamp(0.0, vp.height) as u32;
    if x0 >= x1 || y0 >= y1 {
        return None;
    }
    let reciprocal = |axis: [f32; 2]| {
        let inv = 1.0 / (axis[0] * axis[0] + axis[1] * axis[1]);
        [axis[0] * inv, axis[1] * inv]
    };
    Some(Draw {
        px: p.px,
        py: p.py,
        u1: reciprocal(major),
        u2: reciprocal(minor),
        color: [splat.rgba[0] as f32, splat.rgba[1] as f32, splat.rgba[2] as f32],
        opacity,
        depth: p.depth,
        x0,
        x1,
        y0,
        y1,
    })
}

/// Composites `order` (indices into `draws`, back to front) into the rows
/// `row0 .. row0 + acc.len() / (3·width)` of the float accumulator `acc`.
fn composite_rows(acc: &mut [f32], row0: u32, width: u32, draws: &[Draw], order: &[u32]) {
    let stride = width as usize * 3;
    let row_end = row0 + (acc.len() / stride.max(1)) as u32;
    for &index in order {
        let d = &draws[index as usize];
        for y in d.y0.max(row0)..d.y1.min(row_end) {
            let dy = y as f32 + 0.5 - d.py;
            let (row_q1, row_q2) = (dy * d.u1[1], dy * d.u2[1]);
            let start = (y - row0) as usize * stride;
            let row = &mut acc[start + d.x0 as usize * 3..start + d.x1 as usize * 3];
            for (i, dst) in row.chunks_exact_mut(3).enumerate() {
                let dx = (d.x0 as usize + i) as f32 + 0.5 - d.px;
                // Steps 9–11 of the algorithm in `crate::splat`, with the
                // exponent in its quad form m = 9·|q|². It is the same number
                // as δᵀ·conic·δ (`splat::eval_alpha`) but a sum of squares,
                // so it cannot cancel for needle-like splats.
                let q1 = dx * d.u1[0] + row_q1;
                let q2 = dx * d.u2[0] + row_q2;
                let r2 = q1 * q1 + q2 * q2;
                // False for NaN too.
                if r2 <= 1.0 {
                    let alpha = d.opacity * (-0.5 * CUTOFF_MAHALANOBIS2 * r2).exp();
                    if alpha >= MIN_ALPHA {
                        let keep = 1.0 - alpha;
                        dst[0] = d.color[0] * alpha + dst[0] * keep;
                        dst[1] = d.color[1] * alpha + dst[1] * keep;
                        dst[2] = d.color[2] * alpha + dst[2] * keep;
                    }
                }
            }
        }
    }
}

/// Single-threaded compositing of the whole image.
fn composite_serial(acc: &mut [f32], width: u32, draws: &[Draw], order: &[u32]) {
    composite_rows(acc, 0, width, draws, order);
}

/// Multi-threaded compositing: the image is cut into horizontal bands, every
/// band receives (in order) the splats touching it. Pixels see exactly the
/// same sequence of operations as in [`composite_serial`].
#[cfg(feature = "par")]
fn composite_parallel(acc: &mut [f32], width: u32, height: u32, draws: &[Draw], order: &[u32]) {
    let bands = (rayon::current_num_threads() * 4).clamp(1, height.max(1) as usize);
    let band_rows = (height as usize).div_ceil(bands).max(1) as u32;
    let band_count = height.div_ceil(band_rows) as usize;
    if band_count <= 1 {
        return composite_serial(acc, width, draws, order);
    }
    let mut bins: Vec<Vec<u32>> = vec![Vec::new(); band_count];
    for &index in order {
        let d = &draws[index as usize];
        for bin in &mut bins[(d.y0 / band_rows) as usize..=((d.y1 - 1) / band_rows) as usize] {
            bin.push(index);
        }
    }
    acc.par_chunks_mut(band_rows as usize * width as usize * 3)
        .zip(bins.par_iter())
        .enumerate()
        .for_each(|(band, (rows, bin))| composite_rows(rows, band as u32 * band_rows, width, draws, bin));
}

/// Renders `splats` as seen by `cam`.
///
/// The output has the size of the camera image. See the module documentation
/// of [`crate::splat`] for the exact image formation model.
pub fn rasterize(splats: &[Splat], cam: &Camera, opts: &RasterOptions) -> Image {
    let (width, height) = (cam.width, cam.height);
    let pixels = width as usize * height as usize;
    if pixels == 0 {
        return Image { width, height, rgb: Vec::new() };
    }
    let vp = ViewParams { near: opts.near, splat_scale: opts.splat_scale, ..ViewParams::from_camera(cam) };

    #[cfg(feature = "par")]
    let draws: Vec<Draw> = splats.par_iter().filter_map(|s| prepare(s, &vp, opts)).collect();
    #[cfg(not(feature = "par"))]
    let draws: Vec<Draw> = splats.iter().filter_map(|s| prepare(s, &vp, opts)).collect();

    let mut order = Vec::new();
    DepthSorter::new().sort_by_depth(draws.iter().map(|d| d.depth), &mut order);

    let background = opts.background.map(f32::from);
    let mut acc = Vec::with_capacity(pixels * 3);
    for _ in 0..pixels {
        acc.extend_from_slice(&background);
    }

    #[cfg(feature = "par")]
    composite_parallel(&mut acc, width, height, &draws, &order);
    #[cfg(not(feature = "par"))]
    composite_serial(&mut acc, width, &draws, &order);

    // Saturating cast: clamps to 0..=255.
    let rgb = acc.iter().map(|&v| (v + 0.5) as u8).collect();
    Image { width, height, rgb }
}

#[cfg(test)]
mod tests {
    use glam::{Quat, Vec3};

    use super::*;
    use crate::splat::test_util::{reference_projection, Rng};
    use crate::splat::LOW_PASS_PX2;

    /// Camera at the origin looking down −Z, 200×160, fx = fy = 200.
    fn camera() -> Camera {
        let mut cam = Camera::look_at(Vec3::ZERO, Vec3::NEG_Z, Vec3::Y, 1.0, 200, 160);
        cam.fx = 200.0;
        cam.fy = 200.0;
        cam
    }

    fn ball(pos: Vec3, sigma: f32, rgba: [u8; 4]) -> Splat {
        Splat { pos: pos.to_array(), scale: [sigma; 3], rot: Quat::IDENTITY.to_array(), rgba }
    }

    fn assert_close(actual: [u8; 3], expected: [f32; 3], tol: f32, what: &str) {
        for c in 0..3 {
            assert!((actual[c] as f32 - expected[c]).abs() <= tol, "{what}: {actual:?} vs {expected:?}");
        }
    }

    #[test]
    fn empty_scene_is_background() {
        let opts = RasterOptions { background: [10, 20, 30], ..Default::default() };
        let img = rasterize(&[], &camera(), &opts);
        assert_eq!((img.width, img.height), (200, 160));
        assert_eq!(img, Image::new(200, 160, [10, 20, 30]));
    }

    #[test]
    fn zero_sized_camera_gives_empty_image() {
        let mut cam = camera();
        cam.width = 0;
        let img = rasterize(&[ball(Vec3::new(0.0, 0.0, -2.0), 0.1, [255; 4])], &cam, &RasterOptions::default());
        assert!(img.rgb.is_empty());
    }

    #[test]
    fn opaque_splat_shows_its_color_with_monotonic_falloff() {
        let cam = camera();
        let color = [200u8, 120, 40];
        // σ = 0.2 m at 2 m ⇒ σ_px = 20; centered exactly on a pixel center.
        let center = cam.unproject(100.5, 80.5, 2.0);
        let opts = RasterOptions { background: [0, 0, 255], ..Default::default() };
        let img = rasterize(&[ball(center, 0.2, [color[0], color[1], color[2], 255])], &cam, &opts);

        assert_eq!(img.pixel(100, 80), color);
        // Far away (> 3σ = 60 px): untouched background.
        assert_eq!(img.pixel(100 + 62, 80), [0, 0, 255]);
        assert_eq!(img.pixel(100, 80 - 62), [0, 0, 255]);
        assert_eq!(img.pixel(5, 5), [0, 0, 255]);
        // Monotonic falloff from the center in all four directions.
        for (sx, sy) in [(1i32, 0i32), (-1, 0), (0, 1), (0, -1)] {
            let mut prev = img.pixel(100, 80);
            for step in 1..70 {
                let p = img.pixel((100 + sx * step) as u32, (80 + sy * step) as u32);
                assert!(p[0] <= prev[0] && p[1] <= prev[1] && p[2] >= prev[2], "step {step}: {prev:?} -> {p:?}");
                prev = p;
            }
            assert_eq!(prev, [0, 0, 255]);
        }
        // Symmetry.
        assert_eq!(img.pixel(100 + 15, 80), img.pixel(100 - 15, 80));
        assert_eq!(img.pixel(100, 80 + 15), img.pixel(100, 80 - 15));
    }

    #[test]
    fn alpha_matches_hand_computed_values() {
        let cam = camera();
        let bg = [20.0f32, 40.0, 60.0];
        let opts = RasterOptions { background: [20, 40, 60], ..Default::default() };
        // σ = 0.05 m at 2 m ⇒ variance 25 px² + low-pass, and the amplitude
        // scaled by the ratio of the areas, 25 / 25.3.
        let center = cam.unproject(100.5, 80.5, 2.0);
        let var = 25.0 + LOW_PASS_PX2;
        let gain = 25.0 / var;
        let color = [250.0f32, 10.0, 130.0];
        let opacity = 204u8; // 0.8
        let img = rasterize(&[ball(center, 0.05, [250, 10, 130, opacity])], &cam, &opts);
        for (dx, dy) in [(0i32, 0i32), (3, 0), (0, -4), (5, 5), (-7, 2), (10, -9), (14, 0), (16, 0), (0, 15), (0, 16)] {
            let m = (dx * dx + dy * dy) as f32 / var;
            let mut alpha = gain * opacity as f32 / 255.0 * (-0.5 * m).exp();
            if m > 9.0 || alpha < 1.0 / 255.0 {
                alpha = 0.0;
            }
            let expected = [0, 1, 2].map(|c| color[c] * alpha + bg[c] * (1.0 - alpha));
            let got = img.pixel((100 + dx) as u32, (80 + dy) as u32);
            assert_close(got, expected, 0.75, &format!("offset ({dx}, {dy})"));
        }
        // 3σ = 15.09 px: the pixel 16 px away is exactly the background.
        assert_eq!(img.pixel(116, 80), [20, 40, 60]);
        assert_ne!(img.pixel(115, 80), [20, 40, 60]);
    }

    #[test]
    fn two_translucent_layers_composite_over() {
        let cam = camera();
        let opts = RasterOptions { background: [0, 0, 0], ..Default::default() };
        let near = ball(cam.unproject(100.5, 80.5, 1.0), 0.1, [255, 0, 0, 128]);
        let far = ball(cam.unproject(100.5, 80.5, 3.0), 0.3, [0, 0, 255, 128]);
        let a = 128.0 / 255.0;
        // Back to front: blue over black, then red over that.
        let expected = [255.0 * a, 0.0, 255.0 * a * (1.0 - a)];
        for splats in [[near, far], [far, near]] {
            let img = rasterize(&splats, &cam, &opts);
            assert_close(img.pixel(100, 80), expected, 0.75, "center");
        }
    }

    #[test]
    fn overlapping_opaque_splats_respect_depth_order() {
        let cam = camera();
        let opts = RasterOptions::default();
        let red_near = ball(cam.unproject(90.5, 80.5, 1.5), 0.1, [255, 0, 0, 255]);
        let blue_far = ball(cam.unproject(110.5, 80.5, 2.5), 0.2, [0, 0, 255, 255]);
        for splats in [[red_near, blue_far], [blue_far, red_near]] {
            let img = rasterize(&splats, &cam, &opts);
            // At the center of the near splat only red survives.
            assert_eq!(img.pixel(90, 80), [255, 0, 0]);
            // At the center of the far splat, red is drawn on top of blue.
            let p = img.pixel(110, 80);
            let d2: f32 = 20.0 * 20.0;
            let alpha_red = (-0.5 * d2 / ((200.0f32 * 0.1 / 1.5).powi(2) + LOW_PASS_PX2)).exp();
            assert_close(p, [255.0 * alpha_red, 0.0, 255.0 * (1.0 - alpha_red)], 0.75, "far center");
        }
        // Swap the depths: now blue hides red at red's center.
        let red_far = ball(cam.unproject(90.5, 80.5, 2.5), 0.1, [255, 0, 0, 255]);
        let blue_near = ball(cam.unproject(90.5, 80.5, 1.5), 0.2, [0, 0, 255, 255]);
        assert_eq!(rasterize(&[blue_near, red_far], &cam, &opts).pixel(90, 80), [0, 0, 255]);
    }

    #[test]
    fn invisible_and_out_of_view_splats_are_ignored() {
        let cam = camera();
        let opts = RasterOptions { background: [7, 7, 7], ..Default::default() };
        let splats = [
            ball(Vec3::new(0.0, 0.0, 2.0), 0.2, [255; 4]),            // behind
            ball(Vec3::new(0.0, 0.0, -0.01), 0.2, [255; 4]),          // closer than near
            ball(Vec3::new(50.0, 0.0, -2.0), 0.2, [255; 4]),          // far to the right
            ball(Vec3::new(0.0, 0.0, -2.0), 0.2, [255, 255, 255, 0]), // fully transparent
            ball(Vec3::new(f32::NAN, 0.0, -2.0), 0.2, [255; 4]),      // broken
            ball(Vec3::new(0.0, 0.0, -2.0), f32::NAN, [255; 4]),      // broken
            ball(Vec3::new(0.0, 0.0, -2.0), f32::INFINITY, [255; 4]), // broken
        ];
        assert_eq!(rasterize(&splats, &cam, &opts), Image::new(200, 160, [7, 7, 7]));
    }

    #[test]
    fn splat_partially_off_screen_is_clipped() {
        let cam = camera();
        // Centered on the top-left corner.
        let s = ball(cam.unproject(0.0, 0.0, 2.0), 0.3, [255, 255, 255, 255]);
        let img = rasterize(&[s], &cam, &RasterOptions::default());
        assert!(img.pixel(0, 0)[0] > 250);
        assert_eq!(img.pixel(199, 159), [0, 0, 0]);
        // A splat much larger than the screen covers every pixel.
        let huge = ball(cam.unproject(100.0, 80.0, 1.0), 50.0, [9, 99, 199, 255]);
        let img = rasterize(&[huge], &cam, &RasterOptions::default());
        assert!(img.rgb.chunks_exact(3).all(|p| p == [9, 99, 199]));
    }

    #[test]
    fn splat_scale_option_shrinks_footprints() {
        let cam = camera();
        let s = ball(cam.unproject(100.5, 80.5, 2.0), 0.1, [255, 255, 255, 255]);
        let full = rasterize(&[s], &cam, &RasterOptions::default());
        let half = rasterize(&[s], &cam, &RasterOptions { splat_scale: 0.5, ..Default::default() });
        // The centers differ only by the gain of the low-pass: 100 / 100.3
        // against 25 / 25.3.
        assert_close(full.pixel(100, 80), [255.0 * 100.0 / 100.3; 3], 0.75, "full center");
        assert_close(half.pixel(100, 80), [255.0 * 25.0 / 25.3; 3], 0.75, "half center");
        // 10 px = 1σ at full size, 2σ at half size.
        assert_close(full.pixel(110, 80), [255.0 * 100.0 / 100.3 * (-0.5 * 100.0 / 100.3f32).exp(); 3], 0.75, "full");
        assert_close(half.pixel(110, 80), [255.0 * 25.0 / 25.3 * (-0.5 * 100.0 / 25.3f32).exp(); 3], 0.75, "half");
    }

    /// Sum of the red channel over the image, in units of full pixels.
    fn energy(img: &Image) -> f32 {
        img.rgb.chunks_exact(3).map(|p| p[0] as f32 / 255.0).sum()
    }

    #[test]
    fn low_pass_conserves_the_energy_of_small_splats() {
        let cam = camera();
        // σ from a tenth of a pixel to five pixels: the picture of the splat
        // integrates to its true area 2πσ², wherever its center falls
        // between the pixel centers.
        for sigma_px in [0.1f32, 0.25, 0.5, 1.0, 2.0, 5.0] {
            let exact = std::f32::consts::TAU * sigma_px * sigma_px;
            let (mut lo, mut hi) = (f32::INFINITY, 0.0f32);
            for k in 0..16 {
                let center = cam.unproject(100.0 + (k % 4) as f32 / 4.0, 80.0 + (k / 4) as f32 / 4.0, 2.0);
                let e = energy(&rasterize(&[ball(center, sigma_px * 0.01, [255; 4])], &cam, &RasterOptions::default()));
                (lo, hi) = (lo.min(e), hi.max(e));
            }
            // 8-bit output: a faint splat loses up to half a level per pixel.
            let slack = 0.06 * exact + 0.03;
            assert!(lo > exact - slack && hi < exact + slack, "σ = {sigma_px} px: {lo}..{hi} vs {exact}");
            // No shimmer: the sum barely depends on the sub-pixel position.
            assert!(hi - lo < 0.05 * exact + 0.03, "σ = {sigma_px} px: {lo}..{hi}");
        }
        // The plain dilation inflates a sub-pixel splat to the area of the
        // filter: 2π·(0.01 + 0.3) px² instead of 2π·0.01.
        let tiny = ball(cam.unproject(100.5, 80.5, 2.0), 0.001, [255; 4]);
        let classic = energy(&rasterize(&[tiny], &cam, &RasterOptions { classic_dilation: true, ..Default::default() }));
        assert!(classic > 25.0 * std::f32::consts::TAU * 0.01, "{classic}");
    }

    #[test]
    fn point_mode_draws_fixed_size_dots() {
        let cam = camera();
        let opts = RasterOptions { point_mode: true, point_sigma_px: 1.0, ..Default::default() };
        // Very different world sizes and depths, same dot on screen.
        let a = ball(cam.unproject(50.5, 80.5, 1.0), 0.5, [255, 255, 255, 255]);
        let b = ball(cam.unproject(150.5, 80.5, 6.0), 0.001, [255, 255, 255, 255]);
        let img = rasterize(&[a, b], &cam, &opts);
        for cx in [50u32, 150] {
            assert_eq!(img.pixel(cx, 80), [255; 3]);
            assert_close(img.pixel(cx + 1, 80), [255.0 * (-0.5f32).exp(); 3], 0.75, "1 px");
            assert_close(img.pixel(cx, 82), [255.0 * (-2.0f32).exp(); 3], 0.75, "2 px");
            assert_eq!(img.pixel(cx + 4, 80), [0; 3]);
        }
        let lit = img.rgb.chunks_exact(3).filter(|p| p[0] > 0).count();
        assert!(lit < 2 * 40, "{lit} lit pixels");
    }

    #[test]
    fn dense_surfel_wall_is_solid() {
        // A wall of opaque surfels with the spacing used by the reconstruction
        // must not show the background through.
        let cam = camera();
        let mut splats = Vec::new();
        let step = 0.02;
        for iy in -60..=60 {
            for ix in -60..=60 {
                let pos = Vec3::new(ix as f32 * step, iy as f32 * step, -2.0);
                splats.push(Splat::surfel(pos, Vec3::Z, 0.7 * step, 0.1 * step, [180, 90, 30, 255]));
            }
        }
        let opts = RasterOptions { background: [0, 255, 0], ..Default::default() };
        let img = rasterize(&splats, &cam, &opts);
        for y in (10..150).step_by(7) {
            for x in (10..190).step_by(7) {
                let p = img.pixel(x, y);
                assert!(p[1].abs_diff(90) <= 2 && p[0].abs_diff(180) <= 2, "hole at ({x}, {y}): {p:?}");
            }
        }
    }

    #[test]
    fn image_metrics() {
        let a = Image::new(8, 4, [100, 100, 100]);
        assert_eq!(psnr(&a, &a), f32::INFINITY);
        assert_eq!(mean_abs_error(&a, &a), 0.0);
        let b = Image::new(8, 4, [101, 99, 100]);
        // MSE = 2/3 ⇒ 10·log10(255² · 3/2).
        assert!((psnr(&a, &b) - 49.892).abs() < 1e-2, "{}", psnr(&a, &b));
        assert!((mean_abs_error(&a, &b) - 2.0 / 3.0).abs() < 1e-6);
        let c = Image::new(8, 4, [0, 0, 0]);
        let d = Image::new(8, 4, [255, 255, 255]);
        assert!(psnr(&c, &d).abs() < 1e-4);
        assert_eq!(mean_abs_error(&c, &d), 255.0);
        assert_eq!(psnr(&Image::default(), &Image::default()), f32::INFINITY);
    }

    #[test]
    #[should_panic(expected = "image sizes differ")]
    fn metrics_reject_mismatched_sizes() {
        psnr(&Image::new(2, 2, [0; 3]), &Image::new(2, 3, [0; 3]));
    }

    #[test]
    fn image_from_frame_copies_the_color() {
        let mut frame = RgbdFrame::new(3, 2);
        frame.rgb[3 * 4..3 * 5].copy_from_slice(&[1, 2, 3]);
        let img = Image::from(&frame);
        assert_eq!((img.width, img.height), (3, 2));
        assert_eq!(img.pixel(1, 1), [1, 2, 3]);
        assert_eq!(img.pixel(0, 0), [0, 0, 0]);
    }

    /// Intensity-weighted centroid of one channel, in pixel coordinates.
    fn centroid(img: &Image, channel: usize) -> (f32, f32) {
        let (mut sx, mut sy, mut sw) = (0.0f64, 0.0f64, 0.0f64);
        for y in 0..img.height {
            for x in 0..img.width {
                let v = img.pixel(x, y)[channel] as f64;
                sx += v * (x as f64 + 0.5);
                sy += v * (y as f64 + 0.5);
                sw += v;
            }
        }
        ((sx / sw) as f32, (sy / sw) as f32)
    }

    #[test]
    fn image_is_neither_mirrored_nor_upside_down() {
        // World +X is right of the camera, world +Y is up: red must land on
        // the right half, green on the top half (small v).
        let cam = camera();
        let red = ball(Vec3::new(0.5, 0.0, -2.0), 0.03, [255, 0, 0, 255]);
        let green = ball(Vec3::new(0.0, 0.5, -2.0), 0.03, [0, 255, 0, 255]);
        let img = rasterize(&[red, green], &cam, &RasterOptions::default());
        let (rx, ry) = centroid(&img, 0);
        let (gx, gy) = centroid(&img, 1);
        assert!((rx - 150.0).abs() < 0.2 && (ry - 80.0).abs() < 0.2, "red at ({rx}, {ry})");
        assert!((gx - 100.0).abs() < 0.2 && (gy - 30.0).abs() < 0.2, "green at ({gx}, {gy})");

        // Any camera: a small splat is drawn where `Camera::project` says.
        let mut rng = Rng::new(50);
        let mut checked = 0;
        for _ in 0..40 {
            let eye = Vec3::new(rng.range(-3.0, 3.0), rng.range(0.2, 2.5), rng.range(-3.0, 3.0));
            let cam = Camera::look_at(eye, Vec3::new(0.0, 0.8, 0.0), Vec3::Y, 1.0, 320, 240);
            let p = Vec3::new(rng.range(-0.4, 0.4), rng.range(0.5, 1.1), rng.range(-0.4, 0.4));
            let Some((px, py, _)) = cam.project(p) else { continue };
            if !(20.0..300.0).contains(&px) || !(20.0..220.0).contains(&py) {
                continue;
            }
            checked += 1;
            let img = rasterize(&[ball(p, 0.01, [255; 4])], &cam, &RasterOptions::default());
            let (cx, cy) = centroid(&img, 0);
            assert!((cx - px).abs() < 0.25 && (cy - py).abs() < 0.25, "({cx}, {cy}) vs ({px}, {py})");
        }
        assert!(checked > 20);
    }

    #[test]
    fn needle_keeps_its_orientation_on_screen() {
        // A needle along world (1, 1, 0) runs from bottom-left to top-right.
        let cam = camera();
        let needle = Splat {
            pos: [0.0, 0.0, -2.0],
            scale: [0.3, 0.005, 0.005],
            rot: Quat::from_rotation_z(45f32.to_radians()).to_array(),
            rgba: [255; 4],
        };
        let img = rasterize(&[needle], &cam, &RasterOptions::default());
        // Half a pixel thin: the low-pass spreads it over a pixel and dims it
        // to about two thirds (step 6), 0.77 of which is left 22 px out.
        assert!(img.pixel(100 + 15, 80 - 16)[0] > 110, "{:?}", img.pixel(115, 64));
        assert!(img.pixel(100 - 16, 80 + 15)[0] > 110, "{:?}", img.pixel(84, 95));
        assert_eq!(img.pixel(100 + 15, 80 + 15), [0, 0, 0]);
        assert_eq!(img.pixel(100 - 15, 80 - 15), [0, 0, 0]);
    }

    /// Completely independent renderer: f64, exact depth sort, numeric
    /// Jacobian (`reference_projection`), conic form of the exponent.
    fn brute_force(splats: &[Splat], cam: &Camera, background: [u8; 3]) -> Vec<f64> {
        let view = cam.view_matrix();
        let mut items = Vec::new();
        for s in splats {
            let Some((center, cov, depth)) = reference_projection(s, &view, [cam.fx, cam.fy, cam.cx, cam.cy]) else {
                continue;
            };
            if depth >= DEFAULT_NEAR as f64 {
                let lp = LOW_PASS_PX2 as f64;
                let filtered = [cov[0] + lp, cov[1], cov[2] + lp];
                // Energy-conserving low-pass: the opacity shrinks by the
                // ratio of the areas before and after the dilation.
                let area = |c: [f64; 3]| (c[0] * c[2] - c[1] * c[1]).max(0.0).sqrt();
                let mut rgba = s.rgba.map(f64::from);
                rgba[3] *= area(cov) / area(filtered);
                items.push((depth, center, filtered, rgba));
            }
        }
        items.sort_by(|a, b| b.0.total_cmp(&a.0));
        let mut out = Vec::with_capacity(cam.width as usize * cam.height as usize * 3);
        for y in 0..cam.height {
            for x in 0..cam.width {
                let mut pixel = background.map(f64::from);
                for (_, center, [a, b, c], rgba) in &items {
                    let (dx, dy) = (x as f64 + 0.5 - center[0], y as f64 + 0.5 - center[1]);
                    let m = (c * dx * dx - 2.0 * b * dx * dy + a * dy * dy) / (a * c - b * b);
                    let alpha = rgba[3] / 255.0 * (-0.5 * m).exp();
                    if m <= 9.0 && alpha >= 1.0 / 255.0 {
                        for k in 0..3 {
                            pixel[k] = rgba[k] * alpha + pixel[k] * (1.0 - alpha);
                        }
                    }
                }
                out.extend(pixel);
            }
        }
        out
    }

    #[test]
    fn matches_an_independent_brute_force_renderer() {
        let mut rng = Rng::new(51);
        for round in 0..4 {
            let eye = Vec3::new(rng.range(-2.0, 2.0), rng.range(0.5, 2.0), rng.range(2.0, 3.0));
            let cam = Camera::look_at(eye, Vec3::new(0.0, 0.5, 0.0), Vec3::Y, rng.range(0.7, 1.2), 96, 72);
            // Anisotropic, rotated, translucent splats in front of the camera.
            let splats: Vec<Splat> = (0..150)
                .map(|_| {
                    let mut s = rng.splat(1.0, 0.005, 0.15);
                    s.pos[1] = 0.5 * s.pos[1] + 0.5;
                    s
                })
                .collect();
            let opts = RasterOptions { background: [10, 20, 30], ..Default::default() };
            let img = rasterize(&splats, &cam, &opts);
            let reference = brute_force(&splats, &cam, opts.background);
            // A pixel sitting exactly on a cut-off (3σ or 1/255) may fall on
            // either side; everything else agrees to rounding.
            let off = img.rgb.iter().zip(&reference).filter(|(&a, &b)| (a as f64 - b).abs() > 1.0).count();
            assert!(off <= 6, "round {round}: {off} channels differ from the reference");
            assert!(img.rgb.chunks_exact(3).filter(|p| *p != [10, 20, 30]).count() > 500, "round {round}: empty");
        }
    }

    #[test]
    fn extreme_needle_stays_within_the_legal_color_range() {
        // Regression: for this 1.2 m × 15 µm needle 25 cm from the camera the
        // conic form of the exponent cancelled to negative values, giving
        // alpha > opacity (pixels brighter than any legal blend).
        let mut cam = Camera::look_at(Vec3::ZERO, Vec3::NEG_Z, Vec3::Y, 1.0, 320, 240);
        cam.fx = 400.0;
        cam.fy = 400.0;
        let needle = Splat {
            pos: [-0.139_320_61, -0.280_621_4, -0.252_231_84],
            scale: [1.194_127_1, 1.439_553_9e-5, 2.067_921_6e-5],
            rot: [0.513_172, -0.087_027_34, -0.589_052_4, -0.618_140_8],
            rgba: [255, 255, 255, 128],
        };
        let opts = RasterOptions { background: [100, 100, 100], ..Default::default() };
        let img = rasterize(&[needle], &cam, &opts);
        // White at opacity 128/255 over grey 100: at most 100 + 155·0.502 = 177.8.
        let (min, max) = img.rgb.iter().fold((255u8, 0u8), |(lo, hi), &v| (lo.min(v), hi.max(v)));
        assert!(min == 100 && max <= 178, "range {min}..={max}");
        // It is a few hundredths of a pixel wide: the energy-conserving
        // low-pass leaves it a few grey levels at most, and at this aspect
        // ratio (80 000 : 1) its width, hence its gain, is rounding noise
        // (see "Numerical limits" in `crate::splat`). So only the bound
        // above is asserted for it. With the plain dilation it is drawn as
        // before: a pixel-wide line crossing the image, not a blob.
        let classic = rasterize(&[needle], &cam, &RasterOptions { classic_dilation: true, ..opts });
        let (min, max) = classic.rgb.iter().fold((255u8, 0u8), |(lo, hi), &v| (lo.min(v), hi.max(v)));
        assert!(min == 100 && max <= 178 && max > 150, "range {min}..={max}");
        let lit = classic.rgb.chunks_exact(3).filter(|p| p[0] > 110).count();
        assert!((100..4000).contains(&lit), "{lit} lit pixels");
    }

    #[test]
    fn degenerate_inputs_do_not_panic() {
        let one = [ball(Vec3::new(0.0, 0.0, -1.0), 0.5, [255; 4])];
        for (w, h) in [(0, 0), (0, 5), (5, 0), (1, 1), (1, 7), (7, 1)] {
            let mut cam = camera();
            (cam.width, cam.height) = (w, h);
            assert_eq!(rasterize(&one, &cam, &RasterOptions::default()).rgb.len(), (w * h * 3) as usize);
        }
        let unit = [0.0, 0.0, 0.0, 1.0];
        let odd = [
            Splat { pos: [0.0, 0.0, -1.0], scale: [0.0; 3], rot: [0.0; 4], rgba: [255; 4] },
            Splat { pos: [0.0, 0.0, -1.0], scale: [-0.1, 0.1, 0.1], rot: unit, rgba: [255; 4] },
            Splat { pos: [0.0, 0.0, -1.0], scale: [f32::MAX; 3], rot: unit, rgba: [255; 4] },
            Splat { pos: [f32::MAX, f32::MAX, -f32::MAX], scale: [1.0; 3], rot: unit, rgba: [255; 4] },
            Splat { pos: [1e20, 0.0, -1.0], scale: [1e19; 3], rot: unit, rgba: [255; 4] },
            Splat { pos: [0.0, 0.0, -1.0], scale: [1e-30; 3], rot: [1e-30, 0.0, 0.0, 1e-30], rgba: [255; 4] },
        ];
        let d = RasterOptions::default();
        for opts in [
            d,
            RasterOptions { splat_scale: 0.0, ..d },
            RasterOptions { splat_scale: f32::NAN, ..d },
            RasterOptions { splat_scale: f32::INFINITY, ..d },
            RasterOptions { splat_scale: -1.0, ..d },
            RasterOptions { point_mode: true, point_sigma_px: 0.0, ..d },
            RasterOptions { point_mode: true, point_sigma_px: f32::NAN, ..d },
            RasterOptions { point_mode: true, point_sigma_px: -2.0, ..d },
            RasterOptions { point_mode: true, point_sigma_px: 1e9, ..d },
            RasterOptions { near: -1.0, ..d },
            RasterOptions { near: f32::NAN, ..d },
        ] {
            assert_eq!(rasterize(&odd, &camera(), &opts).rgb.len(), 200 * 160 * 3, "{opts:?}");
        }
        // A zero-scale splat has no area and therefore no energy; with the
        // plain dilation it is a (low-pass sized) dot of its color.
        let point = [Splat { pos: [0.0, 0.0, -1.0], ..odd[0] }];
        assert!(rasterize(&point, &camera(), &d).rgb.iter().all(|&v| v == 0));
        let img = rasterize(&point, &camera(), &RasterOptions { classic_dilation: true, ..d });
        assert!(img.pixel(100, 80)[0] > 100 && img.pixel(104, 80) == [0; 3]);
        // Nonsensical intrinsics.
        for (fx, cx) in [(0.0, 100.0), (f32::NAN, f32::INFINITY), (-200.0, 100.0)] {
            let mut cam = camera();
            (cam.fx, cam.cx) = (fx, cx);
            assert_eq!(rasterize(&odd, &cam, &d).rgb.len(), 200 * 160 * 3);
        }
    }

    #[test]
    fn surfel_wall_hides_what_is_behind_it_at_oblique_angles() {
        let step = 0.02;
        let mut splats = Vec::new();
        for iy in -40..=40 {
            for ix in -40..=40 {
                let pos = Vec3::new(ix as f32 * step, iy as f32 * step, 0.0);
                splats.push(Splat::surfel(pos, Vec3::Z, 0.75 * step, 0.1 * step, [100, 100, 100, 255]));
            }
        }
        splats.push(ball(Vec3::new(0.0, 0.0, -0.3), 0.1, [255, 0, 0, 255]));
        for angle in [0.0f32, 0.5, 1.0, 1.3] {
            let eye = Vec3::new(angle.sin() * 2.0, 0.0, angle.cos() * 2.0);
            let cam = Camera::look_at(eye, Vec3::ZERO, Vec3::Y, 0.5, 160, 120);
            let p = rasterize(&splats, &cam, &RasterOptions::default()).pixel(80, 60);
            assert!(p[0] <= 102 && p[1] >= 98, "angle {angle}: the red ball shows through: {p:?}");
        }
    }

    fn random_cloud(n: usize, seed: u64) -> Vec<Splat> {
        let mut rng = Rng::new(seed);
        (0..n)
            .map(|_| {
                let mut s = rng.splat(1.5, 0.005, 0.08);
                s.pos[2] -= 3.0;
                s
            })
            .collect()
    }

    #[test]
    fn rasterization_is_deterministic() {
        let splats = random_cloud(20_000, 3);
        let a = rasterize(&splats, &camera(), &RasterOptions::default());
        let b = rasterize(&splats, &camera(), &RasterOptions::default());
        assert_eq!(a, b);
        assert!(a.rgb.iter().any(|&v| v > 0));
    }

    /// The banded multi-threaded path must reproduce the serial one exactly.
    #[cfg(feature = "par")]
    #[test]
    fn parallel_compositing_is_bit_identical_to_serial() {
        let cam = camera();
        let opts = RasterOptions { background: [30, 60, 90], ..Default::default() };
        let mut splats = random_cloud(30_000, 4);
        // A few screen-filling splats crossing every band.
        splats.push(ball(Vec3::new(0.0, 0.0, -6.0), 2.0, [255, 0, 255, 90]));
        splats.push(ball(Vec3::new(0.3, -0.2, -1.2), 0.6, [0, 255, 0, 40]));
        let parallel = rasterize(&splats, &cam, &opts);

        let vp = ViewParams { near: opts.near, splat_scale: opts.splat_scale, ..ViewParams::from_camera(&cam) };
        let draws: Vec<Draw> = splats.iter().filter_map(|s| prepare(s, &vp, &opts)).collect();
        let mut order = Vec::new();
        DepthSorter::new().sort_by_depth(draws.iter().map(|d| d.depth), &mut order);
        let mut acc: Vec<f32> = (0..200 * 160).flat_map(|_| [30.0, 60.0, 90.0]).collect();
        composite_serial(&mut acc, 200, &draws, &order);
        let serial: Vec<u8> = acc.iter().map(|&v| (v + 0.5) as u8).collect();
        assert!(parallel.rgb == serial, "parallel and serial outputs differ");
    }

    /// Timing report. Run with
    /// `cargo test -p gs-core --release raster::tests::timing -- --ignored --nocapture`
    /// (add `--features par` for the multi-threaded path).
    #[test]
    #[ignore = "timing report, run in --release"]
    fn timing() {
        let cam = Camera::look_at(Vec3::new(0.0, 0.0, 4.0), Vec3::ZERO, Vec3::Y, 1.0, 640, 480);
        let mut rng = Rng::new(99);
        let splats: Vec<Splat> = (0..1_000_000).map(|_| rng.splat(1.5, 0.002, 0.008)).collect();
        let start = web_time::Instant::now();
        let img = rasterize(&splats, &cam, &RasterOptions::default());
        let ms = start.elapsed().as_secs_f64() * 1e3;
        assert!(img.rgb.iter().any(|&v| v > 0));
        println!("rasterize 1M small splats at 640x480: {ms:.0} ms (par = {})", cfg!(feature = "par"));
    }
}
