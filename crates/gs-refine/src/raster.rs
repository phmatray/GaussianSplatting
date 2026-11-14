//! Differentiable Gaussian rasterizer on the CPU.
//!
//! The forward pass draws the same picture as the viewer does for an
//! imported splat file (`gs_core::raster` with `classic_dilation`, the
//! algorithm documented in `gs_core::splat`): EWA projection with the
//! Jacobian clamped to 1.3 times the frustum, a `0.3 px²` dilation that
//! leaves the opacity alone, a cut-off at three standard deviations and at
//! an alpha of 1/255, "over" compositing in depth order on sRGB values.
//! It composites front to back per 16x16 tile so that a pixel can stop once
//! it is opaque; the result is the same sum.
//!
//! Besides the colour it renders two more channels with the same weights:
//! the accumulated depth `Σ dᵢ·αᵢ·Tᵢ` (camera-axis depth of the centers)
//! and the coverage `Σ αᵢ·Tᵢ`.
//!
//! The backward pass takes the gradient of a loss with respect to those
//! three images and returns it with respect to every parameter of every
//! visible Gaussian ([`crate::model`]) and, on request, to a small motion of
//! the camera. Every formula is checked against finite differences in the
//! tests of this module.
//!
//! # Structure (what a GPU port replaces)
//!
//! 1. *Project* ([`project`]): per Gaussian, independent. Center, depth,
//!    conic, opacity, colour, pixel rectangle.
//! 2. *Bin and sort*: every visible Gaussian is listed in the tiles its
//!    rectangle touches; each tile's list is sorted by `(depth, index)`.
//! 3. *Composite* (`forward_tile`): per pixel, front to back.
//! 4. *Backward, per pixel* (`backward_tile`): replays the pixel, then walks
//!    it back to front and adds the gradient of every contribution to the
//!    Gaussian's ten screen-space quantities ([`ScreenGrad`]).
//! 5. *Backward, per Gaussian* ([`chain`]): screen-space gradient to
//!    parameter gradient, independent per Gaussian.
//!
//! Steps 1 and 3 to 5 run in parallel (rayon). The sums of step 4 are kept
//! per `(tile, Gaussian)` pair and added up in tile order afterwards, so
//! the result does not depend on the number of threads.

use std::time::Instant;

use glam::{Mat3, Quat, Vec3};
use gs_core::Camera;
use rayon::prelude::*;

use crate::model::{sigmoid, Gaussian, Gaussians, COLOR, MAX_OPACITY, OPACITY, PARAMS, POS, ROT, SCALE};

/// Side of a tile in pixels.
pub const TILE: usize = 16;
const TILE_PIXELS: usize = TILE * TILE;
/// Channels of the tile-major scratch image: colour, depth, coverage.
const CHANNELS: usize = 5;

/// Constants of the image formation model. The defaults are those of the
/// viewer; the gradient tests switch the cut-offs off to get a smooth
/// function.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RasterConfig {
    /// Largest Mahalanobis distance² that contributes (3σ: 9).
    pub cutoff_m2: f32,
    /// Contributions with a smaller alpha are skipped.
    pub min_alpha: f32,
    /// A pixel stops once less than this much light gets through.
    pub min_transmittance: f32,
    /// Dilation of the screen-space covariance, pixel².
    pub low_pass: f32,
    /// Gaussians whose center is closer than this are not drawn, meters.
    pub near: f32,
}

impl Default for RasterConfig {
    fn default() -> Self {
        Self {
            cutoff_m2: gs_core::splat::CUTOFF_MAHALANOBIS2,
            min_alpha: gs_core::splat::MIN_ALPHA,
            min_transmittance: 1.0e-4,
            low_pass: gs_core::splat::LOW_PASS_PX2,
            near: gs_core::splat::DEFAULT_NEAR,
        }
    }
}

/// A calibrated view: the conventions of `docs/ARCHITECTURE.md` (camera
/// looks along -Z, +Y up, pixel centers at +0.5, `py` grows downward).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct View {
    pub width: u32,
    pub height: u32,
    pub fx: f32,
    pub fy: f32,
    pub cx: f32,
    pub cy: f32,
    /// World-to-camera rotation.
    pub rot: Mat3,
    /// World-to-camera translation: `t_cam = rot · p + trans`.
    pub trans: Vec3,
    /// Colour behind everything, `0..=1`.
    pub background: [f32; 3],
}

impl View {
    pub fn from_camera(cam: &Camera, background: [f32; 3]) -> Self {
        let cam_from_world = cam.world_from_cam.inverse();
        Self {
            width: cam.width,
            height: cam.height,
            fx: cam.fx,
            fy: cam.fy,
            cx: cam.cx,
            cy: cam.cy,
            rot: Mat3::from(cam_from_world.matrix3),
            trans: Vec3::from(cam_from_world.translation),
            background,
        }
    }

    /// The same view moved by a small rigid motion of the camera frame:
    /// camera-space points become `exp(ω)·t + τ` with `twist = [ω, τ]`.
    /// This is the motion the pose gradient of [`ParamGrads::pose`] refers to.
    pub fn moved(&self, twist: [f32; 6]) -> Self {
        let turn = Mat3::from_quat(Quat::from_scaled_axis(Vec3::new(twist[0], twist[1], twist[2])));
        Self { rot: turn * self.rot, trans: turn * self.trans + Vec3::new(twist[3], twist[4], twist[5]), ..*self }
    }

    pub fn pixels(&self) -> usize {
        self.width as usize * self.height as usize
    }
}

/// The three images of a forward pass, row-major from the top-left.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Rendered {
    pub width: u32,
    pub height: u32,
    /// 3 floats per pixel, `0..=1` (background included).
    pub color: Vec<f32>,
    /// `Σ dᵢ·αᵢ·Tᵢ`, meters; divide by `alpha` for the mean depth.
    pub depth: Vec<f32>,
    /// Coverage `Σ αᵢ·Tᵢ = 1 − T`.
    pub alpha: Vec<f32>,
}

impl Rendered {
    /// The colour as an 8-bit image, rounded like `gs_core::raster`.
    pub fn to_image(&self) -> gs_core::raster::Image {
        gs_core::raster::Image {
            width: self.width,
            height: self.height,
            rgb: self.color.iter().map(|&v| (v * 255.0 + 0.5) as u8).collect(),
        }
    }
}

/// Gradient of a loss with respect to the images of [`Rendered`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ImageGrads {
    pub color: Vec<f32>,
    pub depth: Vec<f32>,
    pub alpha: Vec<f32>,
}

