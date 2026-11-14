//! Image-quality instruments of the quality lab (step 0 of the experiment
//! plan in `docs/RECHERCHE_QUALITE.md`).
//!
//! Everything is computed against the ray-cast reference of the same view:
//!
//! * [`classify`] cuts the reference into four regions: never observed by a
//!   sensor, a band around depth discontinuities (*silhouette*), a band around
//!   colour edges away from silhouettes (*texture*), and the rest
//!   (*interior*);
//! * [`score`] gives PSNR (whole image and observed pixels), SSIM, coverage,
//!   the squared error of every region, and a measure of the high-frequency
//!   noise on flat regions (the "dotted" look of visible splat discs);
//! * [`Summary`] aggregates the scores of several views.

use crate::raster::Image;
use crate::types::RgbdFrame;

/// Region index: neither near a depth discontinuity nor a colour edge.
pub const INTERIOR: usize = 0;
/// Region index: band around colour edges of the reference, away from
/// silhouettes.
pub const TEXTURE: usize = 1;
/// Region index: band around depth discontinuities of the reference.
pub const SILHOUETTE: usize = 2;
/// Region index: the surface point shown was never seen by any sensor.
pub const UNOBSERVED: usize = 3;
/// Names of the regions, by index.
pub const REGION_NAMES: [&str; 4] = ["interior", "texture", "silhouette", "unobserved"];

/// Parameters of [`classify`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MaskParams {
    /// Half-width of the silhouette and texture bands in pixels.
    pub band_px: u32,
    /// Two neighbouring pixels are separated by a depth discontinuity when
    /// their depths differ by more than this fraction (and the step does not
    /// continue a plane seen at a grazing angle).
    pub depth_step: f32,
    /// Two neighbouring pixels are separated by a colour edge when one of
    /// their channels differs by more than this many 8-bit levels.
    pub color_step: u8,
}

impl Default for MaskParams {
    fn default() -> Self {
        Self { band_px: 2, depth_step: 0.03, color_step: 16 }
    }
}

impl MaskParams {
    /// Bands of constant width *on the scene*: `band_px` pixels in a
    /// 320-pixel-wide view, proportionally more in a larger one. A silhouette
    /// displaced by one sensor pixel then falls into the band whatever the
    /// resolution of the evaluation view.
    pub fn scaled_to(self, width: u32) -> Self {
        Self { band_px: (self.band_px * width).div_ceil(320).max(1), ..self }
    }
}

/// Are two neighbouring pixels with the inverse depths `a` and `b` (`0.0` =
/// no surface) on different surfaces? `before` and `after` are the valid
/// inverse depths of the pixels that continue the line on either side.
///
/// A step larger than `rel` is not a discontinuity when it repeats on either
/// side: the inverse depth of a plane is affine in the pixel coordinates, so
/// a plane at a grazing angle steps by the same amount from pixel to pixel.
pub fn is_depth_step(a: f32, b: f32, before: Option<f32>, after: Option<f32>, rel: f32) -> bool {
    if a <= 0.0 || b <= 0.0 {
        return a > 0.0 || b > 0.0;
    }
    if (a - b).abs() <= rel * a.max(b) {
        return false;
    }
    let tolerance = 0.35 * rel * a.max(b);
    let step = b - a;
    let repeats = |outer: Option<f32>| outer.is_some_and(|o| (o - step).abs() <= tolerance);
    !(repeats(before.map(|w| a - w)) || repeats(after.map(|w| w - b)))
}

/// Pixels next to a depth discontinuity of a depth map (`0.0` = no surface),
/// as [`is_depth_step`] defines it.
pub fn depth_edges(depth: &[f32], width: usize, height: usize, rel: f32) -> Vec<bool> {
    let inv: Vec<f32> = depth.iter().map(|&z| if z > 0.0 && z.is_finite() { 1.0 / z } else { 0.0 }).collect();
    let mut edges = vec![false; width * height];
    // `i` and `j = i + stride` are neighbours; `before`/`after` continue the line.
    let valid = |k: Option<usize>| k.map(|k| inv[k]).filter(|&w| w > 0.0);
    // `i` and `j = i + stride` are neighbours; `before`/`after` continue the line.
    let mut check = |i: usize, j: usize, before: Option<usize>, after: Option<usize>| {
        if is_depth_step(inv[i], inv[j], valid(before), valid(after), rel) {
            edges[i] = true;
            edges[j] = true;
        }
    };
    for y in 0..height {
        for x in 0..width {
            let i = y * width + x;
            if x + 1 < width {
                check(i, i + 1, (x > 0).then(|| i - 1), (x + 2 < width).then(|| i + 2));
            }
            if y + 1 < height {
                check(i, i + width, (y > 0).then(|| i - width), (y + 2 < height).then(|| i + 2 * width));
            }
        }
    }
    edges
}

