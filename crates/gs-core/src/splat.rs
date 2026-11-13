//! Covariance math and the EWA projection shared by the CPU and GPU renderers.
//!
//! This module is the **reference** for the splat shader in `gs-render`: the
//! CPU rasterizer ([`crate::raster`]) calls the functions below, and the GLSL
//! must compute the same quantities. Everything follows the conventions of
//! `docs/ARCHITECTURE.md`: camera space is +X right, +Y up, −Z forward; pixel
//! `(0, 0)` is the top-left corner and `py` grows downward.
//!
//! # Reference algorithm
//!
//! Per splat: center `p` (world), covariance `Σ = R·diag(scale²)·Rᵀ` (stored
//! as the six unique terms `xx, xy, xz, yy, yz, zz`, see [`cov6`]), an sRGB
//! color `c` and an opacity `o` in `[0, 1]`.
//! Per view: [`ViewParams`] — the world-to-camera matrix `view` (which may
//! contain a uniform scale), intrinsics `fx, fy, cx, cy` in pixels, the
//! viewport size `W × H` in pixels, `near`, and the user splat scale `k`.
//!
//! **Vertex stage** ([`project_splat`], [`ellipse_axes`])
//!
//! 1. Camera-space center and depth:
//!    `t = view · (p, 1)`, `d = −t.z`. Cull the splat when `d < near`.
//! 2. Center in pixels (identical to `Camera::project`):
//!    `px = cx + fx·t.x/d`, `py = cy − fy·t.y/d`.
//!    The same point in NDC is `(2·px/W − 1, 1 − 2·py/H)`, which is what
//!    `proj · t` followed by the perspective divide gives.
//! 3. Clamp the direction used for the Jacobian to 1.3× the frustum, so that
//!    splats far off-screen do not get absurd footprints:
//!    `u = clamp(t.x/d, (−cx − 0.15·W)/fx, (W − cx + 0.15·W)/fx)`,
//!    `v = clamp(t.y/d, (cy − H − 0.15·H)/fy, (cy + 0.15·H)/fy)`.
//!    For a centered principal point this is the usual
//!    `±1.3·tan(fov/2)`; the general form also covers the asymmetric
//!    per-eye frusta of WebXR.
//! 4. Jacobian of `(px, py)` with respect to camera-space `(x, y, z)`,
//!    evaluated at `(u·d, v·d, −d)`. Note the signs: `d = −z` and `py` grows
//!    downward.
//!    ```text
//!        | fx/d     0     fx·u/d |
//!    J = |                       |
//!        |  0     −fy/d  −fy·v/d |
//!    ```
//! 5. Screen-space covariance in pixel². `Wm` is the upper-left 3×3 of
//!    `view` (`mat3(view)` in GLSL), so a uniform world scale baked into the
//!    view matrix (VR "dollhouse" mode) scales the footprint correctly:
//!    `T = J · Wm` (2×3), `M = k² · T·Σ·Tᵀ = [[ma, mb], [mb, mc]]`,
//!    `Σ' = M + 0.3·I = [[a, b], [b, c]]`, i.e. `a = ma + 0.3`, `b = mb`,
//!    `c = mc + 0.3`. The `0.3 px²` term ([`LOW_PASS_PX2`]) is the
//!    anti-aliasing low-pass of the reference 3DGS implementation; it is
//!    added *after* the `k²` scaling. `M` is positive semi-definite, so
//!    `ma` and `mc` are clamped to `≥ 0` (rounding noise only).
//! 6. Determinant, written so that 32-bit cancellation can never make it
//!    negative or smaller than the low-pass allows:
//!    `det = max(ma·mc − mb², 0) + 0.3·(ma + mc) + 0.09`
//!    (algebraically `a·c − b²`). Cull when `det` is not finite.
//! 7. Eigen-decomposition of `Σ'`:
//!    `mid = (a + c)/2`, `rad = length(((a − c)/2, b))`,
//!    `λ1 = mid + rad`, `λ2 = det/λ1` (stable form of `mid − rad`; `≥ 0.3`),
//!    `v1 = normalize(a ≥ c ? ((a − c)/2 + rad, b) : (b, (c − a)/2 + rad))`
//!    (use `(1, 0)` when that vector is zero, i.e. for a circle),
//!    `v2 = (−v1.y, v1.x)`.
//!    Half-axes of the 3σ ellipse in pixels ([`CUTOFF_SIGMA`]):
//!    `major = 3·√λ1 · v1`, `minor = 3·√λ2 · v2`
//!    ([`Projected::ellipse_axes`]).
//!    The inverse used by the CPU rasterizer is built from the same numbers,
//!    which keeps it positive definite: `conic = Σ'⁻¹ = v1·v1ᵀ/λ1 + v2·v2ᵀ/λ2`,
//!    stored as `(conic.xx, conic.xy, conic.yy)`
//!    (algebraically `(c, −b, a)/det`).
//! 8. The instanced quad has corners `q ∈ {−1, +1}²`. Each vertex is placed at
//!    `offset_px = q.x·major + q.y·minor` around the center, i.e. in NDC
//!    `ndc + offset_px · (2/W, −2/H)` (minus: `py` grows downward). Pass `q`
//!    to the fragment stage. A GPU implementation may clamp the two half-axis
//!    lengths (e.g. to 1024 px) to bound fill-rate; that shrinks huge splats
//!    instead of truncating them.
//!
//! **Fragment stage** ([`eval_alpha`])
//!
//! 9. With `δ = pixel_center − (px, py)`, the Gaussian exponent is
//!    `m = δᵀ·conic·δ = conic.xx·δx² + 2·conic.xy·δx·δy + conic.yy·δy²`.
//!    On the quad of step 8 the interpolated corner coordinate gives the same
//!    number without a matrix: `m = 9·dot(q, q)`.
//! 10. Discard when `m > 9` (outside 3σ). `α = o · exp(−0.5·m)`. Discard when
//!     `α < 1/255` ([`MIN_ALPHA`]). There is no 0.99 clamp on `α`: a fully
//!     opaque splat shows exactly its color at its center. (`m ≥ 0`, so
//!     `α ≤ o`; the CPU code enforces this bound explicitly because the
//!     conic form of `m` can cancel to a negative number for extreme
//!     needles.)
//! 11. Splats are drawn **back to front** ([`crate::sort`]) and composited
//!     with "over" on premultiplied alpha, directly on sRGB values:
//!     `dst.rgb = c·α + dst.rgb·(1 − α)`
//!     (output `(c·α, α)` with `blendFunc(ONE, ONE_MINUS_SRC_ALPHA)`).
//!
//! # GLSL sketch
//!
//! The same steps written the way a WebGL2 shader would (`c0..c5` are the
//! [`cov6`] terms, `o` the opacity, `k` the splat scale, `corner` the quad
//! corner in `{−1, +1}²`):
//!
//! ```text
//! // ---- vertex ----
//! vec4 t = view * vec4(p, 1.0);
//! float d = -t.z;
//! if (d < near) { gl_Position = vec4(0.0, 0.0, 2.0, 1.0); return; }   // culled
//! vec2 uv = t.xy / d;
//! vec2 px = vec2(cx + fx * uv.x, cy - fy * uv.y);                     // pixels, y down
//! uv = clamp(uv, vec2((-cx - 0.15 * W) / fx, (cy - 1.15 * H) / fy),
//!                vec2((W - cx + 0.15 * W) / fx, (cy + 0.15 * H) / fy));
//! mat3 Wt = transpose(mat3(view));                 // Wt[i] = row i of mat3(view)
//! vec3 T0 =  (fx / d) * (Wt[0] + uv.x * Wt[2]);    // rows of T = J * mat3(view)
//! vec3 T1 = -(fy / d) * (Wt[1] + uv.y * Wt[2]);
//! mat3 S = mat3(c0, c1, c2,  c1, c3, c4,  c2, c4, c5);
//! float ma = max(k * k * dot(T0, S * T0), 0.0);
//! float mb = k * k * dot(T0, S * T1);
//! float mc = max(k * k * dot(T1, S * T1), 0.0);
//! float a = ma + 0.3, b = mb, c = mc + 0.3;
//! float det = max(ma * mc - mb * mb, 0.0) + 0.3 * (ma + mc) + 0.09;
//! float mid = 0.5 * (a + c);
//! float rad = length(vec2(0.5 * (a - c), b));
//! float l1 = mid + rad;
//! float l2 = det / l1;
//! vec2 v1 = a >= c ? vec2(0.5 * (a - c) + rad, b) : vec2(b, 0.5 * (c - a) + rad);
//! float len = length(v1);
//! v1 = len > 1e-12 ? v1 / len : vec2(1.0, 0.0);
//! vec2 major = 3.0 * sqrt(l1) * v1;
//! vec2 minor = 3.0 * sqrt(l2) * vec2(-v1.y, v1.x);
//! vec2 offset = corner.x * major + corner.y * minor;                  // pixels, y down
//! vec2 ndc = vec2(2.0 * px.x / W - 1.0, 1.0 - 2.0 * px.y / H);
//! gl_Position = vec4(ndc + offset * vec2(2.0 / W, -2.0 / H), 0.0, 1.0);
//! vCorner = corner;
//! // ---- fragment ----
//! float r2 = dot(vCorner, vCorner);
//! if (r2 > 1.0) discard;                           // outside 3 sigma
//! float alpha = o * exp(-4.5 * r2);
//! if (alpha < 1.0 / 255.0) discard;
//! fragColor = vec4(color * alpha, alpha);          // blendFunc(ONE, ONE_MINUS_SRC_ALPHA)
//! ```
//!
//! # Numerical limits
//!
//! Everything is 32-bit float, like the shader. The world covariance of a
//! needle (`σ_long ≫ σ_thin`) keeps its thin axis only to about
//! `10⁻⁷·σ_long²`, so once the *unforeshortened* long axis exceeds roughly a
//! thousand pixels the thin axis is noise of up to a pixel or so. Steps 5–7
//! are arranged so that this noise can only make such a splat a little
//! thicker: it is never culled and never thinner than the low-pass, and its
//! conic is assembled from two positive eigenvalues. (Evaluating the conic
//! form of step 9 can still cancel at that point, which is why the shader
//! and the CPU rasterizer use the quad form.)
//!
//! # Intrinsics from a projection matrix
//!
//! WebXR hands out a projection matrix per eye instead of intrinsics.
//! [`ViewParams::from_projection`] converts (column-major `P[col][row]`):
//! `fx = P[0][0]·W/2`, `fy = P[1][1]·H/2`, `cx = (1 − P[2][0])·W/2`,
//! `cy = (1 + P[2][1])·H/2`.

