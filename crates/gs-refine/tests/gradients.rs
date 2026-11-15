//! Independent checks of the differentiable rasterizer and of the loss.
//!
//! Everything here is compared with a second implementation of the image
//! formation model and of the loss, written in `f64` from their
//! documentation (`gs_core::splat`, "Reference algorithm"; `gs_refine::loss`)
//! with no tiles, no rectangles and no shared code: one loop over every
//! Gaussian for every pixel. Analytic gradients of the whole loss (rasterizer
//! backward pass fed by the loss gradient) are compared with central
//! differences of that reference, for every parameter group, the camera
//! twist and the per-image colour correction, with the cut-offs of the real
//! configuration left on.

use gs_core::glam::Vec3;
use gs_core::Camera;
use gs_refine::loss::{self, ColorCorrection, LossScratch, LossWeights, Target, BACKGROUND, IGNORE, SCENE};
use gs_refine::model::{Gaussians, COLOR, MAX_OPACITY, OPACITY, PARAMS, POS, ROT, SCALE};
use gs_refine::raster::{CpuRasterizer, ImageGrads, ParamGrads, RasterConfig, Rasterizer, Rendered, View};

// ---------------------------------------------------------------------------
// The reference, f64
// ---------------------------------------------------------------------------

type G64 = [f64; PARAMS];

#[derive(Clone)]
struct View64 {
    width: usize,
    height: usize,
    fx: f64,
    fy: f64,
    cx: f64,
    cy: f64,
    /// World-to-camera rotation, rows.
    rot: [[f64; 3]; 3],
    trans: [f64; 3],
    background: [f64; 3],
}

impl View64 {
    fn of(view: &View) -> Self {
        let c = view.rot.to_cols_array();
        // Column-major: element (row r, column k) is c[k * 3 + r].
        let rot = [0, 1, 2].map(|r| [0, 1, 2].map(|k| c[k * 3 + r] as f64));
        Self {
            width: view.width as usize,
            height: view.height as usize,
            fx: view.fx as f64,
            fy: view.fy as f64,
            cx: view.cx as f64,
            cy: view.cy as f64,
            rot,
            trans: view.trans.to_array().map(|v| v as f64),
            background: view.background.map(|v| v as f64),
        }
    }

    /// The camera frame moved by the twist `[ω, τ]`: `x' = exp(ω)·x + τ`.
    fn moved(&self, twist: [f64; 6]) -> Self {
        let turn = rodrigues([twist[0], twist[1], twist[2]]);
        let rot = mat_mul(&turn, &self.rot);
        let t = mat_vec(&turn, &self.trans);
        Self { rot, trans: [t[0] + twist[3], t[1] + twist[4], t[2] + twist[5]], ..self.clone() }
    }
}

fn mat_mul(a: &[[f64; 3]; 3], b: &[[f64; 3]; 3]) -> [[f64; 3]; 3] {
    [0, 1, 2].map(|i| [0, 1, 2].map(|j| (0..3).map(|k| a[i][k] * b[k][j]).sum()))
}

fn mat_vec(a: &[[f64; 3]; 3], v: &[f64; 3]) -> [f64; 3] {
    [0, 1, 2].map(|i| (0..3).map(|k| a[i][k] * v[k]).sum())
}

/// Rotation matrix of a rotation vector (Rodrigues).
fn rodrigues(w: [f64; 3]) -> [[f64; 3]; 3] {
    let theta = (w[0] * w[0] + w[1] * w[1] + w[2] * w[2]).sqrt();
    let k = [[0.0, -w[2], w[1]], [w[2], 0.0, -w[0]], [-w[1], w[0], 0.0]];
    let (a, b) = if theta < 1e-12 { (1.0, 0.5) } else { (theta.sin() / theta, (1.0 - theta.cos()) / (theta * theta)) };
    let k2 = mat_mul(&k, &k);
    [0, 1, 2].map(|i| [0, 1, 2].map(|j| if i == j { 1.0 } else { 0.0 } + a * k[i][j] + b * k2[i][j]))
}

/// Rotation matrix of a quaternion `x, y, z, w` after normalisation.
fn quat_matrix(q: [f64; 4]) -> [[f64; 3]; 3] {
    let n = (q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3]).sqrt();
    let [x, y, z, w] = q.map(|c| c / n);
    [
        [1.0 - 2.0 * (y * y + z * z), 2.0 * (x * y - w * z), 2.0 * (x * z + w * y)],
        [2.0 * (x * y + w * z), 1.0 - 2.0 * (x * x + z * z), 2.0 * (y * z - w * x)],
        [2.0 * (x * z - w * y), 2.0 * (y * z + w * x), 1.0 - 2.0 * (x * x + y * y)],
    ]
}

struct Proj64 {
    px: f64,
    py: f64,
    depth: f64,
    conic: [f64; 3],
    opacity: f64,
    color: [f64; 3],
}