/// Pixels next to a colour edge: a 4-neighbour differs by more than `step`
/// levels in some channel.
pub fn color_edges(rgb: &[u8], width: usize, height: usize, step: u8) -> Vec<bool> {
    let mut edges = vec![false; width * height];
    let differs = |i: usize, j: usize| (0..3).any(|c| rgb[i * 3 + c].abs_diff(rgb[j * 3 + c]) > step);
    for y in 0..height {
        for x in 0..width {
            let i = y * width + x;
            if x + 1 < width && differs(i, i + 1) {
                edges[i] = true;
                edges[i + 1] = true;
            }
            if y + 1 < height && differs(i, i + width) {
                edges[i] = true;
                edges[i + width] = true;
            }
        }
    }
    edges
}

/// Marks every pixel within `radius` pixels (Chebyshev distance) of a marked
/// one.
pub fn dilate(mask: &[bool], width: usize, height: usize, radius: usize) -> Vec<bool> {
    if radius == 0 {
        return mask.to_vec();
    }
    // Separable: rows, then columns, with a running count of marked pixels.
    let pass = |src: &[bool], len: usize, lines: usize, at: &dyn Fn(usize, usize) -> usize| {
        let mut out = vec![false; src.len()];
        for line in 0..lines {
            let mut count = 0usize;
            for k in 0..radius.min(len) {
                count += usize::from(src[at(line, k)]);
            }
            for k in 0..len {
                if k + radius < len {
                    count += usize::from(src[at(line, k + radius)]);
                }
                if k > radius {
                    count -= usize::from(src[at(line, k - radius - 1)]);
                }
                out[at(line, k)] = count > 0;
            }
        }
        out
    };
    let rows = pass(mask, width, height, &|y, x| y * width + x);
    pass(&rows, height, width, &|x, y| y * width + x)
}

/// Region index ([`INTERIOR`] … [`UNOBSERVED`]) of every pixel of a reference
/// frame. `observed`, when given, marks the pixels whose surface point some
/// sensor has seen; the others are [`UNOBSERVED`] whatever else they are.
pub fn classify(reference: &RgbdFrame, observed: Option<&[bool]>, params: &MaskParams) -> Vec<u8> {
    let (w, h) = (reference.width as usize, reference.height as usize);
    let radius = params.band_px as usize;
    let silhouette = dilate(&depth_edges(&reference.depth, w, h, params.depth_step), w, h, radius);
    let texture = dilate(&color_edges(&reference.rgb, w, h, params.color_step), w, h, radius);
    (0..w * h)
        .map(|i| {
            if observed.is_some_and(|m| !m[i]) {
                UNOBSERVED as u8
            } else if silhouette[i] {
                SILHOUETTE as u8
            } else if texture[i] {
                TEXTURE as u8
            } else {
                INTERIOR as u8
            }
        })
        .collect()
}

/// PSNR in dB of a mean squared error per channel (peak 255).
pub fn psnr_of_mse(mse: f64) -> f32 {
    if mse > 0.0 { (10.0 * (255.0f64 * 255.0 / mse).log10()) as f32 } else { f32::INFINITY }
}

/// Rec. 601 luma of every pixel.
fn luma(image: &Image) -> Vec<f32> {
    image.rgb.chunks_exact(3).map(|p| 0.299 * p[0] as f32 + 0.587 * p[1] as f32 + 0.114 * p[2] as f32).collect()
}