use glam::{Mat3, Mat4, Quat, Vec3};

use crate::types::{Camera, Splat};

/// Anti-aliasing low-pass added to the diagonal of the 2D covariance, pixel².
pub const LOW_PASS_PX2: f32 = 0.3;

/// A splat is cut off at this many standard deviations from its center.
pub const CUTOFF_SIGMA: f32 = 3.0;

/// `CUTOFF_SIGMA²`: largest Mahalanobis distance² that still contributes.
pub const CUTOFF_MAHALANOBIS2: f32 = CUTOFF_SIGMA * CUTOFF_SIGMA;

/// Contributions with a smaller alpha are discarded.
pub const MIN_ALPHA: f32 = 1.0 / 255.0;

/// The Jacobian is evaluated inside this multiple of the view frustum.
pub const FRUSTUM_CLAMP: f32 = 1.3;

/// Near plane used by [`ViewParams::from_camera`], in view-space units.
pub const DEFAULT_NEAR: f32 = 0.05;

/// Rotation matrix of a splat. A zero or non-finite quaternion counts as the
/// identity instead of poisoning the covariance with NaNs.
#[inline]
fn rotation_matrix(rot: [f32; 4]) -> Mat3 {
    let q = Quat::from_array(rot);
    let len2 = q.length_squared();
    if len2.is_finite() && len2 > 1e-24 {
        Mat3::from_quat(q * len2.sqrt().recip())
    } else {
        Mat3::IDENTITY
    }
}

/// World-space covariance `R·diag(scale²)·Rᵀ` of a splat as its six unique
/// terms `[xx, xy, xz, yy, yz, zz]`.
#[inline]
pub fn cov6(splat: &Splat) -> [f32; 6] {
    let r = rotation_matrix(splat.rot);
    // Σ = M·Mᵀ with M = R·diag(scale); m0..m2 are the columns of M.
    let m0 = r.x_axis * splat.scale[0];
    let m1 = r.y_axis * splat.scale[1];
    let m2 = r.z_axis * splat.scale[2];
    [
        m0.x * m0.x + m1.x * m1.x + m2.x * m2.x,
        m0.x * m0.y + m1.x * m1.y + m2.x * m2.y,
        m0.x * m0.z + m1.x * m1.z + m2.x * m2.z,
        m0.y * m0.y + m1.y * m1.y + m2.y * m2.y,
        m0.y * m0.z + m1.y * m1.z + m2.y * m2.z,
        m0.z * m0.z + m1.z * m1.z + m2.z * m2.z,
    ]
}

/// Computes [`cov6`] for a batch of splats into `out`.
///
/// Writes `min(splats.len(), out.len())` entries; the two slices normally have
/// the same length.
pub fn cov6_batch(splats: &[Splat], out: &mut [[f32; 6]]) {
    debug_assert_eq!(splats.len(), out.len());
    for (o, s) in out.iter_mut().zip(splats) {
        *o = cov6(s);
    }
}

/// Expands six covariance terms into the full symmetric matrix.
#[inline]
pub fn cov6_to_mat3(c: &[f32; 6]) -> Mat3 {
    Mat3::from_cols(Vec3::new(c[0], c[1], c[2]), Vec3::new(c[1], c[3], c[4]), Vec3::new(c[2], c[4], c[5]))
}

/// Everything the projection needs to know about one view.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewParams {
    /// World-to-camera matrix. May include a uniform scale (VR "dollhouse"
    /// mode): depths and footprints then come out in scaled view units.
    pub view: Mat4,
    /// Focal lengths in pixels.
    pub fx: f32,
    pub fy: f32,
    /// Principal point in pixels (top-left origin).
    pub cx: f32,
    pub cy: f32,
    /// Viewport size in pixels.
    pub width: f32,
    pub height: f32,
    /// Splats whose center is closer than this (view-space units) are culled.
    pub near: f32,
    /// Multiplies every standard deviation (1 = as reconstructed).
    pub splat_scale: f32,
}

impl ViewParams {
    /// View of a calibrated camera with [`DEFAULT_NEAR`] and `splat_scale = 1`.
    pub fn from_camera(cam: &Camera) -> Self {
        Self {
            view: cam.view_matrix(),
            fx: cam.fx,
            fy: cam.fy,
            cx: cam.cx,
            cy: cam.cy,
            width: cam.width as f32,
            height: cam.height as f32,
            near: DEFAULT_NEAR,
            splat_scale: 1.0,
        }
    }

    /// View described by an OpenGL/WebXR perspective projection matrix (the
    /// kind `Camera::projection_matrix` and `XRView.projectionMatrix` produce)
    /// for a `width × height` pixel viewport.
    ///
    /// The near plane is recovered from the matrix; when it cannot be (not a
    /// perspective matrix) [`DEFAULT_NEAR`] is used.
    pub fn from_projection(view: Mat4, proj: &Mat4, width: f32, height: f32) -> Self {
        // P[2][2] = −(f+n)/(f−n) and P[3][2] = −2fn/(f−n) give n = P[3][2] / (P[2][2] − 1);
        // the same expression holds for an infinite far plane.
        let near = proj.w_axis.z / (proj.z_axis.z - 1.0);
        Self {
            view,
            fx: 0.5 * width * proj.x_axis.x,
            fy: 0.5 * height * proj.y_axis.y,
            cx: 0.5 * width * (1.0 - proj.z_axis.x),
            cy: 0.5 * height * (1.0 + proj.z_axis.y),
            width,
            height,
            near: if near.is_finite() && near > 0.0 { near } else { DEFAULT_NEAR },
            splat_scale: 1.0,
        }
    }
}

