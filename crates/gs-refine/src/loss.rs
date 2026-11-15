//! The loss of one view and its gradient with respect to the rendered
//! images.
//!
//! ```text
//! L = (1 − λ)·L1 + λ·(1 − SSIM) + w_depth·L_depth + w_alpha·L_alpha
//! ```
//!
//! * `L1` and `SSIM` compare the photograph with the rendered colour after
//!   the per-image colour correction ([`ColorCorrection`]: a gain and an
//!   offset per channel, which absorbs the exposure and white balance of
//!   that photograph), clipped at white as the camera clips. SSIM is the usual one: an 11x11 Gaussian window of
//!   `σ = 1.5` on every channel, zero padding, `C1 = 0.01²`, `C2 = 0.03²`.
//! * `L_depth` is the relative difference between the mean rendered depth
//!   and the LiDAR depth, weighted by the sensor's confidence. It holds the
//!   geometry where the photographs say nothing (a white wall).
//! * `L_alpha` is the mean coverage over the pixels known to show something
//!   outside the region of interest (object and person captures).
//!
//! Pixels of class [`IGNORE`] take part in nothing; in the structural term
//! they are painted the same in both pictures so that they cannot leak into
//! the windows of their neighbours.

use rayon::prelude::*;

use crate::raster::{ImageGrads, Rendered};

/// Pixel classes of a target, per pixel of its depth grid.
pub const IGNORE: u8 = 0;
/// Shows the scene: compared with the photograph.
pub const SCENE: u8 = 1;
/// Shows something outside the region of interest: nothing should be there.
pub const BACKGROUND: u8 = 2;

const RADIUS: usize = 5;
const TAPS: usize = 2 * RADIUS + 1;
const C1: f32 = 0.01 * 0.01;
const C2: f32 = 0.03 * 0.03;

/// Coverage below which a pixel has no meaningful mean depth.
const MIN_DEPTH_COVERAGE: f32 = 0.5;

/// The 11-tap Gaussian of `σ = 1.5`, normalised.
fn kernel() -> [f32; TAPS] {
    let mut k = [0.0f32; TAPS];
    let mut sum = 0.0;
    for (i, v) in k.iter_mut().enumerate() {
        let d = i as f32 - RADIUS as f32;
        *v = (-d * d / (2.0 * 1.5 * 1.5)).exp();
        sum += *v;
    }
    k.map(|v| v / sum)
}

/// Weights of the terms of the loss.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct LossWeights {
    /// `λ`: share of the structural term in the photometric loss. The
    /// usual 0.2 is a trade: measured on two simulated rooms (1 500
    /// iterations at 480 px), `0` scored 0.4 to 0.5 dB higher from
    /// viewpoints a meter from the phone's path, with a third fewer
    /// Gaussians and a quarter less time, and 0.5 to 0.6 dB lower at the
    /// phone's own poses (0.2 to 0.3 dB lower 27 cm beside them). To the
    /// eye the two were equally sharp, and without the structural term the
    /// halos around what lies beyond the sensor's range were larger.
    pub ssim: f32,
    pub depth: f32,
    pub alpha: f32,
}

impl Default for LossWeights {
    fn default() -> Self {
        Self { ssim: 0.2, depth: 0.2, alpha: 0.5 }
    }
}

/// Per-image colour correction: `shown = gain · rendered + offset` per
/// channel. The identity for a view rendered as the viewer will show it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ColorCorrection {
    pub gain: [f32; 3],
    pub offset: [f32; 3],
}

impl Default for ColorCorrection {
    fn default() -> Self {
        Self { gain: [1.0; 3], offset: [0.0; 3] }
    }
}

/// What a view is compared with.
#[derive(Clone, Copy, Debug)]
pub struct Target<'a> {
    pub width: u32,
    pub height: u32,
    /// The photograph at the size of the rendering, 3 bytes per pixel.
    pub rgb: &'a [u8],
    /// Size of the depth grid the three planes below live on. They cover
    /// the same field of view as the photograph. Empty planes switch the
    /// corresponding term off (an empty class plane means all [`SCENE`]).
    pub depth_width: u32,
    pub depth_height: u32,
    /// Millimeters, `0` = no value.
    pub depth_mm: &'a [u16],
    /// Trust in each depth value, `0..=255`.
    pub depth_weight: &'a [u8],
    pub class: &'a [u8],
}

