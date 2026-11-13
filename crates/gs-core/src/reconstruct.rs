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
//!    Neighbours across a depth discontinuity are ignored. When both sides of
//!    an axis are usable their differences are blended with weights that
//!    favour the flatter side: on a plane that is the central difference, at a
//!    small step (an arm in front of the chest) it follows the surface the
//!    pixel belongs to instead of bridging the step.
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
//! 4. Observations with `cos θ < min_cos` (grazing) and pixels with too few
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
//! good observation, so a scan grows by appending and never duplicates.

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
    /// footprints. Below about 0.6 neighbouring splats leave gaps (a dotted
    /// look); larger values blur.
    pub footprint: f32,
    /// Standard deviation along the normal, as a fraction of the shorter
    /// tangential one.
    pub thickness: f32,
    /// Opacity of every splat, `0..=1`.
    pub opacity: f32,
    /// Observations whose normal makes a cosine below this with the viewing
    /// ray are dropped (grazing angle).
    pub min_cos: f32,
    /// Two neighbouring pixels belong to different surfaces when their depths
    /// differ by more than this fraction of the depth.
    pub edge_threshold: f32,
    /// Largest ratio between the two tangential standard deviations.
    pub max_anisotropy: f32,
    /// A pixel needs at least this many depth-continuous 4-neighbours;
    /// lonelier ones are flying pixels or speckles and are dropped. With `0`
    /// an isolated pixel becomes a camera-facing isotropic splat.
    pub min_support: u32,
    /// Edge-preserving smoothing passes over the inverse depth before
    /// normals and positions are computed. Each pass is exact on planes;
    /// one or two tame sensor noise.
    pub smooth_passes: u32,
    /// Background subtraction of the live stage.
    pub background: BackgroundParams,
}

