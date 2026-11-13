//! CPU reference Gaussian splat rasterizer.
//!
//! Implements exactly the algorithm documented in [`crate::splat`]: every
//! splat is projected with [`project_splat`], the visible ones are sorted back
//! to front with [`DepthSorter`], and composited with "over" directly on sRGB
//! values. It is the ground truth the WebGL renderer is compared against, and
//! it produces the images for the tests (PSNR against the ray-cast scene) —
//! CI has no GPU.
//!
//! With the `par` feature the work is spread over image bands with rayon; the
//! output is bit-identical to the single-threaded path.

#[cfg(feature = "par")]
use rayon::prelude::*;

use crate::sort::DepthSorter;
use crate::splat::{cov6, project_splat, ViewParams, CUTOFF_MAHALANOBIS2, DEFAULT_NEAR, MIN_ALPHA};
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
}

impl Default for RasterOptions {
    fn default() -> Self {
        Self { background: [0, 0, 0], splat_scale: 1.0, point_mode: false, point_sigma_px: 0.8, near: DEFAULT_NEAR }
    }
}

/// One visible splat, ready to be composited.
#[derive(Clone, Copy)]
struct Draw {
    px: f32,
    py: f32,
    conic: [f32; 3],
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
    let opacity = splat.rgba[3] as f32 / 255.0;
    if opacity < MIN_ALPHA {
        return None;
    }
    let mut p = project_splat(splat.position(), &cov6(splat), vp)?;
    if opts.point_mode {
        p = p.with_point_footprint(opts.point_sigma_px);
    }
    // Alpha drops below 1/255 where the Mahalanobis distance² exceeds
    // 2·ln(255·opacity), so faint splats need less than the full 3σ box.
    let reach2 = (2.0 * (opacity / MIN_ALPHA).ln()).min(CUTOFF_MAHALANOBIS2);
    // Axis-aligned bounds of the ellipse δᵀ·conic·δ ≤ reach²: ±√(reach²·Σ'ₖₖ).
    let ex = (reach2 * p.cov[0]).sqrt();
    let ey = (reach2 * p.cov[2]).sqrt();
    // Pixel i has its center at i + 0.5. Saturating float → int casts clip
    // the rectangle to the image (and send NaN to an empty one).
    let x0 = (p.px - ex - 0.5).ceil().max(0.0) as u32;
    let y0 = (p.py - ey - 0.5).ceil().max(0.0) as u32;
    let x1 = ((p.px + ex - 0.5).floor() + 1.0).clamp(0.0, vp.width) as u32;
    let y1 = ((p.py + ey - 0.5).floor() + 1.0).clamp(0.0, vp.height) as u32;
    if x0 >= x1 || y0 >= y1 {
        return None;
    }
    Some(Draw {
        px: p.px,
        py: p.py,
        conic: p.conic,
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
        let [qa, qb, qc] = d.conic;
        for y in d.y0.max(row0)..d.y1.min(row_end) {
            let dy = y as f32 + 0.5 - d.py;
            let start = (y - row0) as usize * stride;
            let row = &mut acc[start + d.x0 as usize * 3..start + d.x1 as usize * 3];
            for (i, dst) in row.chunks_exact_mut(3).enumerate() {
                let dx = (d.x0 as usize + i) as f32 + 0.5 - d.px;
                // Steps 9–11 of the algorithm in `crate::splat`
                // (`splat::eval_alpha`, spelled out for the inner loop).
                let m = qa * dx * dx + 2.0 * qb * dx * dy + qc * dy * dy;
                // False for NaN too.
                if m <= CUTOFF_MAHALANOBIS2 {
                    let alpha = d.opacity * (-0.5 * m).exp();
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
    use crate::splat::test_util::Rng;
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
        // σ = 0.05 m at 2 m ⇒ variance 25 px² + low-pass.
        let center = cam.unproject(100.5, 80.5, 2.0);
        let var = 25.0 + LOW_PASS_PX2;
        let color = [250.0f32, 10.0, 130.0];
        let opacity = 204u8; // 0.8
        let img = rasterize(&[ball(center, 0.05, [250, 10, 130, opacity])], &cam, &opts);
        for (dx, dy) in [(0i32, 0i32), (3, 0), (0, -4), (5, 5), (-7, 2), (10, -9), (14, 0), (16, 0), (0, 15), (0, 16)] {
            let m = (dx * dx + dy * dy) as f32 / var;
            let mut alpha = opacity as f32 / 255.0 * (-0.5 * m).exp();
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
        assert_eq!(full.pixel(100, 80), half.pixel(100, 80));
        // 10 px = 1σ at full size, 2σ at half size.
        assert_close(full.pixel(110, 80), [255.0 * (-0.5 * 100.0 / 100.3f32).exp(); 3], 0.75, "full");
        assert_close(half.pixel(110, 80), [255.0 * (-0.5 * 100.0 / 25.3f32).exp(); 3], 0.75, "half");
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