/// The value of the loss and of its parts (already weighted).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LossValue {
    pub total: f64,
    pub l1: f64,
    pub dssim: f64,
    pub depth: f64,
    pub alpha: f64,
    /// Pixels compared with the photograph.
    pub pixels: usize,
}

/// Buffers reused from view to view.
#[derive(Default)]
pub struct LossScratch {
    /// Seconds spent in the blurs so far (the part that runs in parallel).
    pub blur_seconds: f64,
    x: Vec<f32>,
    y: Vec<f32>,
    weight: Vec<f32>,
    products: [Vec<f32>; 3],
    blurred: [Vec<f32>; 5],
    partial: [Vec<f32>; 3],
    tmp: Vec<f32>,
    dx: Vec<f32>,
}

/// Separable Gaussian blur of an interleaved 3-channel image with zero
/// padding. Symmetric, so it is its own adjoint.
fn blur(src: &[f32], dst: &mut [f32], tmp: &mut Vec<f32>, width: usize, height: usize) {
    let k = kernel();
    let stride = width * 3;
    tmp.clear();
    tmp.resize(src.len(), 0.0);
    tmp.par_chunks_mut(stride).zip(src.par_chunks(stride)).for_each(|(out, row)| {
        for (tap, &weight) in k.iter().enumerate() {
            // out[x] += weight · row[x + tap − RADIUS], inside the row.
            let shift = tap as isize - RADIUS as isize;
            let (from, to) = ((-shift).max(0) as usize, (width as isize - shift.max(0)).max(0) as usize);
            if from >= to {
                continue;
            }
            let source = &row[((from as isize + shift) as usize) * 3..((to as isize + shift) as usize) * 3];
            for (o, s) in out[from * 3..to * 3].iter_mut().zip(source) {
                *o += weight * s;
            }
        }
    });
    let tmp = &*tmp;
    dst.par_chunks_mut(stride).enumerate().for_each(|(y, out)| {
        out.fill(0.0);
        for (tap, &weight) in k.iter().enumerate() {
            let sy = y as isize + tap as isize - RADIUS as isize;
            if sy < 0 || sy >= height as isize {
                continue;
            }
            for (o, s) in out.iter_mut().zip(&tmp[sy as usize * stride..][..stride]) {
                *o += weight * s;
            }
        }
    });
}

/// Bilinear sample of the depth grid at the center of a pixel of the
/// rendering: `(meters, weight 0..=1)`. The four neighbours must all be
/// trusted and lie on one surface.
#[inline]
fn sample_depth(target: &Target, x: usize, y: usize) -> Option<(f32, f32)> {
    let (dw, dh) = (target.depth_width as usize, target.depth_height as usize);
    let gx = ((x as f32 + 0.5) * dw as f32 / target.width as f32 - 0.5).clamp(0.0, (dw - 1) as f32);
    let gy = ((y as f32 + 0.5) * dh as f32 / target.height as f32 - 0.5).clamp(0.0, (dh - 1) as f32);
    let (x0, y0) = (gx.floor() as usize, gy.floor() as usize);
    let (x1, y1) = ((x0 + 1).min(dw - 1), (y0 + 1).min(dh - 1));
    let (fx, fy) = (gx - x0 as f32, gy - y0 as f32);
    let corner = |cx: usize, cy: usize| {
        let i = cy * dw + cx;
        (target.depth_mm[i] as f32 * 1.0e-3, target.depth_weight[i])
    };
    let corners = [corner(x0, y0), corner(x1, y0), corner(x0, y1), corner(x1, y1)];
    let (mut lo, mut hi, mut weight) = (f32::INFINITY, 0.0f32, 255u8);
    for (z, w) in corners {
        lo = lo.min(z);
        hi = hi.max(z);
        weight = weight.min(w);
    }
    if weight == 0 || lo <= 0.0 || hi > lo * 1.05 {
        return None;
    }
    let top = corners[0].0 + (corners[1].0 - corners[0].0) * fx;
    let bottom = corners[2].0 + (corners[3].0 - corners[2].0) * fx;
    Some((top + (bottom - top) * fy, weight as f32 / 255.0))
}