const NEAR: f64 = 0.05;
const LOW_PASS: f64 = 0.3;
const MIN_ALPHA: f64 = 1.0 / 255.0;
const MIN_TRANSMITTANCE: f64 = 1.0e-4;

/// Steps 1 to 6 of the reference algorithm with the classic dilation (the
/// opacity is left alone), `None` when the Gaussian is culled.
fn project64(g: &G64, v: &View64) -> Option<Proj64> {
    let p = [g[POS], g[POS + 1], g[POS + 2]];
    let t = mat_vec(&v.rot, &p);
    let t = [t[0] + v.trans[0], t[1] + v.trans[1], t[2] + v.trans[2]];
    let d = -t[2];
    if d < NEAR {
        return None;
    }
    let opacity = (1.0 / (1.0 + (-g[OPACITY]).exp())).min(MAX_OPACITY as f64);
    if opacity < MIN_ALPHA {
        return None;
    }
    let (w, h) = (v.width as f64, v.height as f64);
    let (px, py) = (v.cx + v.fx * t[0] / d, v.cy - v.fy * t[1] / d);
    let u = (t[0] / d).clamp((-v.cx - 0.15 * w) / v.fx, (w - v.cx + 0.15 * w) / v.fx);
    let vv = (t[1] / d).clamp((v.cy - h - 0.15 * h) / v.fy, (v.cy + 0.15 * h) / v.fy);
    let jac = [[v.fx / d, 0.0, v.fx * u / d], [0.0, -v.fy / d, -v.fy * vv / d]];
    // T = J · W, 2x3.
    let tm: [[f64; 3]; 2] = [0, 1].map(|i| [0, 1, 2].map(|j| (0..3).map(|k| jac[i][k] * v.rot[k][j]).sum()));
    // Σ = R · diag(σ²) · Rᵀ.
    let r = quat_matrix([g[ROT], g[ROT + 1], g[ROT + 2], g[ROT + 3]]);
    let s2 = [0, 1, 2].map(|k| (2.0 * g[SCALE + k]).exp());
    let sigma: [[f64; 3]; 3] = [0, 1, 2].map(|i| [0, 1, 2].map(|j| (0..3).map(|k| r[i][k] * s2[k] * r[j][k]).sum()));
    let quad = |a: &[f64; 3], b: &[f64; 3]| -> f64 { (0..3).map(|i| (0..3).map(|j| a[i] * sigma[i][j] * b[j]).sum::<f64>()).sum() };
    let (a, b, c) = (quad(&tm[0], &tm[0]) + LOW_PASS, quad(&tm[0], &tm[1]), quad(&tm[1], &tm[1]) + LOW_PASS);
    let det = a * c - b * b;
    if !(det > 0.0 && det.is_finite()) {
        return None;
    }
    Some(Proj64 { px, py, depth: d, conic: [c / det, -b / det, a / det], opacity, color: [g[COLOR], g[COLOR + 1], g[COLOR + 2]] })
}

struct Rendered64 {
    color: Vec<f64>,
    depth: Vec<f64>,
    alpha: Vec<f64>,
}

/// For every pixel, the Gaussians that contribute to it, front to back.
type Plan = Vec<Vec<usize>>;

/// Steps 9 to 11 for every pixel, front to back over all Gaussians. Also
/// returns which Gaussians contributed to each pixel.
///
/// With `frozen`, exactly those contributions are drawn, whatever the
/// cut-offs would say now. The picture as a function of the parameters has
/// a small jump wherever a pixel center crosses a cut-off (three standard
/// deviations, alpha of 1/255, the end of a saturated pixel); a backward
/// pass differentiates between the jumps, and so do finite differences of
/// a frozen plan.
fn render64(model: &[G64], v: &View64, frozen: Option<&Plan>) -> (Rendered64, Plan) {
    let projected: Vec<Option<Proj64>> = model.iter().map(|g| project64(g, v)).collect();
    let mut order: Vec<usize> = (0..model.len()).filter(|&i| projected[i].is_some()).collect();
    // The order of the optimiser: 32-bit depth, then index.
    let depth = |i: usize| projected[i].as_ref().map_or(0.0, |p| p.depth as f32);
    order.sort_by(|&a, &b| depth(a).total_cmp(&depth(b)).then(a.cmp(&b)));
    let n = v.width * v.height;
    let mut out = Rendered64 { color: vec![0.0; n * 3], depth: vec![0.0; n], alpha: vec![0.0; n] };
    let mut plan: Plan = vec![Vec::new(); n];
    for y in 0..v.height {
        for x in 0..v.width {
            let i = y * v.width + x;
            let (cx, cy) = (x as f64 + 0.5, y as f64 + 0.5);
            let (mut transmittance, mut color, mut depth) = (1.0f64, [0.0f64; 3], 0.0f64);
            let list: &[usize] = frozen.map_or(&order, |f| &f[i]);
            for &k in list {
                let p = projected[k].as_ref().expect("a planned Gaussian was culled: the step crossed the near plane");
                let (dx, dy) = (cx - p.px, cy - p.py);
                let m = p.conic[0] * dx * dx + 2.0 * p.conic[1] * dx * dy + p.conic[2] * dy * dy;
                let alpha = p.opacity * (-0.5 * m).exp();
                if frozen.is_none() && (m > 9.0 || alpha < MIN_ALPHA) {
                    continue;
                }
                plan[i].push(k);
                let weight = alpha * transmittance;
                for (sum, value) in color.iter_mut().zip(p.color) {
                    *sum += weight * value;
                }
                depth += weight * p.depth;
                transmittance *= 1.0 - alpha;
                if frozen.is_none() && transmittance < MIN_TRANSMITTANCE {
                    break;
                }
            }
            for (c, value) in color.iter().enumerate() {
                out.color[i * 3 + c] = value + transmittance * v.background[c];
            }
            out.depth[i] = depth;
            out.alpha[i] = 1.0 - transmittance;
        }
    }
    (out, plan)
}