impl ImageGrads {
    /// All zeros for an image of `pixels` pixels.
    pub fn zeros(pixels: usize) -> Self {
        Self { color: vec![0.0; pixels * 3], depth: vec![0.0; pixels], alpha: vec![0.0; pixels] }
    }

    pub fn reset(&mut self, pixels: usize) {
        for (buffer, len) in [(&mut self.color, pixels * 3), (&mut self.depth, pixels), (&mut self.alpha, pixels)] {
            buffer.clear();
            buffer.resize(len, 0.0);
        }
    }
}

/// Result of a backward pass.
#[derive(Clone, Debug, Default)]
pub struct ParamGrads {
    /// Gradient per Gaussian, valid for the indices in `visible` and zero
    /// elsewhere.
    pub grads: Vec<Gaussian>,
    /// Gaussians that were listed in at least one tile, ascending.
    pub visible: Vec<u32>,
    /// Per entry of `visible`: norm of the gradient with respect to the
    /// center on screen, in units of half the image (the "view-space
    /// gradient" that drives densification in 3DGS).
    pub screen_grad: Vec<f32>,
    /// Per entry of `visible`: longest standard deviation on screen, pixels.
    pub radius_px: Vec<f32>,
    /// Gradient with respect to the twist `[ω, τ]` of [`View::moved`] at
    /// zero; zeros unless asked for.
    pub pose: [f64; 6],
}

/// The seam between the optimiser and the renderer. `backward` refers to
/// the last `forward` and must be given the same model and view.
pub trait Rasterizer {
    fn forward(&mut self, model: &Gaussians, view: &View, out: &mut Rendered);
    fn backward(&mut self, model: &Gaussians, view: &View, image_grads: &ImageGrads, want_pose: bool, out: &mut ParamGrads);
}

/// One projected Gaussian (step 1).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Projected {
    pub px: f32,
    pub py: f32,
    /// Distance along the camera axis, meters.
    pub depth: f32,
    /// Inverse of the dilated screen covariance: `m = A·dx² + 2B·dx·dy + C·dy²`.
    pub conic: [f32; 3],
    pub opacity: f32,
    pub color: [f32; 3],
    /// Tile rectangle, end-exclusive; empty when the Gaussian is not drawn.
    pub tiles: [u16; 4],
    /// Longest standard deviation on screen, pixels.
    pub radius_px: f32,
}

impl Projected {
    #[inline]
    fn is_visible(&self) -> bool {
        self.tiles[0] < self.tiles[1] && self.tiles[2] < self.tiles[3]
    }
}

/// Everything about one Gaussian in one view that both passes need.
struct Geometry {
    /// Center in camera space and its depth `d = −t.z`.
    t: Vec3,
    d: f32,
    /// Direction used by the Jacobian (clamped) and whether it was left
    /// alone.
    uc: f32,
    vc: f32,
    u_free: bool,
    v_free: bool,
    /// Rows of `T = J·W`.
    t0: Vec3,
    t1: Vec3,
    /// Unit quaternion, the length of the raw one, and its matrix.
    q: [f32; 4],
    q_len: f32,
    r: Mat3,
    sigma: Vec3,
    /// `M = R·diag(σ)`, so that the covariance is `M·Mᵀ`.
    m: Mat3,
    /// Dilated screen covariance `[[a, b], [b, c]]` and its determinant.
    a: f32,
    b: f32,
    c: f32,
    det: f32,
    px: f32,
    py: f32,
}

#[inline]
fn rows(m: &Mat3) -> (Vec3, Vec3, Vec3) {
    (
        Vec3::new(m.x_axis.x, m.y_axis.x, m.z_axis.x),
        Vec3::new(m.x_axis.y, m.y_axis.y, m.z_axis.y),
        Vec3::new(m.x_axis.z, m.y_axis.z, m.z_axis.z),
    )
}

fn geometry(g: &Gaussian, view: &View, cfg: &RasterConfig) -> Option<Geometry> {
    let p = Vec3::new(g[POS], g[POS + 1], g[POS + 2]);
    let (r0, r1, r2) = rows(&view.rot);
    let t = Vec3::new(r0.dot(p), r1.dot(p), r2.dot(p)) + view.trans;
    let d = -t.z;
    // Written so that a NaN depth is rejected too.
    if !(d >= cfg.near && d > 0.0) {
        return None;
    }
    let inv_d = 1.0 / d;
    let (u, v) = (t.x * inv_d, t.y * inv_d);
    let (w, h) = (view.width as f32, view.height as f32);
    let margin = 0.5 * (gs_core::splat::FRUSTUM_CLAMP - 1.0);
    let uc = u.max((-view.cx - margin * w) / view.fx).min((w - view.cx + margin * w) / view.fx);
    let vc = v.max((view.cy - h - margin * h) / view.fy).min((view.cy + margin * h) / view.fy);
    let t0 = (r0 + r2 * uc) * (view.fx * inv_d);
    let t1 = (r1 + r2 * vc) * (-view.fy * inv_d);

    let raw = [g[ROT], g[ROT + 1], g[ROT + 2], g[ROT + 3]];
    let q_len = raw.iter().map(|c| c * c).sum::<f32>().sqrt();
    let (q, q_len) = if q_len.is_finite() && q_len > 1e-12 { (raw.map(|c| c / q_len), q_len) } else { ([0.0, 0.0, 0.0, 1.0], 1.0) };
    let r = Mat3::from_quat(Quat::from_xyzw(q[0], q[1], q[2], q[3]));
    let sigma = Vec3::new(g[SCALE].exp(), g[SCALE + 1].exp(), g[SCALE + 2].exp());
    let m = Mat3::from_cols(r.x_axis * sigma.x, r.y_axis * sigma.y, r.z_axis * sigma.z);
    // T·Σ·Tᵀ = (Mᵀ·T)·(Mᵀ·T)ᵀ, a sum of squares.
    let (mt0, mt1) = (m.transpose() * t0, m.transpose() * t1);
    let (ma, mb, mc) = (mt0.dot(mt0), mt0.dot(mt1), mt1.dot(mt1));
    let lp = cfg.low_pass;
    // The form of `gs_core::splat`, step 6: cancellation in det(M) cannot
    // make the determinant smaller than the dilation allows.
    let det = (ma * mc - mb * mb).max(0.0) + lp * (ma + mc) + lp * lp;
    let (px, py) = (view.cx + view.fx * u, view.cy - view.fy * v);
    if !(det > 0.0 && det.is_finite() && px.is_finite() && py.is_finite()) {
        return None;
    }
    Some(Geometry { t, d, uc, vc, u_free: uc == u, v_free: vc == v, t0, t1, q, q_len, r, sigma, m, a: ma + lp, b: mb, c: mc + lp, det, px, py })
}

