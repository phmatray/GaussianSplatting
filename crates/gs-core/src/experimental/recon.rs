//! A copy of the per-pixel depth-to-Gaussian conversion of
//! [`crate::reconstruct`] with the hooks the quality lab needs.
//!
//! `reconstruct.rs` keeps its per-pixel geometry private and the lab may not
//! edit it, so the geometry (`FrameGeometry`, `fit_axis`, `tangent_splat`) is
//! copied here verbatim. Two things are added on top:
//!
//! * every splat comes with its source pixel, forward depth, world normal and
//!   incidence cosine ([`PixelSplat`]), which is what the multi-view fusion of
//!   [`super::multiview`] ranks observations by;
//! * the normal of a pixel can be overridden ([`ReconHooks::normals`]) to
//!   measure how much of the error comes from estimated normals.
//!
//! Without hooks [`frame_to_pixel_splats`] returns exactly the splats of
//! [`crate::reconstruct::frame_to_splats`]; a unit test pins that down, so a
//! change of the original shows up as a test failure here (re-sync the copy).

use glam::{Mat3, Quat, Vec3};

use crate::reconstruct::{ReconstructParams, MASK_CHANGED, MASK_FOREGROUND, MASK_SKIP};
use crate::types::{Camera, RgbdFrame, Splat};

// ---------------------------------------------------------------------------
// Verbatim copy of the private per-pixel geometry of `reconstruct.rs`
// (serial: the lab parallelises over cameras, not over rows).
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
        AxisFit { grad, sides, open: [open(n0, -1), open(n1, 1)], steep }
    }

    /// The surface at pixel `(x, y)`, or `None` for an invalid or unsupported
    /// pixel.
    #[inline]
    ///
    /// `normal`, when given, is the camera-space surface normal to use
    /// instead of the one estimated from the depth map (the only change to
    /// the copied code). Which pixels are supported, steep or on a
    /// silhouette is still decided from the depth map alone.
    fn surface(&self, x: usize, y: usize, normal: Option<Vec3>) -> Option<PixelSurface> {
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
        let (mut a, mut b) = (fu.grad, fv.grad);
        let dir = cam.pixel_dir_cam(x as f32 + 0.5, y as f32 + 0.5);
        let p = dir * z;
        if let Some(n) = normal {
            // On the plane `n·P = d` the inverse depth is `n·dir / d`.
            let d = n.dot(p);
            if d.abs() > 1.0e-9 {
                a = n.x / (cam.fx * d);
                b = -n.y / (cam.fy * d);
            }
        }
        let pu = (Vec3::new(1.0 / cam.fx, 0.0, 0.0) - dir * (a * z)) * z;
        let pv = (Vec3::new(0.0, -1.0 / cam.fy, 0.0) - dir * (b * z)) * z;
        // `v` grows downward, so `pv × pu` (not `pu × pv`) faces the camera.
        let n = pv.cross(pu).normalize_or(Vec3::Z);
        let cos = -n.dot(p) / p.length().max(1e-12);
        if !(cos > 0.0 && p.is_finite()) {
            return None;
        }
        Some(PixelSurface { p, pu, pv, n, cos, z, open: [fu.open[0], fu.open[1], fv.open[0], fv.open[1]] })
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
    // The long axis is cut down to the allowed ratio (never the short one
    // widened: that would blur across the surface). The floor on the short
    // axis only guards degenerate tangents.
    let s1 = l1.sqrt();
    let s2 = l2.sqrt().max(s1 * 0.05);
    let s1 = s1.min(s2 * max_anisotropy.max(1.0));
    let rot = Quat::from_mat3(&Mat3::from_cols(major, minor, n));
    Splat { pos: pos.to_array(), scale: [s1, s2, (s2 * thickness).max(1.0e-5)], rot: rot.to_array(), rgba }
}

#[inline]
fn opacity_byte(opacity: f32) -> u8 {
    (opacity.clamp(0.0, 1.0) * 255.0).round() as u8
}