/// A splat projected to the screen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Projected {
    /// Center in pixels, top-left origin (same convention as
    /// `Camera::project`).
    pub px: f32,
    pub py: f32,
    /// Distance along the camera forward axis, in view-space units.
    pub depth: f32,
    /// 2D covariance in pixel², `[xx, xy, yy]`, after the low-pass.
    pub cov: [f32; 3],
    /// Inverse of `cov`, `[xx, xy, yy]`.
    pub conic: [f32; 3],
    /// Radius in pixels of the circle bounding the 3σ ellipse.
    pub radius: f32,
}

impl Projected {
    /// Half-axes `(major, minor)` of the 3σ ellipse in pixels: step 7 of the
    /// module documentation. The quad corner `q ∈ {−1, +1}²` sits at
    /// `q.x·major + q.y·minor` from the center; `minor` is `major` rotated by
    /// +90° and shortened.
    ///
    /// Unlike the free function [`ellipse_axes`], which only sees the
    /// covariance, this stays accurate for extreme needles: the minor axis
    /// comes from the conic, where it is the well-conditioned eigenvalue.
    #[inline]
    pub fn ellipse_axes(&self) -> ([f32; 2], [f32; 2]) {
        let (lambda1, (vx, vy)) = major_eigenpair(self.cov);
        let [qa, qb, qc] = self.conic;
        // 1/λ2 is the larger eigenvalue of the conic: a sum, no cancellation.
        let inv_lambda2 = 0.5 * (qa + qc) + (0.5 * (qa - qc)).hypot(qb);
        let lambda2 = if inv_lambda2 > 0.0 { inv_lambda2.recip().min(lambda1) } else { 0.0 };
        let l1 = CUTOFF_SIGMA * lambda1.max(0.0).sqrt();
        let l2 = CUTOFF_SIGMA * lambda2.max(0.0).sqrt();
        ([vx * l1, vy * l1], [-vy * l2, vx * l2])
    }

    /// Replaces the footprint by an isotropic Gaussian with a fixed standard
    /// deviation in pixels (point-cloud display mode). Center and depth are
    /// kept.
    pub fn with_point_footprint(mut self, sigma_px: f32) -> Self {
        let var = (sigma_px * sigma_px).max(1e-6);
        self.cov = [var, 0.0, var];
        self.conic = [1.0 / var, 0.0, 1.0 / var];
        self.radius = CUTOFF_SIGMA * var.sqrt();
        self
    }
}

/// EWA projection of one Gaussian: steps 1–7 of the module documentation.
///
/// `cov6` is the world-space covariance from [`cov6`]. Returns `None` when the
/// center is closer than `vp.near` (or behind the camera), or when the result
/// is not finite. Splats outside the image are *not* culled here; compare
/// `px, py, radius` with the viewport for that.
///
/// Both eigenvalues of the returned footprint are at least [`LOW_PASS_PX2`]
/// (up to rounding), and `conic` is assembled from them, so a visible splat
/// never degenerates, however anisotropic it is.
#[inline]
pub fn project_splat(pos: Vec3, cov6: &[f32; 6], vp: &ViewParams) -> Option<Projected> {
    project_splat_with_axes(pos, cov6, vp).map(|(projected, _, _)| projected)
}

/// [`project_splat`] together with the half-axes `(major, minor)` of the 3σ
/// ellipse — the same values as [`Projected::ellipse_axes`], straight from
/// the decomposition instead of recomputed.
#[inline]
pub fn project_splat_with_axes(pos: Vec3, cov6: &[f32; 6], vp: &ViewParams) -> Option<(Projected, [f32; 2], [f32; 2])> {
    let m = &vp.view;
    // Rows of the upper-left 3×3 of `view`.
    let r0 = Vec3::new(m.x_axis.x, m.y_axis.x, m.z_axis.x);
    let r1 = Vec3::new(m.x_axis.y, m.y_axis.y, m.z_axis.y);
    let r2 = Vec3::new(m.x_axis.z, m.y_axis.z, m.z_axis.z);

    // 1. Camera-space center.
    let tx = r0.dot(pos) + m.w_axis.x;
    let ty = r1.dot(pos) + m.w_axis.y;
    let depth = -(r2.dot(pos) + m.w_axis.z);
    // Written so that a NaN depth is rejected too.
    if !(depth >= vp.near && depth > 0.0) {
        return None;
    }

    // 2. Center in pixels.
    let inv_d = 1.0 / depth;
    let u = tx * inv_d;
    let v = ty * inv_d;
    let px = vp.cx + vp.fx * u;
    let py = vp.cy - vp.fy * v;

    // 3. Clamp to 1.3× the frustum. `max().min()` instead of `clamp()`: no
    //    panic on nonsensical intrinsics.
    let margin = 0.5 * (FRUSTUM_CLAMP - 1.0);
    let inv_fx = 1.0 / vp.fx;
    let inv_fy = 1.0 / vp.fy;
    let uc = u.max((-vp.cx - margin * vp.width) * inv_fx).min((vp.width - vp.cx + margin * vp.width) * inv_fx);
    let vc = v.max((vp.cy - vp.height - margin * vp.height) * inv_fy).min((vp.cy + margin * vp.height) * inv_fy);

    // 4./5. T = J·Wm, one row per screen axis.
    let j00 = vp.fx * inv_d;
    let j11 = -vp.fy * inv_d;
    let t0 = r0 * j00 + r2 * (j00 * uc);
    let t1 = r1 * j11 + r2 * (j11 * vc);

    let [xx, xy, xz, yy, yz, zz] = *cov6;
    let s0 = Vec3::new(
        xx * t0.x + xy * t0.y + xz * t0.z,
        xy * t0.x + yy * t0.y + yz * t0.z,
        xz * t0.x + yz * t0.y + zz * t0.z,
    );
    let s1 = Vec3::new(
        xx * t1.x + xy * t1.y + xz * t1.z,
        xy * t1.x + yy * t1.y + yz * t1.z,
        xz * t1.x + yz * t1.y + zz * t1.z,
    );
    // 5. M = k²·T·Σ·Tᵀ is positive semi-definite: the clamps only remove
    //    rounding noise (and let NaN through to the check below).
    let k2 = vp.splat_scale * vp.splat_scale;
    let ma = clamp_non_negative(k2 * t0.dot(s0));
    let mb = k2 * t0.dot(s1);
    let mc = clamp_non_negative(k2 * t1.dot(s1));
    let a = ma + LOW_PASS_PX2;
    let b = mb;
    let c = mc + LOW_PASS_PX2;

    // 6. det(M + 0.3·I) = det(M) + 0.3·tr(M) + 0.09, with det(M) ≥ 0. In this
    //    form cancellation in det(M) cannot produce a negative determinant.
    let det = clamp_non_negative(ma * mc - mb * mb) + LOW_PASS_PX2 * (ma + mc) + LOW_PASS_PX2 * LOW_PASS_PX2;
    if !(det > 0.0 && det.is_finite() && px.is_finite() && py.is_finite()) {
        return None;
    }

    // 7. Eigen-decomposition; the conic is assembled from it so that it is
    //    positive definite by construction.
    let (lambda1, (vx, vy)) = major_eigenpair([a, b, c]);
    let lambda2 = det / lambda1;
    let inv1 = 1.0 / lambda1;
    let inv2 = 1.0 / lambda2;
    let l1 = CUTOFF_SIGMA * lambda1.sqrt();
    let l2 = CUTOFF_SIGMA * lambda2.sqrt();
    let projected = Projected {
        px,
        py,
        depth,
        cov: [a, b, c],
        conic: [vx * vx * inv1 + vy * vy * inv2, vx * vy * (inv1 - inv2), vy * vy * inv1 + vx * vx * inv2],
        radius: l1,
    };
    Some((projected, [vx * l1, vy * l1], [-vy * l2, vx * l2]))
}