/// Separable blur with the 11-tap Gaussian window (σ = 1.5) of SSIM; the
/// window is renormalised at the image borders.
fn ssim_blur(src: &[f32], width: usize, height: usize) -> Vec<f32> {
    const RADIUS: isize = 5;
    let mut kernel = [0.0f32; 11];
    for (k, value) in kernel.iter_mut().enumerate() {
        let d = k as f32 - RADIUS as f32;
        *value = (-d * d / (2.0 * 1.5 * 1.5)).exp();
    }
    let pass = |src: &[f32], len: usize, lines: usize, at: &dyn Fn(usize, usize) -> usize| {
        let mut out = vec![0.0f32; src.len()];
        for line in 0..lines {
            for k in 0..len as isize {
                let (mut sum, mut weight) = (0.0f32, 0.0f32);
                for (t, &g) in kernel.iter().enumerate() {
                    let q = k + t as isize - RADIUS;
                    if q >= 0 && q < len as isize {
                        sum += g * src[at(line, q as usize)];
                        weight += g;
                    }
                }
                out[at(line, k as usize)] = sum / weight;
            }
        }
        out
    };
    let rows = pass(src, width, height, &|y, x| y * width + x);
    pass(&rows, height, width, &|x, y| y * width + x)
}

/// Mean structural similarity (SSIM, Wang et al. 2004) of the luma of two
/// images: 11x11 Gaussian window with σ = 1.5, `K1 = 0.01`, `K2 = 0.03`.
/// `1.0` for identical images.
///
/// # Panics
/// When the image sizes differ.
pub fn ssim(a: &Image, b: &Image) -> f32 {
    assert!(a.width == b.width && a.height == b.height, "image sizes differ");
    let (w, h) = (a.width as usize, a.height as usize);
    if w * h == 0 {
        return 1.0;
    }
    let (x, y) = (luma(a), luma(b));
    let product = |p: &[f32], q: &[f32]| -> Vec<f32> { p.iter().zip(q).map(|(u, v)| u * v).collect() };
    let (mx, my) = (ssim_blur(&x, w, h), ssim_blur(&y, w, h));
    let (xx, yy, xy) =
        (ssim_blur(&product(&x, &x), w, h), ssim_blur(&product(&y, &y), w, h), ssim_blur(&product(&x, &y), w, h));
    const C1: f32 = (0.01 * 255.0) * (0.01 * 255.0);
    const C2: f32 = (0.03 * 255.0) * (0.03 * 255.0);
    let mut sum = 0.0f64;
    for i in 0..w * h {
        let (vx, vy, cov) = (xx[i] - mx[i] * mx[i], yy[i] - my[i] * my[i], xy[i] - mx[i] * my[i]);
        let value = (2.0 * mx[i] * my[i] + C1) * (2.0 * cov + C2) / ((mx[i] * mx[i] + my[i] * my[i] + C1) * (vx + vy + C2));
        sum += value as f64;
    }
    (sum / (w * h) as f64) as f32
}

/// Result of comparing one reconstructed view with its reference.
#[derive(Clone, Debug, PartialEq)]
pub struct Score {
    /// PSNR over the whole image, dB.
    pub psnr: f32,
    /// PSNR over the pixels that are not [`UNOBSERVED`], dB.
    pub psnr_observed: f32,
    /// Mean SSIM of the luma.
    pub ssim: f32,
    /// Fraction of the pixels showing scene in the reference where the
    /// reconstruction is at least half opaque.
    pub coverage: f32,
    /// The same among the observed pixels.
    pub coverage_observed: f32,
    /// Sum over pixels and channels of the squared error, per region.
    pub sse: [f64; 4],
    /// Number of pixels per region.
    pub pixels: [u64; 4],
    /// RMS of the 4-neighbour Laplacian of the luma error over interior
    /// pixels, in 8-bit levels: the high-frequency noise the reconstruction
    /// adds on surfaces that are flat in the reference (visible splat discs,
    /// "dotted" walls).
    pub hf_interior: f32,
}

impl Score {
    /// Share of the total squared error that falls into `region`.
    pub fn share(&self, region: usize) -> f32 {
        let total: f64 = self.sse.iter().sum();
        if total > 0.0 { (self.sse[region] / total) as f32 } else { 0.0 }
    }

