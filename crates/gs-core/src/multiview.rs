//! Multi-view fusion for the camera ring.
//!
//! Every ring camera turns its own depth pixels into splats, so a surface
//! that several cameras see is described several times, by observations of
//! very different quality: the camera that faces the surface from nearby
//! samples it finely, one that sees it from afar at a grazing angle smears
//! its texture along the line of sight. [`fuse`] keeps, per surface point,
//! the observations that describe it best and drops the others. On the
//! stage that is about 45 % of the static layer and a third of the live one:
//! the budget that pays for finer sensors.
//!
//! # Rule
//!
//! The quality of an observation is the number of samples it has per square
//! meter of surface, `q = cos θ · (f / z)²`. A splat is *dominated* by the
//! splat of another camera when
//!
//! * its center projects into the pixel that produced the other splat,
//! * both lie at the same depth of that camera within
//!   [`MultiViewParams::tolerance`],
//! * their normals agree ([`MultiViewParams::min_normal_dot`]: the two faces
//!   of an arm are "the same depth" at three meters), and
//! * the other one is better by the margin: `q' > (1 + margin) · q`.
//!
//! A dominated splat is dropped **only if the splat that dominates it is
//! itself kept**. Dropping whatever some camera sees better opens holes
//! along the lines where two cameras are equally good, each one's splat
//! giving way to a neighbour of the other that gives way to a third. With
//! this rule nothing goes without a kept, better observation of the same
//! surface point within one of that observation's pixels, whatever the
//! margin; the margin only sets how much the cameras overlap along their
//! seams, which is what hides the residual differences between them. The
//! rule is a recursion along strictly increasing quality, so it has exactly
//! one solution and needs no sorting: [`fuse`] resolves it with a memo.
//!
//! Pixels on the rim of a camera's splats (image border, silhouette, mask
//! edge) never dominate ([`MultiViewParams::edge_guard`]): their own
//! footprint is the least certain one.
//!
//! # What the measurements say
//!
//! (`examples/quality_lab.rs` and `examples/quality_report.rs`.) Under the
//! normalised blending mode the fused cloud scores level with the stacked
//! one or slightly above it, with crisper floor texture. Under sorted
//! "over" compositing a fused cloud needs a larger footprint to stay closed
//! (a viewer's splat scale of about 1.45): eight overlapping clouds no longer
//! hide the gaps between the splats of one camera.
//!
//! # Cost
//!
//! One projection per splat and per camera that can see its surface, cut
//! short by two exact bounds (the surface faces away from the camera; the
//! camera is too far away to beat the splat's quality at any angle) before
//! any memory is touched. About 60 ns per candidate on one core for eight
//! cameras; the cameras are processed in parallel with the `par` feature.
//! The result does not depend on the thread count.

use glam::{Mat3A, Vec3A};
#[cfg(feature = "par")]
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::reconstruct::SourcedSplat;
use crate::types::{Camera, Splat};

/// Parameters of the multi-view fusion of the live stage.
///
/// Fields missing from serialized data take their [`Default`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MultiViewParams {
    /// Fuse at all. When off, the splats of all cameras are stacked.
    pub enabled: bool,
    /// A better observation must be better by this fraction to replace
    /// another one. `0` keeps a single camera per surface point; larger
    /// values leave more overlap along the seams between cameras.
    pub margin: f32,
    /// Two observations show the same surface when their depths differ by
    /// less than this fraction.
    pub tolerance: f32,
    /// The normals of two observations of the same surface point make at
    /// least this cosine.
    pub min_normal_dot: f32,
    /// A pixel on the rim of its camera's splats (a 4-neighbour produced no
    /// splat) never replaces another camera's splat.
    pub edge_guard: bool,
}

impl Default for MultiViewParams {
    fn default() -> Self {
        Self { enabled: true, margin: 0.25, tolerance: 0.03, min_normal_dot: 0.3, edge_guard: true }
    }
}