/// `max(x, 0)` that keeps NaN (unlike `f32::max`), so that broken input is
/// still rejected by the finiteness checks.
#[inline(always)]
fn clamp_non_negative(x: f32) -> f32 {
    if x < 0.0 {
        0.0
    } else {
        x
    }
}

/// Larger eigenvalue `λ1` of the symmetric matrix `[[a, b], [b, c]]` and its
/// unit eigenvector (`(1, 0)` for a multiple of the identity).
#[inline]
fn major_eigenpair([a, b, c]: [f32; 3]) -> (f32, (f32, f32)) {
    let half_diff = 0.5 * (a - c);
    let rad = half_diff.hypot(b);
    let lambda1 = 0.5 * (a + c) + rad;
    // (λ1 − c, b) and (b, λ1 − a) are both eigenvectors of λ1; take the one
    // whose first/second entry is a sum of non-negative terms (no cancellation).
    let (ex, ey) = if a >= c { (half_diff + rad, b) } else { (b, rad - half_diff) };
    let len = ex.hypot(ey);
    let v = if len > 1e-12 { (ex / len, ey / len) } else { (1.0, 0.0) };
    (lambda1, v)
}

/// Half-axes of the 3σ ellipse of a 2D covariance `[xx, xy, yy]`, in pixels:
/// `(major, minor)`, step 7 of the module documentation.
///
/// The quad corner `q ∈ {−1, +1}²` sits at `q.x·major + q.y·minor` from the
/// center; `minor` is `major` rotated by +90° and shortened.
///
/// This works from the covariance alone, so the minor axis is only as
/// accurate as `a·c − b²` is in 32-bit floats: fine up to an axis ratio of a
/// few hundred. For a projected splat prefer [`Projected::ellipse_axes`].
#[inline]
pub fn ellipse_axes(cov: [f32; 3]) -> ([f32; 2], [f32; 2]) {
    let [a, b, c] = cov;
    let det = a * c - b * b;
    let (lambda1, (vx, vy)) = major_eigenpair(cov);
    let lambda1 = lambda1.max(0.0);
    // det/λ1 instead of mid − rad: less cancellation for needle-like splats.
    let lambda2 = if lambda1 > 0.0 { (det / lambda1).max(0.0) } else { 0.0 };
    let l1 = CUTOFF_SIGMA * lambda1.sqrt();
    let l2 = CUTOFF_SIGMA * lambda2.sqrt();
    ([vx * l1, vy * l1], [-vy * l2, vx * l2])
}

/// Alpha contributed by a splat at offset `(dx, dy)` pixels from its center:
/// steps 9–10 of the module documentation. Returns `0.0` when the sample is
/// discarded (outside 3σ or below [`MIN_ALPHA`]).
#[inline]
pub fn eval_alpha(opacity: f32, conic: [f32; 3], dx: f32, dy: f32) -> f32 {
    let m = conic[0] * dx * dx + 2.0 * conic[1] * dx * dy + conic[2] * dy * dy;
    // Both comparisons are false for NaN, which is discarded as well.
    if m <= CUTOFF_MAHALANOBIS2 {
        // `min(1)`: m ≥ 0 mathematically, but the three terms can cancel to a
        // negative number for extreme needles; alpha must never exceed opacity.
        let alpha = opacity * (-0.5 * m).exp().min(1.0);
        if alpha >= MIN_ALPHA {
            return alpha;
        }
    }
    0.0
}

/// Deterministic pseudo-random helpers shared by the unit tests of this crate.
#[cfg(test)]
pub(crate) mod test_util {
    use glam::{DMat3, DQuat, DVec3, Mat4, Quat, Vec3};

    use crate::types::Splat;

    /// Independent reference for the projection of a splat, in `f64`: the
    /// pinhole model of `docs/ARCHITECTURE.md` (`px = cx + fx·x/d`,
    /// `py = cy − fy·y/d`, `d = −z`) differentiated *numerically*, so it shares
    /// no formula with `project_splat`. No frustum clamp, no low-pass.
    ///
    /// Returns `(center [px, py], covariance [xx, xy, yy], depth)`, or `None`
    /// behind the camera.
    pub fn reference_projection(
        splat: &Splat,
        view: &Mat4,
        [fx, fy, cx, cy]: [f32; 4],
    ) -> Option<([f64; 2], [f64; 3], f64)> {
        let (fx, fy, cx, cy) = (fx as f64, fy as f64, cx as f64, cy as f64);
        let view = view.as_dmat4();
        let project = |world: DVec3| {
            let c = view.transform_point3(world);
            let d = -c.z;
            (cx + fx * c.x / d, cy - fy * c.y / d, d)
        };
        let center = Vec3::from_array(splat.pos).as_dvec3();
        let (px, py, depth) = project(center);
        if depth <= 0.0 {
            return None;
        }
        let h = 1e-6 * depth.max(1.0);
        let mut jx = DVec3::ZERO;
        let mut jy = DVec3::ZERO;
        for k in 0..3 {
            let mut e = DVec3::ZERO;
            e[k] = h;
            let (ax, ay, _) = project(center + e);
            let (bx, by, _) = project(center - e);
            jx[k] = (ax - bx) / (2.0 * h);
            jy[k] = (ay - by) / (2.0 * h);
        }
        let r = DMat3::from_quat(DQuat::from_array(splat.rot.map(f64::from)).normalize());
        let s = splat.scale.map(f64::from);
        let m = DMat3::from_cols(r.x_axis * s[0], r.y_axis * s[1], r.z_axis * s[2]);
        let sigma = m * m.transpose();
        Some(([px, py], [jx.dot(sigma * jx), jx.dot(sigma * jy), jy.dot(sigma * jy)], depth))
    }

    /// SplitMix64: tiny, seedable, good enough for tests.
    pub struct Rng(u64);

    impl Rng {
        pub fn new(seed: u64) -> Self {
            Self(seed)
        }