/// Step 1: projects one Gaussian. The result has an empty tile rectangle
/// when the Gaussian is not drawn.
pub fn project(g: &Gaussian, view: &View, cfg: &RasterConfig) -> Projected {
    let hidden = Projected::default();
    let opacity = sigmoid(g[OPACITY]).min(MAX_OPACITY);
    if opacity.is_nan() || opacity < cfg.min_alpha {
        return hidden;
    }
    let Some(geo) = geometry(g, view, cfg) else { return hidden };
    let inv_det = 1.0 / geo.det;
    // The ellipse on which alpha falls to `min_alpha`, at most the cut-off:
    // its axis-aligned half-extents are √(m²·Σ'ₖₖ).
    let reach = if cfg.min_alpha > 0.0 { (2.0 * (opacity / cfg.min_alpha).ln()).min(cfg.cutoff_m2) } else { cfg.cutoff_m2 };
    let (ex, ey) = ((reach * geo.a).sqrt(), (reach * geo.c).sqrt());
    let (w, h) = (view.width as f32, view.height as f32);
    // Pixel i has its center at i + 0.5. Saturating casts clip to the image.
    let x0 = (geo.px - ex - 0.5).ceil().max(0.0) as usize;
    let y0 = (geo.py - ey - 0.5).ceil().max(0.0) as usize;
    let x1 = ((geo.px + ex - 0.5).floor() + 1.0).clamp(0.0, w) as usize;
    let y1 = ((geo.py + ey - 0.5).floor() + 1.0).clamp(0.0, h) as usize;
    if x0 >= x1 || y0 >= y1 {
        return hidden;
    }
    let mid = 0.5 * (geo.a + geo.c);
    let lambda1 = mid + (0.5 * (geo.a - geo.c)).hypot(geo.b);
    Projected {
        px: geo.px,
        py: geo.py,
        depth: geo.d,
        conic: [geo.c * inv_det, -geo.b * inv_det, geo.a * inv_det],
        opacity,
        color: [g[COLOR], g[COLOR + 1], g[COLOR + 2]],
        tiles: [(x0 / TILE) as u16, x1.div_ceil(TILE) as u16, (y0 / TILE) as u16, y1.div_ceil(TILE) as u16],
        radius_px: lambda1.max(0.0).sqrt(),
    }
}

/// Gradient of the loss with respect to the screen-space quantities of one
/// Gaussian: what step 4 accumulates and step 5 consumes.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ScreenGrad {
    pub px: f32,
    pub py: f32,
    /// With respect to the three conic terms `A, B, C` (`B` counted once).
    pub conic: [f32; 3],
    pub opacity: f32,
    pub color: [f32; 3],
    pub depth: f32,
}

impl ScreenGrad {
    #[inline]
    fn add(&mut self, o: &ScreenGrad) {
        self.px += o.px;
        self.py += o.py;
        for k in 0..3 {
            self.conic[k] += o.conic[k];
            self.color[k] += o.color[k];
        }
        self.opacity += o.opacity;
        self.depth += o.depth;
    }
}

/// Step 5: the gradient with respect to the parameters of one Gaussian,
/// given the gradient with respect to its screen-space quantities. Also
/// returns the gradient with respect to the camera twist `[ω, τ]`.
pub fn chain(g: &Gaussian, view: &View, cfg: &RasterConfig, sg: &ScreenGrad) -> Option<(Gaussian, [f32; 6])> {
    let geo = geometry(g, view, cfg)?;
    let mut out = [0.0f32; PARAMS];
    let (r0, r1, r2) = rows(&view.rot);

    // Conic K = Σ'⁻¹: dL/dΣ' = −K·G·K with G the symmetric gradient matrix.
    let inv_det = 1.0 / geo.det;
    let (ka, kb, kc) = (geo.c * inv_det, -geo.b * inv_det, geo.a * inv_det);
    let [qa, qb, qc] = sg.conic;
    let x00 = ka * qa + 0.5 * kb * qb;
    let x01 = 0.5 * ka * qb + kb * qc;
    let x10 = kb * qa + 0.5 * kc * qb;
    let x11 = 0.5 * kb * qb + kc * qc;
    let g_a = -(x00 * ka + x01 * kb);
    let g_b = -2.0 * (x00 * kb + x01 * kc);
    let g_c = -(x10 * kb + x11 * kc);

    // a = T0ᵀΣT0, b = T0ᵀΣT1, c = T1ᵀΣT1 with Σ = M·Mᵀ.
    let (mt0, mt1) = (geo.m.transpose() * geo.t0, geo.m.transpose() * geo.t1);
    // dL/dM, column j: 2·[(g_a·α0 + g_b/2·α1)·T0 + (g_b/2·α0 + g_c·α1)·T1].
    let column = |j: usize| (geo.t0 * (g_a * mt0[j] + 0.5 * g_b * mt1[j]) + geo.t1 * (0.5 * g_b * mt0[j] + g_c * mt1[j])) * 2.0;
    let dm = [column(0), column(1), column(2)];
    let r_cols = [geo.r.x_axis, geo.r.y_axis, geo.r.z_axis];
    for j in 0..3 {
        // d/d log σ = σ · d/dσ.
        out[SCALE + j] = geo.sigma[j] * r_cols[j].dot(dm[j]);
    }
    // H[i][j] = dL/dR_ij, R row i column j.
    let h = |i: usize, j: usize| dm[j][i] * geo.sigma[j];
    let [x, y, z, w] = geo.q;
    let gq = [
        2.0 * (y * h(0, 1) + z * h(0, 2) + y * h(1, 0) - 2.0 * x * h(1, 1) - w * h(1, 2) + z * h(2, 0) + w * h(2, 1) - 2.0 * x * h(2, 2)),
        2.0 * (-2.0 * y * h(0, 0) + x * h(0, 1) + w * h(0, 2) + x * h(1, 0) + z * h(1, 2) - w * h(2, 0) + z * h(2, 1) - 2.0 * y * h(2, 2)),
        2.0 * (-2.0 * z * h(0, 0) - w * h(0, 1) + x * h(0, 2) + w * h(1, 0) - 2.0 * z * h(1, 1) + y * h(1, 2) + x * h(2, 0) + y * h(2, 1)),
        2.0 * (-z * h(0, 1) + y * h(0, 2) + z * h(1, 0) - x * h(1, 2) - y * h(2, 0) + x * h(2, 1)),
    ];
    // Through the normalisation q = raw / |raw|.
    let along: f32 = (0..4).map(|k| geo.q[k] * gq[k]).sum();
    for k in 0..4 {
        out[ROT + k] = (gq[k] - geo.q[k] * along) / geo.q_len;
    }

    // dL/dT0 and dL/dT1 (Σ·T = M·(Mᵀ·T)).
    let (st0, st1) = (geo.m * mt0, geo.m * mt1);
    let e0 = st0 * (2.0 * g_a) + st1 * g_b;
    let e1 = st1 * (2.0 * g_c) + st0 * g_b;

    // Camera-space center: through the pixel position, the depth channel,
    // and the Jacobian inside T.
    let inv_d = 1.0 / geo.d;
    let inv_d2 = inv_d * inv_d;
    let (fx, fy) = (view.fx, view.fy);
    let mut gt = Vec3::new(sg.px * fx * inv_d, -sg.py * fy * inv_d, 0.0);
    let mut gd = (-sg.px * fx * geo.t.x + sg.py * fy * geo.t.y) * inv_d2 + sg.depth;
    let (s0, q0, s1, q1) = (e0.dot(r2), e0.dot(r0), e1.dot(r2), e1.dot(r1));
    if geo.u_free {
        gt.x += fx * inv_d2 * s0;
        gd -= fx * inv_d2 * (q0 + 2.0 * geo.t.x * inv_d * s0);
    } else {
        gd -= fx * inv_d2 * (q0 + geo.uc * s0);
    }
    if geo.v_free {
        gt.y -= fy * inv_d2 * s1;
        gd += fy * inv_d2 * (q1 + 2.0 * geo.t.y * inv_d * s1);
    } else {
        gd += fy * inv_d2 * (q1 + geo.vc * s1);
    }
    gt.z = -gd;
    let gp = r0 * gt.x + r1 * gt.y + r2 * gt.z;
    out[POS..POS + 3].copy_from_slice(&gp.to_array());

    // At the cap the slope of the sigmoid is kept: the cap is where the
    // optimiser parks a logit, and it must be able to leave again.
    let opacity = sigmoid(g[OPACITY]).min(MAX_OPACITY);
    out[OPACITY] = sg.opacity * opacity * (1.0 - opacity);
    out[COLOR..COLOR + 3].copy_from_slice(&sg.color);

    // Camera twist: the center moves by ω × t + τ and W turns by [ω]×.
    let j0 = Vec3::new(fx * inv_d, 0.0, fx * geo.uc * inv_d);
    let j1 = Vec3::new(0.0, -fy * inv_d, -fy * geo.vc * inv_d);
    let omega = geo.t.cross(gt) + (view.rot * e0).cross(j0) + (view.rot * e1).cross(j1);
    Some((out, [omega.x, omega.y, omega.z, gt.x, gt.y, gt.z]))
}