impl MultiViewParams {
    /// The same parameters with every field forced into its safe range;
    /// values that are not finite numbers are replaced by their default.
    pub fn sanitized(self) -> Self {
        let d = Self::default();
        let clamp = |value: f32, fallback: f32, lo: f32, hi: f32| if value.is_finite() { value.clamp(lo, hi) } else { fallback };
        Self {
            enabled: self.enabled,
            margin: clamp(self.margin, d.margin, 0.0, 10.0),
            tolerance: clamp(self.tolerance, d.tolerance, 1.0e-4, 0.5),
            min_normal_dot: clamp(self.min_normal_dot, d.min_normal_dot, -1.0, 1.0),
            edge_guard: self.edge_guard,
        }
    }
}

/// One camera of the ring at one instant: its calibration and the splats it
/// produced, in pixel order (as [`crate::reconstruct::frame_to_sourced_splats`]
/// returns them).
#[derive(Clone, Copy, Debug)]
pub struct View<'a> {
    pub cam: &'a Camera,
    pub splats: &'a [SourcedSplat],
}

/// What [`fuse`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FusionStats {
    /// Candidate splats of all cameras.
    pub input: usize,
    /// Splats in the output.
    pub kept: usize,
}

/// Samples per square meter of surface of an observation: what the best
/// view is chosen by.
#[inline]
pub fn quality(cam: &Camera, splat: &SourcedSplat) -> f32 {
    let scale = cam.fx / splat.depth.max(1.0e-6);
    splat.cos.max(0.0) * scale * scale
}

const NONE: u32 = u32::MAX;

/// What the fusion needs to know about a splat that may dominate another.
#[derive(Clone, Copy)]
struct Cover {
    normal: Vec3A,
    depth: f32,
    quality: f32,
}

/// Per-camera lookup tables.
struct Table {
    /// World to camera: `cam = rotation · (world − position)`.
    rotation: Mat3A,
    position: Vec3A,
    fx: f32,
    fy: f32,
    cx: f32,
    cy: f32,
    width: f32,
    height: f32,
    stride: usize,
    /// Pixel → index of the splat it produced, when that splat may dominate
    /// others; [`NONE`] otherwise.
    slot: Vec<u32>,
    covers: Vec<Cover>,
    /// Index of the camera's first splat in the global numbering.
    first: u32,
}

impl Table {
    fn new(view: &View, params: &MultiViewParams, first: u32) -> Self {
        let cam = view.cam;
        let (w, h) = (cam.width as usize, cam.height as usize);
        let mut emitted = vec![NONE; w * h];
        for (k, s) in view.splats.iter().enumerate() {
            if let Some(slot) = emitted.get_mut(s.pixel as usize) {
                *slot = k as u32;
            }
        }
        let slot = if params.edge_guard {
            let has = |x: usize, y: usize| emitted[y * w + x] != NONE;
            let mut inner = vec![NONE; w * h];
            for y in 1..h.saturating_sub(1) {
                for x in 1..w.saturating_sub(1) {
                    if has(x - 1, y) && has(x + 1, y) && has(x, y - 1) && has(x, y + 1) {
                        inner[y * w + x] = emitted[y * w + x];
                    }
                }
            }
            inner
        } else {
            emitted
        };
        let covers = view
            .splats
            .iter()
            .map(|s| Cover { normal: Vec3A::from(s.normal), depth: s.depth, quality: quality(cam, s) })
            .collect();
        let inverse = cam.world_from_cam.inverse();
        Self {
            rotation: inverse.matrix3,
            position: cam.world_from_cam.translation,
            fx: cam.fx,
            fy: cam.fy,
            cx: cam.cx,
            cy: cam.cy,
            width: w as f32,
            height: h as f32,
            stride: w,
            slot,
            covers,
            first,
        }
    }
}

/// The splats that dominate the splats of one camera: for splat `k`, the
/// global indices `list[end[k − 1]..end[k]]`.
#[derive(Default)]
struct Dominators {
    end: Vec<u32>,
    list: Vec<u32>,
}