        pub fn next_u64(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        /// Uniform in `[0, 1)`.
        pub fn unit(&mut self) -> f32 {
            (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
        }

        pub fn range(&mut self, lo: f32, hi: f32) -> f32 {
            lo + (hi - lo) * self.unit()
        }

        pub fn below(&mut self, n: usize) -> usize {
            (self.next_u64() % n.max(1) as u64) as usize
        }

        /// Standard normal (Box–Muller).
        pub fn normal(&mut self) -> f32 {
            let u1 = (1.0 - self.unit()).max(1e-12);
            let u2 = self.unit();
            (-2.0 * u1.ln()).sqrt() * (std::f32::consts::TAU * u2).cos()
        }

        pub fn vec3(&mut self, lo: f32, hi: f32) -> Vec3 {
            Vec3::new(self.range(lo, hi), self.range(lo, hi), self.range(lo, hi))
        }

        /// Uniformly distributed rotation.
        pub fn quat(&mut self) -> Quat {
            Quat::from_xyzw(self.normal(), self.normal(), self.normal(), self.normal()).normalize()
        }

        /// Random splat inside the cube `[-extent, extent]³` with standard
        /// deviations in `[min_scale, max_scale]`.
        pub fn splat(&mut self, extent: f32, min_scale: f32, max_scale: f32) -> Splat {
            Splat {
                pos: self.vec3(-extent, extent).to_array(),
                scale: [
                    self.range(min_scale, max_scale),
                    self.range(min_scale, max_scale),
                    self.range(min_scale, max_scale),
                ],
                rot: self.quat().to_array(),
                rgba: [self.below(256) as u8, self.below(256) as u8, self.below(256) as u8, self.below(256) as u8],
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use glam::{Affine3A, Vec2};

    use super::test_util::{self, Rng};
    use super::*;

    fn camera() -> Camera {
        Camera::look_at(Vec3::new(1.5, 1.2, 2.5), Vec3::new(0.0, 0.8, 0.0), Vec3::Y, 55f32.to_radians(), 640, 480)
    }

    fn splat(pos: Vec3, scale: [f32; 3], rot: Quat) -> Splat {
        Splat { pos: pos.to_array(), scale, rot: rot.to_array(), rgba: [255; 4] }
    }

    #[test]
    fn cov6_matches_reference_covariance() {
        let mut rng = Rng::new(1);
        for _ in 0..50 {
            let s = rng.splat(2.0, 0.01, 0.5);
            let c = cov6(&s);
            let m = s.covariance();
            assert!(cov6_to_mat3(&c).abs_diff_eq(m, 1e-5), "{c:?} vs {m:?}");
        }
    }

    #[test]
    fn cov6_tolerates_unnormalized_and_degenerate_quaternions() {
        let q = Quat::from_rotation_y(0.7) * Quat::from_rotation_x(-0.3);
        let a = cov6(&splat(Vec3::ZERO, [0.1, 0.2, 0.3], q));
        let b = cov6(&Splat { rot: (q * 3.0).to_array(), ..splat(Vec3::ZERO, [0.1, 0.2, 0.3], q) });
        for i in 0..6 {
            assert!((a[i] - b[i]).abs() < 1e-6);
        }
        for bad in [[0.0; 4], [f32::NAN; 4], [f32::INFINITY, 0.0, 0.0, 1.0]] {
            let c = cov6(&Splat { rot: bad, ..splat(Vec3::ZERO, [0.1, 0.2, 0.3], q) });
            let expect = [0.01, 0.0, 0.0, 0.04, 0.0, 0.09];
            for i in 0..6 {
                assert!((c[i] - expect[i]).abs() < 1e-7, "{bad:?} -> {c:?}");
            }
        }
    }

    #[test]
    fn cov6_batch_fills_the_slice() {
        let mut rng = Rng::new(2);
        let splats: Vec<Splat> = (0..7).map(|_| rng.splat(1.0, 0.01, 0.1)).collect();
        let mut out = vec![[0.0; 6]; splats.len()];
        cov6_batch(&splats, &mut out);
        for (s, c) in splats.iter().zip(&out) {
            assert_eq!(*c, cov6(s));
        }
    }

    #[test]
    fn projected_center_equals_camera_project() {
        let cam = camera();
        let vp = ViewParams::from_camera(&cam);
        let mut rng = Rng::new(3);
        let mut seen = 0;
        for _ in 0..500 {
            let s = rng.splat(1.5, 0.01, 0.1);
            let expected = cam.project(s.position()).filter(|&(_, _, d)| d >= vp.near);
            let got = project_splat(s.position(), &cov6(&s), &vp);
            match (expected, got) {
                (Some((px, py, d)), Some(p)) => {
                    seen += 1;
                    assert!((p.px - px).abs() < 1e-2 && (p.py - py).abs() < 1e-2, "{p:?} vs {px} {py}");
                    assert!((p.depth - d).abs() < 1e-4);
                }
                (None, None) => {}
                other => panic!("visibility mismatch: {other:?}"),
            }
        }
        assert!(seen > 400);
    }

    #[test]
    fn splats_behind_or_too_close_are_culled() {
        let cam = camera();
        let vp = ViewParams::from_camera(&cam);
        let cov = cov6(&splat(Vec3::ZERO, [0.05; 3], Quat::IDENTITY));
        let behind = cam.position() - cam.forward();
        assert!(project_splat(behind, &cov, &vp).is_none());
        let too_close = cam.position() + cam.forward() * (0.5 * vp.near);
        assert!(project_splat(too_close, &cov, &vp).is_none());
        let fine = cam.position() + cam.forward() * (2.0 * vp.near);
        assert!(project_splat(fine, &cov, &vp).is_some());
        assert!(project_splat(Vec3::splat(f32::NAN), &cov, &vp).is_none());
        assert!(project_splat(fine, &[f32::NAN; 6], &vp).is_none());
    }

    #[test]
    fn isotropic_gaussian_projects_to_a_circle() {
        let cam = camera();
        let vp = ViewParams::from_camera(&cam);
        for &(dist, sigma) in &[(1.0f32, 0.02f32), (2.5, 0.05), (6.0, 0.1)] {
            let pos = cam.position() + cam.forward() * dist;
            let s = splat(pos, [sigma; 3], Quat::from_rotation_z(0.4));
            let p = project_splat(pos, &cov6(&s), &vp).unwrap();
            let expected = (cam.fx * sigma / dist).powi(2);
            let (a, b, c) = (p.cov[0] - LOW_PASS_PX2, p.cov[1], p.cov[2] - LOW_PASS_PX2);
            assert!((a - expected).abs() < 1e-3 * expected, "{a} vs {expected}");
            assert!((c - expected).abs() < 1e-3 * expected, "{c} vs {expected}");
            assert!(b.abs() < 1e-3 * expected);
            let r = CUTOFF_SIGMA * (expected + LOW_PASS_PX2).sqrt();
            assert!((p.radius - r).abs() < 1e-2 * r);
        }
    }

    #[test]
    fn conic_is_the_inverse_of_cov() {
        let cam = camera();
        let vp = ViewParams::from_camera(&cam);
        let mut rng = Rng::new(4);
        for _ in 0..200 {
            let s = rng.splat(0.8, 0.005, 0.2);
            let Some(p) = project_splat(s.position(), &cov6(&s), &vp) else { continue };
            let [a, b, c] = p.cov;
            let [qa, qb, qc] = p.conic;
            // Σ'·Σ'⁻¹ = I; tolerance scales with the conditioning.
            let tol = 1e-3 * (1.0 + (a + c) * (qa + qc) * 1e-2);
            assert!((a * qa + b * qb - 1.0).abs() < tol, "{p:?}");
            assert!((b * qb + c * qc - 1.0).abs() < tol, "{p:?}");
            assert!((a * qb + b * qc).abs() < tol, "{p:?}");
        }
    }

    #[test]
    fn image_y_axis_points_down_in_the_covariance() {
        // A needle pointing up-right in the image (world +X and +Y for this
        // camera) must have a negative xy term: going right, py decreases.
        let cam = Camera::look_at(Vec3::new(0.0, 0.0, 3.0), Vec3::ZERO, Vec3::Y, 60f32.to_radians(), 640, 480);
        let vp = ViewParams::from_camera(&cam);
        let s = splat(Vec3::ZERO, [0.3, 0.01, 0.01], Quat::from_rotation_z(45f32.to_radians()));
        let p = project_splat(s.position(), &cov6(&s), &vp).unwrap();
        assert!(p.cov[1] < -1.0, "{p:?}");
        let (major, _) = ellipse_axes(p.cov);
        assert!(major[0] * major[1] < 0.0, "major axis {major:?} should go up-right");
    }

    #[test]
    fn covariance_matches_monte_carlo_projection() {
        // Sample the 3D Gaussian, push every sample through the exact pinhole
        // projection and compare the sample covariance of the pixels with the
        // linearized (EWA) one. Valid for a small Gaussian near the axis.
        let cam = camera();
        let vp = ViewParams::from_camera(&cam);
        let center = Vec3::new(0.05, 0.85, -0.1);
        let s = splat(center, [0.03, 0.012, 0.004], Quat::from_euler(glam::EulerRot::YXZ, 0.6, -0.4, 1.1));
        let p = project_splat(center, &cov6(&s), &vp).unwrap();

        let rot = s.rotation();
        let scale = Vec3::from_array(s.scale);
        let mut rng = Rng::new(5);
        let n = 200_000;
        let (mut sx, mut sy, mut sxx, mut sxy, mut syy) = (0f64, 0f64, 0f64, 0f64, 0f64);
        for _ in 0..n {
            let local = Vec3::new(rng.normal(), rng.normal(), rng.normal()) * scale;
            let (px, py, _) = cam.project(center + rot * local).unwrap();
            let (x, y) = ((px - p.px) as f64, (py - p.py) as f64);
            sx += x;
            sy += y;
            sxx += x * x;
            sxy += x * y;
            syy += y * y;
        }
        let nf = n as f64;
        let (mx, my) = (sx / nf, sy / nf);
        let mc = [sxx / nf - mx * mx, sxy / nf - mx * my, syy / nf - my * my];
        let ewa = [(p.cov[0] - LOW_PASS_PX2) as f64, p.cov[1] as f64, (p.cov[2] - LOW_PASS_PX2) as f64];
        let scale_ref = ewa[0].max(ewa[2]);
        assert!(scale_ref > 4.0, "test splat too small on screen: {ewa:?}");
        for i in 0..3 {
            assert!((mc[i] - ewa[i]).abs() < 0.02 * scale_ref, "term {i}: monte-carlo {mc:?} vs ewa {ewa:?}");
        }
        assert!(mx.abs() < 0.05 && my.abs() < 0.05, "mean offset {mx} {my}");
    }

    #[test]
    fn uniform_scale_in_view_matrix_equals_scaling_the_scene() {
        let cam = camera();
        let mut rng = Rng::new(6);
        for &k in &[0.05f32, 0.3, 4.0] {
            // (a) scale baked into the view matrix, scene untouched.
            let mut vp_view = ViewParams::from_camera(&cam);
            vp_view.view = cam.view_matrix() * Mat4::from_scale(Vec3::splat(k));
            // (b) plain view matrix, scene actually scaled.
            let vp_plain = ViewParams::from_camera(&cam);
            let mut compared = 0;
            for _ in 0..200 {
                let s = rng.splat(1.0 / k, 0.02 / k, 0.2 / k);
                let scaled = Splat {
                    pos: (s.position() * k).to_array(),
                    scale: [s.scale[0] * k, s.scale[1] * k, s.scale[2] * k],
                    ..s
                };
                let a = project_splat(s.position(), &cov6(&s), &vp_view);
                let b = project_splat(scaled.position(), &cov6(&scaled), &vp_plain);
                match (a, b) {
                    (Some(a), Some(b)) => {
                        compared += 1;
                        assert!((a.px - b.px).abs() < 2e-2 && (a.py - b.py).abs() < 2e-2, "{a:?} {b:?}");
                        assert!((a.depth - b.depth).abs() < 1e-4 * (1.0 + b.depth));
                        for i in 0..3 {
                            let tol = 2e-3 * (b.cov[0] + b.cov[2]);
                            assert!((a.cov[i] - b.cov[i]).abs() < tol, "{a:?} {b:?}");
                        }
                        assert!((a.radius - b.radius).abs() < 1e-2 * b.radius);
                    }
                    (None, None) => {}
                    // The near-plane test may disagree in the last ulp only.
                    (a, b) => {
                        let d = a.or(b).unwrap().depth;
                        assert!((d - vp_plain.near).abs() < 1e-4, "visibility mismatch at depth {d}");
                    }
                }
            }
            assert!(compared > 50, "k = {k}: only {compared} splats compared");
        }
    }

    #[test]
    fn splat_scale_multiplies_standard_deviations() {
        let cam = camera();
        let mut vp = ViewParams::from_camera(&cam);
        let s = splat(Vec3::new(0.1, 0.9, 0.0), [0.05, 0.02, 0.01], Quat::from_rotation_x(0.5));
        let base = project_splat(s.position(), &cov6(&s), &vp).unwrap();
        vp.splat_scale = 0.5;
        let half = project_splat(s.position(), &cov6(&s), &vp).unwrap();
        assert_eq!((base.px, base.py, base.depth), (half.px, half.py, half.depth));
        for (i, lp) in [(0, LOW_PASS_PX2), (1, 0.0), (2, LOW_PASS_PX2)] {
            let expected = 0.25 * (base.cov[i] - lp) + lp;
            assert!((half.cov[i] - expected).abs() < 1e-4 * base.cov[0].max(base.cov[2]));
        }
    }

    #[test]
    fn far_off_axis_footprint_is_bounded_by_the_frustum_clamp() {
        // Same depth, increasingly off-axis: the footprint stops growing once
        // outside 1.3× the frustum.
        let cam = Camera::look_at(Vec3::ZERO, Vec3::NEG_Z, Vec3::Y, 60f32.to_radians(), 640, 480);
        let vp = ViewParams::from_camera(&cam);
        let cov = cov6(&splat(Vec3::ZERO, [0.05; 3], Quat::IDENTITY));
        let at = |x: f32| project_splat(Vec3::new(x, 0.0, -1.0), &cov, &vp).unwrap();
        let tan_half_x = 0.5 * vp.width / vp.fx;
        let inside = at(0.5 * tan_half_x);
        let edge = at(FRUSTUM_CLAMP * tan_half_x);
        let outside = at(10.0 * tan_half_x);
        assert!(inside.cov[0] < edge.cov[0]);
        assert!((outside.cov[0] - edge.cov[0]).abs() < 1e-2 * edge.cov[0], "{outside:?} vs {edge:?}");
        // The center itself is not clamped.
        assert!(outside.px > edge.px + 1000.0);
    }

    #[test]
    fn from_projection_recovers_intrinsics() {
        let mut cam = camera();
        // Off-center principal point, as in a VR eye.
        cam.cx = 280.0;
        cam.cy = 260.0;
        cam.fy *= 1.1;
        let a = ViewParams::from_camera(&cam);
        for far in [100.0, 1.0e7] {
            let b = ViewParams::from_projection(cam.view_matrix(), &cam.projection_matrix(0.1, far), 640.0, 480.0);
            assert!((a.fx - b.fx).abs() < 1e-2 && (a.fy - b.fy).abs() < 1e-2, "{a:?} {b:?}");
            assert!((a.cx - b.cx).abs() < 1e-2 && (a.cy - b.cy).abs() < 1e-2, "{a:?} {b:?}");
            assert!((b.near - 0.1).abs() < 1e-3, "near {}", b.near);
        }
        // Not a perspective matrix: the near plane falls back to the default.
        assert_eq!(ViewParams::from_projection(Mat4::IDENTITY, &Mat4::IDENTITY, 640.0, 480.0).near, DEFAULT_NEAR);
    }

    #[test]
    fn ellipse_axes_span_the_three_sigma_contour() {
        let cam = camera();
        let vp = ViewParams::from_camera(&cam);
        let mut rng = Rng::new(7);
        let mut covs: Vec<[f32; 3]> =
            vec![[4.0, 0.0, 4.0], [9.0, 0.0, 1.0], [1.0, 0.0, 9.0], [5.0, 3.0, 5.0], [5.0, -3.0, 5.0]];
        for _ in 0..200 {
            let s = rng.splat(0.8, 0.003, 0.3);
            if let Some(p) = project_splat(s.position(), &cov6(&s), &vp) {
                covs.push(p.cov);
            }
        }
        for cov in covs {
            let [a, b, c] = cov;
            let det = a * c - b * b;
            let conic = [c / det, -b / det, a / det];
            let (major, minor) = ellipse_axes(cov);
            let (major, minor) = (Vec2::from(major), Vec2::from(minor));
            assert!(major.length() >= minor.length() * 0.999, "{cov:?}");
            assert!(major.dot(minor).abs() < 1e-3 * major.length() * minor.length().max(1e-6), "{cov:?}");
            // minor is major turned by +90°.
            assert!(major.perp_dot(minor) > 0.0, "{cov:?}");
            // Mahalanobis distance² of each axis end point is 3² = 9; so is
            // 9·|q|² for any q on the unit circle of the quad.
            for (qx, qy) in [(1.0f32, 0.0f32), (0.0, 1.0), (0.6, 0.8), (-0.8, 0.6)] {
                let d = major * qx + minor * qy;
                let m = conic[0] * d.x * d.x + 2.0 * conic[1] * d.x * d.y + conic[2] * d.y * d.y;
                let tol = 0.02 * (1.0 + (a + c) * (conic[0] + conic[2]) * 1e-3);
                assert!((m - 9.0).abs() < 9.0 * tol, "{cov:?}: m = {m}");
            }
        }
    }

    #[test]
    fn radius_bounds_the_ellipse() {
        let cam = camera();
        let vp = ViewParams::from_camera(&cam);
        let mut rng = Rng::new(8);
        for _ in 0..200 {
            let s = rng.splat(0.8, 0.003, 0.3);
            let Some(p) = project_splat(s.position(), &cov6(&s), &vp) else { continue };
            let (major, minor) = ellipse_axes(p.cov);
            let major_len = Vec2::from(major).length();
            assert!((p.radius - major_len).abs() < 1e-3 * major_len + 1e-3, "{p:?}");
            assert!(Vec2::from(minor).length() <= p.radius * 1.001);
        }
    }

    #[test]
    fn projection_matches_numeric_jacobian_for_general_cameras() {
        // Second, independent derivation: numeric differentiation of the
        // pinhole model in f64. Cameras have roll, non-square pixels and an
        // off-center principal point (VR eyes); every other view also carries
        // a uniform scale.
        let mut rng = Rng::new(40);
        let mut compared = 0;
        for i in 0..3000 {
            let eye = Vec3::new(rng.range(-3.0, 3.0), rng.range(0.2, 2.5), rng.range(-3.0, 3.0));
            let target = Vec3::new(rng.range(-0.5, 0.5), rng.range(0.3, 1.2), rng.range(-0.5, 0.5));
            let mut cam = Camera::look_at(eye, target, Vec3::Y, rng.range(0.5, 1.6), 640, 480);
            cam.world_from_cam *= Affine3A::from_rotation_z(rng.range(-1.0, 1.0));
            cam.fx *= rng.range(0.7, 1.4);
            cam.cx += rng.range(-150.0, 150.0);
            cam.cy += rng.range(-100.0, 100.0);
            let k = if i % 2 == 0 { 1.0 } else { rng.range(0.05, 5.0) };
            let mut vp = ViewParams::from_camera(&cam);
            vp.view = cam.view_matrix() * Mat4::from_scale(Vec3::splat(k));
            let mut s = rng.splat(1.0, 0.002, 0.08);
            s.pos[1] += 0.7;
            for v in s.pos.iter_mut().chain(&mut s.scale) {
                *v /= k;
            }
            let Some(p) = project_splat(s.position(), &cov6(&s), &vp) else { continue };
            // Outside the image the Jacobian is clamped on purpose.
            if !(0.0..=vp.width).contains(&p.px) || !(0.0..=vp.height).contains(&p.py) {
                continue;
            }
            let (center, cov, depth) =
                test_util::reference_projection(&s, &vp.view, [vp.fx, vp.fy, vp.cx, vp.cy]).expect("in front");
            compared += 1;
            assert!((p.px as f64 - center[0]).abs() < 0.02 && (p.py as f64 - center[1]).abs() < 0.02, "{p:?}");
            assert!((p.depth as f64 - depth).abs() < 1e-4 * (1.0 + depth));
            let tol = 2e-3 * (cov[0] + cov[2]) + 1e-3;
            let lp = LOW_PASS_PX2 as f64;
            for (got, want) in p.cov.iter().zip([cov[0] + lp, cov[1], cov[2] + lp]) {
                assert!((*got as f64 - want).abs() < tol, "{p:?} vs {cov:?}");
            }
        }
        assert!(compared > 1000, "{compared}");
    }

    /// Literal transcription of the GLSL sketch in the module documentation.
    /// Returns `(px, [a, b, c], major, minor)`.
    fn glsl_sketch(p: Vec3, c: &[f32; 6], vp: &ViewParams) -> Option<(Vec2, [f32; 3], Vec2, Vec2)> {
        let (fx, fy, cx, cy, w, h, k) = (vp.fx, vp.fy, vp.cx, vp.cy, vp.width, vp.height, vp.splat_scale);
        let t = vp.view * p.extend(1.0);
        let d = -t.z;
        if d < vp.near {
            return None;
        }
        let mut uv = Vec2::new(t.x, t.y) / d;
        let px = Vec2::new(cx + fx * uv.x, cy - fy * uv.y);
        uv = uv.clamp(
            Vec2::new((-cx - 0.15 * w) / fx, (cy - 1.15 * h) / fy),
            Vec2::new((w - cx + 0.15 * w) / fx, (cy + 0.15 * h) / fy),
        );
        let wt = Mat3::from_mat4(vp.view).transpose();
        let t0 = (fx / d) * (wt.x_axis + uv.x * wt.z_axis);
        let t1 = -(fy / d) * (wt.y_axis + uv.y * wt.z_axis);
        let s = Mat3::from_cols_array(&[c[0], c[1], c[2], c[1], c[3], c[4], c[2], c[4], c[5]]);
        let ma = (k * k * t0.dot(s * t0)).max(0.0);
        let mb = k * k * t0.dot(s * t1);
        let mc = (k * k * t1.dot(s * t1)).max(0.0);
        let (a, b, c) = (ma + 0.3, mb, mc + 0.3);
        let det = (ma * mc - mb * mb).max(0.0) + 0.3 * (ma + mc) + 0.09;
        let mid = 0.5 * (a + c);
        let rad = Vec2::new(0.5 * (a - c), b).length();
        let l1 = mid + rad;
        let l2 = det / l1;
        let mut v1 = if a >= c { Vec2::new(0.5 * (a - c) + rad, b) } else { Vec2::new(b, 0.5 * (c - a) + rad) };
        let len = v1.length();
        v1 = if len > 1e-12 { v1 / len } else { Vec2::X };
        Some((px, [a, b, c], 3.0 * l1.sqrt() * v1, 3.0 * l2.sqrt() * Vec2::new(-v1.y, v1.x)))
    }

    #[test]
    fn documented_glsl_sketch_computes_the_same_quad() {
        let mut rng = Rng::new(41);
        let mut compared = 0;
        for _ in 0..2000 {
            let eye = Vec3::new(rng.range(-3.0, 3.0), rng.range(0.2, 2.5), rng.range(-3.0, 3.0));
            let mut cam = Camera::look_at(eye, Vec3::new(0.0, 0.8, 0.0), Vec3::Y, rng.range(0.6, 1.4), 640, 480);
            cam.cx += rng.range(-80.0, 80.0);
            cam.cy += rng.range(-80.0, 80.0);
            let mut vp = ViewParams::from_camera(&cam);
            vp.splat_scale = rng.range(0.3, 1.5);
            let mut s = rng.splat(1.0, 0.002, 0.2);
            s.pos[1] += 0.7;
            let cov = cov6(&s);
            let reference = project_splat_with_axes(s.position(), &cov, &vp);
            let sketch = glsl_sketch(s.position(), &cov, &vp);
            assert_eq!(reference.is_some(), sketch.is_some());
            let (Some((p, major, minor)), Some((px, abc, g_major, g_minor))) = (reference, sketch) else { continue };
            compared += 1;
            assert!((p.px - px.x).abs() < 1e-2 && (p.py - px.y).abs() < 1e-2);
            let size = abc[0] + abc[2];
            for i in 0..3 {
                assert!((p.cov[i] - abc[i]).abs() < 1e-4 * size, "{p:?} vs {abc:?}");
            }
            let tol = 1e-3 * g_major.length() + 1e-3;
            assert!((Vec2::from(major) - g_major).length() < tol, "{major:?} vs {g_major:?}");
            assert!((Vec2::from(minor) - g_minor).length() < tol, "{minor:?} vs {g_minor:?}");
            // Fragment stage: alpha from the quad coordinate equals alpha
            // from the conic at the same pixel offset.
            let q = Vec2::new(rng.range(-0.7, 0.7), rng.range(-0.7, 0.7));
            let offset = q.x * g_major + q.y * g_minor;
            let alpha_quad = 0.8 * (-4.5 * q.dot(q)).exp();
            let alpha_conic = eval_alpha(0.8, p.conic, offset.x, offset.y);
            if alpha_quad > 0.02 && size * (p.conic[0] + p.conic[2]) < 1e4 {
                assert!((alpha_quad - alpha_conic).abs() < 0.01, "{alpha_quad} vs {alpha_conic}");
            }
        }
        assert!(compared > 1500);
    }

    #[test]
    fn axes_from_projection_method_and_free_function_agree() {
        let cam = camera();
        let vp = ViewParams::from_camera(&cam);
        let mut rng = Rng::new(42);
        for _ in 0..300 {
            let s = rng.splat(0.8, 0.003, 0.3);
            let Some((p, major, minor)) = project_splat_with_axes(s.position(), &cov6(&s), &vp) else { continue };
            assert_eq!(project_splat(s.position(), &cov6(&s), &vp), Some(p));
            let (major, minor) = (Vec2::from(major), Vec2::from(minor));
            let tol = 2e-3 * major.length() + 1e-3;
            let (m1, m2) = p.ellipse_axes();
            assert!((Vec2::from(m1) - major).length() < tol && (Vec2::from(m2) - minor).length() < tol, "{p:?}");
            let (f1, f2) = ellipse_axes(p.cov);
            assert!((Vec2::from(f1) - major).length() < tol, "{p:?}");
            // The free function only sees the covariance: looser on the minor axis.
            assert!((Vec2::from(f2) - minor).length() < 0.05 * minor.length() + tol, "{p:?}");
        }
        // Point footprints go through the method too.
        let s = splat(Vec3::new(0.0, 0.8, 0.0), [0.3, 0.01, 0.05], Quat::IDENTITY);
        let q = project_splat(s.position(), &cov6(&s), &vp).unwrap().with_point_footprint(2.0);
        assert_eq!(q.ellipse_axes(), ([6.0, 0.0], [0.0, 6.0]));
    }

    #[test]
    fn extreme_needles_are_never_culled_thinned_or_indefinite() {
        // Meters long, microns thin, a few decimeters from the camera: the
        // thin axis is far below f32 resolution of the world covariance.
        // Regression: `a·c − b²` used to cancel to ≤ 0 (splat culled) or to a
        // minor axis below the low-pass, and the conic could be indefinite.
        let cam = Camera::look_at(Vec3::ZERO, Vec3::NEG_Z, Vec3::Y, 1.0, 640, 480);
        let vp = ViewParams::from_camera(&cam);
        let floor = CUTOFF_SIGMA * LOW_PASS_PX2.sqrt();
        let mut rng = Rng::new(43);
        for _ in 0..20_000 {
            let long = rng.range(0.1, 5.0);
            let thin = 10f32.powf(rng.range(-7.0, -2.0));
            let s = Splat {
                pos: [rng.range(-1.0, 1.0), rng.range(-1.0, 1.0), -rng.range(0.06, 3.0)],
                scale: [long, thin, thin * rng.range(0.5, 2.0)],
                rot: rng.quat().to_array(),
                rgba: [255; 4],
            };
            let (p, major, minor) =
                project_splat_with_axes(s.position(), &cov6(&s), &vp).unwrap_or_else(|| panic!("culled: {s:?}"));
            let (major, minor) = (Vec2::from(major), Vec2::from(minor));
            assert!(major.is_finite() && minor.is_finite(), "{p:?}");
            assert!(minor.length() >= 0.999 * floor, "minor axis {} below the low-pass: {p:?}", minor.length());
            assert!(minor.length() <= major.length() * 1.0001 && (p.radius - major.length()).abs() < 1e-3 * p.radius);
            assert!(p.conic[0] > 0.0 && p.conic[2] > 0.0 && p.conic.iter().all(|q| q.is_finite()), "{p:?}");
            let (m1, m2) = p.ellipse_axes();
            assert!((Vec2::from(m1) - major).length() <= 1e-3 * major.length(), "{p:?}");
            assert!((Vec2::from(m2).length() / minor.length() - 1.0).abs() < 0.05, "{p:?}: {m2:?} vs {minor:?}");
            // Alpha never exceeds the opacity, wherever it is sampled.
            for i in -10..=10 {
                let d = major * (i as f32 / 10.0);
                let alpha = eval_alpha(0.5, p.conic, d.x.round(), d.y.round());
                assert!((0.0..=0.5).contains(&alpha), "alpha {alpha} for {p:?}");
            }
        }
    }

    #[test]
    fn eval_alpha_is_bounded_by_the_opacity_when_the_conic_form_cancels() {
        // Conic of a 4 m × 15 µm needle seen from 0.5 m. It is (barely)
        // positive definite, but at this offset along the needle the three
        // terms of δᵀ·conic·δ cancel to a negative f32.
        let conic = [4.144_829_8, -2.821_335_3, 1.920_448_8];
        let (dx, dy) = (371.0, 545.0);
        assert!(conic[0] * dx * dx + 2.0 * conic[1] * dx * dy + conic[2] * dy * dy < 0.0);
        assert_eq!(eval_alpha(0.5, conic, dx, dy), 0.5);
    }

    #[test]
    fn eval_alpha_follows_the_documented_formula() {
        let conic = [1.0 / 4.0, 0.0, 1.0 / 16.0]; // σx = 2, σy = 4
        assert_eq!(eval_alpha(1.0, conic, 0.0, 0.0), 1.0);
        assert!((eval_alpha(0.8, conic, 2.0, 0.0) - 0.8 * (-0.5f32).exp()).abs() < 1e-6);
        assert!((eval_alpha(0.8, conic, 0.0, 4.0) - 0.8 * (-0.5f32).exp()).abs() < 1e-6);
        // Just inside / outside 3σ.
        assert!(eval_alpha(1.0, conic, 5.99, 0.0) > 0.0);
        assert_eq!(eval_alpha(1.0, conic, 6.01, 0.0), 0.0);
        // Below 1/255.
        assert_eq!(eval_alpha(0.003, conic, 0.0, 0.0), 0.0);
        assert_eq!(eval_alpha(0.02, conic, 4.0, 0.0), 0.0);
        assert_eq!(eval_alpha(1.0, conic, f32::NAN, 0.0), 0.0);
    }

    #[test]
    fn point_footprint_is_isotropic_and_keeps_the_center() {
        let cam = camera();
        let vp = ViewParams::from_camera(&cam);
        let s = splat(Vec3::new(0.0, 0.8, 0.0), [0.3, 0.01, 0.05], Quat::from_rotation_z(0.3));
        let p = project_splat(s.position(), &cov6(&s), &vp).unwrap();
        let q = p.with_point_footprint(1.5);
        assert_eq!((p.px, p.py, p.depth), (q.px, q.py, q.depth));
        assert_eq!(q.cov, [2.25, 0.0, 2.25]);
        assert!((q.conic[0] - 1.0 / 2.25).abs() < 1e-6 && q.conic[1] == 0.0);
        assert!((q.radius - 4.5).abs() < 1e-5);
    }

    #[test]
    fn rigid_view_change_keeps_camera_space_result() {
        // Moving the camera and the scene together changes nothing.
        let cam = camera();
        let moved_pose = Affine3A::from_rotation_translation(Quat::from_rotation_y(1.0), Vec3::new(3.0, -1.0, 2.0));
        let mut cam2 = cam;
        cam2.world_from_cam = moved_pose * cam.world_from_cam;
        let s = splat(Vec3::new(0.2, 0.7, -0.3), [0.08, 0.02, 0.04], Quat::from_rotation_x(0.9));
        let s2 = Splat {
            pos: moved_pose.transform_point3(s.position()).to_array(),
            rot: (Quat::from_rotation_y(1.0) * s.rotation()).to_array(),
            ..s
        };
        let a = project_splat(s.position(), &cov6(&s), &ViewParams::from_camera(&cam)).unwrap();
        let b = project_splat(s2.position(), &cov6(&s2), &ViewParams::from_camera(&cam2)).unwrap();
        assert!((a.px - b.px).abs() < 1e-2 && (a.py - b.py).abs() < 1e-2);
        for i in 0..3 {
            assert!((a.cov[i] - b.cov[i]).abs() < 1e-3 * (a.cov[0] + a.cov[2]));
        }
    }
}
