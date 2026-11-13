//! RGB-D to Gaussians, background subtraction, voxel fusion.
//!
//! Everything here consumes only what a real rig delivers: an [`RgbdFrame`]
//! and the calibration of the [`Camera`] that took it. Nothing looks at the
//! scene geometry.
//!
//! # From a depth pixel to a Gaussian
//!
//! For a plane `n·P = d` in camera space the inverse depth `w = 1/z` is an
//! *affine* function of the pixel coordinates, so finite differences of `w`
//! recover the plane exactly (differences of `z` do not). Per valid pixel:
//!
//! 1. `w` and its gradient `(a, b) = (∂w/∂u, ∂w/∂v)` from the 4-neighbours.
//!    Neighbours across a depth discontinuity are ignored. A step within the
//!    edge threshold is always one surface; a larger one only if it repeats
//!    (the same step on the other side of the pixel, or one pixel further
//!    out), which is what a plane at a grazing angle does and a depth edge
//!    does not. When both sides of an axis are usable their differences are
//!    blended with weights that favour the flatter side: on a plane that is
//!    the central difference, at a small step (an arm in front of the chest)
//!    it follows the surface the pixel belongs to instead of bridging it.
//! 2. With the pixel ray `dir(u, v) = ((u − cx)/fx, −(v − cy)/fy, −1)` the
//!    surface is `P(u, v) = dir/w`, hence its tangents per pixel step
//!    `Pu = z·((1/fx, 0, 0) − dir·a·z)` and `Pv = z·((0, −1/fy, 0) − dir·b·z)`
//!    and the normal `n ∝ Pv × Pu` (facing the camera). Without any usable
//!    neighbour `a = b = 0`: a splat facing the camera.
//! 3. The Gaussian is the image of a round pixel-sized blob pushed onto the
//!    surface: `Σ = k²·(Pu·Puᵀ + Pv·Pvᵀ)`. At the image center this is the
//!    familiar pair of tangent axes `k·r` and `k·r / cos θ` with the pixel
//!    footprint `r = z/fx`; off-center it also accounts for the obliquity of
//!    the pixel ray. Its projection into the source camera covers about one
//!    pixel whatever the slant. The long axis is capped at
//!    `max_anisotropy ×` the short one, and the Gaussian is thin along `n`.
//! 4. On a silhouette (the neighbour is farther away or has no depth) the
//!    surface ends somewhere before the next pixel center, so the footprint
//!    is narrowed across the silhouette ([`ReconstructParams::edge_shrink`]).
//! 5. Observations with `cos θ < min_cos` (grazing) and pixels with too few
//!    continuous neighbours (flying pixels, speckles) are dropped.
//!
//! # Live stage: background subtraction
//!
//! [`BackgroundPlate`] holds the empty stage as seen by one camera.
//! [`foreground_mask`] marks the pixels that are clearly closer than the
//! plate ([`MASK_FOREGROUND`]) and those at plate depth whose color changed
//! ([`MASK_CHANGED`]: the dancer's contact shadow). The latter sit exactly on
//! the static layer, see [`BackgroundParams::shadow_lift`] for how they win.
//!
//! # Home scan: voxel fusion
//!
//! [`VoxelFusion`] keeps one splat per surface cell, created by the first
//! good observation, so a scan grows by appending and never duplicates;
//! later, better views refine a cell's splat in place.

use std::collections::HashMap;
use std::hash::{BuildHasherDefault, Hasher};

use glam::{Mat3, Quat, Vec3};
#[cfg(feature = "par")]
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::types::{Camera, RgbdFrame, Splat};

/// Mask value: the pixel produces no splat.
pub const MASK_SKIP: u8 = 0;
/// Mask value: the pixel is in front of the background plate.
pub const MASK_FOREGROUND: u8 = 1;
/// Mask value: the pixel lies on the background surface but its color
/// changed (a shadow cast by the foreground). Its splat is lifted off the
/// surface, see [`BackgroundParams::shadow_lift`].
pub const MASK_CHANGED: u8 = 2;

/// `value` when it is a finite number, `fallback` otherwise, then clamped.
#[inline]
fn finite_clamp(value: f32, fallback: f32, lo: f32, hi: f32) -> f32 {
    if value.is_finite() { value.clamp(lo, hi) } else { fallback }
}

/// Parameters of the depth-to-Gaussian conversion.
///
/// Fields missing from serialized data take their [`Default`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReconstructParams {
    /// Footprint factor `k`: the tangential standard deviation is `k` pixel
    /// footprints. A single camera needs about 0.6 for a closed surface;
    /// where the views of several cameras overlap, as on the stage, their
    /// splats interleave and a smaller value gives a sharper picture (the
    /// default was chosen by held-out PSNR with 3 to 12 ring cameras).
    /// Larger values blur and fatten silhouettes.
    pub footprint: f32,
    /// Standard deviation along the normal, as a fraction of the shorter
    /// tangential one.
    pub thickness: f32,
    /// Opacity of every splat, `0..=1`.
    pub opacity: f32,
    /// Observations whose normal makes a cosine below this with the viewing
    /// ray are dropped (grazing angle).
    pub min_cos: f32,
    /// Two neighbouring pixels are on the same surface when their depths
    /// differ by less than this fraction of the depth. Larger steps are depth
    /// edges, unless they continue a plane seen at a grazing angle (see the
    /// module documentation).
    pub edge_threshold: f32,
    /// Largest ratio between the two tangential standard deviations.
    pub max_anisotropy: f32,
    /// A pixel needs at least this many depth-continuous 4-neighbours;
    /// lonelier ones are flying pixels or speckles and are dropped. With `0`
    /// an isolated pixel, whose normal cannot be estimated, becomes a
    /// camera-facing isotropic splat.
    pub min_support: u32,
    /// Edge-preserving smoothing passes over the inverse depth before
    /// normals and positions are computed. Each pass is exact on planes;
    /// one or two tame sensor noise.
    pub smooth_passes: u32,
    /// Factor applied to the footprint of a silhouette pixel across its
    /// silhouette. The pixel center lies on the surface, the surface ends
    /// somewhere before the next pixel center: a full-size Gaussian would
    /// stick out and fatten every outline by a fraction of a pixel. `1`
    /// disables the correction.
    pub edge_shrink: f32,
    /// Background subtraction of the live stage.
    pub background: BackgroundParams,
}

impl Default for ReconstructParams {
    fn default() -> Self {
        Self {
            footprint: 0.45,
            thickness: 0.1,
            opacity: 0.85,
            min_cos: 0.2,
            edge_threshold: 0.03,
            max_anisotropy: 4.0,
            min_support: 1,
            smooth_passes: 2,
            edge_shrink: 0.5,
            background: BackgroundParams::default(),
        }
    }
}

impl ReconstructParams {
    /// Largest accepted [`ReconstructParams::smooth_passes`].
    pub const MAX_SMOOTH_PASSES: u32 = 4;

    /// The same parameters with every field forced into its safe range;
    /// values that are not finite numbers are replaced by their default.
    pub fn sanitized(self) -> Self {
        let d = Self::default();
        Self {
            footprint: finite_clamp(self.footprint, d.footprint, 0.1, 4.0),
            thickness: finite_clamp(self.thickness, d.thickness, 0.005, 1.0),
            opacity: finite_clamp(self.opacity, d.opacity, 0.02, 1.0),
            min_cos: finite_clamp(self.min_cos, d.min_cos, 0.0, 0.95),
            edge_threshold: finite_clamp(self.edge_threshold, d.edge_threshold, 1.0e-3, 1.0),
            max_anisotropy: finite_clamp(self.max_anisotropy, d.max_anisotropy, 1.0, 20.0),
            min_support: self.min_support.min(4),
            smooth_passes: self.smooth_passes.min(Self::MAX_SMOOTH_PASSES),
            edge_shrink: finite_clamp(self.edge_shrink, d.edge_shrink, 0.25, 1.0),
            background: self.background.sanitized(),
        }
    }
}

/// Parameters of [`foreground_mask`].
///
/// Fields missing from serialized data take their [`Default`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BackgroundParams {
    /// A pixel is foreground when it is closer than the plate by more than
    /// `depth_margin + noise_sigmas · depth_sigma_at_1m · z²` meters.
    pub depth_margin: f32,
    /// Expected depth noise of the sensor at 1 m, in meters (it grows with
    /// `z²`). This is a property of the sensor, known from its data sheet.
    pub depth_sigma_at_1m: f32,
    /// How many noise standard deviations the depth test tolerates.
    pub noise_sigmas: f32,
    /// A pixel at plate depth counts as "changed appearance" when one of its
    /// color channels differs from the plate by more than this many 8-bit
    /// levels. `255` or more disables the test (no shadows in the live layer).
    pub color_threshold: f32,
    /// How far splats of "changed appearance" pixels are moved along the
    /// surface normal toward the camera, in pixel footprints (`z / fx`).
    ///
    /// Such a splat lies exactly on the static-layer surface that it must
    /// hide. Renderers sort by the depth of the splat centers, so coplanar
    /// splats of both layers would interleave at random and the shadow would
    /// come out half-transparent and shimmer. Lifting the live splat puts it
    /// nearer to any viewer on the visible side of the surface. A viewer
    /// looking straight down needs next to nothing; one looking along the
    /// floor at elevation `e` needs the lift times `tan e` to exceed the
    /// spacing of the splat centers, which is a fraction of the footprint.
    /// Hence a lift proportional to the footprint: a couple of centimeters
    /// with the default 160x120 cameras, millimeters with HD ones.
    pub shadow_lift: f32,
    /// Mask clean-up: a marked pixel survives only when at least this many of
    /// its 8 neighbours are marked too. `0` disables the clean-up.
    pub min_neighbors: u32,
}

impl Default for BackgroundParams {
    fn default() -> Self {
        Self {
            depth_margin: 0.02,
            depth_sigma_at_1m: 0.0015,
            noise_sigmas: 4.0,
            color_threshold: 20.0,
            shadow_lift: 0.6,
            min_neighbors: 2,
        }
    }
}

impl BackgroundParams {
    /// The same parameters with every field forced into its safe range.
    pub fn sanitized(self) -> Self {
        let d = Self::default();
        Self {
            depth_margin: finite_clamp(self.depth_margin, d.depth_margin, 0.0, 10.0),
            depth_sigma_at_1m: finite_clamp(self.depth_sigma_at_1m, d.depth_sigma_at_1m, 0.0, 0.1),
            noise_sigmas: finite_clamp(self.noise_sigmas, d.noise_sigmas, 0.0, 20.0),
            color_threshold: finite_clamp(self.color_threshold, d.color_threshold, 1.0, 255.0),
            shadow_lift: finite_clamp(self.shadow_lift, d.shadow_lift, 0.0, 4.0),
            min_neighbors: self.min_neighbors.min(8),
        }
    }
}

// ---------------------------------------------------------------------------
// Per-pixel geometry
// ---------------------------------------------------------------------------

/// Inverse depth map of a frame (`0.0` where invalid), optionally smoothed.
fn inverse_depth(frame: &RgbdFrame, params: &ReconstructParams, edge: f32) -> Vec<f32> {
    let n = frame.width as usize * frame.height as usize;
    let mut inv: Vec<f32> =
        frame.depth.iter().take(n).map(|&z| if z > 0.0 && z.is_finite() { 1.0 / z } else { 0.0 }).collect();
    inv.resize(n, 0.0);
    let (w, h) = (frame.width as usize, frame.height as usize);
    if w >= 3 && h >= 3 {
        for _ in 0..params.smooth_passes.min(ReconstructParams::MAX_SMOOTH_PASSES) {
            inv = smooth_pass(&inv, w, h, edge);
        }
    }
    inv
}

/// One pass of a 3x3 binomial filter that only averages *opposite pairs* of
/// depth-continuous neighbours. Because every contribution is symmetric
/// around the pixel, an affine inverse depth (a plane) is left untouched,
/// also next to holes and silhouettes where a plain average would drift.
fn smooth_pass(inv: &[f32], w: usize, h: usize, edge: f32) -> Vec<f32> {
    let row = |y: usize, out: &mut [f32]| {
        for (x, o) in out.iter_mut().enumerate() {
            let i = y * w + x;
            let c = inv[i];
            *o = c;
            if c <= 0.0 || x == 0 || y == 0 || x + 1 == w || y + 1 == h {
                continue;
            }
            let near = |v: f32| v > 0.0 && (v - c).abs() <= edge * v.max(c);
            let mut sum = 4.0 * c;
            let mut weight = 4.0;
            // (offset to one neighbour, weight); its partner is the mirror image.
            for (d, k) in [(1, 2.0), (w, 2.0), (w + 1, 1.0), (w - 1, 1.0)] {
                let (p, q) = (inv[i - d], inv[i + d]);
                if near(p) && near(q) {
                    sum += k * (p + q);
                    weight += 2.0 * k;
                }
            }
            *o = sum / weight;
        }
    };
    let mut out = vec![0.0; inv.len()];
    #[cfg(feature = "par")]
    out.par_chunks_mut(w).enumerate().for_each(|(y, o)| row(y, o));
    #[cfg(not(feature = "par"))]
    out.chunks_mut(w).enumerate().for_each(|(y, o)| row(y, o));
    out
}