/// Finds, for every splat of camera `own`, the splats of the other cameras
/// that dominate it.
fn dominators(tables: &[Table], own: usize, view: &View, params: &MultiViewParams) -> Dominators {
    let threshold = 1.0 + params.margin.max(0.0);
    let tolerance = params.tolerance;
    // A dominating splat lies within the tolerance of the depth `z` at which
    // the other camera sees this point, so its quality is at most
    // `(f / (z · (1 − tolerance)))²`.
    let nearest = (1.0 - tolerance).max(1.0e-3);
    let mut out = Dominators { end: Vec::with_capacity(view.splats.len()), list: Vec::new() };
    for (k, splat) in view.splats.iter().enumerate() {
        let p = Vec3A::from(splat.splat.pos);
        let normal = Vec3A::from(splat.normal);
        let needed = threshold * tables[own].covers[k].quality;
        for (j, table) in tables.iter().enumerate() {
            if j == own {
                continue;
            }
            let offset = p - table.position;
            // The camera is behind the surface: it does not see this point.
            if normal.dot(offset) >= 0.0 {
                continue;
            }
            let c = table.rotation * offset;
            let z = -c.z;
            if z <= 1.0e-6 {
                continue;
            }
            // Too far away to do better at any angle.
            let reach = table.fx / (z * nearest);
            if reach * reach <= needed {
                continue;
            }
            let inv = 1.0 / z;
            let (u, v) = (table.cx + table.fx * c.x * inv, table.cy - table.fy * c.y * inv);
            // Also false for coordinates that are not numbers.
            if !(u >= 0.0 && v >= 0.0 && u < table.width && v < table.height) {
                continue;
            }
            let index = table.slot[v as usize * table.stride + u as usize];
            if index == NONE {
                continue;
            }
            let other = &table.covers[index as usize];
            if other.quality > needed && (other.depth - z).abs() <= tolerance * z && other.normal.dot(normal) >= params.min_normal_dot {
                out.list.push(table.first + index);
            }
        }
        out.end.push(out.list.len() as u32);
    }
    out
}

/// Decides which splats stay: a splat goes when one of its dominators stays.
///
/// `dominated(g)` lists the dominators of splat `g`; all of them have a
/// strictly higher quality than `g`, so the recursion ends.
fn resolve<'a>(total: usize, dominated: impl Fn(usize) -> &'a [u32]) -> Vec<bool> {
    const UNKNOWN: u8 = 0;
    const KEPT: u8 = 1;
    const DROPPED: u8 = 2;
    let mut state = vec![UNKNOWN; total];
    let mut stack: Vec<u32> = Vec::new();
    for start in 0..total {
        if state[start] != UNKNOWN {
            continue;
        }
        stack.push(start as u32);
        while let Some(&g) = stack.last() {
            let g = g as usize;
            let mut verdict = KEPT;
            for &c in dominated(g) {
                match state[c as usize] {
                    KEPT => {
                        verdict = DROPPED;
                        break;
                    }
                    UNKNOWN => {
                        // Decide the dominator first, then come back.
                        verdict = UNKNOWN;
                        stack.push(c);
                        break;
                    }
                    _ => {}
                }
            }
            if verdict != UNKNOWN {
                state[g] = verdict;
                stack.pop();
            }
        }
    }
    state.into_iter().map(|s| s == KEPT).collect()
}

/// Fuses the splats of a camera ring into `out` (cleared first): the kept
/// splats in camera order, each camera's in pixel order. See the module
/// documentation.
///
/// With [`MultiViewParams::enabled`] off, or a single view, this is the
/// plain concatenation. A view whose splats do not belong to its camera's
/// image (a pixel index out of range) neither dominates nor crashes.
pub fn fuse(views: &[View], params: &MultiViewParams, out: &mut Vec<Splat>) -> FusionStats {
    out.clear();
    let total: usize = views.iter().map(|v| v.splats.len()).sum();
    out.reserve(total);
    if !params.enabled || views.len() < 2 || total >= NONE as usize {
        out.extend(views.iter().flat_map(|v| v.splats.iter().map(|s| s.splat)));
        return FusionStats { input: total, kept: total };
    }
    let mut firsts = Vec::with_capacity(views.len());
    let mut first = 0u32;
    for view in views {
        firsts.push(first);
        first += view.splats.len() as u32;
    }
    let table = |i: usize| Table::new(&views[i], params, firsts[i]);
    #[cfg(feature = "par")]
    let tables: Vec<Table> = (0..views.len()).into_par_iter().map(table).collect();
    #[cfg(not(feature = "par"))]
    let tables: Vec<Table> = (0..views.len()).map(table).collect();

    let find = |i: usize| dominators(&tables, i, &views[i], params);
    #[cfg(feature = "par")]
    let found: Vec<Dominators> = (0..views.len()).into_par_iter().map(find).collect();
    #[cfg(not(feature = "par"))]
    let found: Vec<Dominators> = (0..views.len()).map(find).collect();

    // Global index → camera, by the camera's first index.
    let mut owner = Vec::with_capacity(total);
    for (i, view) in views.iter().enumerate() {
        owner.resize(owner.len() + view.splats.len(), i as u32);
    }
    let kept = resolve(total, |g| {
        let i = owner[g] as usize;
        let k = g - firsts[i] as usize;
        let list = &found[i];
        let start = if k == 0 { 0 } else { list.end[k - 1] as usize };
        &list.list[start..list.end[k] as usize]
    });
    let mut g = 0;
    for view in views {
        for s in view.splats {
            if kept[g] {
                out.push(s.splat);
            }
            g += 1;
        }
    }
    FusionStats { input: total, kept: out.len() }
}