/// What a view is compared with, owned.
#[derive(Clone)]
struct Planes {
    width: usize,
    height: usize,
    rgb: Vec<u8>,
    dw: usize,
    dh: usize,
    depth_mm: Vec<u16>,
    depth_weight: Vec<u8>,
    class: Vec<u8>,
}

impl Planes {
    fn target(&self) -> Target<'_> {
        Target {
            width: self.width as u32,
            height: self.height as u32,
            rgb: &self.rgb,
            depth_width: self.dw as u32,
            depth_height: self.dh as u32,
            depth_mm: &self.depth_mm,
            depth_weight: &self.depth_weight,
            class: &self.class,
        }
    }

    fn class_at(&self, x: usize, y: usize) -> u8 {
        if self.class.is_empty() {
            return SCENE;
        }
        // Nearest pixel of the class plane: the one the pixel center falls in.
        let cx = (((x as f64 + 0.5) * self.dw as f64 / self.width as f64).floor() as usize).min(self.dw - 1);
        let cy = (((y as f64 + 0.5) * self.dh as f64 / self.height as f64).floor() as usize).min(self.dh - 1);
        self.class[cy * self.dw + cx]
    }

    /// Bilinear depth at a pixel center, with its trust, when the four
    /// neighbours are all trusted and within 5 % of each other.
    fn depth_at(&self, x: usize, y: usize) -> Option<(f64, f64)> {
        if self.depth_mm.is_empty() {
            return None;
        }
        let gx = ((x as f64 + 0.5) * self.dw as f64 / self.width as f64 - 0.5).clamp(0.0, (self.dw - 1) as f64);
        let gy = ((y as f64 + 0.5) * self.dh as f64 / self.height as f64 - 0.5).clamp(0.0, (self.dh - 1) as f64);
        let (x0, y0) = (gx.floor() as usize, gy.floor() as usize);
        let (x1, y1) = ((x0 + 1).min(self.dw - 1), (y0 + 1).min(self.dh - 1));
        let z = |cx: usize, cy: usize| self.depth_mm[cy * self.dw + cx] as f64 * 1e-3;
        let trust = |cx: usize, cy: usize| self.depth_weight[cy * self.dw + cx];
        let zs = [z(x0, y0), z(x1, y0), z(x0, y1), z(x1, y1)];
        let weight = trust(x0, y0).min(trust(x1, y0)).min(trust(x0, y1)).min(trust(x1, y1));
        let (lo, hi) = (zs.iter().cloned().fold(f64::INFINITY, f64::min), zs.iter().cloned().fold(0.0, f64::max));
        if weight == 0 || lo <= 0.0 || hi > lo * 1.05 {
            return None;
        }
        let (fx, fy) = (gx - x0 as f64, gy - y0 as f64);
        let top = zs[0] + (zs[1] - zs[0]) * fx;
        let bottom = zs[2] + (zs[3] - zs[2]) * fx;
        Some((top + (bottom - top) * fy, weight as f64 / 255.0))
    }
}

/// 11-tap Gaussian of σ = 1.5, normalised.
fn kernel64() -> [f64; 11] {
    let mut k = [0.0; 11];
    for (i, v) in k.iter_mut().enumerate() {
        let d = i as f64 - 5.0;
        *v = (-d * d / 4.5).exp();
    }
    let sum: f64 = k.iter().sum();
    k.map(|v| v / sum)
}