    /// PSNR restricted to `region`, dB (infinite for an empty region).
    pub fn psnr_region(&self, region: usize) -> f32 {
        if self.pixels[region] == 0 {
            return f32::INFINITY;
        }
        psnr_of_mse(self.sse[region] / (3 * self.pixels[region]) as f64)
    }
}

/// Compares `reconstruction` with the reference frame.
///
/// `alpha` is the accumulated opacity of the reconstruction per pixel (for
/// the coverage) and `regions` the output of [`classify`] for `reference`.
///
/// # Panics
/// When the sizes do not match.
pub fn score(reference: &RgbdFrame, reconstruction: &Image, alpha: &[f32], regions: &[u8]) -> Score {
    let (w, h) = (reference.width as usize, reference.height as usize);
    let n = w * h;
    assert!(reconstruction.width == reference.width && reconstruction.height == reference.height, "image sizes differ");
    assert!(alpha.len() == n && regions.len() == n && reference.rgb.len() >= n * 3, "buffer sizes differ");
    let reference_image = Image { width: reference.width, height: reference.height, rgb: reference.rgb[..n * 3].to_vec() };
    let mut sse = [0.0f64; 4];
    let mut pixels = [0u64; 4];
    let (mut scene, mut covered, mut seen, mut seen_covered) = (0u64, 0u64, 0u64, 0u64);
    for i in 0..n {
        let region = regions[i] as usize;
        let error: u32 = (0..3)
            .map(|c| {
                let d = reference.rgb[i * 3 + c].abs_diff(reconstruction.rgb[i * 3 + c]) as u32;
                d * d
            })
            .sum();
        sse[region] += error as f64;
        pixels[region] += 1;
        let is_scene = reference.depth[i] > 0.0;
        let opaque = alpha[i] >= 0.5;
        scene += u64::from(is_scene);
        covered += u64::from(is_scene && opaque);
        if region != UNOBSERVED {
            seen += 1;
            seen_covered += u64::from(opaque || !is_scene);
        }
    }
    let ratio = |a: u64, b: u64| if b > 0 { a as f32 / b as f32 } else { 1.0 };
    let total: f64 = sse.iter().sum();
    let observed_sse = total - sse[UNOBSERVED];

    // High-frequency error on interior pixels whose 4-neighbours are interior.
    let (la, lb) = (luma(&reference_image), luma(reconstruction));
    let error = |i: usize| lb[i] - la[i];
    let (mut lap2, mut count) = (0.0f64, 0u64);
    for y in 1..h.saturating_sub(1) {
        for x in 1..w - 1 {
            let i = y * w + x;
            let neighbours = [i - 1, i + 1, i - w, i + w];
            if regions[i] as usize == INTERIOR && neighbours.iter().all(|&j| regions[j] as usize == INTERIOR) {
                let lap = 4.0 * error(i) - neighbours.iter().map(|&j| error(j)).sum::<f32>();
                lap2 += (lap * lap) as f64;
                count += 1;
            }
        }
    }
    Score {
        psnr: psnr_of_mse(total / (3 * n.max(1)) as f64),
        psnr_observed: psnr_of_mse(observed_sse / (3 * seen.max(1)) as f64),
        ssim: ssim(&reference_image, reconstruction),
        coverage: ratio(covered, scene),
        coverage_observed: ratio(seen_covered, seen),
        sse,
        pixels,
        hf_interior: if count > 0 { (lap2 / count as f64).sqrt() as f32 } else { 0.0 },
    }
}

/// How far a rendered outline is from the true one, in pixel counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OutlineBias {
    /// Pixels the render covers (at least half opaque) outside the true shape.
    pub fat: u64,
    /// Pixels of the true shape the render leaves uncovered.
    pub thin: u64,
    /// Pixels of the true shape that touch its outside (its outline length).
    pub perimeter: u64,
}

impl OutlineBias {
    /// Mean outward and inward displacement of the outline in pixels:
    /// `(fat, thin) / perimeter`. A render that is merely blurred has both
    /// near zero; a fattened one has a large first value.
    pub fn displacement(&self) -> (f32, f32) {
        let p = self.perimeter.max(1) as f32;
        (self.fat as f32 / p, self.thin as f32 / p)
    }