#[cfg(test)]
mod tests {
    use glam::Vec3;

    use super::*;
    use crate::reconstruct::{frame_to_sourced_splats, ReconstructParams};
    use crate::types::RgbdFrame;

    /// Renders the plane `y = 0` (a floor, where `floor` says so) and an
    /// optional sphere into a frame.
    fn shoot(cam: &Camera, floor: impl Fn(Vec3) -> bool, ball: Option<(Vec3, f32)>) -> RgbdFrame {
        let mut frame = RgbdFrame::new(cam.width, cam.height);
        let forward = cam.forward();
        for y in 0..cam.height {
            for x in 0..cam.width {
                let (origin, dir) = cam.ray(x as f32 + 0.5, y as f32 + 0.5);
                let mut nearest = f32::INFINITY;
                if dir.y < -1e-6 {
                    let t = -origin.y / dir.y;
                    if floor(origin + dir * t) {
                        nearest = t;
                    }
                }
                if let Some((center, radius)) = ball {
                    let oc = origin - center;
                    let b = oc.dot(dir);
                    let disc = b * b - (oc.length_squared() - radius * radius);
                    if disc > 0.0 && -b - disc.sqrt() > 0.0 {
                        nearest = nearest.min(-b - disc.sqrt());
                    }
                }
                if nearest.is_finite() {
                    let i = frame.index(x, y);
                    frame.depth[i] = nearest * dir.dot(forward);
                    frame.rgb[i * 3..i * 3 + 3].copy_from_slice(&[150, 150, 150]);
                }
            }
        }
        frame
    }

    fn camera(eye: Vec3) -> Camera {
        Camera::look_at(eye, Vec3::ZERO, Vec3::Y, 0.8, 80, 60)
    }

    fn candidates(cams: &[Camera], frames: &[RgbdFrame]) -> Vec<Vec<SourcedSplat>> {
        let params = ReconstructParams::default();
        cams.iter()
            .zip(frames)
            .map(|(cam, frame)| {
                let mut out = Vec::new();
                frame_to_sourced_splats(frame, cam, &params, None, &mut out);
                out
            })
            .collect()
    }

    fn views<'a>(cams: &'a [Camera], splats: &'a [Vec<SourcedSplat>]) -> Vec<View<'a>> {
        cams.iter().zip(splats).map(|(cam, splats)| View { cam, splats }).collect()
    }

    /// Fuses and returns the kept splats per camera.
    fn fused(cams: &[Camera], splats: &[Vec<SourcedSplat>], params: &MultiViewParams) -> (Vec<Vec<Splat>>, FusionStats) {
        let mut out = Vec::new();
        let stats = fuse(&views(cams, splats), params, &mut out);
        assert_eq!(stats.kept, out.len());
        // The output is a subsequence of the concatenated input.
        let mut per_view = Vec::new();
        let mut rest = out.as_slice();
        for list in splats {
            let mut kept = Vec::new();
            for s in list {
                if rest.first() == Some(&s.splat) {
                    kept.push(s.splat);
                    rest = &rest[1..];
                }
            }
            per_view.push(kept);
        }
        assert!(rest.is_empty(), "the output is not in input order");
        (per_view, stats)
    }

    const GREEDY: MultiViewParams = MultiViewParams { enabled: true, margin: 0.0, tolerance: 0.03, min_normal_dot: 0.3, edge_guard: true };