/// Local surface seen by one pixel, in camera space.
struct PixelSurface {
    /// Surface point.
    p: Vec3,
    /// Surface displacement per pixel step along image `u` and `v`.
    pu: Vec3,
    pv: Vec3,
    /// Unit normal facing the camera.
    n: Vec3,
    /// Cosine between the normal and the direction to the camera.
    cos: f32,
    /// Forward depth.
    z: f32,
    /// The surface ends (silhouette) toward `-u`, `+u`, `-v`, `+v`.
    open: [bool; 4],
    /// The surface curves or creases at this pixel.
    bent: bool,
}

/// Inverse-depth fit along one image axis.
struct AxisFit {
    /// Gradient of the inverse depth per pixel.
    grad: f32,
    /// Number of depth-continuous neighbours (0..=2).
    sides: u32,
    /// Silhouette on the lower / higher coordinate side.
    open: [bool; 2],
    /// No neighbour continues the surface, yet the depth keeps stepping by
    /// the same amount: a plane beyond the grazing limit, not an edge.
    steep: bool,
    /// Both neighbours continue the surface but their slopes differ: the
    /// surface curves or creases at this pixel.
    bent: bool,
}

/// One neighbour of a pixel along an image axis.
#[derive(Clone, Copy, PartialEq)]
enum Neighbor {
    /// Possibly the same surface; carries the inverse-depth difference
    /// `neighbour − pixel`.
    Near(f32),
    /// On a nearer surface (the pixel's surface goes on behind it).
    Nearer,
    /// On a farther surface: the pixel is on a silhouette.
    Farther,
    /// No depth.
    Invalid,
    /// Outside the image.
    Outside,
}

/// A pixel as a position along one image axis.
#[derive(Clone, Copy)]
struct Along {
    /// Index of the pixel in the frame.
    index: usize,
    /// Its coordinate along the axis, of `len`.
    pos: usize,
    len: usize,
    /// Index distance between neighbours along the axis.
    stride: usize,
}

impl Along {
    /// Index of the pixel `step` further along the axis, if inside the image.
    #[inline]
    fn offset(self, step: isize) -> Option<usize> {
        let q = self.pos as isize + step;
        (q >= 0 && q < self.len as isize).then(|| (self.index as isize + step * self.stride as isize) as usize)
    }

    /// The same axis seen from the pixel `index`, `step` further along.
    #[inline]
    fn moved(self, index: usize, step: isize) -> Self {
        Self { index, pos: (self.pos as isize + step) as usize, ..self }
    }
}

/// Intrinsics and the prepared depth of one frame.
struct FrameGeometry<'a> {
    cam: &'a Camera,
    inv: Vec<f32>,
    w: usize,
    h: usize,
    /// Relative depth step that always counts as one surface.
    edge: f32,
    /// Largest relative depth step per pixel of a surface at the grazing
    /// limit, along `u` and `v` (never below `edge`).
    edge_cap: [f32; 2],
    min_support: u32,
}

/// A step steeper than this slope (depth change per pixel footprint) next to
/// a clearly flatter one is a depth discontinuity, not a crease.
const KINK_SLOPE: f32 = 1.5;
/// Two successive inverse-depth differences belong to one plane when they
/// agree within this fraction of the edge threshold.
const LINEARITY: f32 = 0.35;

/// The relative depth step per pixel of a plane at the grazing limit
/// `min_cos`, for an image axis with focal length `f`: `tan θ / f`, with a
/// margin. Never below the configured edge threshold.
fn edge_cap(params: &ReconstructParams, f: f32, min_cos: f32) -> f32 {
    let cos = min_cos.clamp(0.05, 1.0);
    let tan = (1.0 - cos * cos).sqrt() / cos;
    params.edge_threshold.max(1.1 * tan / f.max(1.0))
}

impl<'a> FrameGeometry<'a> {
    /// `min_cos` is the grazing limit the caller will apply to the result.
    fn new(frame: &RgbdFrame, cam: &'a Camera, params: &ReconstructParams, min_cos: f32) -> Self {
        Self {
            cam,
            inv: inverse_depth(frame, params, params.edge_threshold),
            w: frame.width as usize,
            h: frame.height as usize,
            edge: params.edge_threshold,
            edge_cap: [edge_cap(params, cam.fx, min_cos), edge_cap(params, cam.fy, min_cos)],
            min_support: params.min_support,
        }
    }

    /// Inverse depth `step` pixels from `at` along its axis; `None` outside
    /// the image or without a depth.
    #[inline]
    fn raw(&self, at: Along, step: isize) -> Option<f32> {
        at.offset(step).map(|j| self.inv[j]).filter(|&v| v > 0.0)
    }

    /// The neighbour `step` pixels from `at` along its axis. `c` is the
    /// inverse depth the neighbour is compared with and `limit` the largest
    /// relative step of one surface.
    #[inline]
    fn neighbor(&self, c: f32, at: Along, step: isize, limit: f32) -> Neighbor {
        let Some(j) = at.offset(step) else { return Neighbor::Outside };
        let v = self.inv[j];
        if v <= 0.0 {
            Neighbor::Invalid
        } else if (v - c).abs() <= limit * v.max(c) {
            Neighbor::Near(v - c)
        } else if v > c {
            Neighbor::Nearer
        } else {
            Neighbor::Farther
        }
    }

    /// Fits the inverse depth `c` of the pixel `at` along one image axis:
    /// decides which of the two neighbours continue the pixel's surface,
    /// blends their differences and detects silhouettes.
    ///
    /// A neighbour within the edge threshold is on the same surface. A
    /// larger step is ambiguous at first sight: a plane seen at a grazing
    /// angle steps just as much per pixel as a real depth edge. But on a
    /// plane the inverse depth is affine, so the step repeats: it equals the
    /// step on the other side of the pixel, or the next step outward. A step
    /// that does not repeat is an edge.
    #[inline]
    fn fit_axis(&self, c: f32, at: Along, axis: usize, slope_scale: f32) -> AxisFit {
        let cap = self.edge_cap[axis];
        let tolerance = LINEARITY * self.edge * c;
        // `neighbour − pixel` differences, oriented along the axis.
        let diff = |n: Neighbor, step: isize| match n {
            Neighbor::Near(d) => Some(d * step as f32),
            _ => None,
        };
        let (n0, n1) = (self.neighbor(c, at, -1, cap), self.neighbor(c, at, 1, cap));
        let (d0, d1) = (diff(n0, -1), diff(n1, 1));
        // Does the step `d` toward `step` repeat one pixel further out?
        let repeats = |d: f32, step: isize| {
            let w1 = c + d * step as f32;
            let second = at.offset(step).and_then(|j| self.raw(at.moved(j, step), step));
            second.is_some_and(|w2| ((w2 - w1) * step as f32 - d).abs() <= tolerance)
        };
        // Does the surface demonstrably go on toward `step` with slope `d`?
        let confirmed = |d: f32, step: isize| d.abs() <= self.edge * c || repeats(d, step);
        let (keep0, keep1) = match (d0, d1) {
            (Some(a), Some(b)) => {
                if (a - b).abs() <= tolerance {
                    (true, true)
                } else {
                    // A kink. Keep the flatter side if it is a surface in
                    // its own right; the steeper one only if it is not
                    // steep enough to be a depth step.
                    let a_is_steeper = a.abs() > b.abs();
                    let (steep, flat) = if a_is_steeper { (a, b) } else { (b, a) };
                    let keep_steep = steep.abs() * slope_scale <= KINK_SLOPE || steep.abs() <= self.edge * c;
                    let keep_flat = confirmed(flat, if a_is_steeper { 1 } else { -1 });
                    if a_is_steeper { (keep_steep, keep_flat) } else { (keep_flat, keep_steep) }
                }
            }
            (Some(a), None) => (confirmed(a, -1), false),
            (None, Some(b)) => (false, confirmed(b, 1)),
            (None, None) => (false, false),
        };
        // A rejected neighbour is on another surface, nearer or farther.
        let settle = |n: Neighbor, keep: bool| match n {
            Neighbor::Near(d) if !keep => {
                if d > 0.0 {
                    Neighbor::Nearer
                } else {
                    Neighbor::Farther
                }
            }
            other => other,
        };
        let open = |n: Neighbor, step: isize| match n {
            Neighbor::Farther => true,
            // A lone missing pixel inside a surface is a sensor dropout, not
            // a silhouette.
            Neighbor::Invalid => !matches!(self.neighbor(c, at, 2 * step, 2.0 * cap), Neighbor::Near(_)),
            _ => false,
        };
        let bent = matches!((d0.filter(|_| keep0), d1.filter(|_| keep1)), (Some(a), Some(b)) if (a - b).abs() > tolerance);
        let (grad, sides) = match (d0.filter(|_| keep0), d1.filter(|_| keep1)) {
            (Some(a), Some(b)) => {
                // Differences that agree within the tolerance average (the
                // central difference); the more they disagree, the more the
                // flatter one counts, so that the normal next to a small
                // step or a crease is that of the pixel's own surface.
                let weight = |d: f32| 1.0 / (tolerance * tolerance + d * d);
                let (ga, gb) = (weight(a), weight(b));
                ((a * ga + b * gb) / (ga + gb), 2)
            }
            (Some(d), None) | (None, Some(d)) => (d, 1),
            (None, None) => (0.0, 0),
        };
        // A step too large for one surface that nevertheless repeats one
        // pixel further out: a plane beyond the grazing limit.
        let beyond = |n: Neighbor, step: isize| {
            matches!(n, Neighbor::Nearer | Neighbor::Farther)
                && self.raw(at, step).is_some_and(|w1| repeats((w1 - c) * step as f32, step))
        };
        let steep = sides == 0 && (beyond(n0, -1) || beyond(n1, 1));
        let (n0, n1) = (settle(n0, keep0), settle(n1, keep1));
        AxisFit { grad, sides, open: [open(n0, -1), open(n1, 1)], steep, bent }
    }

    /// The surface at pixel `(x, y)`, or `None` for an invalid or unsupported
    /// pixel.
    #[inline]
    fn surface(&self, x: usize, y: usize) -> Option<PixelSurface> {
        let i = y * self.w + x;
        let c = self.inv[i];
        if c <= 0.0 {
            return None;
        }
        let z = 1.0 / c;
        let cam = self.cam;
        let fu = self.fit_axis(c, Along { index: i, pos: x, len: self.w, stride: 1 }, 0, cam.fx * z);
        let fv = self.fit_axis(c, Along { index: i, pos: y, len: self.h, stride: self.w }, 1, cam.fy * z);
        if fu.sides + fv.sides < self.min_support || fu.steep || fv.steep {
            return None;
        }
        let (a, b) = (fu.grad, fv.grad);
        let dir = cam.pixel_dir_cam(x as f32 + 0.5, y as f32 + 0.5);
        let p = dir * z;
        let pu = (Vec3::new(1.0 / cam.fx, 0.0, 0.0) - dir * (a * z)) * z;
        let pv = (Vec3::new(0.0, -1.0 / cam.fy, 0.0) - dir * (b * z)) * z;
        // `v` grows downward, so `pv × pu` (not `pu × pv`) faces the camera.
        let n = pv.cross(pu).normalize_or(Vec3::Z);
        let cos = -n.dot(p) / p.length().max(1e-12);
        if !(cos > 0.0 && p.is_finite()) {
            return None;
        }
        let open = [fu.open[0], fu.open[1], fv.open[0], fv.open[1]];
        Some(PixelSurface { p, pu, pv, n, cos, z, open, bent: fu.bent || fv.bent })
    }
}