/// The tile's copy of what a pixel needs from a Gaussian: the pixel loops
/// then read memory in order instead of chasing indices.
#[derive(Clone, Copy)]
struct Local {
    px: f32,
    py: f32,
    conic: [f32; 3],
    opacity: f32,
    color: [f32; 3],
    depth: f32,
}

fn gather(items: &[u64], projected: &[Projected], local: &mut Vec<Local>) {
    local.clear();
    local.extend(items.iter().map(|&key| {
        let p = &projected[key as u32 as usize];
        Local { px: p.px, py: p.py, conic: p.conic, opacity: p.opacity, color: p.color, depth: p.depth }
    }));
}

/// Alpha of `g` at a pixel center, or `None` when it does not contribute.
#[inline(always)]
fn alpha_at(g: &Local, x: f32, y: f32, cfg: &RasterConfig) -> Option<(f32, f32, f32)> {
    let (dx, dy) = (x - g.px, y - g.py);
    let m = g.conic[0] * dx * dx + 2.0 * g.conic[1] * dx * dy + g.conic[2] * dy * dy;
    if m.is_nan() || m > cfg.cutoff_m2 {
        return None;
    }
    // The conic form can cancel to a hair below zero.
    let alpha = g.opacity * (-0.5 * m.max(0.0)).exp();
    (alpha >= cfg.min_alpha).then_some((alpha, dx, dy))
}

/// Step 3 for one tile: `out` holds [`CHANNELS`] floats per pixel of the
/// tile, row-major within the tile.
fn forward_tile((tx, ty): (usize, usize), local: &[Local], view: &View, cfg: &RasterConfig, out: &mut [f32]) {
    let (width, height) = (view.width as usize, view.height as usize);
    for iy in 0..TILE {
        let yi = ty * TILE + iy;
        if yi >= height {
            break;
        }
        let y = yi as f32 + 0.5;
        for ix in 0..TILE {
            let xi = tx * TILE + ix;
            if xi >= width {
                break;
            }
            let x = xi as f32 + 0.5;
            let mut transmittance = 1.0f32;
            let (mut color, mut depth) = ([0.0f32; 3], 0.0f32);
            for g in local {
                let Some((alpha, ..)) = alpha_at(g, x, y, cfg) else { continue };
                let weight = alpha * transmittance;
                color[0] += weight * g.color[0];
                color[1] += weight * g.color[1];
                color[2] += weight * g.color[2];
                depth += weight * g.depth;
                transmittance *= 1.0 - alpha;
                if transmittance < cfg.min_transmittance {
                    break;
                }
            }
            let o = &mut out[(iy * TILE + ix) * CHANNELS..][..CHANNELS];
            for k in 0..3 {
                o[k] = color[k] + transmittance * view.background[k];
            }
            o[3] = depth;
            o[4] = 1.0 - transmittance;
        }
    }
}