/// Class of a pixel of the rendering (nearest pixel of the class plane).
#[inline]
fn class_at(target: &Target, x: usize, y: usize) -> u8 {
    if target.class.is_empty() {
        return SCENE;
    }
    let (dw, dh) = (target.depth_width as usize, target.depth_height as usize);
    let cx = ((x * 2 + 1) * dw / (2 * target.width as usize)).min(dw - 1);
    let cy = ((y * 2 + 1) * dh / (2 * target.height as usize)).min(dh - 1);
    target.class[cy * dw + cx]
}

/// Computes the loss of `rendered` against `target` and writes its gradient
/// with respect to the three rendered images into `grads`. Returns the
/// value and the gradient with respect to the colour correction (gain,
/// then offset).
///
/// # Panics
/// When the sizes of `rendered` and `target` disagree.
pub fn evaluate(
    rendered: &Rendered,
    correction: &ColorCorrection,
    target: &Target,
    weights: &LossWeights,
    grads: &mut ImageGrads,
    scratch: &mut LossScratch,
) -> (LossValue, [f64; 6]) {
    let (width, height) = (target.width as usize, target.height as usize);
    let pixels = width * height;
    assert!(
        rendered.width == target.width && rendered.height == target.height && target.rgb.len() == pixels * 3 && rendered.color.len() == pixels * 3,
        "the rendering and its target differ in size"
    );
    grads.reset(pixels);
    let mut value = LossValue::default();
    if pixels == 0 {
        return (value, [0.0; 6]);
    }
    let depth_pixels = target.depth_width as usize * target.depth_height as usize;
    let has_depth = weights.depth > 0.0 && depth_pixels > 0 && target.depth_mm.len() == depth_pixels && target.depth_weight.len() == depth_pixels;
    assert!(target.class.is_empty() || target.class.len() == depth_pixels, "class plane of the wrong size");

    // Every pass below works row by row in parallel and returns its sums
    // per row; adding those in row order keeps the result independent of
    // the number of threads.
    let s = scratch;
    let row3 = width * 3;
    for buffer in [&mut s.x, &mut s.y, &mut s.dx] {
        buffer.clear();
        buffer.resize(pixels * 3, 0.0);
    }
    s.weight.clear();
    s.weight.resize(pixels, 0.0);

    // Pass 1: corrected rendering and photograph, both blanked where
    // ignored; the sign of their difference; the absolute difference.
    let counts: Vec<(usize, usize, f64)> = (s.x.par_chunks_mut(row3), s.y.par_chunks_mut(row3), s.dx.par_chunks_mut(row3), s.weight.par_chunks_mut(width))
        .into_par_iter()
        .enumerate()
        .map(|(y, (xs, ys, signs, ws))| {
            let (mut scene, mut background, mut l1) = (0usize, 0usize, 0.0f64);
            for x in 0..width {
                match class_at(target, x, y) {
                    SCENE => {
                        scene += 1;
                        ws[x] = 1.0;
                        let i = y * width + x;
                        for c in 0..3 {
                            // A camera clips at white: beyond it the photograph
                            // says nothing about how bright the scene is.
                            let shown = (correction.gain[c] * rendered.color[i * 3 + c] + correction.offset[c]).min(1.0);
                            let photo = target.rgb[i * 3 + c] as f32 / 255.0;
                            let diff = shown - photo;
                            xs[x * 3 + c] = shown;
                            ys[x * 3 + c] = photo;
                            l1 += diff.abs() as f64;
                            // Zero where the two agree exactly.
                            signs[x * 3 + c] = if diff > 0.0 { 1.0 } else if diff < 0.0 { -1.0 } else { 0.0 };
                        }
                    }
                    BACKGROUND => background += 1,
                    _ => {}
                }
            }
            (scene, background, l1)
        })
        .collect();
    let (scene, background, l1) = counts.iter().fold((0, 0, 0.0), |a, r| (a.0 + r.0, a.1 + r.1, a.2 + r.2));
    value.pixels = scene;
    let norm = 1.0 / (3.0 * scene.max(1) as f32);
    let l1_weight = (1.0 - weights.ssim) * norm;
    value.l1 = l1 * l1_weight as f64;

    // Structural term.
    let with_ssim = weights.ssim > 0.0 && scene > 0;
    if with_ssim {
        let [xx, yy, xy] = &mut s.products;
        for buffer in [&mut *xx, &mut *yy, &mut *xy] {
            buffer.clear();
            buffer.resize(pixels * 3, 0.0);
        }
        (xx.par_chunks_mut(row3), yy.par_chunks_mut(row3), xy.par_chunks_mut(row3), s.x.par_chunks(row3), s.y.par_chunks(row3)).into_par_iter().for_each(|(xx, yy, xy, x, y)| {
            for i in 0..row3 {
                xx[i] = x[i] * x[i];
                yy[i] = y[i] * y[i];
                xy[i] = x[i] * y[i];
            }
        });
        for buffer in s.blurred.iter_mut().chain(s.partial.iter_mut()) {
            buffer.clear();
            buffer.resize(pixels * 3, 0.0);
        }
        let [mu_x, mu_y, bxx, byy, bxy] = &mut s.blurred;
        let blur_clock = std::time::Instant::now();
        blur(&s.x, mu_x, &mut s.tmp, width, height);
        blur(&s.y, mu_y, &mut s.tmp, width, height);
        blur(xx, bxx, &mut s.tmp, width, height);
        blur(yy, byy, &mut s.tmp, width, height);
        blur(xy, bxy, &mut s.tmp, width, height);
        s.blur_seconds += blur_clock.elapsed().as_secs_f64();

        // Per pixel and channel: SSIM, and the three partials that the
        // adjoint blur spreads back: with a = dS/dμx − 2μx·b − μy·c,
        // b = dS/dσxx and c = dS/dσxy,
        // dL/dx = G⋆(g·a) + 2x·G⋆(g·b) + y·G⋆(g·c).
        let g = -weights.ssim * norm;
        let [pa, pb, pc] = &mut s.partial;
        let (mu_x_ref, mu_y_ref, bxx_ref, byy_ref, bxy_ref) = (&*mu_x, &*mu_y, &*bxx, &*byy, &*bxy);
        let sums: Vec<f64> = (pa.par_chunks_mut(row3), pb.par_chunks_mut(row3), pc.par_chunks_mut(row3), s.weight.par_chunks(width))
            .into_par_iter()
            .enumerate()
            .map(|(y, (pa, pb, pc, ws))| {
                let mut sum = 0.0f64;
                for k in 0..row3 {
                    if ws[k / 3] == 0.0 {
                        continue;
                    }
                    let i = y * row3 + k;
                    let (mx, my) = (mu_x_ref[i], mu_y_ref[i]);
                    let (sxx, syy, sxy) = (bxx_ref[i] - mx * mx, byy_ref[i] - my * my, bxy_ref[i] - mx * my);
                    let (a1, a2) = (2.0 * mx * my + C1, 2.0 * sxy + C2);
                    let (b1, b2) = (mx * mx + my * my + C1, sxx + syy + C2);
                    let ssim = a1 * a2 / (b1 * b2);
                    sum += (1.0 - ssim) as f64;
                    let d_mu = 2.0 * (my * a2 - mx * ssim * b2) / (b1 * b2);
                    let d_sxx = -ssim / b2;
                    let d_sxy = 2.0 * a1 / (b1 * b2);
                    pa[k] = g * (d_mu - 2.0 * mx * d_sxx - my * d_sxy);
                    pb[k] = g * d_sxx;
                    pc[k] = g * d_sxy;
                }
                sum
            })
            .collect();
        value.dssim = sums.iter().sum::<f64>() * (weights.ssim * norm) as f64;
        // Three of the forward buffers take the blurred partials.
        let blur_clock = std::time::Instant::now();
        blur(pa, mu_x, &mut s.tmp, width, height);
        blur(pb, bxx, &mut s.tmp, width, height);
        blur(pc, bxy, &mut s.tmp, width, height);
        s.blur_seconds += blur_clock.elapsed().as_secs_f64();
    }

    // Last pass: the gradient of the photometric terms back through the
    // blanking, the clipping and the colour correction; depth and coverage.
    let depth_norm = weights.depth / scene.max(1) as f32;
    let alpha_norm = weights.alpha / background.max(1) as f32;
    let with_alpha = background > 0 && weights.alpha > 0.0;
    let (blurred_a, blurred_b, blurred_c) = (&s.blurred[0], &s.blurred[2], &s.blurred[4]);
    let (signs, xs, ys, ws) = (&s.dx, &s.x, &s.y, &s.weight);
    let rows: Vec<([f64; 6], f64, f64)> = (grads.color.par_chunks_mut(row3), grads.depth.par_chunks_mut(width), grads.alpha.par_chunks_mut(width))
        .into_par_iter()
        .enumerate()
        .map(|(y, (g_color, g_depth, g_alpha))| {
            let (mut correction_grad, mut depth_sum, mut alpha_sum) = ([0.0f64; 6], 0.0f64, 0.0f64);
            for x in 0..width {
                let i = y * width + x;
                let coverage = rendered.alpha[i];
                if ws[i] == 0.0 {
                    if with_alpha && class_at(target, x, y) == BACKGROUND {
                        alpha_sum += coverage as f64;
                        g_alpha[x] = alpha_norm;
                    }
                    continue;
                }
                for c in 0..3 {
                    let k = i * 3 + c;
                    if correction.gain[c] * rendered.color[k] + correction.offset[c] >= 1.0 {
                        continue;
                    }
                    let mut d = l1_weight * signs[k];
                    if with_ssim {
                        d += blurred_a[k] + 2.0 * xs[k] * blurred_b[k] + ys[k] * blurred_c[k];
                    }
                    g_color[x * 3 + c] = correction.gain[c] * d;
                    correction_grad[c] += (rendered.color[k] * d) as f64;
                    correction_grad[3 + c] += d as f64;
                }
                if !has_depth || coverage < MIN_DEPTH_COVERAGE {
                    continue;
                }
                let Some((z, trust)) = sample_depth(target, x, y) else { continue };
                let mean = rendered.depth[i] / coverage;
                let relative = (mean - z) / z;
                depth_sum += (trust * relative.abs()) as f64;
                let g = depth_norm * trust / z * if relative > 0.0 { 1.0 } else if relative < 0.0 { -1.0 } else { 0.0 };
                g_depth[x] = g / coverage;
                g_alpha[x] = -g * mean / coverage;
            }
            (correction_grad, depth_sum, alpha_sum)
        })
        .collect();
    let mut correction_grad = [0.0f64; 6];
    let (mut depth_sum, mut alpha_sum) = (0.0f64, 0.0f64);
    for (row_grad, row_depth, row_alpha) in &rows {
        for k in 0..6 {
            correction_grad[k] += row_grad[k];
        }
        depth_sum += row_depth;
        alpha_sum += row_alpha;
    }
    value.depth = depth_sum * depth_norm as f64;
    value.alpha = alpha_sum * alpha_norm as f64;
    value.total = value.l1 + value.dssim + value.depth + value.alpha;
    (value, correction_grad)
}