/// Windowed mean of one channel plane with zero padding, written as the
/// plain double sum over the 11x11 window.
fn window_mean(src: &[f64], width: usize, height: usize) -> Vec<f64> {
    let k = kernel64();
    let mut out = vec![0.0; src.len()];
    for y in 0..height {
        for x in 0..width {
            let mut sum = 0.0;
            for (j, wy) in k.iter().enumerate() {
                let sy = y as isize + j as isize - 5;
                if sy < 0 || sy >= height as isize {
                    continue;
                }
                for (i, wx) in k.iter().enumerate() {
                    let sx = x as isize + i as isize - 5;
                    if sx < 0 || sx >= width as isize {
                        continue;
                    }
                    sum += wy * wx * src[sy as usize * width + sx as usize];
                }
            }
            out[y * width + x] = sum;
        }
    }
    out
}

/// The loss of the module documentation of `gs_refine::loss`, in f64.
fn loss64(r: &Rendered64, correction: &([f64; 3], [f64; 3]), planes: &Planes, weights: &LossWeights) -> f64 {
    let (w, h) = (planes.width, planes.height);
    let n = w * h;
    let class: Vec<u8> = (0..n).map(|i| planes.class_at(i % w, i / w)).collect();
    let scene = class.iter().filter(|c| **c == SCENE).count();
    let background = class.iter().filter(|c| **c == BACKGROUND).count();
    let (mut l1, mut dssim, mut depth, mut alpha) = (0.0f64, 0.0f64, 0.0f64, 0.0f64);
    for c in 0..3 {
        // Shown and photographed planes, both black where not compared.
        let mut x = vec![0.0f64; n];
        let mut y = vec![0.0f64; n];
        for i in 0..n {
            if class[i] == SCENE {
                x[i] = (correction.0[c] * r.color[i * 3 + c] + correction.1[c]).min(1.0);
                y[i] = planes.rgb[i * 3 + c] as f64 / 255.0;
                l1 += (x[i] - y[i]).abs();
            }
        }
        if weights.ssim > 0.0 {
            let mul = |a: &[f64], b: &[f64]| a.iter().zip(b).map(|(p, q)| p * q).collect::<Vec<f64>>();
            let (mx, my) = (window_mean(&x, w, h), window_mean(&y, w, h));
            let (bxx, byy, bxy) = (window_mean(&mul(&x, &x), w, h), window_mean(&mul(&y, &y), w, h), window_mean(&mul(&x, &y), w, h));
            for i in 0..n {
                if class[i] != SCENE {
                    continue;
                }
                let (sxx, syy, sxy) = (bxx[i] - mx[i] * mx[i], byy[i] - my[i] * my[i], bxy[i] - mx[i] * my[i]);
                let (c1, c2) = (0.01f64 * 0.01, 0.03f64 * 0.03);
                let ssim = (2.0 * mx[i] * my[i] + c1) * (2.0 * sxy + c2) / ((mx[i] * mx[i] + my[i] * my[i] + c1) * (sxx + syy + c2));
                dssim += 1.0 - ssim;
            }
        }
    }
    for (i, class) in class.iter().enumerate() {
        match *class {
            SCENE => {
                if weights.depth > 0.0 && r.alpha[i] >= 0.5 {
                    if let Some((z, trust)) = planes.depth_at(i % w, i / w) {
                        depth += trust * (r.depth[i] / r.alpha[i] - z).abs() / z;
                    }
                }
            }
            BACKGROUND => alpha += r.alpha[i],
            _ => {}
        }
    }
    let per_value = 1.0 / (3.0 * scene.max(1) as f64);
    (1.0 - weights.ssim as f64) * l1 * per_value
        + weights.ssim as f64 * dssim * per_value
        + weights.depth as f64 * depth / scene.max(1) as f64
        + weights.alpha as f64 * alpha / background.max(1) as f64
}

// ---------------------------------------------------------------------------
// Scenes
// ---------------------------------------------------------------------------

/// SplitMix64, uniform floats.
struct Rng(u64);

impl Rng {
    fn unit(&mut self) -> f32 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        ((z ^ (z >> 31)) >> 40) as f32 / (1u64 << 24) as f32
    }

    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.unit()
    }
}

fn logit(p: f32) -> f32 {
    (p / (1.0 - p)).ln()
}

/// An asymmetric camera: off-center principal point, different focal
/// lengths, not looking along an axis.
fn camera(width: u32, height: u32) -> Camera {
    let mut cam = Camera::look_at(Vec3::new(0.35, 0.25, 2.4), Vec3::new(-0.1, 0.05, 0.0), Vec3::new(0.08, 1.0, -0.05).normalize(), 0.9, width, height);
    cam.fy *= 1.07;
    cam.cx += 0.031 * width as f32;
    cam.cy -= 0.043 * height as f32;
    cam
}

struct Spec {
    count: usize,
    /// Standard deviations, meters.
    sigma: (f32, f32),
    /// Box the centers are drawn from, world.
    lo: [f32; 3],
    hi: [f32; 3],
    opacity: (f32, f32),
}