// ---------------------------------------------------------------------------
// Lab additions
// ---------------------------------------------------------------------------

/// A splat and the observation it was made from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PixelSplat {
    pub splat: Splat,
    /// Row-major index of the source pixel.
    pub pixel: u32,
    /// Forward depth of the surface point in the source camera, meters.
    pub depth: f32,
    /// Cosine between the surface normal and the direction to the camera.
    pub cos: f32,
    /// World-space unit normal, facing the source camera.
    pub normal: Vec3,
}

/// Optional inputs of [`frame_to_pixel_splats`].
#[derive(Clone, Copy, Default)]
pub struct ReconHooks<'a> {
    /// Per-pixel class as for [`crate::reconstruct::frame_to_splats`].
    pub mask: Option<&'a [u8]>,
    /// World-space surface normal per pixel, replacing the estimated one
    /// (an oracle: the simulator knows it). A zero vector keeps the estimate.
    pub normals: Option<&'a [Vec3]>,
}

/// The splats of [`crate::reconstruct::frame_to_splats`] (same order, same
/// values when no normal override is given), each with its provenance.
///
/// A frame that does not match the camera, or a hook slice of the wrong
/// length, produces nothing.
pub fn frame_to_pixel_splats(
    frame: &RgbdFrame,
    cam: &Camera,
    params: &ReconstructParams,
    hooks: &ReconHooks,
) -> Vec<PixelSplat> {
    let n = frame.width as usize * frame.height as usize;
    let fits = |len: Option<usize>| len.is_none_or(|l| l == n);
    if n == 0
        || frame.width != cam.width
        || frame.height != cam.height
        || frame.depth.len() < n
        || frame.rgb.len() < n * 3
        || !fits(hooks.mask.map(<[u8]>::len))
        || !fits(hooks.normals.map(<[Vec3]>::len))
    {
        return Vec::new();
    }
    let geo = FrameGeometry::new(frame, cam, params, params.min_cos);
    let rotation = cam.world_from_cam.matrix3;
    let cam_from_world = Mat3::from(rotation).transpose();
    let alpha = opacity_byte(params.opacity);
    let k = params.footprint;
    let mut out = Vec::new();
    for y in 0..geo.h {
        for x in 0..geo.w {
            let i = y * geo.w + x;
            let class = hooks.mask.map_or(MASK_FOREGROUND, |m| m[i]);
            if class == MASK_SKIP {
                continue;
            }
            let normal = hooks.normals.map(|m| m[i]).filter(|v| v.length_squared() > 0.25).map(|v| cam_from_world * v);
            let Some(s) = geo.surface(x, y, normal) else { continue };
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
            let normal_world = rotation * s.n;
            let splat = tangent_splat(
                cam.world_from_cam.transform_point3(p),
                rotation * (s.pu * ku),
                rotation * (s.pv * kv),
                normal_world,
                params.thickness,
                params.max_anisotropy,
                [rgb[0], rgb[1], rgb[2], alpha],
            );
            if splat.pos.iter().chain(&splat.scale).chain(&splat.rot).all(|v| v.is_finite()) {
                out.push(PixelSplat { splat, pixel: i as u32, depth: s.z, cos: s.cos, normal: normal_world });
            }
        }
    }
    out
}