/// Step 4 for one tile: adds to `out[k]` the gradient of the `k`-th
/// Gaussian of the tile's list.
fn backward_tile(
    (tx, ty): (usize, usize),
    local: &[Local],
    view: &View,
    cfg: &RasterConfig,
    grads: &ImageGrads,
    out: &mut [ScreenGrad],
    stack: &mut Vec<(u32, f32, f32, f32, f32)>,
) {
    let (width, height) = (view.width as usize, view.height as usize);
    for iy in 0..TILE {
        let yi = ty * TILE + iy;
        if yi >= height {
            break;
        }
        let y = yi as f32 + 0.5;
        for ix in 0..TILE {
            let xi = tx * TILE + ix;
            if xi >= width {
                break;
            }
            let pixel = yi * width + xi;
            let gc = [grads.color[pixel * 3], grads.color[pixel * 3 + 1], grads.color[pixel * 3 + 2]];
            let (gdepth, galpha) = (grads.depth[pixel], grads.alpha[pixel]);
            if gc == [0.0; 3] && gdepth == 0.0 && galpha == 0.0 {
                continue;
            }
            let x = xi as f32 + 0.5;
            // Replay the pixel exactly as the forward pass drew it.
            stack.clear();
            let mut transmittance = 1.0f32;
            for (k, g) in local.iter().enumerate() {
                let Some((alpha, dx, dy)) = alpha_at(g, x, y, cfg) else { continue };
                stack.push((k as u32, alpha, transmittance, dx, dy));
                transmittance *= 1.0 - alpha;
                if transmittance < cfg.min_transmittance {
                    break;
                }
            }
            // Back to front, carrying what lies behind the current Gaussian:
            // its colour, its accumulated depth and its coverage.
            let (mut behind_color, mut behind_depth, mut behind_alpha) = (view.background, 0.0f32, 0.0f32);
            for &(k, alpha, t, dx, dy) in stack.iter().rev() {
                let g = &local[k as usize];
                let o = &mut out[k as usize];
                let weight = alpha * t;
                let mut d_alpha = (g.depth - behind_depth) * gdepth + (1.0 - behind_alpha) * galpha;
                for c in 0..3 {
                    o.color[c] += weight * gc[c];
                    d_alpha += (g.color[c] - behind_color[c]) * gc[c];
                    behind_color[c] = alpha * g.color[c] + (1.0 - alpha) * behind_color[c];
                }
                d_alpha *= t;
                o.depth += weight * gdepth;
                behind_depth = alpha * g.depth + (1.0 - alpha) * behind_depth;
                behind_alpha = alpha + (1.0 - alpha) * behind_alpha;

                // alpha = opacity · exp(−m/2).
                if g.opacity > 0.0 {
                    o.opacity += alpha / g.opacity * d_alpha;
                }
                let d_m = -0.5 * alpha * d_alpha;
                o.conic[0] += d_m * dx * dx;
                o.conic[1] += d_m * 2.0 * dx * dy;
                o.conic[2] += d_m * dy * dy;
                o.px -= d_m * 2.0 * (g.conic[0] * dx + g.conic[1] * dy);
                o.py -= d_m * 2.0 * (g.conic[1] * dx + g.conic[2] * dy);
            }
        }
    }
}

/// Seconds spent in each step since the rasterizer was made, for the
/// timing reports: which loops are worth porting, and how much of a pass
/// does not run in parallel.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct RasterTiming {
    /// Step 1, parallel over Gaussians.
    pub project: f64,
    /// Step 2: listing (serial) and sorting (parallel over tiles).
    pub bin: f64,
    pub sort: f64,
    /// Step 3, parallel over tiles, and the copy into row-major images.
    pub composite: f64,
    /// Step 4, parallel over tiles.
    pub backward_pixels: f64,
    /// The ordered sum of step 4's results (serial).
    pub reduce: f64,
    /// Step 5, parallel over Gaussians, and the serial write-out.
    pub chain: f64,
}

/// The CPU implementation of [`Rasterizer`]. Reuses its buffers from call
/// to call; runs on the rayon pool it is called from.
#[derive(Default)]
pub struct CpuRasterizer {
    pub config: RasterConfig,
    pub timing: RasterTiming,
    projected: Vec<Projected>,
    visible: Vec<u32>,
    tiles_x: usize,
    tiles_y: usize,
    /// Per tile: first entry of its list in `items`; one more at the end.
    tile_start: Vec<u32>,
    /// `(depth bits << 32) | index`, sorted within each tile.
    items: Vec<u64>,
    scratch: Vec<f32>,
    screen: Vec<ScreenGrad>,
    reduced: Vec<ScreenGrad>,
}

impl CpuRasterizer {
    pub fn new(config: RasterConfig) -> Self {
        Self { config, ..Self::default() }
    }

    /// The projections of the last forward pass, one per Gaussian.
    pub fn projected(&self) -> &[Projected] {
        &self.projected
    }

    /// Number of `(tile, Gaussian)` pairs of the last forward pass.
    pub fn pairs(&self) -> usize {
        self.items.len()
    }

    /// Steps 1 and 2.
    fn bin(&mut self, model: &Gaussians, view: &View) {
        let cfg = self.config;
        let mut clock = Instant::now();
        let mut lap = |slot: &mut f64| {
            *slot += clock.elapsed().as_secs_f64();
            clock = Instant::now();
        };
        self.projected.resize(model.len(), Projected::default());
        self.projected.par_iter_mut().zip(model.data.par_iter()).with_min_len(1024).for_each(|(p, g)| *p = project(g, view, &cfg));
        lap(&mut self.timing.project);

        self.tiles_x = (view.width as usize).div_ceil(TILE);
        self.tiles_y = (view.height as usize).div_ceil(TILE);
        let tiles = self.tiles_x * self.tiles_y;
        self.visible.clear();
        self.tile_start.clear();
        self.tile_start.resize(tiles + 1, 0);
        for (i, p) in self.projected.iter().enumerate() {
            if !p.is_visible() {
                continue;
            }
            self.visible.push(i as u32);
            for ty in p.tiles[2]..p.tiles[3] {
                for tx in p.tiles[0]..p.tiles[1] {
                    self.tile_start[ty as usize * self.tiles_x + tx as usize + 1] += 1;
                }
            }
        }
        for t in 0..tiles {
            self.tile_start[t + 1] += self.tile_start[t];
        }
        self.items.clear();
        self.items.resize(self.tile_start[tiles] as usize, 0);
        let mut cursor: Vec<u32> = self.tile_start[..tiles].to_vec();
        for &i in &self.visible {
            let p = &self.projected[i as usize];
            // Depths are positive, so their bit patterns sort like the values.
            let key = ((p.depth.to_bits() as u64) << 32) | i as u64;
            for ty in p.tiles[2]..p.tiles[3] {
                for tx in p.tiles[0]..p.tiles[1] {
                    let slot = &mut cursor[ty as usize * self.tiles_x + tx as usize];
                    self.items[*slot as usize] = key;
                    *slot += 1;
                }
            }
        }
        lap(&mut self.timing.bin);
        per_tile(&mut self.items, &self.tile_start).into_par_iter().for_each(|list| list.sort_unstable());
        lap(&mut self.timing.sort);
    }
}