fn random_model(spec: &Spec, seed: u64) -> Gaussians {
    let mut rng = Rng(seed);
    let data = (0..spec.count)
        .map(|_| {
            let mut g = [0.0f32; PARAMS];
            for k in 0..3 {
                g[POS + k] = rng.range(spec.lo[k], spec.hi[k]);
                g[SCALE + k] = rng.range(spec.sigma.0, spec.sigma.1).ln();
                g[COLOR + k] = rng.range(0.05, 0.95);
            }
            for k in 0..4 {
                g[ROT + k] = rng.range(-1.0, 1.0);
            }
            g[OPACITY] = logit(rng.range(spec.opacity.0, spec.opacity.1));
            g
        })
        .collect();
    Gaussians { data }
}

fn random_planes(width: u32, height: u32, dw: u32, dh: u32, seed: u64, classes: bool, center_depth: f32) -> Planes {
    let mut rng = Rng(seed);
    let (n, d) = ((width * height) as usize, (dw * dh) as usize);
    Planes {
        width: width as usize,
        height: height as usize,
        rgb: (0..n * 3).map(|_| rng.range(8.0, 250.0) as u8).collect(),
        dw: dw as usize,
        dh: dh as usize,
        // A slope with a step in the middle: bilinear samples on the slope,
        // none across the step.
        depth_mm: (0..d)
            .map(|i| {
                let (x, y) = (i as u32 % dw, i as u32 / dw);
                let z = center_depth * (0.9 + 0.012 * x as f32 + if y > dh / 2 { 0.4 } else { 0.0 });
                (z * 1000.0 + rng.range(0.0, 6.0)) as u16
            })
            .collect(),
        depth_weight: (0..d).map(|_| [0u8, 96, 255, 255][(rng.unit() * 4.0) as usize % 4]).collect(),
        class: if classes { (0..d).map(|_| [SCENE, SCENE, SCENE, BACKGROUND, IGNORE][(rng.unit() * 5.0) as usize % 5]).collect() } else { Vec::new() },
    }
}

fn to64(model: &Gaussians) -> Vec<G64> {
    model.data.iter().map(|g| g.map(|v| v as f64)).collect()
}

/// Everything the optimiser computes for one view: images, loss, gradients.
struct Analytic {
    rendered: Rendered,
    total: f64,
    grads: ParamGrads,
    correction: [f64; 6],
}

fn analytic(model: &Gaussians, view: &View, correction: &ColorCorrection, planes: &Planes, weights: &LossWeights) -> Analytic {
    let mut raster = CpuRasterizer::new(RasterConfig::default());
    let mut rendered = Rendered::default();
    raster.forward(model, view, &mut rendered);
    let mut image_grads = ImageGrads::default();
    let (value, correction_grad) = loss::evaluate(&rendered, correction, &planes.target(), weights, &mut image_grads, &mut LossScratch::default());
    let mut grads = ParamGrads::default();
    raster.backward(model, view, &image_grads, true, &mut grads);
    Analytic { rendered, total: value.total, grads, correction: correction_grad }
}

/// Largest absolute difference between the optimiser's forward images and
/// the reference's.
fn forward_difference(rendered: &Rendered, reference: &Rendered64) -> f64 {
    let worst = |a: &[f32], b: &[f64]| a.iter().zip(b).map(|(x, y)| (*x as f64 - y).abs()).fold(0.0, f64::max);
    worst(&rendered.color, &reference.color).max(worst(&rendered.depth, &reference.depth)).max(worst(&rendered.alpha, &reference.alpha))
}

struct Case {
    name: &'static str,
    model: Gaussians,
    view: View,
    planes: Planes,
    correction: ColorCorrection,
    weights: LossWeights,
}