    /// Adds the counts of another view.
    pub fn add(&mut self, other: &OutlineBias) {
        self.fat += other.fat;
        self.thin += other.thin;
        self.perimeter += other.perimeter;
    }
}

/// Compares the coverage of a render (`alpha`, opaque from 0.5) with the
/// true shape (`truth`): does the render fatten or thin the silhouette?
///
/// # Panics
/// When the buffers do not have `width·height` entries.
pub fn outline_bias(truth: &[bool], alpha: &[f32], width: usize, height: usize) -> OutlineBias {
    assert!(truth.len() == width * height && alpha.len() == width * height, "buffer sizes differ");
    let mut bias = OutlineBias::default();
    for y in 0..height {
        for x in 0..width {
            let i = y * width + x;
            let covered = alpha[i] >= 0.5;
            if truth[i] {
                bias.thin += u64::from(!covered);
                let outside = |dx: isize, dy: isize| {
                    let (nx, ny) = (x as isize + dx, y as isize + dy);
                    nx >= 0 && ny >= 0 && nx < width as isize && ny < height as isize && !truth[ny as usize * width + nx as usize]
                };
                bias.perimeter += u64::from(outside(-1, 0) || outside(1, 0) || outside(0, -1) || outside(0, 1));
            } else {
                bias.fat += u64::from(covered);
            }
        }
    }
    bias
}

/// Scores of several views put together.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Summary {
    /// Number of views.
    pub views: usize,
    /// Mean over the views of the PSNR, dB (as the novel-view tests report).
    pub psnr: f32,
    /// Lowest PSNR of a view, dB.
    pub psnr_worst: f32,
    /// Mean over the views of the PSNR on observed pixels, dB.
    pub psnr_observed: f32,
    /// Mean SSIM.
    pub ssim: f32,
    /// Mean coverage.
    pub coverage: f32,
    /// Mean coverage of the observed pixels.
    pub coverage_observed: f32,
    /// Share of the squared error of all views per region.
    pub share: [f32; 4],
    /// Share of the pixels of all views per region.
    pub area: [f32; 4],
    /// PSNR per region over the pooled pixels of all views, dB.
    pub psnr_region: [f32; 4],
    /// Mean of [`Score::hf_interior`].
    pub hf_interior: f32,
}