/// Splits `data` into the per-tile slices that `tile_start` describes.
fn per_tile<'a, T>(mut data: &'a mut [T], tile_start: &[u32]) -> Vec<&'a mut [T]> {
    let mut lists = Vec::with_capacity(tile_start.len().saturating_sub(1));
    for bounds in tile_start.windows(2) {
        let (head, tail) = data.split_at_mut((bounds[1] - bounds[0]) as usize);
        lists.push(head);
        data = tail;
    }
    lists
}

impl Rasterizer for CpuRasterizer {
    fn forward(&mut self, model: &Gaussians, view: &View, out: &mut Rendered) {
        self.bin(model, view);
        let cfg = self.config;
        let (width, height) = (view.width as usize, view.height as usize);
        if width == 0 || height == 0 {
            *out = Rendered { width: view.width, height: view.height, ..Rendered::default() };
            return;
        }
        let clock = Instant::now();
        let (tiles_x, tiles) = (self.tiles_x, self.tiles_x * self.tiles_y);
        self.scratch.clear();
        self.scratch.resize(tiles * TILE_PIXELS * CHANNELS, 0.0);
        let (projected, items, tile_start) = (&self.projected, &self.items, &self.tile_start);
        self.scratch.par_chunks_mut(TILE_PIXELS * CHANNELS).enumerate().for_each_init(Vec::new, |local, (tile, block)| {
            gather(&items[tile_start[tile] as usize..tile_start[tile + 1] as usize], projected, local);
            forward_tile((tile % tiles_x, tile / tiles_x), local, view, &cfg, block);
        });

        out.width = view.width;
        out.height = view.height;
        out.color.resize(width * height * 3, 0.0);
        out.depth.resize(width * height, 0.0);
        out.alpha.resize(width * height, 0.0);
        let scratch = &self.scratch;
        let color_rows = out.color.par_chunks_mut(width * 3);
        let depth_rows = out.depth.par_chunks_mut(width);
        let alpha_rows = out.alpha.par_chunks_mut(width);
        color_rows.zip(depth_rows).zip(alpha_rows).enumerate().for_each(|(y, ((color, depth), alpha))| {
            let (ty, iy) = (y / TILE, y % TILE);
            for x in 0..width {
                let src = &scratch[((ty * tiles_x + x / TILE) * TILE_PIXELS + iy * TILE + x % TILE) * CHANNELS..][..CHANNELS];
                color[x * 3..x * 3 + 3].copy_from_slice(&src[..3]);
                depth[x] = src[3];
                alpha[x] = src[4];
            }
        });
        self.timing.composite += clock.elapsed().as_secs_f64();
    }

    fn backward(&mut self, model: &Gaussians, view: &View, image_grads: &ImageGrads, want_pose: bool, out: &mut ParamGrads) {
        let cfg = self.config;
        let pixels = view.pixels();
        assert!(
            self.projected.len() == model.len() && image_grads.color.len() == pixels * 3 && image_grads.depth.len() == pixels,
            "backward does not match the last forward pass"
        );
        let tiles_x = self.tiles_x;
        let mut clock = Instant::now();
        let mut lap = |slot: &mut f64| {
            *slot += clock.elapsed().as_secs_f64();
            clock = Instant::now();
        };
        self.screen.clear();
        self.screen.resize(self.items.len(), ScreenGrad::default());
        let (projected, items, tile_start) = (&self.projected, &self.items, &self.tile_start);
        per_tile(&mut self.screen, tile_start).into_par_iter().enumerate().for_each_init(
            || (Vec::new(), Vec::new()),
            |(local, stack), (tile, block)| {
                if block.is_empty() {
                    return;
                }
                gather(&items[tile_start[tile] as usize..tile_start[tile + 1] as usize], projected, local);
                backward_tile((tile % tiles_x, tile / tiles_x), local, view, &cfg, image_grads, block, stack);
            },
        );

        lap(&mut self.timing.backward_pixels);
        // Fixed order of the sums: tile after tile, whatever the threads did.
        self.reduced.resize(model.len(), ScreenGrad::default());
        for (&key, sg) in self.items.iter().zip(&self.screen) {
            self.reduced[key as u32 as usize].add(sg);
        }
        lap(&mut self.timing.reduce);

        // Entries of the previous call back to zero, then this view's.
        for &i in &out.visible {
            if let Some(g) = out.grads.get_mut(i as usize) {
                *g = [0.0; PARAMS];
            }
        }
        out.grads.resize(model.len(), [0.0; PARAMS]);
        out.visible.clone_from(&self.visible);
        let n = self.visible.len();
        out.screen_grad.resize(n, 0.0);
        out.radius_px.resize(n, 0.0);
        let (half_w, half_h) = (0.5 * view.width as f32, 0.5 * view.height as f32);
        let reduced = &self.reduced;
        let per_gaussian: Vec<(Gaussian, [f32; 6], f32)> = self
            .visible
            .par_iter()
            .with_min_len(256)
            .map(|&i| {
                let sg = &reduced[i as usize];
                let (grad, pose) = chain(&model.data[i as usize], view, &cfg, sg).unwrap_or(([0.0; PARAMS], [0.0; 6]));
                (grad, pose, (sg.px * half_w).hypot(sg.py * half_h))
            })
            .collect();
        out.pose = [0.0; 6];
        for (slot, (&i, (grad, pose, screen))) in self.visible.iter().zip(&per_gaussian).enumerate() {
            out.grads[i as usize] = *grad;
            out.screen_grad[slot] = *screen;
            out.radius_px[slot] = self.projected[i as usize].radius_px;
            if want_pose {
                for (sum, value) in out.pose.iter_mut().zip(pose) {
                    *sum += *value as f64;
                }
            }
            self.reduced[i as usize] = ScreenGrad::default();
        }
        lap(&mut self.timing.chain);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use gs_core::raster::{rasterize, RasterOptions};
    use gs_core::Splat;

    use super::*;
    use crate::model::logit;

    /// SplitMix64, uniform floats.
    pub struct Rng(pub u64);

    impl Rng {
        pub fn unit(&mut self) -> f32 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            ((z ^ (z >> 31)) >> 40) as f32 / (1u64 << 24) as f32
        }

        pub fn range(&mut self, lo: f32, hi: f32) -> f32 {
            lo + (hi - lo) * self.unit()
        }
    }

    pub fn camera(width: u32, height: u32) -> Camera {
        Camera::look_at(Vec3::new(0.3, 0.2, 2.5), Vec3::new(0.0, 0.0, 0.0), Vec3::Y, 0.9, width, height)
    }