/// Builds the splat whose covariance is `tu·tuᵀ + tv·tvᵀ` in the plane with
/// unit normal `n` (`tu`, `tv` already include the footprint factor).
#[inline]
fn tangent_splat(pos: Vec3, tu: Vec3, tv: Vec3, n: Vec3, thickness: f32, max_anisotropy: f32, rgba: [u8; 4]) -> Splat {
    // Principal axes from the 2x2 Gram matrix [[g11, g12], [g12, g22]].
    let (g11, g12, g22) = (tu.length_squared(), tu.dot(tv), tv.length_squared());
    let mid = 0.5 * (g11 + g22);
    let rad = (0.25 * (g11 - g22) * (g11 - g22) + g12 * g12).sqrt();
    let l1 = mid + rad;
    let l2 = (mid - rad).max(0.0);
    // Eigenvector of the larger eigenvalue, in (tu, tv) coordinates.
    let (e1, e2) = if g11 >= g22 { (0.5 * (g11 - g22) + rad, g12) } else { (g12, 0.5 * (g22 - g11) + rad) };
    let mut major = tu * e1 + tv * e2;
    // Keep the frame orthonormal even when the tangents are not quite in
    // the plane (noise) or the footprint is round (no preferred axis).
    major -= n * n.dot(major);
    let major = major.try_normalize().unwrap_or_else(|| n.any_orthonormal_vector());
    let minor = n.cross(major);
    let s1 = l1.sqrt();
    let (s1, s2) = if std::env::var("GS_CAPLONG").is_ok() {
        let s2 = l2.sqrt().max(s1 / 20.0);
        (s1.min(s2 * max_anisotropy.max(1.0)), s2)
    } else {
        let s2 = l2.sqrt().max(s1 / max_anisotropy.max(1.0));
        (s1.min(s2 * max_anisotropy.max(1.0)), s2)
    };
    let rot = Quat::from_mat3(&Mat3::from_cols(major, minor, n));
    Splat { pos: pos.to_array(), scale: [s1, s2, (s2 * thickness).max(1.0e-5)], rot: rot.to_array(), rgba }
}