/// The bare splats of a list of [`PixelSplat`]s.
pub fn splats_of(list: &[PixelSplat]) -> Vec<Splat> {
    list.iter().map(|p| p.splat).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::{render_rgbd, CaptureOptions};
    use crate::reconstruct::{foreground_mask, frame_to_splats, BackgroundPlate};
    use crate::scene::{make_scene, SceneKind};

    fn stage_camera() -> Camera {
        Camera::look_at(Vec3::new(1.2, 1.6, 3.0), Vec3::new(0.0, 1.0, 0.0), Vec3::Y, 0.9, 96, 72)
    }

    #[test]
    fn the_copy_reproduces_frame_to_splats_exactly() {
        // If this fails, `reconstruct.rs` has changed: re-sync the copied
        // geometry above (the lab's baseline must be the product's).
        let scene = make_scene(SceneKind::Stage);
        let cam = stage_camera();
        let opts = CaptureOptions::default();
        let empty = render_rgbd(&scene.snapshot_static(), &cam, &opts);
        let live = render_rgbd(&scene.snapshot(1.3), &cam, &opts);
        let params = ReconstructParams::default();
        let mask = foreground_mask(&live, &BackgroundPlate::from_frame(&empty), &params);
        for mask in [None, Some(mask.as_slice())] {
            let mut expected = Vec::new();
            frame_to_splats(&live, &cam, &params, mask, &mut expected);
            let got = frame_to_pixel_splats(&live, &cam, &params, &ReconHooks { mask, normals: None });
            assert!(expected.len() > 300, "{} splats", expected.len());
            assert_eq!(splats_of(&got), expected);
        }
    }

    #[test]
    fn provenance_matches_the_splat() {
        let scene = make_scene(SceneKind::Stage);
        let cam = stage_camera();
        let frame = render_rgbd(&scene.snapshot(0.4), &cam, &CaptureOptions::default());
        let list = frame_to_pixel_splats(&frame, &cam, &ReconstructParams::default(), &ReconHooks::default());
        for p in &list {
            let (px, py, z) = cam.project(p.splat.position()).unwrap();
            assert_eq!(py as u32 * cam.width + px as u32, p.pixel);
            assert!((z - p.depth).abs() < 1e-3 * z);
            let to_cam = (cam.position() - p.splat.position()).normalize();
            assert!((p.normal.dot(to_cam) - p.cos).abs() < 1e-3);
            assert!((p.normal - p.splat.rotation() * Vec3::Z).length() < 1e-3);
        }
    }

    #[test]
    fn exact_normals_replace_the_estimate() {
        // A slanted plane: the estimate is already exact, so overriding with
        // the true normal changes nothing, and with a wrong one tilts it.
        let cam = Camera::look_at(Vec3::new(0.5, 1.4, 3.0), Vec3::new(0.0, 0.8, 0.0), Vec3::Y, 0.9, 48, 36);
        let (point, normal) = (Vec3::new(0.0, 0.7, 0.0), Vec3::new(0.3, 0.5, 0.8).normalize());
        let mut frame = RgbdFrame::new(cam.width, cam.height);
        for y in 0..cam.height {
            for x in 0..cam.width {
                let dir = cam.world_from_cam.transform_vector3(cam.pixel_dir_cam(x as f32 + 0.5, y as f32 + 0.5));
                let i = frame.index(x, y);
                frame.depth[i] = normal.dot(point - cam.position()) / normal.dot(dir);
                frame.rgb[i * 3..i * 3 + 3].copy_from_slice(&[90, 120, 30]);
            }
        }
        let params = ReconstructParams { smooth_passes: 0, ..Default::default() };
        let n = (cam.width * cam.height) as usize;
        let plain = frame_to_pixel_splats(&frame, &cam, &params, &ReconHooks::default());
        let truth = vec![normal; n];
        let same = frame_to_pixel_splats(&frame, &cam, &params, &ReconHooks { mask: None, normals: Some(&truth) });
        assert_eq!(plain.len(), n);
        assert_eq!(same.len(), n);
        for (a, b) in plain.iter().zip(&same) {
            assert!((a.normal - b.normal).length() < 2e-3, "{} vs {}", a.normal, b.normal);
            assert!((a.splat.scale[0] / b.splat.scale[0] - 1.0).abs() < 0.02);
        }
        let other = Vec3::new(-0.2, 0.6, 0.77).normalize();
        let wrong = vec![other; n];
        let tilted = frame_to_pixel_splats(&frame, &cam, &params, &ReconHooks { mask: None, normals: Some(&wrong) });
        assert!(tilted.iter().all(|p| (p.normal - other).length() < 2e-3));
    }
}