    #[test]
    fn the_frontal_camera_wins_and_nothing_else_changes() {
        // One camera straight above a floor patch, one at a grazing angle.
        let cams = [camera(Vec3::new(0.0, 3.0, 0.01)), camera(Vec3::new(0.0, 0.9, 3.0))];
        let frames: Vec<RgbdFrame> = cams.iter().map(|c| shoot(c, |p| p.length() < 0.8, None)).collect();
        let splats = candidates(&cams, &frames);
        assert!(splats[0].len() > 300 && splats[1].len() > 100, "{} {}", splats[0].len(), splats[1].len());
        let (per_view, stats) = fused(&cams, &splats, &GREEDY);
        assert_eq!(stats.input, splats[0].len() + splats[1].len());
        // Nothing of the frontal view is lost; of the grazing view only the
        // rim survives (where the frontal view's pixels are guarded).
        assert_eq!(per_view[0].len(), splats[0].len());
        assert!(per_view[1].len() < splats[1].len() / 2, "{}", per_view[1].len());
        // A large margin, or the switch, keeps everything.
        for keep_all in [MultiViewParams { margin: 10.0, ..GREEDY }, MultiViewParams { enabled: false, ..GREEDY }] {
            let (_, stats) = fused(&cams, &splats, &keep_all);
            assert_eq!(stats.kept, stats.input);
        }
        // A single camera has nothing to be fused with.
        let (_, stats) = fused(&cams[..1], &splats[..1], &GREEDY);
        assert_eq!(stats.kept, splats[0].len());
    }

    #[test]
    fn the_two_faces_of_a_thin_part_do_not_eliminate_each_other() {
        // A small ball seen from opposite sides: within the depth tolerance
        // of 3 % (8 to 9 cm at these distances) the far side of a 6 cm ball
        // is "the same depth", but its normal faces the other way.
        let fine = |eye: Vec3| Camera::look_at(eye, Vec3::new(0.0, 0.3, 0.0), Vec3::Y, 0.8, 160, 120);
        let cams = [fine(Vec3::new(0.0, 0.3, 3.0)), fine(Vec3::new(0.0, 0.3, -2.6))];
        let ball = Some((Vec3::new(0.0, 0.3, 0.0), 0.03));
        let frames: Vec<RgbdFrame> = cams.iter().map(|c| shoot(c, |_| false, ball)).collect();
        let splats = candidates(&cams, &frames);
        assert!(splats[0].len() >= 4, "{}", splats[0].len());
        let params = MultiViewParams { edge_guard: false, ..GREEDY };
        let (_, stats) = fused(&cams, &splats, &params);
        assert_eq!(stats.kept, stats.input);
    }

    #[test]
    fn nothing_is_dropped_without_a_kept_better_observation_nearby() {
        // Three cameras around a floor: with a zero margin every camera gives
        // way to another one somewhere, and every dropped splat must still
        // lie within reach of a kept splat of another camera.
        let cams = [camera(Vec3::new(0.0, 2.0, 2.5)), camera(Vec3::new(2.2, 2.0, -1.2)), camera(Vec3::new(-2.2, 2.0, -1.2))];
        let frames: Vec<RgbdFrame> = cams.iter().map(|c| shoot(c, |p| p.length() < 1.2, None)).collect();
        let splats = candidates(&cams, &frames);
        let (per_view, stats) = fused(&cams, &splats, &GREEDY);
        assert!(stats.kept < stats.input * 2 / 3, "{stats:?}");
        let kept: Vec<Vec3> = per_view.iter().flatten().map(|s| s.position()).collect();
        // Pixel footprint on the floor is about 3 m / 71 px ≈ 4 cm.
        for list in &splats {
            for s in list {
                let p = s.splat.position();
                let nearest = kept.iter().map(|k| (*k - p).length()).fold(f32::INFINITY, f32::min);
                assert!(nearest < 0.07, "a hole of {nearest} m at {p}");
            }
        }
        // More margin, more overlap.
        let (_, wide) = fused(&cams, &splats, &MultiViewParams { margin: 0.5, ..GREEDY });
        assert!(wide.kept > stats.kept && wide.kept < stats.input, "{wide:?} against {stats:?}");
    }