/// Compares every analytic gradient of `case` with central differences of
/// the reference loss. Returns the worst relative error.
fn check(case: &Case, picks: usize) -> f64 {
    let Case { name, model, view, planes, correction, weights } = case;
    let done = analytic(model, view, correction, planes, weights);
    let (model64, view64) = (to64(model), View64::of(view));
    let correction64 = (correction.gain.map(|v| v as f64), correction.offset.map(|v| v as f64));
    let (reference, plan) = render64(&model64, &view64, None);

    // The forward pass first: same pictures, same loss.
    let forward = forward_difference(&done.rendered, &reference);
    let total64 = loss64(&reference, &correction64, planes, weights);
    assert!(forward < 2e-4, "{name}: forward images differ by {forward}");
    assert!((done.total - total64).abs() < 2e-5 * total64.abs().max(1.0), "{name}: loss {} against {total64}", done.total);

    let value = |m: &[G64], v: &View64, c: &([f64; 3], [f64; 3])| loss64(&render64(m, v, Some(&plan)).0, c, planes, weights);
    let cap = (MAX_OPACITY as f64 / (1.0 - MAX_OPACITY as f64)).ln();
    // Scale of the gradients of each parameter group over the visible
    // Gaussians: the floor under which a difference is rounding.
    let group = |k: usize| match k {
        k if k < SCALE => 0,
        k if k < ROT => 1,
        k if k < OPACITY => 2,
        OPACITY => 3,
        _ => 4,
    };
    let mut scale = [0.0f64; 5];
    for &i in &done.grads.visible {
        for k in 0..PARAMS {
            scale[group(k)] = scale[group(k)].max(done.grads.grads[i as usize][k].abs() as f64);
        }
    }
    let (mut worst, mut checked, mut nonzero) = (0.0f64, 0usize, 0usize);
    let mut compare = |numeric: f64, analytic: f64, floor: f64, what: String| {
        let error = (numeric - analytic).abs() / (numeric.abs().max(analytic.abs()) + floor);
        assert!(error < 2e-3, "{name}, {what}: numeric {numeric:e}, analytic {analytic:e}, error {error:.2e}");
        worst = worst.max(error);
        checked += 1;
        nonzero += (numeric.abs() > floor) as usize;
    };
    // The Gaussians with the largest gradients, plus a few arbitrary ones.
    let mut order: Vec<u32> = done.grads.visible.clone();
    order.sort_by(|a, b| {
        let size = |i: &u32| done.grads.grads[*i as usize].iter().map(|v| v.abs()).fold(0.0, f32::max);
        size(b).total_cmp(&size(a))
    });
    let mut chosen: Vec<usize> = order.iter().take(picks).map(|&i| i as usize).collect();
    chosen.extend(done.grads.visible.iter().step_by((done.grads.visible.len() / picks.max(1)).max(1)).map(|&i| i as usize));
    chosen.sort_unstable();
    chosen.dedup();
    for &i in &chosen {
        for k in 0..PARAMS {
            if k == OPACITY && model64[i][k] >= cap {
                // At the cap the forward pass is flat and the backward pass
                // keeps the slope on purpose: nothing to compare.
                continue;
            }
            let eps = if k < SCALE { 1e-6 } else { 1e-5 };
            let (mut plus, mut minus) = (model64.clone(), model64.clone());
            plus[i][k] += eps;
            minus[i][k] -= eps;
            let numeric = (value(&plus, &view64, &correction64) - value(&minus, &view64, &correction64)) / (2.0 * eps);
            compare(numeric, done.grads.grads[i][k] as f64, 2e-3 * scale[group(k)], format!("Gaussian {i} parameter {k}"));
        }
    }
    // Gaussians that are not drawn have no gradient.
    for (i, g) in done.grads.grads.iter().enumerate() {
        if !done.grads.visible.contains(&(i as u32)) {
            assert!(g.iter().all(|v| *v == 0.0), "{name}: hidden Gaussian {i} has a gradient");
        }
    }
    // The camera twist.
    let pose_scale = done.grads.pose.iter().map(|v| v.abs()).fold(0.0, f64::max);
    for k in 0..6 {
        let eps = 1e-6;
        let mut twist = [0.0f64; 6];
        twist[k] = eps;
        let plus = value(&model64, &view64.moved(twist), &correction64);
        twist[k] = -eps;
        let minus = value(&model64, &view64.moved(twist), &correction64);
        compare((plus - minus) / (2.0 * eps), done.grads.pose[k], 2e-3 * pose_scale, format!("twist {k}"));
    }
    // The colour correction: gains, then offsets.
    let correction_scale = done.correction.iter().map(|v| v.abs()).fold(0.0, f64::max);
    for k in 0..6 {
        let eps = 1e-6;
        let (mut plus, mut minus) = (correction64, correction64);
        if k < 3 {
            plus.0[k] += eps;
            minus.0[k] -= eps;
        } else {
            plus.1[k - 3] += eps;
            minus.1[k - 3] -= eps;
        }
        let numeric = (loss64(&reference, &plus, planes, weights) - loss64(&reference, &minus, planes, weights)) / (2.0 * eps);
        compare(numeric, done.correction[k], 2e-3 * correction_scale, format!("correction {k}"));
    }
    assert!(nonzero * 2 > checked, "{name}: only {nonzero} of {checked} gradients are away from zero");
    eprintln!("{name}: forward within {forward:.1e}, worst gradient error {worst:.2e} over {checked} values ({} Gaussians)", chosen.len());
    worst
}

fn all_terms() -> LossWeights {
    LossWeights { ssim: 0.2, depth: 0.2, alpha: 0.5 }
}

fn skewed() -> ColorCorrection {
    ColorCorrection { gain: [1.12, 0.91, 1.04], offset: [0.02, -0.03, 0.01] }
}

#[test]
fn overlapping_gaussians_whole_loss() {
    // Large translucent Gaussians: every pixel blends many of them.
    let spec = Spec { count: 40, sigma: (0.06, 0.3), lo: [-0.9, -0.7, -0.8], hi: [0.9, 0.7, 0.8], opacity: (0.1, 0.9) };
    let view = View::from_camera(&camera(44, 34), [0.21, 0.52, 0.83]);
    for (seed, classes) in [(11u64, false), (12, true)] {
        let case = Case {
            name: if classes { "overlapping, classes" } else { "overlapping" },
            model: random_model(&spec, seed),
            view,
            planes: random_planes(44, 34, 13, 10, seed + 100, classes, 2.4),
            correction: skewed(),
            weights: all_terms(),
        };
        check(&case, 8);
    }
}