/// Which pixels of the rendering are compared with the photograph: `None`
/// when all are.
pub fn scene_mask(target: &Target) -> Option<Vec<bool>> {
    if target.class.is_empty() {
        return None;
    }
    let (w, h) = (target.width as usize, target.height as usize);
    Some((0..w * h).map(|i| class_at(target, i % w, i / w) == SCENE).collect())
}

/// Least-squares gain per channel (no offset) that brings `rendered`
/// closest to `target` over the pixels where `mask` is set, leaving out
/// what the photograph clipped. `1` for a channel with nothing to fit.
pub fn fit_gain(rendered: &[f32], target: &[u8], mask: Option<&[bool]>) -> [f32; 3] {
    let mut out = [1.0f32; 3];
    for (c, gain) in out.iter_mut().enumerate() {
        let (mut sxx, mut sxy) = (0.0f64, 0.0f64);
        for (i, (r, t)) in rendered.chunks_exact(3).zip(target.chunks_exact(3)).enumerate() {
            if mask.is_some_and(|m| !m[i]) || t[c] <= 2 || t[c] >= 253 {
                continue;
            }
            sxx += (r[c] * r[c]) as f64;
            sxy += r[c] as f64 * t[c] as f64 / 255.0;
        }
        if sxx > 1e-3 {
            *gain = (sxy / sxx).clamp(0.5, 2.0) as f32;
        }
    }
    out
}