    #[test]
    fn the_rule_is_resolved_like_a_best_first_sweep() {
        // The reference formulation: visit the splats from the best to the
        // worst and drop one when a kept splat dominates it.
        let cams = [camera(Vec3::new(0.0, 2.0, 2.5)), camera(Vec3::new(2.2, 1.2, -1.2)), camera(Vec3::new(-2.2, 2.6, -1.2))];
        let ball = Some((Vec3::new(0.2, 0.4, 0.1), 0.4));
        let frames: Vec<RgbdFrame> = cams.iter().map(|c| shoot(c, |p| p.length() < 1.2, ball)).collect();
        let splats = candidates(&cams, &frames);
        for params in [GREEDY, MultiViewParams::default(), MultiViewParams { edge_guard: false, ..GREEDY }] {
            let all = views(&cams, &splats);
            let tables: Vec<Table> = {
                let mut first = 0;
                all.iter()
                    .map(|v| {
                        let t = Table::new(v, &params, first);
                        first += v.splats.len() as u32;
                        t
                    })
                    .collect()
            };
            let lists: Vec<Dominators> = (0..all.len()).map(|i| dominators(&tables, i, &all[i], &params)).collect();
            let quality: Vec<f32> = tables.iter().flat_map(|t| t.covers.iter().map(|c| c.quality)).collect();
            let mut dominated: Vec<Vec<u32>> = Vec::new();
            for list in &lists {
                let mut start = 0;
                for &end in &list.end {
                    dominated.push(list.list[start..end as usize].to_vec());
                    start = end as usize;
                }
            }
            // Every dominator is strictly better.
            for (g, list) in dominated.iter().enumerate() {
                assert!(list.iter().all(|&c| quality[c as usize] > quality[g]));
            }
            let mut order: Vec<usize> = (0..quality.len()).collect();
            order.sort_by(|&a, &b| quality[b].total_cmp(&quality[a]).then(a.cmp(&b)));
            let mut expected = vec![true; quality.len()];
            for &g in &order {
                expected[g] = !dominated[g].iter().any(|&c| expected[c as usize]);
            }
            let mut out = Vec::new();
            let stats = fuse(&all, &params, &mut out);
            let reference: Vec<Splat> =
                splats.iter().flatten().zip(&expected).filter(|(_, keep)| **keep).map(|(s, _)| s.splat).collect();
            assert_eq!(out, reference, "{params:?}");
            assert!(stats.kept < stats.input);
        }
    }

    #[test]
    fn hostile_input_is_survived() {
        let cams = [camera(Vec3::new(0.0, 3.0, 0.01)), camera(Vec3::new(0.0, 0.9, 3.0))];
        let frames: Vec<RgbdFrame> = cams.iter().map(|c| shoot(c, |p| p.length() < 0.8, None)).collect();
        let mut splats = candidates(&cams, &frames);
        // Pixels outside the image, positions and depths that are not numbers.
        splats[0][3].pixel = u32::MAX;
        splats[0][4].splat.pos = [f32::NAN; 3];
        splats[1][5].depth = f32::NAN;
        splats[1][6].depth = 0.0;
        splats[1][7].normal = Vec3::splat(f32::INFINITY);
        let mut out = Vec::new();
        let stats = fuse(&views(&cams, &splats), &GREEDY, &mut out);
        assert!(stats.kept > 0 && stats.kept <= stats.input && stats.kept == out.len());
        let empty: [Vec<SourcedSplat>; 2] = [Vec::new(), Vec::new()];
        assert_eq!(fused(&cams, &empty, &GREEDY).1, FusionStats::default());
        assert_eq!(fuse(&[], &GREEDY, &mut out), FusionStats::default());
        assert!(out.is_empty());
        // Hostile parameters are tamed.
        let wild = MultiViewParams { enabled: true, margin: f32::NAN, tolerance: -3.0, min_normal_dot: f32::INFINITY, edge_guard: false };
        let tame = wild.sanitized();
        assert_eq!((tame.margin, tame.tolerance, tame.min_normal_dot), (0.25, 1.0e-4, 0.3));
        assert_eq!(tame.sanitized(), tame);
        assert_eq!(serde_json::from_str::<MultiViewParams>("{}").unwrap(), MultiViewParams::default());
    }
}