#[test]
fn each_term_of_the_loss_alone() {
    let spec = Spec { count: 30, sigma: (0.05, 0.25), lo: [-0.9, -0.7, -0.8], hi: [0.9, 0.7, 0.8], opacity: (0.2, 0.9) };
    let view = View::from_camera(&camera(40, 30), [0.4, 0.3, 0.6]);
    for (name, weights) in [
        ("L1 alone", LossWeights { ssim: 0.0, depth: 0.0, alpha: 0.0 }),
        ("structural term alone", LossWeights { ssim: 1.0, depth: 0.0, alpha: 0.0 }),
        ("depth and coverage", LossWeights { ssim: 0.5, depth: 3.0, alpha: 2.0 }),
    ] {
        let case = Case { name, model: random_model(&spec, 21), view, planes: random_planes(40, 30, 10, 8, 22, true, 2.4), correction: skewed(), weights };
        check(&case, 6);
    }
}

#[test]
fn gaussians_clipped_by_the_image_and_beyond_the_frustum() {
    // Centers up to three image widths outside: clipped rectangles, and
    // the clamp of the Jacobian for those beyond 1.3 times the frustum.
    let spec = Spec { count: 60, sigma: (0.1, 0.5), lo: [-4.5, -3.5, -1.0], hi: [4.5, 3.5, 1.0], opacity: (0.2, 0.95) };
    let view = View::from_camera(&camera(40, 30), [0.1, 0.1, 0.1]);
    let case = Case { name: "clipped", model: random_model(&spec, 31), view, planes: random_planes(40, 30, 10, 8, 32, false, 2.4), correction: skewed(), weights: all_terms() };
    let model64 = to64(&case.model);
    let view64 = View64::of(&view);
    // The scene must exercise what it is meant to.
    let outside = model64.iter().filter_map(|g| project64(g, &view64)).filter(|p| p.px < -6.0 || p.px > 46.0 || p.py < -4.5 || p.py > 34.5).count();
    assert!(outside >= 15, "only {outside} Gaussians beyond the clamp");
    check(&case, 10);
}

#[test]
fn gaussians_across_the_near_plane() {
    // A cloud around the camera: centers in front of, on and behind the
    // near plane, large enough to reach across it.
    let cam = camera(40, 30);
    let eye = cam.position();
    let forward = cam.forward();
    let mut model = random_model(&Spec { count: 50, sigma: (0.02, 0.2), lo: [-0.3, -0.3, -0.3], hi: [0.3, 0.3, 0.3], opacity: (0.1, 0.8) }, 41);
    for g in &mut model.data {
        let p = eye + forward * 0.25 + Vec3::new(g[POS], g[POS + 1], g[POS + 2]);
        g[POS..POS + 3].copy_from_slice(&p.to_array());
    }
    let view = View::from_camera(&cam, [0.7, 0.2, 0.3]);
    let view64 = View64::of(&view);
    let model64 = to64(&model);
    let drawn = model64.iter().filter(|g| project64(g, &view64).is_some()).count();
    let close = model64.iter().filter_map(|g| project64(g, &view64)).filter(|p| p.depth < 0.15).count();
    assert!(drawn < 45 && drawn > 20 && close >= 4, "{drawn} drawn, {close} within 15 cm");
    let case = Case { name: "near plane", model, view, planes: random_planes(40, 30, 10, 8, 42, false, 0.3), correction: skewed(), weights: all_terms() };
    check(&case, 10);
}

#[test]
fn sub_pixel_gaussians() {
    // Far smaller than a pixel: the 0.3 px² dilation is the footprint.
    let spec = Spec { count: 500, sigma: (0.0004, 0.004), lo: [-0.8, -0.6, -0.2], hi: [0.8, 0.6, 0.2], opacity: (0.3, 0.99) };
    let view = View::from_camera(&camera(36, 28), [0.0, 0.0, 0.0]);
    let case = Case { name: "sub-pixel", model: random_model(&spec, 51), view, planes: random_planes(36, 28, 9, 7, 52, true, 2.4), correction: skewed(), weights: all_terms() };
    check(&case, 12);
}