#[inline]
fn opacity_byte(opacity: f32) -> u8 {
    (opacity.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// Converts one image row into splats (appended to `out`).
fn row_to_splats(
    geo: &FrameGeometry,
    frame: &RgbdFrame,
    params: &ReconstructParams,
    mask: Option<&[u8]>,
    y: usize,
    out: &mut Vec<Splat>,
) {
    let cam = geo.cam;
    let rotation = cam.world_from_cam.matrix3;
    let alpha = opacity_byte(params.opacity);
    let k = params.footprint;
    for x in 0..geo.w {
        let i = y * geo.w + x;
        let class = mask.map_or(MASK_FOREGROUND, |m| m[i]);
        if class == MASK_SKIP {
            continue;
        }
        let Some(s) = geo.surface(x, y) else { continue };
        if s.cos < params.min_cos {
            continue;
        }
        let mut p = s.p;
        if class == MASK_CHANGED {
            p += s.n * (params.background.shadow_lift * s.z / cam.fx);
        }
        let shrink = |a: bool, b: bool| if a || b { k * params.edge_shrink } else { k };
        let (ku, kv) = (shrink(s.open[0], s.open[1]), shrink(s.open[2], s.open[3]));
        let rgb = &frame.rgb[i * 3..i * 3 + 3];
        let splat = tangent_splat(
            cam.world_from_cam.transform_point3(p),
            rotation * (s.pu * ku),
            rotation * (s.pv * kv),
            rotation * s.n,
            params.thickness,
            {
                let env = |k: &str, d: f32| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
                let open = s.open.iter().any(|&o| o);
                if s.bent { env("GS_BENT", params.max_anisotropy) } else if open { env("GS_OPEN", params.max_anisotropy) } else { params.max_anisotropy }
            },
            [rgb[0], rgb[1], rgb[2], alpha],
        );
        if is_finite_splat(&splat) {
            out.push(splat);
        }
    }
}

/// True when `frame` has the buffers its size promises and matches `cam`,
/// and `cam` is a usable calibration (finite pose, finite focal lengths in a
/// range where the per-pixel geometry does not underflow or overflow).
fn frame_matches(frame: &RgbdFrame, cam: &Camera) -> bool {
    let n = frame.width as usize * frame.height as usize;
    let focal = |f: f32| (1.0e-3..=1.0e9).contains(&f);
    let pose = &cam.world_from_cam;
    frame.width == cam.width
        && frame.height == cam.height
        && frame.depth.len() >= n
        && frame.rgb.len() >= n * 3
        && focal(cam.fx)
        && focal(cam.fy)
        && cam.cx.is_finite()
        && cam.cy.is_finite()
        && pose.matrix3.is_finite()
        && pose.translation.is_finite()
}

/// True when every number of `splat` is finite. Depths near the limits of
/// `f32` can overflow on the way to world space; such pixels are dropped.
#[inline]
fn is_finite_splat(splat: &Splat) -> bool {
    splat.pos.iter().chain(&splat.scale).chain(&splat.rot).all(|v| v.is_finite())
}

/// Converts the valid pixels of `frame` into Gaussians appended to `out` and
/// returns how many were added (row-major pixel order).
///
/// `mask`, when given, holds one byte per pixel: [`MASK_SKIP`] pixels are
/// ignored, [`MASK_CHANGED`] ones are lifted off their surface (see
/// [`BackgroundParams::shadow_lift`]), any other value is a plain foreground
/// pixel. Normals always use the whole depth map, masked or not.
///
/// A frame that does not match the camera's image size, a camera whose
/// calibration is not finite, or a mask of the wrong length, produces nothing. With the `par` feature rows are processed
/// in parallel; the output is identical to the single-threaded one.
pub fn frame_to_splats(
    frame: &RgbdFrame,
    cam: &Camera,
    params: &ReconstructParams,
    mask: Option<&[u8]>,
    out: &mut Vec<Splat>,
) -> usize {
    let n = frame.width as usize * frame.height as usize;
    if n == 0 || !frame_matches(frame, cam) || mask.is_some_and(|m| m.len() != n) {
        return 0;
    }
    let geo = FrameGeometry::new(frame, cam, params, params.min_cos);
    let before = out.len();
    #[cfg(feature = "par")]
    {
        // Bands of rows keep the per-task overhead low; concatenating them in
        // order reproduces the serial output exactly.
        const BAND: usize = 8;
        let bands: Vec<Vec<Splat>> = (0..geo.h.div_ceil(BAND))
            .into_par_iter()
            .map(|band| {
                let mut splats = Vec::new();
                for y in band * BAND..((band + 1) * BAND).min(geo.h) {
                    row_to_splats(&geo, frame, params, mask, y, &mut splats);
                }
                splats
            })
            .collect();
        out.reserve(bands.iter().map(Vec::len).sum());
        for band in &bands {
            out.extend_from_slice(band);
        }
    }
    #[cfg(not(feature = "par"))]
    for y in 0..geo.h {
        row_to_splats(&geo, frame, params, mask, y, out);
    }
    out.len() - before
}

// ---------------------------------------------------------------------------
// Background subtraction
// ---------------------------------------------------------------------------

/// The empty scene as seen by one static camera: what every live frame of
/// that camera is compared against.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct BackgroundPlate {
    frame: RgbdFrame,
    samples: u32,
}

impl BackgroundPlate {
    /// Plate from a single exposure of the empty scene.
    pub fn from_frame(frame: &RgbdFrame) -> Self {
        Self::from_frames(std::slice::from_ref(frame))
    }

    /// Plate averaged over several exposures of the empty scene, which
    /// divides the sensor noise by the square root of their number and fills
    /// dropped pixels. Each pixel averages the exposures in which it has a
    /// depth. Frames whose size differs from the first one are ignored.
    pub fn from_frames(frames: &[RgbdFrame]) -> Self {
        let Some(first) = frames.first() else { return Self::default() };
        let n = first.width as usize * first.height as usize;
        let mut depth = vec![0.0f64; n];
        let mut rgb = vec![0u32; n * 3];
        let mut count = vec![0u32; n];
        let mut samples = 0;
        for f in frames {
            if f.width != first.width || f.height != first.height || f.depth.len() < n || f.rgb.len() < n * 3 {
                continue;
            }
            samples += 1;
            for i in 0..n {
                let z = f.depth[i];
                if z > 0.0 && z.is_finite() {
                    depth[i] += z as f64;
                    count[i] += 1;
                    for c in 0..3 {
                        rgb[i * 3 + c] += f.rgb[i * 3 + c] as u32;
                    }
                }
            }
        }
        let mut frame = RgbdFrame::new(first.width, first.height);
        for i in 0..n {
            let k = count[i];
            if k > 0 {
                frame.depth[i] = (depth[i] / k as f64) as f32;
                for c in 0..3 {
                    frame.rgb[i * 3 + c] = ((rgb[i * 3 + c] + k / 2) / k) as u8;
                }
            }
        }
        Self { frame, samples }
    }

    /// The averaged empty-scene image (reconstruct the static layer from it).
    pub fn frame(&self) -> &RgbdFrame {
        &self.frame
    }

    /// Number of exposures that were averaged.
    pub fn samples(&self) -> u32 {
        self.samples
    }
}

/// Classifies every pixel of a live frame against the empty-scene plate.
///
/// Returns one byte per pixel:
/// * [`MASK_FOREGROUND`] when the pixel has a depth and is clearly closer
///   than the plate (or the plate has no depth there);
/// * [`MASK_CHANGED`] when it lies at plate depth but its color differs
///   clearly, which is how a contact shadow on the floor shows up;
/// * [`MASK_SKIP`] otherwise.
///
/// The depth tolerance grows with `z²` like the sensor noise. Afterwards
/// marked pixels with fewer than [`BackgroundParams::min_neighbors`] marked
/// neighbours are cleared, which removes the speckles sensor noise leaves.
///
/// A plate of another size than the frame yields an all-foreground mask of
/// the valid pixels (nothing can be subtracted).
pub fn foreground_mask(frame: &RgbdFrame, plate: &BackgroundPlate, params: &ReconstructParams) -> Vec<u8> {
    let (w, h) = (frame.width as usize, frame.height as usize);
    let n = w * h;
    let bg = &params.background;
    let reference = &plate.frame;
    let comparable = reference.width == frame.width
        && reference.height == frame.height
        && reference.depth.len() >= n
        && reference.rgb.len() >= n * 3;
    let color_threshold = bg.color_threshold.max(0.0) as i32;
    let mut mask = vec![MASK_SKIP; n];
    for (i, m) in mask.iter_mut().enumerate() {
        let z = frame.depth.get(i).copied().unwrap_or(0.0);
        if !(z > 0.0 && z.is_finite()) {
            continue;
        }
        let plate_z = if comparable { reference.depth[i] } else { 0.0 };
        if plate_z <= 0.0 {
            *m = MASK_FOREGROUND;
            continue;
        }
        let tolerance = bg.depth_margin + bg.noise_sigmas * bg.depth_sigma_at_1m * plate_z * plate_z;
        if plate_z - z > tolerance {
            *m = MASK_FOREGROUND;
        } else if z - plate_z <= tolerance && frame.rgb.len() >= n * 3 {
            let differs = (0..3).any(|c| {
                (frame.rgb[i * 3 + c] as i32 - reference.rgb[i * 3 + c] as i32).abs() > color_threshold
            });
            if differs {
                *m = MASK_CHANGED;
            }
        }
    }
    if bg.min_neighbors > 0 && w >= 3 && h >= 3 {
        despeckle(&mut mask, w, h, bg.min_neighbors);
    }
    mask
}

/// Clears marked pixels with fewer than `min_neighbors` marked 8-neighbours.
/// Decisions are taken on the input mask, so the result does not depend on
/// the scan order.
fn despeckle(mask: &mut [u8], w: usize, h: usize, min_neighbors: u32) {
    let src = mask.to_vec();
    for y in 0..h {
        for x in 0..w {
            let i = y * w + x;
            if src[i] == MASK_SKIP {
                continue;
            }
            let mut count = 0;
            for yy in y.saturating_sub(1)..(y + 2).min(h) {
                for xx in x.saturating_sub(1)..(x + 2).min(w) {
                    count += u32::from(src[yy * w + xx] != MASK_SKIP);
                }
            }
            // `count` includes the pixel itself.
            if count - 1 < min_neighbors {
                mask[i] = MASK_SKIP;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Voxel fusion
// ---------------------------------------------------------------------------

/// Multiplicative hasher for the already well-mixed 64-bit cell keys.
#[derive(Default)]
struct CellHasher(u64);

impl Hasher for CellHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 ^ b as u64).wrapping_mul(0x0000_0100_0000_01B3);
        }
    }

    fn write_u64(&mut self, key: u64) {
        // Fibonacci hashing; the high bits (which hashbrown uses for its
        // control bytes) and the low bits both depend on the whole key.
        let h = key.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        self.0 = h ^ (h >> 32);
    }
}

type CellMap = HashMap<u64, u32, BuildHasherDefault<CellHasher>>;

/// Bits per cell coordinate in a key.
const CELL_BITS: u32 = 20;
const CELL_LIMIT: i64 = 1 << (CELL_BITS - 1);

/// One usable pixel of a frame, in world space.
#[derive(Clone, Copy)]
struct Observation {
    pos: Vec3,
    normal: Vec3,
    /// Surface displacement per pixel step along image `u` and `v`.
    tu: Vec3,
    tv: Vec3,
    /// Longest side of the pixel's footprint on the surface, in meters: the
    /// resolution of this observation (smaller is better).
    footprint: f32,
    rgb: [u8; 3],
    /// Silhouette toward `-u`, `+u`, `-v`, `+v`.
    open: [bool; 4],
}

/// Samples that fall into the same cell.
#[derive(Clone, Copy, Debug)]
struct Accum {
    normal: Vec3,
    height: f32,
    rgb: [u32; 3],
    count: u32,
    footprint: f32,
}

impl Accum {
    fn new(normal: Vec3, height: f32, rgb: [u8; 3], footprint: f32) -> Self {
        Self { normal, height, rgb: [rgb[0] as u32, rgb[1] as u32, rgb[2] as u32], count: 1, footprint }
    }

    fn add(&mut self, normal: Vec3, height: f32, rgb: [u8; 3], footprint: f32) {
        self.normal += normal;
        self.height += height;
        self.count += 1;
        self.footprint = self.footprint.min(footprint);
        for (sum, &channel) in self.rgb.iter_mut().zip(&rgb) {
            *sum += channel as u32;
        }
    }

    fn merge(&mut self, other: &Accum) {
        self.normal += other.normal;
        self.height += other.height;
        self.count += other.count;
        self.footprint = self.footprint.min(other.footprint);
        for c in 0..3 {
            self.rgb[c] += other.rgb[c];
        }
    }
}

/// Where a cell sits: facing class and column.
#[derive(Clone, Copy)]
struct CellSite {
    /// Facing class `0..6`: dominant axis of the normal times two, plus one
    /// when the normal points to the negative side.
    class: u8,
    /// Cell indices along the two tangent axes.
    iu: i32,
    iv: i32,
}

/// A cell touched by the frame being integrated: either new, or an existing
/// one that this frame sees at a clearly better resolution.
struct Pending {
    site: CellSite,
    /// Index of the cell's splat.
    index: u32,
    accum: Accum,
}

/// Incremental surface fusion for the home scan.
///
/// Space is cut into cells of [`VoxelFusion::voxel_size`]. Each surface
/// element is filed under the axis its normal is closest to (six *facings*:
/// ±X, ±Y, ±Z), in a column of one voxel by one voxel perpendicular to that
/// axis. The first good observation of a column creates one round splat at
/// the center of the column's patch of surface. Consequences:
///
/// * splats sit on a regular grid along the surface, one per voxel, so equal
///   radii leave no holes, however dense or sparse the pixels were;
/// * the two faces of a thin wall have opposite facings and never share a
///   cell, so neither side bleeds through or suppresses the other;
/// * along its axis a column is cut into slabs two voxels thick, and an
///   observation also checks the neighbouring slab on its nearer side. A
///   surface that happens to lie on a slab boundary, or that sensor noise
///   moves by less than a voxel, is therefore still one surface, while
///   parallel surfaces of the same facing more than three voxels apart are
///   always kept separate.
///
/// A pixel seen from far away or at a slant covers more than one cell. Its
/// footprint on the surface is therefore sampled densely enough to reach
/// every cell under it, which keeps distant surfaces free of the dotted look
/// a one-cell-per-pixel rule gives. All samples of a frame that fall into the
/// same cell are averaged (color, normal, height), which low-pass filters
/// close-up views down to the voxel size.
///
/// # Refinement
///
/// The first observation of a cell is often a distant one. Every cell
/// remembers the pixel footprint (meters of surface per pixel) of the views
/// it is built from. A later frame that sees it with a footprint below
/// [`VoxelFusion::refine_ratio`] times that replaces the cell's content; a
/// frame of comparable resolution is averaged in, until the cell has a
/// handful of samples (this takes the staircase out of color edges and
/// tames sensor noise); coarser views are ignored. In all cases the splat
/// is rewritten *in place* (same index, same cell center). Such rewrites are
/// not part of the splats [`VoxelFusion::integrate`] returns; they are
/// counted by [`VoxelFusion::refined`] so that the owner can decide when to
/// publish [`VoxelFusion::splats`] as a whole again.
///
/// # Memory
///
/// At most [`VoxelFusion::max_voxels`] cells are created (about 100 bytes
/// each: the 44-byte splat, its 36 bytes of accumulated samples and its hash
/// table entry); observations of new surface beyond that are dropped.
#[derive(Clone, Debug)]
pub struct VoxelFusion {
    voxel_size: f32,
    /// Observations farther than this forward distance are not used (m).
    pub max_range: f32,
    /// Observations whose normal makes a cosine below this with the viewing
    /// ray are not used.
    pub min_cos: f32,
    /// Tangential standard deviation of a splat in voxels. `0.6` is the
    /// smallest value that closes the gaps between neighbouring cells.
    pub radius_factor: f32,
    /// Opacity of the splats, `0..=1`. The map is a single layer of splats,
    /// so anything below 1 lets what is behind a surface shine through
    /// between the cells (the far face of a wall, the void). The opacity of
    /// [`ReconstructParams`] is not used: it is tuned for the overlapping
    /// views of a camera ring.
    pub opacity: f32,
    /// A cell's content is replaced when a frame observes it with a pixel
    /// footprint smaller than this fraction of the recorded one (see
    /// "Refinement" above). `0` keeps the first observation for good.
    pub refine_ratio: f32,
    /// Upper bound on the number of cells (and splats).
    pub max_voxels: usize,
    cells: CellMap,
    splats: Vec<Splat>,
    /// Per splat: the samples it is built from.
    accums: Vec<Accum>,
    /// Per splat: slot in the pending list of the frame being integrated,
    /// or `NO_SLOT`.
    slots: Vec<u32>,
    refined: u64,
}

const NO_SLOT: u32 = u32::MAX;

/// Entry of the per-frame lookup cache of [`VoxelFusion::integrate`].
#[derive(Clone, Copy)]
struct Lookup {
    /// Cell key (`u64::MAX`, which no cell has, marks an empty entry).
    key: u64,
    /// Index of the cell's splat.
    index: u32,
    /// Samples with a larger footprint are ignored by this cell.
    limit: f32,
}

const LOOKUP_CACHE_BITS: u32 = 10;

impl Lookup {
    const EMPTY: Self = Self { key: u64::MAX, index: 0, limit: 0.0 };

    /// Cache slot of a key.
    #[inline]
    fn slot(key: u64) -> usize {
        (key.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> (64 - LOOKUP_CACHE_BITS)) as usize
    }
}

impl VoxelFusion {
    /// Smallest and largest accepted voxel sizes in meters.
    pub const MIN_VOXEL_SIZE: f32 = 0.005;
    pub const MAX_VOXEL_SIZE: f32 = 1.0;
    /// Default bound on the number of cells: about 100 MB of memory and
    /// 16 MB on the wire.
    pub const DEFAULT_MAX_VOXELS: usize = 1_000_000;
    /// A pixel's footprint is sampled at most this many times per image axis.
    const MAX_SUBSAMPLES: u32 = 12;
    /// Spacing of the footprint samples in voxels.
    const SAMPLE_SPACING: f32 = 0.7;
    /// A cell averages the frames whose pixel footprint is at most this
    /// multiple of the best one it has seen.
    const ACCUMULATE_RATIO: f32 = 1.25;
    /// A cell that has averaged this many samples only changes again for a
    /// clearly better view (bounds the work per cell).
    const SATURATION: u32 = 8;

    /// An empty map with cells of `voxel_size` meters (clamped to
    /// [`VoxelFusion::MIN_VOXEL_SIZE`]`..=`[`VoxelFusion::MAX_VOXEL_SIZE`];
    /// 3 cm when it is not a number).
    pub fn new(voxel_size: f32) -> Self {
        Self {
            voxel_size: finite_clamp(voxel_size, 0.03, Self::MIN_VOXEL_SIZE, Self::MAX_VOXEL_SIZE),
            max_range: 6.0,
            min_cos: 0.2,
            radius_factor: 0.6,
            opacity: 1.0,
            refine_ratio: 0.7,
            max_voxels: Self::DEFAULT_MAX_VOXELS,
            cells: CellMap::default(),
            splats: Vec::new(),
            accums: Vec::new(),
            slots: Vec::new(),
            refined: 0,
        }
    }

    /// Edge length of a cell in meters.
    pub fn voxel_size(&self) -> f32 {
        self.voxel_size
    }

    /// Every splat created so far, in creation order. The slice only ever
    /// grows at its end (until [`VoxelFusion::clear`]); refinement rewrites
    /// splats in place.
    pub fn splats(&self) -> &[Splat] {
        &self.splats
    }

    /// Number of splats (occupied cells).
    pub fn len(&self) -> usize {
        self.splats.len()
    }

    pub fn is_empty(&self) -> bool {
        self.splats.is_empty()
    }

    /// How many times an existing splat has been rewritten from a better
    /// view since the last [`VoxelFusion::clear`].
    pub fn refined(&self) -> u64 {
        self.refined
    }

    /// Forgets everything.
    pub fn clear(&mut self) {
        self.cells.clear();
        self.splats.clear();
        self.accums.clear();
        self.slots.clear();
        self.refined = 0;
    }

    /// Fuses one frame and returns the splats of the surface cells it
    /// observed for the first time (also appended to
    /// [`VoxelFusion::splats`]). The result is deterministic, with or without
    /// the `par` feature.
    ///
    /// `params` supplies the depth-edge threshold, the smoothing and the
    /// relative thickness. Its footprint factor, opacity and grazing limit
    /// are not used: the splat size follows the voxel size, and the map has
    /// its own [`VoxelFusion::opacity`] and [`VoxelFusion::min_cos`].
    pub fn integrate(&mut self, frame: &RgbdFrame, cam: &Camera, params: &ReconstructParams) -> Vec<Splat> {
        let n = frame.width as usize * frame.height as usize;
        if n == 0 || !frame_matches(frame, cam) {
            return Vec::new();
        }
        let geo = FrameGeometry::new(frame, cam, params, self.min_cos);
        let rows = self.observe(&geo, frame);

        let base = self.splats.len();
        let mut created = 0usize;
        let mut pending: Vec<Pending> = Vec::new();
        let voxel = self.voxel_size;
        let inv_voxel = 1.0 / voxel;
        let sample_step = Self::SAMPLE_SPACING * voxel;
        let limit = self.max_voxels.min(NO_SLOT as usize - 1);
        let mut cache = [Lookup::EMPTY; 1 << LOOKUP_CACHE_BITS];
        // Existing cells take samples up to this much coarser than their own.
        let accept = if self.refine_ratio > 0.0 { Self::ACCUMULATE_RATIO } else { 0.0 };
        let cell_range = (1 - CELL_LIMIT) as f32..(CELL_LIMIT - 1) as f32;
        for obs in rows.iter().flatten() {
            let n = obs.normal;
            let abs = n.abs();
            let axis = if abs.x >= abs.y && abs.x >= abs.z {
                0
            } else if abs.y >= abs.z {
                1
            } else {
                2
            };
            let (ua, va) = ((axis + 1) % 3, (axis + 2) % 3);
            let class = (axis * 2) as u64 + u64::from(n[axis] < 0.0);
            // Everything in column coordinates: `(u, v)` across, `h` along
            // the facing axis. The tangent plane through the observation is
            // `h = h0 − su·(u − u0) − sv·(v − v0)`.
            let (u0, v0, h0) = (obs.pos[ua], obs.pos[va], obs.pos[axis]);
            let (su, sv) = (n[ua] / n[axis], n[va] / n[axis]);
            let (tuu, tuv, tvu, tvv) = (obs.tu[ua], obs.tu[va], obs.tv[ua], obs.tv[va]);

            // The cell under the sample at `(a, b)` pixels from the pixel
            // center, and the height of the tangent plane at its center.
            let locate = |a: f32, b: f32| {
                let fu = ((u0 + tuu * a + tvu * b) * inv_voxel).floor();
                let fv = ((v0 + tuv * a + tvv * b) * inv_voxel).floor();
                let height = h0 - su * ((fu + 0.5) * voxel - u0) - sv * ((fv + 0.5) * voxel - v0);
                let slab = height * (0.5 * inv_voxel);
                let fs = slab.floor();
                // Also false for anything that is not a number.
                let valid = cell_range.contains(&fu) && cell_range.contains(&fv) && cell_range.contains(&fs);
                valid.then(|| {
                    let pack = |c: f32| (c as i64 + CELL_LIMIT) as u64;
                    let column = class << (3 * CELL_BITS) | pack(fu) << (2 * CELL_BITS) | pack(fv) << CELL_BITS;
                    // The neighbouring slab on the nearer side.
                    let other = if slab - fs < 0.5 { fs - 1.0 } else { fs + 1.0 };
                    (column | pack(fs), column | pack(other), fu as i32, fv as i32, height)
                })
            };

            // Samples across the pixel's footprint, dense enough to reach
            // every column under it. Toward a silhouette the surface ends
            // somewhere before the next pixel center: only the pixel center
            // is known to lie on it, so nothing is created beyond.
            let count = |along_u: f32, along_v: f32| {
                let extent = along_u.abs().max(along_v.abs());
                ((extent / sample_step).ceil() as u32).clamp(1, Self::MAX_SUBSAMPLES)
            };
            let (mu, mv) = (count(tuu, tuv), count(tvu, tvv));
            let range = |lo_open: bool, hi_open: bool| (if lo_open { 0.0 } else { -0.5 }, if hi_open { 0.0 } else { 0.5 });
            let ((a0, a1), (b0, b1)) = (range(obs.open[0], obs.open[1]), range(obs.open[2], obs.open[3]));
            // When the cell under the pixel center already holds clearly
            // better views, so do its neighbours (with rare exceptions at
            // the rim of a close-up, which the next view fills): skip the
            // whole pixel. This is what keeps re-observation cheap.
            if let Some((primary, other, ..)) = locate(0.0, 0.0) {
                let hit = self.lookup(&mut cache, primary, other, base, accept);
                if accept > 0.0 && hit.is_some_and(|hit| obs.footprint > hit.limit) {
                    continue;
                }
            }
            for j in 0..mv {
                let b = b0 + (b1 - b0) * (j as f32 + 0.5) / mv as f32;
                for i in 0..mu {
                    let a = a0 + (a1 - a0) * (i as f32 + 0.5) / mu as f32;
                    let Some((primary, other, iu, iv, height)) = locate(a, b) else { continue };
                    let hit = match self.lookup(&mut cache, primary, other, base, accept) {
                        Some(hit) => hit,
                        None => {
                            if base + created >= limit {
                                continue;
                            }
                            let index = (base + created) as u32;
                            created += 1;
                            self.cells.insert(primary, index);
                            self.slots.push(NO_SLOT);
                            // Placeholder; the frame's samples replace it below.
                            self.accums.push(Accum::new(n, height, obs.rgb, obs.footprint));
                            let hit = Lookup { key: primary, index, limit: f32::INFINITY };
                            cache[Lookup::slot(primary)] = hit;
                            hit
                        }
                    };
                    if obs.footprint > hit.limit {
                        // Much coarser than what the cell already has.
                        continue;
                    }
                    let index = hit.index as usize;
                    let slot = self.slots[index];
                    if slot != NO_SLOT {
                        pending[slot as usize].accum.add(n, height, obs.rgb, obs.footprint);
                    } else {
                        self.slots[index] = pending.len() as u32;
                        let site = CellSite { class: class as u8, iu, iv };
                        pending.push(Pending { site, index: hit.index, accum: Accum::new(n, height, obs.rgb, obs.footprint) });
                    }
                }
            }
        }

        // New cells were numbered in the order they entered `pending`, so
        // pushing in that order puts every splat at its index.
        let alpha = opacity_byte(self.opacity);
        self.splats.reserve(created);
        for p in &pending {
            let index = p.index as usize;
            self.slots[index] = NO_SLOT;
            if index < base {
                let stored = &mut self.accums[index];
                if p.accum.footprint < self.refine_ratio * stored.footprint {
                    // A clearly better view replaces what was there.
                    *stored = p.accum;
                } else {
                    // A comparable view: averaging over frames takes the
                    // staircase out of color edges and tames sensor noise.
                    stored.merge(&p.accum);
                }
                self.splats[index] = self.cell_splat(p.site, &self.accums[index], params.thickness, alpha);
                self.refined += 1;
            } else {
                debug_assert_eq!(index, self.splats.len());
                self.accums[index] = p.accum;
                self.splats.push(self.cell_splat(p.site, &p.accum, params.thickness, alpha));
            }
        }
        self.splats[base..].to_vec()
    }

    /// Finds the cell with key `primary`, or the one in the neighbouring slab
    /// `other`, and the coarsest footprint it still takes samples from.
    ///
    /// Neighbouring samples mostly hit the same few cells: a small
    /// direct-mapped cache in front of the big table saves most of its cache
    /// misses.
    #[inline]
    fn lookup(&self, cache: &mut [Lookup], primary: u64, other: u64, base: usize, accept: f32) -> Option<Lookup> {
        let cached = &mut cache[Lookup::slot(primary)];
        if cached.key != primary {
            let index = self.cells.get(&primary).or_else(|| self.cells.get(&other)).copied()?;
            let limit = if index as usize >= base {
                // Created earlier in this frame: takes every sample.
                f32::INFINITY
            } else {
                let stored = &self.accums[index as usize];
                stored.footprint * if stored.count >= Self::SATURATION { self.refine_ratio } else { accept }
            };
            *cached = Lookup { key: primary, index, limit };
        }
        Some(*cached)
    }

    /// The splat of a cell from the samples collected for it.
    fn cell_splat(&self, site: CellSite, acc: &Accum, thickness: f32, alpha: u8) -> Splat {
        let voxel = self.voxel_size;
        let axis = (site.class / 2) as usize;
        let (ua, va) = ((axis + 1) % 3, (axis + 2) % 3);
        let sign = if site.class & 1 == 0 { 1.0 } else { -1.0 };
        let mut n = acc.normal.normalize_or(Vec3::ZERO);
        // The averaged normal keeps its facing (all its terms have the same
        // sign along the axis); guard the degenerate cases anyway.
        if n[axis] * sign < 0.5 {
            n = Vec3::ZERO;
            n[axis] = sign;
        }
        let mut pos = Vec3::ZERO;
        pos[ua] = (site.iu as f32 + 0.5) * voxel;
        pos[va] = (site.iv as f32 + 0.5) * voxel;
        pos[axis] = acc.height / acc.count as f32;
        let half = acc.count / 2;
        let rgba = [
            ((acc.rgb[0] + half) / acc.count) as u8,
            ((acc.rgb[1] + half) / acc.count) as u8,
            ((acc.rgb[2] + half) / acc.count) as u8,
            alpha,
        ];
        // A round disc, whatever the slant. Stretching it to the column's
        // whole patch (1 / cos longer on a slanted surface) closes the last
        // gaps on slopes, but the normals of the cells along creases and
        // object outlines are the least reliable ones, and every stretched,
        // tilted disc there sticks out of both surfaces as a visible spike.
        // Round discs still overlap on a 45 degree slope (accumulated
        // opacity above 0.75 everywhere) and score 0.3 dB better.
        let sigma = self.radius_factor * voxel;
        Splat::surfel(pos, n, sigma, (sigma * thickness).max(1.0e-5), rgba)
    }

    /// The good observations of a frame, row by row.
    fn observe(&self, geo: &FrameGeometry, frame: &RgbdFrame) -> Vec<Vec<Observation>> {
        let cam = geo.cam;
        let rotation = cam.world_from_cam.matrix3;
        let row = |y: usize| {
            let mut out = Vec::new();
            for x in 0..geo.w {
                let Some(s) = geo.surface(x, y) else { continue };
                if s.cos < self.min_cos || s.z > self.max_range {
                    continue;
                }
                let i = y * geo.w + x;
                out.push(Observation {
                    pos: cam.world_from_cam.transform_point3(s.p),
                    normal: rotation * s.n,
                    tu: rotation * s.pu,
                    tv: rotation * s.pv,
                    footprint: s.pu.length().max(s.pv.length()),
                    rgb: [frame.rgb[i * 3], frame.rgb[i * 3 + 1], frame.rgb[i * 3 + 2]],
                    open: s.open,
                });
            }
            out
        };
        #[cfg(feature = "par")]
        return (0..geo.h).into_par_iter().map(row).collect();
        #[cfg(not(feature = "par"))]
        (0..geo.h).map(row).collect()
    }
}

impl Default for VoxelFusion {
    /// 3 cm cells.
    fn default() -> Self {
        Self::new(0.03)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::splat::{cov6, project_splat, ViewParams, LOW_PASS_PX2};

    fn camera(eye: Vec3, target: Vec3, fov_deg: f32, width: u32, height: u32) -> Camera {
        Camera::look_at(eye, target, Vec3::Y, fov_deg.to_radians(), width, height)
    }

    /// Draws the plane through `point` with normal `normal` into `frame`
    /// wherever it is nearer than what is there, colored by `color(world)`.
    fn draw_plane(frame: &mut RgbdFrame, cam: &Camera, point: Vec3, normal: Vec3, color: impl Fn(Vec3) -> Option<[u8; 3]>) {
        for y in 0..cam.height {
            for x in 0..cam.width {
                let dir = cam.world_from_cam.transform_vector3(cam.pixel_dir_cam(x as f32 + 0.5, y as f32 + 0.5));
                let depth = normal.dot(point - cam.position()) / normal.dot(dir);
                if !(depth.is_finite() && depth > 0.05) {
                    continue;
                }
                let i = frame.index(x, y);
                let Some(rgb) = color(cam.position() + dir * depth) else { continue };
                if frame.depth[i] == 0.0 || depth < frame.depth[i] {
                    frame.depth[i] = depth;
                    frame.rgb[i * 3..i * 3 + 3].copy_from_slice(&rgb);
                }
            }
        }
    }

    fn plane_frame(cam: &Camera, point: Vec3, normal: Vec3, rgb: [u8; 3]) -> RgbdFrame {
        let mut frame = RgbdFrame::new(cam.width, cam.height);
        draw_plane(&mut frame, cam, point, normal, |_| Some(rgb));
        frame
    }

    fn normal_of(splat: &Splat) -> Vec3 {
        splat.rotation() * Vec3::Z
    }

    /// No smoothing, no silhouette correction: the bare geometry.
    fn plain() -> ReconstructParams {
        ReconstructParams { smooth_passes: 0, ..ReconstructParams::default() }
    }

    fn splats_of(frame: &RgbdFrame, cam: &Camera, params: &ReconstructParams) -> Vec<Splat> {
        let mut out = Vec::new();
        let added = frame_to_splats(frame, cam, params, None, &mut out);
        assert_eq!(added, out.len());
        out
    }

    #[test]
    fn default_params_are_sane_and_partial_json_works() {
        let d = ReconstructParams::default();
        assert_eq!(d.sanitized(), d);
        let partial: ReconstructParams = serde_json::from_str(r#"{"footprint":0.9,"background":{"shadow_lift":1.5}}"#).unwrap();
        assert_eq!(partial.footprint, 0.9);
        assert_eq!(partial.background.shadow_lift, 1.5);
        assert_eq!(partial.opacity, d.opacity);
        assert_eq!(partial.background.color_threshold, d.background.color_threshold);
        let wild = ReconstructParams {
            footprint: f32::NAN,
            thickness: -1.0,
            opacity: 7.0,
            min_cos: 2.0,
            edge_threshold: 0.0,
            max_anisotropy: f32::INFINITY,
            min_support: 99,
            smooth_passes: u32::MAX,
            edge_shrink: 0.0,
            background: BackgroundParams { depth_margin: f32::NAN, min_neighbors: 99, ..BackgroundParams::default() },
        }
        .sanitized();
        assert_eq!(wild.footprint, d.footprint);
        assert!(wild.thickness > 0.0 && wild.opacity == 1.0 && wild.min_cos < 1.0 && wild.edge_threshold > 0.0);
        assert_eq!(wild.max_anisotropy, d.max_anisotropy);
        assert!(wild.min_support <= 4 && wild.smooth_passes <= ReconstructParams::MAX_SMOOTH_PASSES);
        assert!(wild.edge_shrink > 0.0 && wild.background.min_neighbors <= 8);
        assert_eq!(wild.background.depth_margin, d.background.depth_margin);
        assert_eq!(wild.sanitized(), wild);
    }

    #[test]
    fn slanted_plane_gives_surfels_on_the_plane_facing_the_camera() {
        let cam = camera(Vec3::new(0.5, 1.4, 3.0), Vec3::new(0.0, 0.8, 0.0), 50.0, 64, 48);
        let (point, normal) = (Vec3::new(0.0, 0.7, 0.0), Vec3::new(0.3, 0.5, 0.8).normalize());
        let frame = plane_frame(&cam, point, normal, [10, 200, 30]);
        let params = plain();
        let splats = splats_of(&frame, &cam, &params);
        assert_eq!(splats.len(), 64 * 48, "every pixel of a well-seen plane gives a splat");
        let vp = ViewParams::from_camera(&cam);
        let k2 = params.footprint * params.footprint;
        for (i, s) in splats.iter().enumerate() {
            let p = s.position();
            assert!(normal.dot(p - point).abs() < 2e-4, "splat {i} is off the plane");
            let n = normal_of(s);
            assert!(n.dot(normal) > 0.9995, "splat {i} has normal {n}");
            assert!(n.dot(cam.position() - p) > 0.0, "splat {i} faces away");
            assert_eq!(s.rgba, [10, 200, 30, 217]);
            assert!((s.rotation().length() - 1.0).abs() < 1e-4);
            assert!(s.scale[2] <= s.scale[1] * params.thickness * 1.001 && s.scale[1] <= s.scale[0]);
            // Row-major order: splat i comes from pixel i.
            let (px, py, _) = cam.project(p).unwrap();
            assert_eq!((px as usize, py as usize), (i % 64, i / 64));
            // Seen from its own camera every splat is a round blob of
            // `footprint` pixels.
            let projected = project_splat(p, &cov6(s), &vp).unwrap();
            let [xx, xy, yy] = projected.cov;
            for v in [xx - LOW_PASS_PX2, yy - LOW_PASS_PX2] {
                assert!((v - k2).abs() < 0.06 * k2, "splat {i} covers {v} px², expected {k2}");
            }
            assert!(xy.abs() < 0.06 * k2);
        }
    }

    #[test]
    fn grazing_planes_keep_their_normal_up_to_the_limit_and_are_dropped_beyond() {
        // A floor seen from 1.2 m above, looking almost along it: with these
        // coarse pixels the depth steps by more than the edge threshold per
        // row, which must not be mistaken for depth edges.
        let cam = camera(Vec3::new(0.0, 1.2, 0.0), Vec3::new(0.0, 0.9, -3.0), 60.0, 64, 48);
        let frame = plane_frame(&cam, Vec3::ZERO, Vec3::Y, [128; 3]);
        let params = plain();
        let splats = splats_of(&frame, &cam, &params);
        assert!(splats.len() > 500);
        let mut steep = 0;
        for s in &splats {
            let p = s.position();
            assert!(p.y.abs() < 1e-3);
            assert!(normal_of(s).y > 0.999, "normal {} at {p}", normal_of(s));
            let cos = (cam.position() - p).normalize().y;
            assert!(cos > params.min_cos * 0.93, "a splat at cosine {cos} survived");
            steep += usize::from(cos < 0.3);
        }
        assert!(steep > 100, "grazing but acceptable observations must be kept ({steep})");
        // The valid pixels that produced no splat are the ones beyond the limit.
        let valid = frame.depth.iter().filter(|&&z| z > 0.0).count();
        assert!(splats.len() < valid);
        let strict = ReconstructParams { min_cos: 0.5, ..params };
        assert!(splats_of(&frame, &cam, &strict).iter().all(|s| (cam.position() - s.position()).normalize().y > 0.47));
    }

    /// A 1 m square 2 m in front of the camera, before a wall at 3 m.
    fn square_before_wall(cam: &Camera) -> RgbdFrame {
        let mut frame = plane_frame(cam, Vec3::new(0.0, 0.0, -3.0), Vec3::Z, [200, 200, 200]);
        draw_plane(&mut frame, cam, Vec3::new(0.0, 0.0, -2.0), Vec3::Z, |p| {
            (p.x.abs() < 0.5 && p.y.abs() < 0.5).then_some([220, 40, 40])
        });
        frame
    }

    #[test]
    fn depth_edges_are_not_bridged_and_silhouettes_are_narrowed() {
        let cam = camera(Vec3::ZERO, Vec3::NEG_Z, 60.0, 64, 48);
        let frame = square_before_wall(&cam);
        let params = plain();
        let splats = splats_of(&frame, &cam, &params);
        assert_eq!(splats.len(), 64 * 48);
        let interior = params.footprint * 2.0 / cam.fx;
        let (mut narrowed, mut full) = (0, 0);
        for s in &splats {
            let p = s.position();
            let front = s.rgba[0] == 220;
            assert!((p.z - if front { -2.0 } else { -3.0 }).abs() < 1e-4, "a splat floats at {p}");
            assert!(normal_of(s).z > 0.999, "a splat leans across the edge: {}", normal_of(s));
            if front {
                // Border pixels of the square are narrowed across the border.
                let ratio = s.scale[1] / interior;
                if ratio < 0.75 {
                    narrowed += 1;
                    let other = s.scale[0] / interior;
                    assert!((ratio - params.edge_shrink).abs() < 0.02 || (other - params.edge_shrink).abs() < 0.02);
                } else {
                    full += 1;
                    assert!((ratio - 1.0).abs() < 0.02);
                }
            } else {
                // The wall goes on behind the square: full size everywhere.
                assert!((s.scale[1] / (params.footprint * 3.0 / cam.fx) - 1.0).abs() < 0.02);
            }
        }
        // A square of about 20 x 20 pixels: its rim is narrowed, the rest is not.
        assert!((60..=90).contains(&narrowed) && full > 3 * narrowed, "{narrowed} narrowed, {full} full");
        // Without the correction every splat of the square has full size.
        let off = ReconstructParams { edge_shrink: 1.0, ..params };
        assert!(splats_of(&frame, &cam, &off).iter().filter(|s| s.rgba[0] == 220).all(|s| s.scale[1] > 0.98 * interior));
    }

    #[test]
    fn small_steps_follow_their_own_surface() {
        // 3 cm in front of a wall at 2 m: well within the edge threshold,
        // yet neither surface may tilt toward the other.
        let cam = camera(Vec3::ZERO, Vec3::NEG_Z, 60.0, 64, 48);
        let mut frame = plane_frame(&cam, Vec3::new(0.0, 0.0, -2.0), Vec3::Z, [200; 3]);
        draw_plane(&mut frame, &cam, Vec3::new(0.0, 0.0, -1.97), Vec3::Z, |p| (p.x < 0.0).then_some([40; 3]));
        for s in splats_of(&frame, &cam, &plain()) {
            assert!(normal_of(&s).z > 0.97, "normal {} at {}", normal_of(&s), s.position());
        }
    }

    #[test]
    fn lonely_pixels_are_dropped_or_face_the_camera() {
        let cam = camera(Vec3::ZERO, Vec3::NEG_Z, 60.0, 32, 24);
        let mut frame = RgbdFrame::new(32, 24);
        let i = frame.index(20, 7);
        frame.depth[i] = 2.0;
        frame.rgb[i * 3..i * 3 + 3].copy_from_slice(&[1, 2, 3]);
        assert!(splats_of(&frame, &cam, &plain()).is_empty(), "a flying pixel must not become a splat");

        let params = ReconstructParams { min_support: 0, ..plain() };
        let splats = splats_of(&frame, &cam, &params);
        assert_eq!(splats.len(), 1);
        let s = &splats[0];
        assert!((s.position() - cam.unproject(20.5, 7.5, 2.0)).length() < 1e-5);
        // No normal can be estimated: the splat is perpendicular to the
        // optical axis and (a silhouette all around) narrowed both ways.
        assert!(normal_of(s).z > 0.9999);
        let expected = params.footprint * params.edge_shrink * 2.0 / cam.fx;
        assert!((s.scale[0] / expected - 1.0).abs() < 0.01 && (s.scale[1] / expected - 1.0).abs() < 0.01);
        assert_eq!(s.rgba, [1, 2, 3, 217]);
    }

    #[test]
    fn a_dropout_is_not_a_silhouette() {
        let cam = camera(Vec3::ZERO, Vec3::NEG_Z, 60.0, 32, 24);
        let mut frame = plane_frame(&cam, Vec3::new(0.0, 0.0, -2.0), Vec3::Z, [90; 3]);
        let hole = frame.index(16, 12);
        frame.depth[hole] = 0.0;
        let splats = splats_of(&frame, &cam, &plain());
        assert_eq!(splats.len(), 32 * 24 - 1);
        let size = plain().footprint * 2.0 / cam.fx;
        assert!(splats.iter().all(|s| (s.scale[1] / size - 1.0).abs() < 0.01), "neighbours of a dropout shrank");
        // Not-a-number and negative depths count as invalid too.
        frame.depth[0] = f32::NAN;
        frame.depth[1] = -1.0;
        frame.depth[2] = f32::INFINITY;
        assert_eq!(splats_of(&frame, &cam, &plain()).len(), 32 * 24 - 4);
    }

    #[test]
    fn masks_select_and_lift() {
        let cam = camera(Vec3::new(0.0, 2.0, 2.0), Vec3::ZERO, 50.0, 32, 24);
        let frame = plane_frame(&cam, Vec3::ZERO, Vec3::Y, [50; 3]);
        let params = plain();
        let mut mask = vec![MASK_SKIP; 32 * 24];
        let (plain_px, lifted_px) = (frame.index(10, 12), frame.index(20, 12));
        mask[plain_px] = MASK_FOREGROUND;
        mask[lifted_px] = MASK_CHANGED;
        let mut out = Vec::new();
        assert_eq!(frame_to_splats(&frame, &cam, &params, Some(&mask), &mut out), 2);
        assert!(out[0].pos[1].abs() < 1e-4);
        // Lifted along the normal by `shadow_lift` pixel footprints.
        let lift = params.background.shadow_lift * frame.depth[lifted_px] / cam.fx;
        assert!(lift > 0.005 && (out[1].pos[1] - lift).abs() < 1e-4, "lifted by {} instead of {lift}", out[1].pos[1]);
        // The normals still come from the whole depth map.
        assert!(normal_of(&out[0]).y > 0.999 && normal_of(&out[1]).y > 0.999);

        // Appends, and rejects what does not fit.
        assert_eq!(frame_to_splats(&frame, &cam, &params, Some(&mask[1..]), &mut out), 0);
        assert_eq!(frame_to_splats(&frame, &cam.resized(16, 12), &params, None, &mut out), 0);
        assert_eq!(frame_to_splats(&RgbdFrame::default(), &cam.resized(0, 0), &params, None, &mut out), 0);
        let mut short = frame.clone();
        short.depth.truncate(10);
        assert_eq!(frame_to_splats(&short, &cam, &params, None, &mut out), 0);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn smoothing_keeps_planes_exact_and_tames_noise() {
        let cam = camera(Vec3::new(0.5, 1.4, 3.0), Vec3::new(0.0, 0.8, 0.0), 50.0, 64, 48);
        let (point, normal) = (Vec3::new(0.0, 0.7, 0.0), Vec3::new(0.2, 0.4, 0.9).normalize());
        let clean = plane_frame(&cam, point, normal, [128; 3]);
        let smoothed = ReconstructParams { smooth_passes: 3, ..plain() };
        for s in splats_of(&clean, &cam, &smoothed) {
            assert!(normal.dot(s.position() - point).abs() < 3e-4);
            assert!(normal_of(&s).dot(normal) > 0.9995);
        }
        // Deterministic pseudo-noise of about 4 mm.
        let mut noisy = clean.clone();
        let mut state = 12345u32;
        for z in &mut noisy.depth {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            *z += ((state >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.014;
        }
        let spread = |params: &ReconstructParams| {
            let splats = splats_of(&noisy, &cam, params);
            let tilt: f32 = splats.iter().map(|s| 1.0 - normal_of(s).dot(normal)).sum::<f32>() / splats.len() as f32;
            let off: f32 = splats.iter().map(|s| normal.dot(s.position() - point).abs()).sum::<f32>() / splats.len() as f32;
            (tilt, off)
        };
        let (raw, one, three) = (spread(&plain()), spread(&ReconstructParams { smooth_passes: 1, ..plain() }), spread(&smoothed));
        assert!(one.0 < 0.5 * raw.0 && three.0 < 0.8 * one.0, "normal error {raw:?} {one:?} {three:?}");
        assert!(one.1 < 0.7 * raw.1 && three.1 < one.1, "position error {raw:?} {one:?} {three:?}");
    }

    #[cfg(feature = "par")]
    #[test]
    fn parallel_rows_match_the_serial_order() {
        let cam = camera(Vec3::ZERO, Vec3::NEG_Z, 60.0, 64, 48);
        let frame = square_before_wall(&cam);
        let params = ReconstructParams::default();
        let geo = FrameGeometry::new(&frame, &cam, &params, params.min_cos);
        let mut serial = Vec::new();
        for y in 0..48 {
            row_to_splats(&geo, &frame, &params, None, y, &mut serial);
        }
        assert_eq!(splats_of(&frame, &cam, &params), serial);
    }

    // -- background subtraction ---------------------------------------------

    #[test]
    fn plates_average_exposures_and_fill_dropouts() {
        let mut a = RgbdFrame::new(2, 1);
        let mut b = RgbdFrame::new(2, 1);
        a.depth = vec![2.0, 0.0];
        b.depth = vec![3.0, 4.0];
        a.rgb = vec![10, 20, 30, 99, 99, 99];
        b.rgb = vec![20, 40, 61, 7, 8, 9];
        let plate = BackgroundPlate::from_frames(&[a.clone(), b, RgbdFrame::new(3, 3)]);
        assert_eq!(plate.samples(), 2, "the frame of another size is ignored");
        assert_eq!(plate.frame().depth, [2.5, 4.0]);
        // Colors only count where the exposure has a depth.
        assert_eq!(plate.frame().rgb, [15, 30, 46, 7, 8, 9]);
        assert_eq!(BackgroundPlate::from_frame(&a).frame(), &RgbdFrame { rgb: vec![10, 20, 30, 0, 0, 0], ..a });
        assert_eq!(BackgroundPlate::from_frames(&[]), BackgroundPlate::default());
        assert_eq!(BackgroundPlate::default().samples(), 0);
    }

    #[test]
    fn mask_separates_foreground_shadow_and_background() {
        let cam = camera(Vec3::new(0.0, 2.0, 3.0), Vec3::ZERO, 50.0, 64, 48);
        let floor = |frame: &mut RgbdFrame, shade: &dyn Fn(Vec3) -> u8| {
            draw_plane(frame, &cam, Vec3::ZERO, Vec3::Y, |p| Some([shade(p); 3]));
        };
        let mut empty = RgbdFrame::new(64, 48);
        floor(&mut empty, &|_| 200);
        let plate = BackgroundPlate::from_frame(&empty);
        let params = ReconstructParams::default();

        // Nothing changed: nothing is marked, also with a little noise.
        assert!(foreground_mask(&empty, &plate, &params).iter().all(|&m| m == MASK_SKIP));
        let mut jitter = empty.clone();
        for (i, z) in jitter.depth.iter_mut().enumerate() {
            *z += if i % 2 == 0 { 0.01 } else { -0.01 };
            jitter.rgb[i * 3] = jitter.rgb[i * 3].wrapping_add((i % 7) as u8);
        }
        assert!(foreground_mask(&jitter, &plate, &params).iter().all(|&m| m == MASK_SKIP));

        // A box standing on the floor and its shadow next to it.
        let mut live = RgbdFrame::new(64, 48);
        floor(&mut live, &|p| if (0.4..0.9).contains(&p.x) && p.z.abs() < 0.3 { 110 } else { 200 });
        draw_plane(&mut live, &cam, Vec3::new(0.0, 0.0, 0.2), Vec3::Z, |p| {
            (p.x.abs() < 0.3 && (0.0..0.8).contains(&p.y)).then_some([30, 60, 200])
        });
        let mask = foreground_mask(&live, &plate, &params);
        let (mut front, mut changed) = (0, 0);
        for (i, &m) in mask.iter().enumerate() {
            let is_box = live.rgb[i * 3 + 2] == 200 && live.rgb[i * 3] == 30;
            let is_shadow = live.rgb[i * 3] == 110;
            match m {
                MASK_FOREGROUND => {
                    assert!(is_box, "pixel {i} is not on the box");
                    front += 1;
                }
                // The foot of the box is within the depth margin of the
                // floor; there it is its color that gives it away.
                MASK_CHANGED => {
                    assert!(is_shadow || is_box, "pixel {i} is neither shadow nor box");
                    changed += usize::from(is_shadow);
                }
                _ => assert!(!is_shadow, "shadow pixel {i} was missed"),
            }
        }
        let box_pixels = (0..64 * 48).filter(|&i| live.rgb[i * 3] == 30).count();
        // The lowest rows of the box are within the depth margin of the floor.
        assert!(front > box_pixels * 8 / 10 && front <= box_pixels, "{front} of {box_pixels}");
        assert!(changed > 20);

        // Shadows can be switched off.
        let mut no_shadow = params;
        no_shadow.background.color_threshold = 255.0;
        assert!(!foreground_mask(&live, &plate, &no_shadow).contains(&MASK_CHANGED));

        // Something behind the plate (a hole in the floor) is not foreground.
        let mut behind = empty.clone();
        for z in &mut behind.depth {
            *z += 1.0;
        }
        assert!(foreground_mask(&behind, &plate, &params).iter().all(|&m| m == MASK_SKIP));
    }

    #[test]
    fn mask_edge_cases() {
        let params = ReconstructParams::default();
        let mut plate_frame = RgbdFrame::new(8, 8);
        plate_frame.depth.fill(3.0);
        plate_frame.depth[0] = 0.0;
        let plate = BackgroundPlate::from_frame(&plate_frame);
        let mut live = plate_frame.clone();
        live.depth.fill(3.0);
        // A 3x3 blob and one lonely pixel in front of the plate.
        for (x, y) in [(2, 2), (3, 2), (4, 2), (2, 3), (3, 3), (4, 3), (2, 4), (3, 4), (4, 4), (7, 7)] {
            let i = live.index(x, y);
            live.depth[i] = 2.0;
        }
        let mask = foreground_mask(&live, &plate, &params);
        assert_eq!(mask.iter().filter(|&&m| m == MASK_FOREGROUND).count(), 9, "the speckle goes, the blob stays");
        assert_eq!(mask[live.index(7, 7)], MASK_SKIP);
        // Where the plate has no depth, a lone valid pixel is a speckle too.
        assert_eq!(mask[0], MASK_SKIP);
        let mut keep = params;
        keep.background.min_neighbors = 0;
        let mask = foreground_mask(&live, &plate, &keep);
        assert_eq!(mask[live.index(7, 7)], MASK_FOREGROUND);
        assert_eq!(mask[0], MASK_FOREGROUND, "no plate depth: anything valid is foreground");

        // Invalid live pixels are never marked.
        let (a, b) = (live.index(3, 3), live.index(3, 2));
        live.depth[a] = 0.0;
        live.depth[b] = f32::NAN;
        let mask = foreground_mask(&live, &plate, &keep);
        assert_eq!((mask[live.index(3, 3)], mask[live.index(3, 2)]), (MASK_SKIP, MASK_SKIP));

        // A plate of another size cannot subtract anything.
        let other = BackgroundPlate::from_frame(&RgbdFrame::new(4, 4));
        let mask = foreground_mask(&live, &other, &keep);
        assert_eq!(mask.iter().filter(|&&m| m == MASK_FOREGROUND).count(), 62);
        assert!(foreground_mask(&RgbdFrame::default(), &plate, &params).is_empty());
    }

    #[test]
    fn far_foreground_needs_a_larger_margin_than_near_foreground() {
        let params = ReconstructParams::default();
        let frame_at = |z: f32| {
            let mut f = RgbdFrame::new(4, 4);
            f.depth.fill(z);
            f
        };
        // 5 cm in front of the plate: clear at 1 m, within the noise at 6 m.
        for (z, expected) in [(1.0, MASK_FOREGROUND), (6.0, MASK_SKIP)] {
            let plate = BackgroundPlate::from_frame(&frame_at(z));
            let mask = foreground_mask(&frame_at(z - 0.05), &plate, &params);
            assert!(mask.iter().all(|&m| m == expected), "at {z} m");
        }
    }

    // -- voxel fusion -------------------------------------------------------

    /// A wall in the plane `z = 0` facing `+z`, two colors side by side.
    fn wall(cam: &Camera) -> RgbdFrame {
        let mut frame = RgbdFrame::new(cam.width, cam.height);
        draw_plane(&mut frame, cam, Vec3::ZERO, Vec3::Z, |p| {
            (p.x.abs() < 1.0 && p.y.abs() < 0.75).then_some(if p.x < 0.0 { [200, 30, 30] } else { [30, 30, 200] })
        });
        frame
    }

    /// Checks that the splats of facing `+z` near `z = 0` fill the grid over
    /// `|x| < half_x`, `|y| < half_y` without holes or duplicates.
    fn assert_full_grid(fusion: &VoxelFusion, half_x: f32, half_y: f32) {
        let voxel = fusion.voxel_size();
        let (nx, ny) = ((2.0 * half_x / voxel).round() as i32, (2.0 * half_y / voxel).round() as i32);
        let mut seen = vec![0u8; (nx * ny) as usize];
        for s in fusion.splats() {
            let cell = |p: f32, half: f32| ((p + half) / voxel - 0.5).round() as i32;
            let (ix, iy) = (cell(s.pos[0], half_x), cell(s.pos[1], half_y));
            if normal_of(s).z > 0.9 && s.pos[2].abs() < voxel && (0..nx).contains(&ix) && (0..ny).contains(&iy) {
                seen[(iy * nx + ix) as usize] += 1;
                // On the cell center.
                let cx = (ix as f32 + 0.5) * voxel - half_x;
                assert!((s.pos[0] - cx).abs() < 1e-4, "splat at {:?} is off its cell center", s.pos);
            }
        }
        let holes = seen.iter().filter(|&&c| c == 0).count();
        let doubles = seen.iter().filter(|&&c| c > 1).count();
        assert_eq!((holes, doubles), (0, 0), "of {} cells", seen.len());
    }

    #[test]
    fn fusion_creates_each_surface_cell_once() {
        let mut fusion = VoxelFusion::new(0.05);
        let params = ReconstructParams::default();
        let cam = camera(Vec3::new(0.2, 0.1, 2.0), Vec3::ZERO, 70.0, 96, 72);
        let fresh = fusion.integrate(&wall(&cam), &cam, &params);
        assert_eq!(fresh.len(), fusion.len());
        assert_eq!(fresh, fusion.splats());
        assert!(!fusion.is_empty());
        // 2 m x 1.5 m of wall in 5 cm cells, a little less at the borders.
        assert!((1100..=1200).contains(&fusion.len()), "{}", fusion.len());
        assert_full_grid(&fusion, 0.9, 0.65);
        for s in fusion.splats() {
            assert!(s.pos[2].abs() < 1e-3 && normal_of(s).z > 0.999);
            // Overlapping neighbours: at least 0.6 voxels of radius.
            assert!(s.scale[0] >= 0.6 * 0.05 - 1e-6 && s.scale[1] >= 0.6 * 0.05 - 1e-6);
            assert!(s.scale[2] < s.scale[1]);
            let expected = if s.pos[0] < 0.0 { [200, 30, 30] } else { [30, 30, 200] };
            // The cells on the color boundary average both sides.
            if s.pos[0].abs() > 0.05 {
                assert_eq!(&s.rgba[..3], &expected);
            }
            assert_eq!(s.rgba[3], 255);
        }

        // The same view again, and a view from elsewhere: nothing new, in
        // the same order.
        let before = fusion.splats().to_vec();
        assert!(fusion.integrate(&wall(&cam), &cam, &params).is_empty());
        let other = camera(Vec3::new(-0.6, 0.3, 1.6), Vec3::new(0.1, 0.0, 0.0), 70.0, 96, 72);
        let fresh = fusion.integrate(&wall(&other), &other, &params);
        assert!(fresh.len() < before.len() / 20, "{} new cells for a seen wall", fresh.len());
        assert_eq!(fusion.len(), before.len() + fresh.len());
        for (a, b) in before.iter().zip(fusion.splats()) {
            assert!((a.position() - b.position()).length() < 1e-3);
        }
        assert_full_grid(&fusion, 0.9, 0.65);

        fusion.clear();
        assert!(fusion.is_empty() && fusion.refined() == 0);
        assert_eq!(fusion.len(), 0);
        assert_eq!(fusion.integrate(&wall(&cam), &cam, &params).len(), before.len());
    }

    #[test]
    fn distant_views_fill_every_cell() {
        // From 4 m a pixel is 6 cm wide: three cells of 2 cm.
        let mut fusion = VoxelFusion::new(0.02);
        let cam = camera(Vec3::new(0.0, 0.0, 4.0), Vec3::ZERO, 50.0, 96, 72);
        fusion.integrate(&wall(&cam), &cam, &ReconstructParams::default());
        assert_full_grid(&fusion, 0.9, 0.64);
        // And at a slant.
        let mut fusion = VoxelFusion::new(0.02);
        let cam = camera(Vec3::new(2.5, 0.3, 2.0), Vec3::ZERO, 50.0, 96, 72);
        fusion.integrate(&wall(&cam), &cam, &ReconstructParams::default());
        assert_full_grid(&fusion, 0.9, 0.64);
        for s in fusion.splats() {
            assert!(s.pos[2].abs() < 2e-3, "a cell floats {} m off the wall", s.pos[2]);
        }
    }

    #[test]
    fn nothing_is_created_beyond_a_silhouette() {
        let mut fusion = VoxelFusion::new(0.02);
        let cam = camera(Vec3::new(0.0, 0.0, 4.0), Vec3::ZERO, 50.0, 96, 72);
        fusion.integrate(&wall(&cam), &cam, &ReconstructParams::default());
        for s in fusion.splats() {
            // Cell centers stay inside the wall (plus the cell they are in).
            assert!(s.pos[0].abs() < 1.0 + 0.02 && s.pos[1].abs() < 0.75 + 0.02, "overhang at {:?}", s.pos);
        }
    }

    #[test]
    fn both_faces_of_a_thin_wall_are_kept_apart() {
        let mut fusion = VoxelFusion::new(0.05);
        let params = ReconstructParams::default();
        // The wall is 1 cm thick: the red face looks toward +z, the green
        // one toward -z.
        let front = camera(Vec3::new(0.0, 0.0, 2.0), Vec3::ZERO, 70.0, 96, 72);
        let mut frame = RgbdFrame::new(96, 72);
        draw_plane(&mut frame, &front, Vec3::ZERO, Vec3::Z, |p| (p.x.abs() < 1.0 && p.y.abs() < 0.75).then_some([220, 20, 20]));
        let first = fusion.integrate(&frame, &front, &params).len();
        let back = camera(Vec3::new(0.0, 0.0, -2.0), Vec3::new(0.0, 0.0, -0.01), 70.0, 96, 72);
        let mut frame = RgbdFrame::new(96, 72);
        draw_plane(&mut frame, &back, Vec3::new(0.0, 0.0, -0.01), Vec3::NEG_Z, |p| {
            (p.x.abs() < 1.0 && p.y.abs() < 0.75).then_some([20, 220, 20])
        });
        let second = fusion.integrate(&frame, &back, &params).len();
        assert!(first > 1000 && (second as i64 - first as i64).abs() < 30, "{first} then {second}");
        for s in fusion.splats() {
            let n = normal_of(s);
            if s.rgba[0] == 220 {
                assert!(n.z > 0.99 && s.pos[2].abs() < 1e-3);
            } else {
                assert_eq!(&s.rgba[..3], &[20, 220, 20], "the faces bled into each other");
                assert!(n.z < -0.99 && (s.pos[2] + 0.01).abs() < 1e-3);
            }
        }
    }

    #[test]
    fn noise_does_not_double_surfaces_but_distinct_ones_are_kept() {
        let params = ReconstructParams::default();
        let cam = camera(Vec3::new(0.0, 0.0, 2.0), Vec3::ZERO, 70.0, 96, 72);
        let wall_at = |z: f32| {
            let mut frame = RgbdFrame::new(96, 72);
            draw_plane(&mut frame, &cam, Vec3::new(0.0, 0.0, z), Vec3::Z, |p| {
                (p.x.abs() < 1.0 && p.y.abs() < 0.75).then_some([128; 3])
            });
            frame
        };
        for offset in [-0.9f32, -0.5, 0.3, 0.9] {
            // The wall right on a slab boundary, then moved by under a voxel.
            for base in [0.0f32, 0.1, 0.149] {
                let mut fusion = VoxelFusion::new(0.05);
                let first = fusion.integrate(&wall_at(base), &cam, &params).len();
                let again = fusion.integrate(&wall_at(base + offset * 0.05), &cam, &params).len();
                assert!(again * 20 < first, "{again} duplicates at {base} + {offset} voxels");
            }
        }
        // Four voxels behind: a surface of its own.
        let mut fusion = VoxelFusion::new(0.05);
        let first = fusion.integrate(&wall_at(0.0), &cam, &params).len();
        let behind = fusion.integrate(&wall_at(-0.2), &cam, &params).len();
        assert!(behind as f32 > 0.9 * first as f32, "{behind} of {first}");
    }

    #[test]
    fn closer_views_refine_in_place() {
        let params = ReconstructParams::default();
        let far = camera(Vec3::new(0.0, 0.0, 4.0), Vec3::ZERO, 50.0, 96, 72);
        let near = camera(Vec3::new(0.0, 0.0, 1.0), Vec3::ZERO, 50.0, 96, 72);
        // A wall with stripes of 3 cm that the far view cannot resolve.
        let striped = |cam: &Camera| {
            let mut frame = RgbdFrame::new(96, 72);
            draw_plane(&mut frame, cam, Vec3::ZERO, Vec3::Z, |p| {
                (p.x.abs() < 1.0 && p.y.abs() < 0.75)
                    .then_some(if (p.x / 0.03).floor() as i32 % 2 == 0 { [255; 3] } else { [0; 3] })
            });
            frame
        };
        let contrast = |fusion: &VoxelFusion| {
            let center: Vec<&Splat> = fusion.splats().iter().filter(|s| s.pos[0].abs() < 0.2 && s.pos[1].abs() < 0.15).collect();
            let crisp = center.iter().filter(|s| s.rgba[0] < 40 || s.rgba[0] > 215).count();
            crisp as f32 / center.len() as f32
        };
        let mut fusion = VoxelFusion::new(0.01);
        fusion.integrate(&striped(&far), &far, &params);
        let (count, order) = (fusion.len(), fusion.splats().iter().map(|s| s.pos).collect::<Vec<_>>());
        assert_eq!(fusion.refined(), 0);
        let fresh = fusion.integrate(&striped(&near), &near, &params);
        assert!(fresh.len() < count / 50, "the near view found {} new cells", fresh.len());
        assert!(fusion.refined() > 1000);
        assert!(contrast(&fusion) > 0.6, "stripes still blurred: {}", contrast(&fusion));
        // Same splats at the same indices.
        for (a, b) in order.iter().zip(fusion.splats()) {
            assert!((Vec3::from_array(*a) - b.position()).length() < 2e-3);
        }
        // More far views leave the refined cells alone; the cells the near
        // view did not reach average a few of them and then settle.
        let mut refined = fusion.refined();
        for round in 0.. {
            assert!(fusion.integrate(&striped(&far), &far, &params).is_empty());
            if fusion.refined() == refined {
                break;
            }
            refined = fusion.refined();
            assert!(round < 40, "the map never settles");
        }
        assert!(contrast(&fusion) > 0.6, "the far view blurred the stripes again: {}", contrast(&fusion));

        // With refinement off the first view stays.
        let mut frozen = VoxelFusion::new(0.01);
        frozen.refine_ratio = 0.0;
        frozen.integrate(&striped(&far), &far, &params);
        let before = frozen.splats().to_vec();
        frozen.integrate(&striped(&near), &near, &params);
        assert_eq!(frozen.refined(), 0);
        assert_eq!(&frozen.splats()[..before.len()], &before[..]);
    }

    #[test]
    fn range_angle_and_memory_limits() {
        let params = ReconstructParams::default();
        let cam = camera(Vec3::new(0.0, 0.0, 2.0), Vec3::ZERO, 70.0, 96, 72);
        let frame = wall(&cam);

        let mut fusion = VoxelFusion::new(0.05);
        fusion.max_range = 1.5;
        assert!(fusion.integrate(&frame, &cam, &params).is_empty(), "beyond the range");

        // Seen at about 80 degrees from its normal.
        let grazing = camera(Vec3::new(3.0, 0.0, 0.5), Vec3::ZERO, 50.0, 96, 72);
        let mut fusion = VoxelFusion::new(0.05);
        fusion.min_cos = 0.3;
        // (A few corner pixels have too few neighbours to tell their slant.)
        let kept = fusion.integrate(&wall(&grazing), &grazing, &params).len();
        assert!(kept <= 8, "{kept} cells from a view that is too grazing");
        fusion.clear();
        fusion.min_cos = 0.1;
        let kept = fusion.integrate(&wall(&grazing), &grazing, &params).len();
        assert!(kept > 900, "{kept}");
        assert!(fusion.splats().iter().filter(|s| normal_of(s).z > 0.99 && s.pos[2].abs() < 2e-3).count() > kept * 95 / 100);

        let mut fusion = VoxelFusion::new(0.05);
        fusion.max_voxels = 100;
        assert_eq!(fusion.integrate(&frame, &cam, &params).len(), 100);
        assert!(fusion.integrate(&frame, &cam, &params).is_empty());
        assert_eq!(fusion.len(), 100);

        // Sizes are clamped; frames that do not fit the camera are ignored.
        assert_eq!(VoxelFusion::new(0.0).voxel_size(), VoxelFusion::MIN_VOXEL_SIZE);
        assert_eq!(VoxelFusion::new(1.0e9).voxel_size(), VoxelFusion::MAX_VOXEL_SIZE);
        assert_eq!(VoxelFusion::new(f32::NAN).voxel_size(), VoxelFusion::default().voxel_size());
        let mut fusion = VoxelFusion::default();
        assert!(fusion.integrate(&frame, &cam.resized(48, 36), &params).is_empty());
        assert!(fusion.integrate(&RgbdFrame::default(), &cam.resized(0, 0), &params).is_empty());
        // Far from the origin the cell indices would not fit their key.
        let mut far = frame.clone();
        for z in &mut far.depth {
            *z *= 1.0e7;
        }
        fusion.max_range = f32::INFINITY;
        fusion.integrate(&far, &cam, &params);
        assert!(fusion.splats().iter().all(|s| s.position().is_finite()));
    }

    #[test]
    fn cells_are_round_discs_also_on_slopes_and_creases() {
        // A 45 degree slope meeting a wall: cell normals vary along the
        // crease, but no splat may be stretched into a spike.
        let cam = camera(Vec3::new(0.3, 0.4, 2.5), Vec3::ZERO, 60.0, 96, 72);
        let mut frame = wall(&cam);
        let slope = Vec3::new(0.0, 1.0, 1.0).normalize();
        draw_plane(&mut frame, &cam, Vec3::new(0.0, -0.3, 0.0), slope, |p| (p.y < 0.2).then_some([40, 40, 40]));
        let mut fusion = VoxelFusion::new(0.04);
        let params = ReconstructParams::default();
        fusion.integrate(&frame, &cam, &params);
        assert!(fusion.len() > 500);
        let sigma = fusion.radius_factor * fusion.voxel_size();
        let mut sloped = 0;
        for s in fusion.splats() {
            assert_eq!(s.scale[0], sigma);
            assert_eq!(s.scale[1], sigma);
            assert!((s.scale[2] - sigma * params.thickness).abs() < 1e-7);
            assert!((s.rotation().length() - 1.0).abs() < 1e-4);
            sloped += usize::from(normal_of(s).dot(slope) > 0.99);
        }
        assert!(sloped > 100, "the slope keeps its own normal ({sloped} cells)");
    }

    #[test]
    fn broken_calibration_and_extreme_depths_never_give_broken_splats() {
        let good = camera(Vec3::new(0.0, 1.0, 3.0), Vec3::ZERO, 60.0, 16, 12);
        let frame = plane_frame(&good, Vec3::ZERO, Vec3::Z, [9, 9, 9]);
        let params = ReconstructParams::default();
        assert!(!splats_of(&frame, &good, &params).is_empty());
        let nan_pose = glam::Affine3A::from_translation(Vec3::splat(f32::NAN));
        for cam in [
            Camera { fx: 0.0, ..good },
            Camera { fy: -10.0, ..good },
            Camera { fx: f32::NAN, ..good },
            Camera { fy: f32::INFINITY, ..good },
            Camera { fx: 1.0e30, fy: 1.0e30, ..good },
            Camera { cx: f32::NAN, ..good },
            Camera { world_from_cam: nan_pose, ..good },
        ] {
            assert!(splats_of(&frame, &cam, &params).is_empty(), "{cam:?}");
            assert!(VoxelFusion::default().integrate(&frame, &cam, &params).is_empty(), "{cam:?}");
        }
        // Depths a sensor never delivers, next to good ones.
        let odd = [2.0, f32::NAN, 1.0e30, 0.0, -3.0, 1.0e-30, f32::INFINITY, 2.01, f32::MAX, f32::MIN_POSITIVE];
        let mut weird = RgbdFrame::new(16, 12);
        for (i, z) in weird.depth.iter_mut().enumerate() {
            *z = odd[(i * 7 + i / 16) % odd.len()];
        }
        let sound = |s: &Splat| {
            s.position().is_finite()
                && s.scale.iter().all(|v| v.is_finite() && *v > 0.0)
                && (s.rotation().length() - 1.0).abs() < 1e-3
        };
        for min_support in [0, 1] {
            let p = ReconstructParams { min_support, ..params };
            let splats = splats_of(&weird, &good, &p);
            assert!(splats.iter().all(sound), "min_support {min_support}");
            let mut fusion = VoxelFusion::new(0.05);
            fusion.max_range = f32::INFINITY;
            fusion.integrate(&weird, &good, &p);
            assert!(fusion.splats().iter().all(sound));
        }
        // The smallest images.
        for (w, h) in [(1, 1), (1, 5), (5, 1), (2, 2)] {
            let cam = camera(Vec3::new(0.0, 0.0, 2.0), Vec3::ZERO, 60.0, w, h);
            let tiny = plane_frame(&cam, Vec3::ZERO, Vec3::Z, [1, 2, 3]);
            let lone = ReconstructParams { min_support: 0, ..params };
            assert_eq!(splats_of(&tiny, &cam, &lone).len(), (w * h) as usize);
            assert!(splats_of(&tiny, &cam, &lone).iter().all(sound));
            let plate = BackgroundPlate::from_frame(&tiny);
            assert_eq!(foreground_mask(&tiny, &plate, &params), vec![MASK_SKIP; (w * h) as usize]);
            VoxelFusion::default().integrate(&tiny, &cam, &params);
        }
    }

    #[test]
    fn fusion_is_deterministic() {
        let run = || {
            let mut fusion = VoxelFusion::new(0.04);
            let params = ReconstructParams::default();
            let eyes = [Vec3::new(0.2, 0.1, 3.0), Vec3::new(-0.6, 0.3, 1.6), Vec3::new(1.5, -0.2, 1.2)];
            for (i, eye) in eyes.into_iter().enumerate() {
                let cam = camera(eye, Vec3::new(0.1 * i as f32, 0.0, 0.0), 70.0, 96, 72);
                fusion.integrate(&wall(&cam), &cam, &params);
            }
            (fusion.splats().to_vec(), fusion.refined())
        };
        assert_eq!(run(), run());
    }
}