impl Summary {
    /// Aggregates the scores of several views.
    pub fn of(scores: &[Score]) -> Self {
        if scores.is_empty() {
            return Self::default();
        }
        let mean = |f: &dyn Fn(&Score) -> f32| scores.iter().map(f).sum::<f32>() / scores.len() as f32;
        let mut sse = [0.0f64; 4];
        let mut pixels = [0u64; 4];
        for s in scores {
            for r in 0..4 {
                sse[r] += s.sse[r];
                pixels[r] += s.pixels[r];
            }
        }
        let (total_sse, total_pixels) = (sse.iter().sum::<f64>(), pixels.iter().sum::<u64>());
        let mut out = Self {
            views: scores.len(),
            psnr: mean(&|s| s.psnr),
            psnr_worst: scores.iter().map(|s| s.psnr).fold(f32::INFINITY, f32::min),
            psnr_observed: mean(&|s| s.psnr_observed),
            ssim: mean(&|s| s.ssim),
            coverage: mean(&|s| s.coverage),
            coverage_observed: mean(&|s| s.coverage_observed),
            hf_interior: mean(&|s| s.hf_interior),
            ..Self::default()
        };
        for r in 0..4 {
            out.share[r] = if total_sse > 0.0 { (sse[r] / total_sse) as f32 } else { 0.0 };
            out.area[r] = if total_pixels > 0 { pixels[r] as f32 / total_pixels as f32 } else { 0.0 };
            out.psnr_region[r] = if pixels[r] > 0 { psnr_of_mse(sse[r] / (3 * pixels[r]) as f64) } else { f32::INFINITY };
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(width: u32, height: u32, depth: impl Fn(u32, u32) -> f32, rgb: impl Fn(u32, u32) -> [u8; 3]) -> RgbdFrame {
        let mut f = RgbdFrame::new(width, height);
        for y in 0..height {
            for x in 0..width {
                let i = f.index(x, y);
                f.depth[i] = depth(x, y);
                f.rgb[i * 3..i * 3 + 3].copy_from_slice(&rgb(x, y));
            }
        }
        f
    }

    #[test]
    fn depth_edges_find_steps_and_holes_but_not_grazing_planes() {
        // A step from 2 m to 3 m between columns 9 and 10.
        let step = frame(20, 4, |x, _| if x < 10 { 2.0 } else { 3.0 }, |_, _| [0; 3]);
        let edges = depth_edges(&step.depth, 20, 4, 0.03);
        for x in 0..20 {
            assert_eq!(edges[20 + x], x == 9 || x == 10, "column {x}");
        }
        // A hole.
        let hole = frame(20, 4, |x, _| if x == 5 { 0.0 } else { 2.0 }, |_, _| [0; 3]);
        let edges = depth_edges(&hole.depth, 20, 4, 0.03);
        assert!(edges[4] && edges[5] && edges[6] && !edges[3] && !edges[7]);
        // A plane at a grazing angle: inverse depth affine with 10 % steps.
        let plane = frame(20, 4, |x, _| 1.0 / (0.2 + 0.05 * x as f32), |_, _| [0; 3]);
        assert!(depth_edges(&plane.depth, 20, 4, 0.03).iter().all(|&e| !e));
    }

    #[test]
    fn dilation_grows_a_square_band() {
        let mut mask = vec![false; 15 * 11];
        mask[5 * 15 + 7] = true;
        let grown = dilate(&mask, 15, 11, 2);
        for y in 0..11usize {
            for x in 0..15usize {
                assert_eq!(grown[y * 15 + x], x.abs_diff(7) <= 2 && y.abs_diff(5) <= 2, "({x}, {y})");
            }
        }
        assert_eq!(dilate(&mask, 15, 11, 0), mask);
        // At the border.
        let mut corner = vec![false; 15 * 11];
        corner[0] = true;
        let grown = dilate(&corner, 15, 11, 3);
        assert_eq!(grown.iter().filter(|&&m| m).count(), 16);
    }

    #[test]
    fn regions_follow_their_priority() {
        // Depth step at x = 20, colour edge at x = 40, unobserved column 50+.
        let reference = frame(
            64,
            8,
            |x, _| if x < 20 { 2.0 } else { 3.0 },
            |x, _| if x < 40 { [200, 200, 200] } else { [20, 20, 20] },
        );
        let observed: Vec<bool> = (0..64 * 8).map(|i| i % 64 < 50).collect();
        let regions = classify(&reference, Some(&observed), &MaskParams::default());
        let row = &regions[64..128];
        for (x, &r) in row.iter().enumerate() {
            let expected = if x >= 50 {
                UNOBSERVED
            } else if (17..=22).contains(&x) {
                SILHOUETTE
            } else if (37..=42).contains(&x) {
                TEXTURE
            } else {
                INTERIOR
            };
            assert_eq!(r as usize, expected, "column {x}");
        }
        assert_eq!(MaskParams::default().scaled_to(1280).band_px, 8);
        assert_eq!(MaskParams::default().scaled_to(320).band_px, 2);
    }

    #[test]
    fn score_decomposes_the_error() {
        let reference = frame(
            64,
            16,
            |x, _| if x < 20 { 2.0 } else { 3.0 },
            |x, _| if x < 40 { [200, 200, 200] } else { [20, 20, 20] },
        );
        let regions = classify(&reference, None, &MaskParams::default());
        let exact = Image::from_frame(&reference);
        let alpha = vec![1.0; 64 * 16];
        let perfect = score(&reference, &exact, &alpha, &regions);
        assert_eq!(perfect.psnr, f32::INFINITY);
        assert!((perfect.ssim - 1.0).abs() < 1e-6);
        assert_eq!(perfect.coverage, 1.0);
        assert_eq!(perfect.hf_interior, 0.0);

        // Error of 10 levels in the silhouette band only (column 19).
        let mut off = exact.clone();
        for y in 0..16 {
            for c in 0..3 {
                off.rgb[(y * 64 + 19) * 3 + c] -= 10;
            }
        }
        let s = score(&reference, &off, &alpha, &regions);
        assert!((s.share(SILHOUETTE) - 1.0).abs() < 1e-6);
        assert_eq!(s.sse[SILHOUETTE], 16.0 * 3.0 * 100.0);
        let mse = 16.0 * 100.0 / (64.0 * 16.0);
        assert!((s.psnr - psnr_of_mse(mse)).abs() < 1e-4);
        assert!(s.psnr_region(SILHOUETTE) < s.psnr && s.psnr_region(INTERIOR).is_infinite());
        assert!(s.ssim < 1.0);

        // Speckle on the interior shows up as high-frequency error.
        let mut dotted = exact.clone();
        for y in (2..14).step_by(2) {
            for x in (4..14).step_by(2) {
                dotted.rgb[(y * 64 + x) * 3] -= 30;
            }
        }
        let d = score(&reference, &dotted, &alpha, &regions);
        assert!((d.share(INTERIOR) - 1.0).abs() < 1e-6);
        assert!(d.hf_interior > 3.0, "{}", d.hf_interior);

        // Half of the scene uncovered.
        let half: Vec<f32> = (0..64 * 16).map(|i| if i % 64 < 32 { 1.0 } else { 0.0 }).collect();
        assert!((score(&reference, &exact, &half, &regions).coverage - 0.5).abs() < 1e-6);

        let summary = Summary::of(&[perfect, s.clone(), d]);
        assert_eq!(summary.views, 3);
        assert!(summary.psnr.is_infinite() && summary.psnr_worst <= s.psnr);
        assert!((summary.share.iter().sum::<f32>() - 1.0).abs() < 1e-5);
        assert!((summary.area.iter().sum::<f32>() - 1.0).abs() < 1e-5);
    }

    #[test]
    fn outline_bias_measures_fattening_and_thinning() {
        // A 20x10 rectangle in a 40x30 image.
        let inside = |x: usize, y: usize, grow: isize| {
            (x as isize) >= 10 - grow && (x as isize) < 30 + grow && (y as isize) >= 10 - grow && (y as isize) < 20 + grow
        };
        let mask = |grow: isize| -> Vec<bool> { (0..40 * 30).map(|i| inside(i % 40, i / 40, grow)).collect() };
        let alpha = |grow: isize| -> Vec<f32> { mask(grow).iter().map(|&m| if m { 1.0 } else { 0.0 }).collect() };
        let truth = mask(0);
        let exact = outline_bias(&truth, &alpha(0), 40, 30);
        assert_eq!(exact, OutlineBias { fat: 0, thin: 0, perimeter: 56 });
        // Grown by two pixels on every side: 24x14 − 20x10 = 136 extra pixels.
        let fat = outline_bias(&truth, &alpha(2), 40, 30);
        assert_eq!((fat.fat, fat.thin), (136, 0));
        assert!((fat.displacement().0 - 136.0 / 56.0).abs() < 1e-6);
        let thin = outline_bias(&truth, &alpha(-1), 40, 30);
        assert_eq!((thin.fat, thin.thin), (0, 56));
        let mut total = fat;
        total.add(&thin);
        assert_eq!(total, OutlineBias { fat: 136, thin: 56, perimeter: 112 });
    }

    #[test]
    fn ssim_behaves() {
        let a = Image::new(32, 32, [120, 120, 120]);
        assert!((ssim(&a, &a) - 1.0).abs() < 1e-6);
        // A constant offset barely changes the structure; noise does.
        let brighter = Image::new(32, 32, [130, 130, 130]);
        let mut noisy = a.clone();
        let mut state = 12345u32;
        for v in noisy.rgb.iter_mut() {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *v = (*v as i32 + ((state >> 24) as i32 - 128) / 4).clamp(0, 255) as u8;
        }
        let (offset, noise) = (ssim(&a, &brighter), ssim(&a, &noisy));
        assert!(offset > 0.98, "{offset}");
        assert!(noise < 0.5, "{noise}");
        // Symmetric.
        assert!((ssim(&a, &noisy) - ssim(&noisy, &a)).abs() < 1e-6);
    }
}