    /// Anisotropic, rotated, translucent Gaussians in front of [`camera`].
    pub fn random_model(n: usize, seed: u64, sigma: (f32, f32)) -> Gaussians {
        let mut rng = Rng(seed);
        let data = (0..n)
            .map(|_| {
                let mut g = [0.0f32; PARAMS];
                for k in 0..3 {
                    g[POS + k] = rng.range(-0.8, 0.8);
                    g[SCALE + k] = rng.range(sigma.0, sigma.1).ln();
                    g[COLOR + k] = rng.unit();
                }
                for k in 0..4 {
                    g[ROT + k] = rng.range(-1.0, 1.0);
                }
                g[OPACITY] = logit(rng.range(0.1, 0.95));
                g
            })
            .collect();
        Gaussians { data }
    }

    fn smooth() -> RasterConfig {
        RasterConfig { cutoff_m2: 1.0e9, min_alpha: 0.0, min_transmittance: 0.0, ..RasterConfig::default() }
    }

    /// A fixed random linear functional of the three images, summed in f64.
    fn functional(r: &Rendered, w: &ImageGrads) -> f64 {
        let dot = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(&x, &y)| x as f64 * y as f64).sum::<f64>();
        dot(&r.color, &w.color) + dot(&r.depth, &w.depth) + dot(&r.alpha, &w.alpha)
    }

    fn random_weights(pixels: usize, seed: u64) -> ImageGrads {
        let mut rng = Rng(seed);
        let mut fill = |n: usize| (0..n).map(|_| rng.range(-1.0, 1.0)).collect::<Vec<f32>>();
        ImageGrads { color: fill(pixels * 3), depth: fill(pixels), alpha: fill(pixels) }
    }

    #[test]
    fn the_forward_pass_draws_what_the_viewer_draws() {
        let cam = camera(160, 120);
        for (seed, sigma) in [(1u64, (0.01, 0.12)), (2, (0.003, 0.03)), (3, (0.05, 0.3))] {
            // Through 8-bit splats, so that both renderers see the same numbers.
            let splats: Vec<Splat> = random_model(400, seed, sigma).to_splats();
            let model = Gaussians::from_splats(&splats, 1.0, 1.0, 0.0);
            let background = [10u8, 20, 30];
            let reference = rasterize(&splats, &cam, &RasterOptions { background, classic_dilation: true, ..Default::default() });
            let mut out = Rendered::default();
            let view = View::from_camera(&cam, background.map(|v| v as f32 / 255.0));
            CpuRasterizer::default().forward(&model, &view, &mut out);
            let image = out.to_image();
            // The reference sorts by a 16-bit depth key and composites back
            // to front without stopping; the differences are rounding and
            // the order of a few near-ties.
            let worst = image.rgb.iter().zip(&reference.rgb).map(|(a, b)| a.abs_diff(*b)).max().unwrap();
            let off = image.rgb.iter().zip(&reference.rgb).filter(|(a, b)| a.abs_diff(**b) > 1).count();
            let psnr = gs_core::raster::psnr(&image, &reference);
            assert!(psnr > 50.0 && worst <= 12 && off * 200 < image.rgb.len(), "seed {seed}: PSNR {psnr}, worst {worst}, {off} off");
            assert!(image.rgb.chunks_exact(3).filter(|p| *p != background).count() > 2000, "seed {seed}: empty picture");
        }
    }

    #[test]
    fn depth_and_coverage_follow_their_definitions() {
        // One opaque-ish ball in front of the camera at a known distance.
        let cam = Camera::look_at(Vec3::ZERO, Vec3::NEG_Z, Vec3::Y, 1.0, 64, 48);
        let mut g = [0.0f32; PARAMS];
        g[POS + 2] = -2.0;
        g[SCALE..SCALE + 3].copy_from_slice(&[0.2f32.ln(); 3]);
        g[ROT + 3] = 1.0;
        g[OPACITY] = logit(0.8);
        g[COLOR] = 1.0;
        let mut out = Rendered::default();
        CpuRasterizer::default().forward(&Gaussians { data: vec![g] }, &View::from_camera(&cam, [0.0, 0.0, 1.0]), &mut out);
        let center = 24 * 64 + 32;
        // The pixel center is half a pixel off the projected center, in both
        // directions, on a footprint of variance σ² + 0.3 px².
        let sigma_px = 0.2 / 2.0 * cam.fx;
        let expected = 0.8 * (-0.5 * 0.5 / (sigma_px * sigma_px + 0.3)).exp();
        assert!((out.alpha[center] - expected).abs() < 1e-4, "{} vs {expected}", out.alpha[center]);
        assert!((out.depth[center] / out.alpha[center] - 2.0).abs() < 1e-5);
        assert!((out.color[center * 3] - out.alpha[center]).abs() < 1e-6 && (out.color[center * 3 + 2] - (1.0 - out.alpha[center])).abs() < 1e-6);
        assert_eq!((out.alpha[0], out.depth[0]), (0.0, 0.0));
        assert_eq!(&out.color[..3], &[0.0, 0.0, 1.0]);
    }

    /// Central differences of the functional against the analytic gradient,
    /// for every parameter of a few Gaussians.
    fn check_gradients(model: &Gaussians, view: &View, cfg: RasterConfig, picks: &[usize], step: f32, tolerance: f64) {
        let weights = random_weights(view.pixels(), 77);
        let mut raster = CpuRasterizer::new(cfg);
        let mut out = Rendered::default();
        raster.forward(model, view, &mut out);
        let mut grads = ParamGrads::default();
        raster.backward(model, view, &weights, true, &mut grads);

        let value = |m: &Gaussians, v: &View| {
            let mut r = Rendered::default();
            CpuRasterizer::new(cfg).forward(m, v, &mut r);
            functional(&r, &weights)
        };
        let mut worst = 0.0f64;
        let mut checked = 0;
        for &i in picks {
            assert!(grads.visible.contains(&(i as u32)), "Gaussian {i} is not visible");
            for k in 0..PARAMS {
                let eps = step * if (POS..POS + 3).contains(&k) { 2e-3 } else { 5e-3 };
                let (mut plus, mut minus) = (model.clone(), model.clone());
                plus.data[i][k] += eps;
                minus.data[i][k] -= eps;
                let numeric = (value(&plus, view) - value(&minus, view)) / (2.0 * eps as f64);
                let analytic = grads.grads[i][k] as f64;
                let error = (numeric - analytic).abs() / (numeric.abs().max(analytic.abs()) + 0.05);
                assert!(error < tolerance, "Gaussian {i} parameter {k}: numeric {numeric}, analytic {analytic}");
                worst = worst.max(error);
                checked += 1;
            }
        }
        // The camera twist.
        for k in 0..6 {
            let eps = step * 1e-3;
            let mut twist = [0.0f32; 6];
            twist[k] = eps;
            let plus = value(model, &view.moved(twist));
            twist[k] = -eps;
            let minus = value(model, &view.moved(twist));
            let numeric = (plus - minus) / (2.0 * eps as f64);
            let analytic = grads.pose[k];
            let error = (numeric - analytic).abs() / (numeric.abs().max(analytic.abs()) + 0.5);
            assert!(error < tolerance, "twist {k}: numeric {numeric}, analytic {analytic}");
            worst = worst.max(error);
        }
        assert!(checked > 0);
        eprintln!("worst relative gradient error {worst:.2e} over {checked} parameters and the twist");
    }

    #[test]
    fn gradients_match_finite_differences() {
        // Few, large, overlapping Gaussians: every pixel blends several.
        let model = random_model(24, 5, (0.08, 0.35));
        let view = View::from_camera(&camera(48, 36), [0.2, 0.5, 0.8]);
        check_gradients(&model, &view, smooth(), &[0, 1, 2, 3, 7, 11, 19], 1.0, 0.02);
    }

    #[test]
    fn gradients_match_with_small_splats_and_the_low_pass() {
        // Splats of about a pixel: the dilation dominates the footprint.
        let mut model = random_model(60, 6, (0.004, 0.02));
        for g in &mut model.data {
            g[POS + 2] *= 0.2;
        }
        let view = View::from_camera(&camera(40, 30), [0.0; 3]);
        check_gradients(&model, &view, smooth(), &[0, 5, 9, 14, 33], 1.0, 0.02);
    }

    #[test]
    fn gradients_match_where_the_jacobian_is_clamped() {
        // Gaussians beyond 1.3 times the frustum that still reach into it.
        let mut model = random_model(6, 8, (0.3, 0.6));
        for (i, g) in model.data.iter_mut().enumerate() {
            let side = if i % 2 == 0 { 1.0 } else { -1.0 };
            g[POS] = side * 3.4;
            g[POS + 1] = if i >= 4 { side * 2.6 } else { 0.1 };
            g[POS + 2] = 0.0;
        }
        let view = View::from_camera(&camera(40, 30), [0.1; 3]);
        let cfg = smooth();
        let geo = geometry(&model.data[0], &view, &cfg).unwrap();
        assert!(!geo.u_free, "the test must exercise the clamp");
        assert!(!geometry(&model.data[4], &view, &cfg).unwrap().v_free);
        // Far off screen the gradients are small against the rounding of a
        // 32-bit forward pass: wider steps, which these wide Gaussians allow.
        check_gradients(&model, &view, cfg, &[0, 1, 4, 5], 5.0, 0.02);
    }

    #[test]
    fn the_result_does_not_depend_on_the_number_of_threads() {
        let model = random_model(3000, 9, (0.005, 0.08));
        let view = View::from_camera(&camera(200, 150), [0.3; 3]);
        let weights = random_weights(view.pixels(), 3);
        let run = |threads: usize| {
            rayon::ThreadPoolBuilder::new().num_threads(threads).build().unwrap().install(|| {
                let mut raster = CpuRasterizer::default();
                let (mut out, mut grads) = (Rendered::default(), ParamGrads::default());
                raster.forward(&model, &view, &mut out);
                raster.backward(&model, &view, &weights, true, &mut grads);
                (out, grads.grads, grads.pose, grads.screen_grad)
            })
        };
        let (one, four) = (run(1), run(4));
        assert!(one.0 == four.0 && one.1 == four.1 && one.2 == four.2 && one.3 == four.3);
        assert!(one.1.iter().flatten().all(|v| v.is_finite()));
        assert!(one.1.iter().flatten().any(|v| *v != 0.0));
    }

    #[test]
    fn a_second_backward_clears_what_the_first_left() {
        let model = random_model(50, 10, (0.02, 0.1));
        let cam = camera(64, 48);
        let view = View::from_camera(&cam, [0.0; 3]);
        // The same cloud seen from behind a wall of nothing: look away.
        let away = View::from_camera(&Camera::look_at(Vec3::new(0.0, 0.0, 5.0), Vec3::new(0.0, 0.0, 10.0), Vec3::Y, 0.9, 64, 48), [0.0; 3]);
        let weights = random_weights(view.pixels(), 4);
        let mut raster = CpuRasterizer::default();
        let (mut out, mut grads) = (Rendered::default(), ParamGrads::default());
        raster.forward(&model, &view, &mut out);
        raster.backward(&model, &view, &weights, false, &mut grads);
        assert!(!grads.visible.is_empty() && grads.pose == [0.0; 6]);
        raster.forward(&model, &away, &mut out);
        raster.backward(&model, &away, &weights, false, &mut grads);
        assert!(grads.visible.is_empty());
        assert!(grads.grads.iter().flatten().all(|v| *v == 0.0));
    }

    #[test]
    fn degenerate_input_does_not_panic() {
        let mut model = random_model(8, 11, (0.02, 0.1));
        model.data[0][POS] = f32::NAN;
        model.data[1][SCALE] = f32::INFINITY;
        model.data[2][ROT..ROT + 4].copy_from_slice(&[0.0; 4]);
        model.data[3][OPACITY] = f32::NAN;
        model.data[4][SCALE..SCALE + 3].copy_from_slice(&[-80.0; 3]);
        model.data[5][OPACITY] = 60.0;
        for (w, h) in [(0u32, 0u32), (1, 1), (17, 5), (64, 48)] {
            let mut cam = camera(64, 48);
            (cam.width, cam.height) = (w, h);
            let view = View::from_camera(&cam, [0.0; 3]);
            let mut raster = CpuRasterizer::default();
            let (mut out, mut grads) = (Rendered::default(), ParamGrads::default());
            raster.forward(&model, &view, &mut out);
            assert_eq!(out.color.len(), (w * h * 3) as usize);
            assert!(out.color.iter().chain(&out.depth).chain(&out.alpha).all(|v| v.is_finite()));
            raster.backward(&model, &view, &random_weights(view.pixels(), 1), true, &mut grads);
            assert!(grads.grads.iter().flatten().all(|v| v.is_finite()), "{w}x{h}");
        }
    }
}