/// Least-squares gain and offset per channel that bring `rendered` closest
/// to `target` over the pixels where `mask` is set (all when `None`),
/// leaving out what the photograph clipped to black or white. Used to
/// score a held-out photograph whose exposure nobody optimised.
pub fn fit_correction(rendered: &[f32], target: &[u8], mask: Option<&[bool]>) -> ColorCorrection {
    let mut out = ColorCorrection::default();
    for c in 0..3 {
        let (mut n, mut sx, mut sy, mut sxx, mut sxy) = (0.0f64, 0.0f64, 0.0f64, 0.0f64, 0.0f64);
        for (i, (r, t)) in rendered.chunks_exact(3).zip(target.chunks_exact(3)).enumerate() {
            if mask.is_some_and(|m| !m[i]) {
                continue;
            }
            if t[c] <= 2 || t[c] >= 253 {
                continue;
            }
            let (x, y) = (r[c] as f64, t[c] as f64 / 255.0);
            n += 1.0;
            sx += x;
            sy += y;
            sxx += x * x;
            sxy += x * y;
        }
        let variance = n * sxx - sx * sx;
        if n >= 16.0 && variance > 1e-6 * n * n {
            let gain = ((n * sxy - sx * sy) / variance).clamp(0.5, 2.0);
            out.gain[c] = gain as f32;
            out.offset[c] = ((sy - gain * sx) / n) as f32;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::raster::tests::Rng;

    fn random_rendered(width: u32, height: u32, seed: u64) -> Rendered {
        let mut rng = Rng(seed);
        let n = (width * height) as usize;
        let alpha: Vec<f32> = (0..n).map(|_| rng.range(0.3, 1.0)).collect();
        Rendered {
            width,
            height,
            color: (0..n * 3).map(|_| rng.range(0.05, 0.95)).collect(),
            depth: alpha.iter().map(|a| a * rng.range(1.0, 3.0)).collect(),
            alpha,
        }
    }

    struct Planes {
        rgb: Vec<u8>,
        depth_mm: Vec<u16>,
        depth_weight: Vec<u8>,
        class: Vec<u8>,
    }

    fn random_planes(width: u32, height: u32, dw: u32, dh: u32, seed: u64, classes: bool) -> Planes {
        let mut rng = Rng(seed);
        let n = (width * height) as usize;
        let d = (dw * dh) as usize;
        Planes {
            rgb: (0..n * 3).map(|_| rng.range(10.0, 245.0) as u8).collect(),
            // A smooth slope, so that the bilinear samples pass the 5 % test.
            depth_mm: (0..d).map(|i| 1800 + (i as u32 % dw) as u16 * 20 + rng.range(0.0, 8.0) as u16).collect(),
            depth_weight: (0..d).map(|_| [0u8, 80, 255][(rng.unit() * 3.0) as usize % 3]).collect(),
            class: if classes { (0..d).map(|_| [SCENE, SCENE, BACKGROUND, IGNORE][(rng.unit() * 4.0) as usize % 4]).collect() } else { Vec::new() },
        }
    }

    fn target<'a>(p: &'a Planes, width: u32, height: u32, dw: u32, dh: u32) -> Target<'a> {
        Target { width, height, rgb: &p.rgb, depth_width: dw, depth_height: dh, depth_mm: &p.depth_mm, depth_weight: &p.depth_weight, class: &p.class }
    }

    /// Finite differences of every input of the loss on a small image.
    fn check(classes: bool, weights: LossWeights) {
        let (w, h, dw, dh) = (14u32, 11u32, 7u32, 5u32);
        let rendered = random_rendered(w, h, 21);
        let planes = random_planes(w, h, dw, dh, 22, classes);
        let target = target(&planes, w, h, dw, dh);
        let correction = ColorCorrection { gain: [1.1, 0.9, 1.05], offset: [0.02, -0.03, 0.01] };
        let mut scratch = LossScratch::default();
        let mut grads = ImageGrads::default();
        let (value, correction_grad) = evaluate(&rendered, &correction, &target, &weights, &mut grads, &mut scratch);
        assert!(value.total > 0.0 && (value.l1 > 0.0) == (weights.ssim < 1.0));
        assert_eq!(value.dssim > 0.0, weights.ssim > 0.0);
        assert_eq!(value.depth > 0.0, weights.depth > 0.0);
        assert_eq!(value.alpha > 0.0, classes && weights.alpha > 0.0);

        let mut loss = |r: &Rendered, c: &ColorCorrection| evaluate(r, c, &target, &weights, &mut ImageGrads::default(), &mut scratch).0.total;
        let eps = 2e-3f32;
        let mut worst = 0.0f64;
        let mut compare = |numeric: f64, analytic: f64, what: String| {
            let error = (numeric - analytic).abs() / (numeric.abs().max(analytic.abs()) + 2e-4);
            assert!(error < 0.03, "{what}: numeric {numeric}, analytic {analytic}");
            worst = worst.max(error);
        };
        for i in (0..rendered.color.len()).step_by(7) {
            let (mut plus, mut minus) = (rendered.clone(), rendered.clone());
            plus.color[i] += eps;
            minus.color[i] -= eps;
            compare((loss(&plus, &correction) - loss(&minus, &correction)) / (2.0 * eps as f64), grads.color[i] as f64, format!("colour {i}"));
        }
        for i in (0..rendered.depth.len()).step_by(3) {
            let (mut plus, mut minus) = (rendered.clone(), rendered.clone());
            plus.depth[i] += eps;
            minus.depth[i] -= eps;
            compare((loss(&plus, &correction) - loss(&minus, &correction)) / (2.0 * eps as f64), grads.depth[i] as f64, format!("depth {i}"));
            let (mut plus, mut minus) = (rendered.clone(), rendered.clone());
            plus.alpha[i] += eps;
            minus.alpha[i] -= eps;
            compare((loss(&plus, &correction) - loss(&minus, &correction)) / (2.0 * eps as f64), grads.alpha[i] as f64, format!("alpha {i}"));
        }
        for (k, &analytic) in correction_grad.iter().enumerate() {
            let (mut plus, mut minus) = (correction, correction);
            if k < 3 {
                plus.gain[k] += eps;
                minus.gain[k] -= eps;
            } else {
                plus.offset[k - 3] += eps;
                minus.offset[k - 3] -= eps;
            }
            compare((loss(&rendered, &plus) - loss(&rendered, &minus)) / (2.0 * eps as f64), analytic, format!("correction {k}"));
        }
        eprintln!("worst relative error {worst:.2e}");
    }

    #[test]
    fn the_gradient_of_the_l1_term_matches_finite_differences() {
        check(false, LossWeights { ssim: 0.0, depth: 0.0, alpha: 0.0 });
    }

    #[test]
    fn the_gradient_of_the_structural_term_matches_finite_differences() {
        check(false, LossWeights { ssim: 1.0, depth: 0.0, alpha: 0.0 });
    }

    #[test]
    fn the_gradient_of_the_whole_loss_matches_with_classes_and_depth() {
        check(true, LossWeights { ssim: 0.2, depth: 0.3, alpha: 0.5 });
        check(false, LossWeights { ssim: 0.2, depth: 0.3, alpha: 0.5 });
    }

    #[test]
    fn identical_pictures_have_no_photometric_loss() {
        let (w, h) = (32u32, 24u32);
        let mut rendered = random_rendered(w, h, 5);
        let rgb: Vec<u8> = rendered.color.iter().map(|v| (v * 255.0 + 0.5) as u8).collect();
        for (v, b) in rendered.color.iter_mut().zip(&rgb) {
            *v = *b as f32 / 255.0;
        }
        let target = Target { width: w, height: h, rgb: &rgb, depth_width: 0, depth_height: 0, depth_mm: &[], depth_weight: &[], class: &[] };
        let mut grads = ImageGrads::default();
        let (value, _) = evaluate(&rendered, &ColorCorrection::default(), &target, &LossWeights::default(), &mut grads, &mut LossScratch::default());
        assert!(value.total.abs() < 1e-6, "{value:?}");
        assert!(grads.color.iter().all(|g| g.abs() < 1e-6));
    }

    #[test]
    fn the_structural_term_agrees_with_the_metric_of_the_lab() {
        // gs_core's SSIM works on luma with its own window; the two must at
        // least rank a blurred and a noisy picture the same way and agree on
        // identical ones. Here: the loss of a picture against a brighter
        // copy is small but not zero, against noise it is large.
        let (w, h) = (48u32, 36u32);
        let base = random_rendered(w, h, 6);
        let rgb: Vec<u8> = base.color.iter().map(|v| (v * 255.0 + 0.5) as u8).collect();
        let target = Target { width: w, height: h, rgb: &rgb, depth_width: 0, depth_height: 0, depth_mm: &[], depth_weight: &[], class: &[] };
        let only_ssim = LossWeights { ssim: 1.0, depth: 0.0, alpha: 0.0 };
        let run = |r: &Rendered| evaluate(r, &ColorCorrection::default(), &target, &only_ssim, &mut ImageGrads::default(), &mut LossScratch::default()).0.dssim;
        let mut brighter = base.clone();
        brighter.color.iter_mut().for_each(|v| *v = (*v * 1.05).min(1.0));
        let noise = random_rendered(w, h, 7);
        let (same, bright, other) = (run(&base), run(&brighter), run(&noise));
        assert!(same < 1e-4 && bright > same && bright < 0.05 && other > 0.5, "{same} {bright} {other}");
    }

    #[test]
    fn the_fitted_correction_recovers_a_gain_and_an_offset() {
        let rendered = random_rendered(40, 30, 8);
        // Nothing clips: 0.95 · 1.02 + 0.03 < 1 and 0.05 · 1.0 − 0.02 > 0.
        let truth = ColorCorrection { gain: [1.02, 0.85, 1.0], offset: [0.03, 0.05, -0.02] };
        let rgb: Vec<u8> = rendered.color.chunks_exact(3).flat_map(|p| [0, 1, 2].map(|c| ((truth.gain[c] * p[c] + truth.offset[c]).clamp(0.0, 1.0) * 255.0 + 0.5) as u8)).collect();
        let fitted = fit_correction(&rendered.color, &rgb, None);
        for c in 0..3 {
            assert!((fitted.gain[c] - truth.gain[c]).abs() < 0.02 && (fitted.offset[c] - truth.offset[c]).abs() < 0.01, "{fitted:?}");
        }
        // Too little to fit: the identity.
        assert_eq!(fit_correction(&rendered.color[..9], &rgb[..9], None), ColorCorrection::default());
        // Gains alone, on a picture that differs by gains alone.
        let gains = [0.8f32, 1.0, 1.05];
        let rgb: Vec<u8> = rendered.color.chunks_exact(3).flat_map(|p| [0, 1, 2].map(|c| ((gains[c] * p[c]).min(1.0) * 255.0 + 0.5) as u8)).collect();
        let fitted = fit_gain(&rendered.color, &rgb, None);
        for c in 0..3 {
            assert!((fitted[c] - gains[c]).abs() < 0.01, "{fitted:?}");
        }
        assert_eq!(fit_gain(&[], &[], None), [1.0; 3]);
    }
}