#[test]
fn opaque_layers_and_capped_opacities() {
    // Walls of nearly opaque surfels behind each other: pixels that stop
    // early, Gaussians at the opacity cap, colours that clip at white.
    let mut model = random_model(&Spec { count: 400, sigma: (0.08, 0.25), lo: [-1.0, -0.8, -0.6], hi: [1.0, 0.8, 0.6], opacity: (0.9, 0.994) }, 61);
    for (i, g) in model.data.iter_mut().enumerate() {
        // Flat discs.
        g[SCALE + 2] = 0.002f32.ln();
        if i % 5 == 0 {
            g[OPACITY] = 9.0;
        }
        if i % 7 == 0 {
            g[COLOR..COLOR + 3].copy_from_slice(&[0.99, 0.97, 1.0]);
        }
    }
    let view = View::from_camera(&camera(40, 30), [0.9, 0.9, 0.9]);
    let done = {
        let planes = random_planes(40, 30, 10, 8, 62, false, 2.4);
        let case = Case { name: "opaque layers", model: model.clone(), view, planes, correction: ColorCorrection { gain: [1.2, 1.1, 1.3], offset: [0.0; 3] }, weights: all_terms() };
        check(&case, 10);
        analytic(&case.model, &case.view, &case.correction, &case.planes, &case.weights)
    };
    // Documented behaviour at the cap: the logit keeps the slope of the
    // capped opacity, so that a parked Gaussian can become translucent again.
    let capped: Vec<usize> = done.grads.visible.iter().map(|&i| i as usize).filter(|&i| model.data[i][OPACITY] == 9.0).collect();
    assert!(capped.len() > 20 && capped.iter().any(|&i| done.grads.grads[i][OPACITY] != 0.0));
    let saturated = done.rendered.alpha.iter().filter(|a| **a > 0.9999).count();
    assert!(saturated > 100, "only {saturated} pixels saturate");
}

#[test]
fn the_tiles_and_rectangles_lose_nothing_at_larger_sizes() {
    // Forward only, at a size with many tiles and a ragged last row and
    // column, against the reference that knows no tiles.
    for (seed, sigma, count) in [(71u64, (0.004, 0.03), 900usize), (72, (0.02, 0.4), 200), (73, (0.0005, 0.006), 3000)] {
        let spec = Spec { count, sigma, lo: [-2.0, -1.5, -1.0], hi: [2.0, 1.5, 1.0], opacity: (0.05, 0.995) };
        let model = random_model(&spec, seed);
        let view = View::from_camera(&camera(150, 109), [0.3, 0.6, 0.1]);
        let mut rendered = Rendered::default();
        CpuRasterizer::default().forward(&model, &view, &mut rendered);
        let (reference, _) = render64(&to64(&model), &View64::of(&view), None);
        let difference = forward_difference(&rendered, &reference);
        // An f32 depth sum of a few meters carries about 1e-6 per term.
        assert!(difference < 3e-4, "seed {seed}: forward images differ by {difference}");
        assert!(rendered.alpha.iter().filter(|a| **a > 0.05).count() > 2000, "seed {seed}: empty picture");
    }
}

#[test]
fn the_viewer_draws_the_same_pixels() {
    // `gs_core::raster` with the classic dilation is what a viewer shows for
    // an imported file. It sorts by a 16-bit depth key, so two splats at
    // nearly the same depth may swap; with a few dozen splats none do, and
    // then every pixel of the three renderers is the same 8-bit value.
    use gs_core::raster::{rasterize, RasterOptions};
    let background = [31u8, 96, 160];
    for (seed, sigma, count) in [(81u64, (0.02, 0.3), 40), (82, (0.004, 0.05), 120), (83, (0.1, 0.6), 40)] {
        let spec = Spec { count, sigma, lo: [-1.6, -1.2, -1.0], hi: [1.6, 1.2, 1.0], opacity: (0.05, 0.995) };
        // Through 8-bit splats, so that all three start from the same numbers.
        let splats = random_model(&spec, seed).to_splats();
        let model = Gaussians::from_splats(&splats, 1.0, 1.0, 0.0);
        let cam = camera(96, 72);
        let viewer = rasterize(&splats, &cam, &RasterOptions { background, classic_dilation: true, ..Default::default() });
        let view = View::from_camera(&cam, background.map(|v| v as f32 / 255.0));
        let mut mine = Rendered::default();
        CpuRasterizer::default().forward(&model, &view, &mut mine);
        let (reference, _) = render64(&to64(&model), &View64::of(&view), None);
        let mine = mine.to_image();
        let (mut worst_viewer, mut worst_mine, mut drawn) = (0u8, 0u8, 0usize);
        for (i, value) in reference.color.iter().enumerate() {
            let expected = (value * 255.0 + 0.5) as u8;
            worst_viewer = worst_viewer.max(viewer.rgb[i].abs_diff(expected));
            worst_mine = worst_mine.max(mine.rgb[i].abs_diff(expected));
            drawn += (reference.alpha[i / 3] > 0.05) as usize;
        }
        assert!(worst_viewer <= 1 && worst_mine <= 1, "seed {seed}: the viewer is off by {worst_viewer} levels, the optimiser by {worst_mine}");
        assert!(drawn > 600, "seed {seed}: only {drawn} values drawn");
    }
}