impl Default for ReconstructParams {
    fn default() -> Self {
        Self {
            footprint: 0.7,
            thickness: 0.15,
            opacity: 0.9,
            min_cos: 0.15,
            edge_threshold: 0.04,
            max_anisotropy: 4.0,
            min_support: 1,
            smooth_passes: 1,
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
fn inverse_depth(frame: &RgbdFrame, params: &ReconstructParams) -> Vec<f32> {
    let n = frame.width as usize * frame.height as usize;
    let mut inv: Vec<f32> =
        frame.depth.iter().take(n).map(|&z| if z > 0.0 && z.is_finite() { 1.0 / z } else { 0.0 }).collect();
    inv.resize(n, 0.0);
    let (w, h) = (frame.width as usize, frame.height as usize);
    if w >= 3 && h >= 3 {
        for _ in 0..params.smooth_passes.min(ReconstructParams::MAX_SMOOTH_PASSES) {
            inv = smooth_pass(&inv, w, h, params.edge_threshold);
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
            let near = |v: f32| v > 0.0 && (v - c).abs() <= edge * v;
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
}

/// Intrinsics and the prepared depth of one frame.
struct FrameGeometry<'a> {
    cam: &'a Camera,
    inv: Vec<f32>,
    w: usize,
    h: usize,
    edge: f32,
    min_support: u32,
}

impl<'a> FrameGeometry<'a> {
    fn new(frame: &RgbdFrame, cam: &'a Camera, params: &ReconstructParams) -> Self {
        Self {
            cam,
            inv: inverse_depth(frame, params),
            w: frame.width as usize,
            h: frame.height as usize,
            edge: params.edge_threshold,
            min_support: params.min_support,
        }
    }

    /// Blends the backward and forward differences of the inverse depth along
    /// one image axis. Returns the gradient and the number of usable sides.
    #[inline]
    fn gradient(&self, c: f32, before: Option<f32>, after: Option<f32>, slope_scale: f32) -> (f32, u32) {
        let diff = |v: Option<f32>, sign: f32| {
            v.filter(|&v| v > 0.0 && (v - c).abs() <= self.edge * v).map(|v| sign * (v - c))
        };
        match (diff(before, -1.0), diff(after, 1.0)) {
            (Some(d0), Some(d1)) => {
                // `s` is the slope in pixel footprints of depth per pixel
                // (the tangent of the slant). Equal slopes average exactly;
                // a flat side next to a steep one dominates.
                let weight = |d: f32| {
                    let s = d * slope_scale;
                    1.0 / (1.0 + s * s)
                };
                let (g0, g1) = (weight(d0), weight(d1));
                ((d0 * g0 + d1 * g1) / (g0 + g1), 2)
            }
            (Some(d), None) | (None, Some(d)) => (d, 1),
            (None, None) => (0.0, 0),
        }
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
        let at = |ok: bool, j: usize| if ok { Some(self.inv[j]) } else { None };
        let (a, su) = self.gradient(c, at(x > 0, i.wrapping_sub(1)), at(x + 1 < self.w, i + 1), cam.fx * z);
        let (b, sv) =
            self.gradient(c, at(y > 0, i.wrapping_sub(self.w)), at(y + 1 < self.h, i + self.w), cam.fy * z);
        if su + sv < self.min_support {
            return None;
        }
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
        Some(PixelSurface { p, pu, pv, n, cos, z })
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
    let s2 = l2.sqrt().max(s1 / max_anisotropy.max(1.0));
    let s1 = s1.min(s2 * max_anisotropy.max(1.0));
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
        let rgb = &frame.rgb[i * 3..i * 3 + 3];
        out.push(tangent_splat(
            cam.world_from_cam.transform_point3(p),
            rotation * (s.pu * k),
            rotation * (s.pv * k),
            Vec3::from(rotation * s.n),
            params.thickness,
            params.max_anisotropy,
            [rgb[0], rgb[1], rgb[2], alpha],
        ));
    }
}

/// True when `frame` has the buffers its size promises and matches `cam`.
fn frame_matches(frame: &RgbdFrame, cam: &Camera) -> bool {
    let n = frame.width as usize * frame.height as usize;
    frame.width == cam.width && frame.height == cam.height && frame.depth.len() >= n && frame.rgb.len() >= n * 3
}

/// Converts the valid pixels of `frame` into Gaussians appended to `out` and
/// returns how many were added (row-major pixel order).
///
/// `mask`, when given, holds one byte per pixel: [`MASK_SKIP`] pixels are
/// ignored, [`MASK_CHANGED`] ones are lifted off their surface (see
/// [`BackgroundParams::shadow_lift`]), any other value is a plain foreground
/// pixel. Normals always use the whole depth map, masked or not.
///
/// A frame that does not match the camera's image size, or a mask of the
/// wrong length, produces nothing. With the `par` feature rows are processed
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
    let geo = FrameGeometry::new(frame, cam, params);
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
    rgb: [u8; 3],
}

/// A cell first observed in the frame being integrated.
struct Pending {
    /// Facing class `0..6`: dominant axis of the normal times two, plus one
    /// when the normal points to the negative side.
    class: u8,
    /// Cell indices along the two tangent axes.
    iu: i64,
    iv: i64,
    normal: Vec3,
    height: f32,
    rgb: [u32; 3],
    count: u32,
}

/// Incremental surface fusion for the home scan.
///
/// Space is cut into cells of [`VoxelFusion::voxel_size`]. Each surface
/// element is filed under the axis its normal is closest to (six *facings*:
/// ±X, ±Y, ±Z), in a column of one voxel by one voxel perpendicular to that
/// axis. The first good observation of a column creates one splat that
/// covers the column's whole patch of surface; later observations of the
/// same patch are ignored. Consequences:
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
/// All pixels of a frame that fall into a new cell are averaged (color and
/// normal), which low-pass filters close-up views down to the voxel size.
///
/// Memory is bounded: at most [`VoxelFusion::max_voxels`] cells are created
/// (44 bytes per splat plus about 20 bytes of hash table each); observations
/// of new surface beyond that are dropped.
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
    /// Upper bound on the number of cells (and splats).
    pub max_voxels: usize,
    cells: CellMap,
    splats: Vec<Splat>,
}

impl VoxelFusion {
    /// Smallest and largest accepted voxel sizes in meters.
    pub const MIN_VOXEL_SIZE: f32 = 0.005;
    pub const MAX_VOXEL_SIZE: f32 = 1.0;
    /// Default bound on the number of cells: about 100 MB of memory and
    /// 24 MB on the wire.
    pub const DEFAULT_MAX_VOXELS: usize = 1_500_000;

    /// An empty map with cells of `voxel_size` meters (clamped to
    /// [`VoxelFusion::MIN_VOXEL_SIZE`]`..=`[`VoxelFusion::MAX_VOXEL_SIZE`];
    /// 3 cm when it is not a number).
    pub fn new(voxel_size: f32) -> Self {
        Self {
            voxel_size: finite_clamp(voxel_size, 0.03, Self::MIN_VOXEL_SIZE, Self::MAX_VOXEL_SIZE),
            max_range: 5.0,
            min_cos: 0.3,
            radius_factor: 0.6,
            max_voxels: Self::DEFAULT_MAX_VOXELS,
            cells: CellMap::default(),
            splats: Vec::new(),
        }
    }

    /// Edge length of a cell in meters.
    pub fn voxel_size(&self) -> f32 {
        self.voxel_size
    }

    /// Every splat created so far, in creation order. The slice only ever
    /// grows at its end (until [`VoxelFusion::clear`]).
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

    /// Forgets everything.
    pub fn clear(&mut self) {
        self.cells.clear();
        self.splats.clear();
    }

    /// Fuses one frame and returns the splats of the surface cells it
    /// observed for the first time (also appended to
    /// [`VoxelFusion::splats`]). The result is deterministic, with or without
    /// the `par` feature.
    ///
    /// `params` supplies the depth-edge threshold, the smoothing, the
    /// opacity and the relative thickness; the footprint factor is not used
    /// because the splat size follows the voxel size.
    pub fn integrate(&mut self, frame: &RgbdFrame, cam: &Camera, params: &ReconstructParams) -> Vec<Splat> {
        let n = frame.width as usize * frame.height as usize;
        if n == 0 || !frame_matches(frame, cam) {
            return Vec::new();
        }
        let geo = FrameGeometry::new(frame, cam, params);
        let rows = self.observe(&geo, frame);

        let base = self.splats.len();
        let mut pending: Vec<Pending> = Vec::new();
        let voxel = self.voxel_size;
        let inv_voxel = 1.0 / voxel;
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
            let class = (axis * 2) as u8 + u8::from(n[axis] < 0.0);
            let fu = (obs.pos[ua] * inv_voxel).floor();
            let fv = (obs.pos[va] * inv_voxel).floor();
            // Height of the observed tangent plane above the column center:
            // the same for every pixel of a flat patch, wherever it falls
            // inside the column.
            let du = (fu + 0.5) * voxel - obs.pos[ua];
            let dv = (fv + 0.5) * voxel - obs.pos[va];
            let height = obs.pos[axis] - (n[ua] * du + n[va] * dv) / n[axis];
            let slab = height * (0.5 * inv_voxel);
            let fs = slab.floor();
            let (iu, iv, is) = (fu as i64, fv as i64, fs as i64);
            if !height.is_finite() || [iu, iv, is].iter().any(|c| !(1 - CELL_LIMIT..CELL_LIMIT - 1).contains(c)) {
                continue;
            }
            let key = |slab: i64| {
                let pack = |c: i64| (c + CELL_LIMIT) as u64;
                (class as u64) << (3 * CELL_BITS) | pack(iu) << (2 * CELL_BITS) | pack(iv) << CELL_BITS | pack(slab)
            };
            // The neighbouring slab on the nearer side.
            let other = if slab - fs < 0.5 { is - 1 } else { is + 1 };
            let found = self.cells.get(&key(is)).or_else(|| self.cells.get(&key(other))).copied();
            match found {
                Some(index) if (index as usize) < base => {}
                Some(index) => {
                    let p = &mut pending[index as usize - base];
                    p.normal += n;
                    p.height += height;
                    p.count += 1;
                    for c in 0..3 {
                        p.rgb[c] += obs.rgb[c] as u32;
                    }
                }
                None => {
                    if base + pending.len() >= self.max_voxels.min(u32::MAX as usize) {
                        continue;
                    }
                    self.cells.insert(key(is), (base + pending.len()) as u32);
                    pending.push(Pending {
                        class,
                        iu,
                        iv,
                        normal: n,
                        height,
                        rgb: [obs.rgb[0] as u32, obs.rgb[1] as u32, obs.rgb[2] as u32],
                        count: 1,
                    });
                }
            }
        }

        let alpha = opacity_byte(params.opacity);
        let sigma = self.radius_factor * voxel;
        self.splats.reserve(pending.len());
        for p in &pending {
            let axis = (p.class / 2) as usize;
            let (ua, va) = ((axis + 1) % 3, (axis + 2) % 3);
            let sign = if p.class % 2 == 0 { 1.0 } else { -1.0 };
            let mut n = p.normal.normalize_or(Vec3::ZERO);
            // The averaged normal keeps its facing (all its terms have the
            // same sign along the axis); guard the degenerate cases anyway.
            if n[axis] * sign < 0.5 {
                n = Vec3::ZERO;
                n[axis] = sign;
            }
            let mut pos = Vec3::ZERO;
            pos[ua] = (p.iu as f32 + 0.5) * voxel;
            pos[va] = (p.iv as f32 + 0.5) * voxel;
            pos[axis] = p.height / p.count as f32;
            // Vectors in the surface plane that project onto the column's
            // two unit axes: together they span the column's patch.
            let mut tu = Vec3::ZERO;
            tu[ua] = 1.0;
            tu[axis] = -n[ua] / n[axis];
            let mut tv = Vec3::ZERO;
            tv[va] = 1.0;
            tv[axis] = -n[va] / n[axis];
            let half = p.count / 2;
            let rgba = [
                ((p.rgb[0] + half) / p.count) as u8,
                ((p.rgb[1] + half) / p.count) as u8,
                ((p.rgb[2] + half) / p.count) as u8,
                alpha,
            ];
            self.splats.push(tangent_splat(pos, tu * sigma, tv * sigma, n, params.thickness, 2.0, rgba));
        }
        self.splats[base..].to_vec()
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
                    normal: Vec3::from(rotation * s.n),
                    rgb: [frame.rgb[i * 3], frame.rgb[i * 3 + 1], frame.rgb[i * 3 + 2]],
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
