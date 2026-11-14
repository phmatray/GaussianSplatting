//! Multi-view fusion for the camera ring (recommendation A1 of
//! `docs/RECHERCHE_QUALITE.md`).
//!
//! The product concatenates the splats of all ring cameras. Here every
//! candidate splat is reprojected into the other cameras and two things are
//! decided from what those cameras measured:
//!
//! 1. **Carving.** If another camera that faces the splat sees clearly
//!    farther along the ray through it, and no camera confirms a surface
//!    there, the splat floats in free space (a flying pixel, an outlier) and
//!    is dropped.
//! 2. **Best view.** Where several cameras see the same surface point, the
//!    observation with the most samples per square meter of surface,
//!    `q = cos θ · (f / z)²`, describes it; the others are dropped (or, for a
//!    normalised renderer, kept with a reduced weight).
//!
//! # Seams
//!
//! Dropping "whatever another camera sees better" ([`Selection::Dominated`],
//! the rule as written in the report) can open holes along the line where
//! two cameras are equally good: each camera's splat is dropped in favour of
//! the other camera's neighbouring pixel, which is itself dropped in favour
//! of a third. The report's answer is a margin (`q_j > (1 + m)·q_i`), which
//! keeps both cameras in a band around the seam; the band is wide, because
//! `q` varies slowly along a surface.
//!
//! [`Selection::Greedy`] removes the cause instead: splats are visited from
//! the best to the worst, and a splat is only dropped when the better splat
//! that covers it *has itself been kept*. Nothing is ever dropped without a
//! kept, better observation of the same surface point at less than one of
//! that observation's pixels, so no margin is needed for closure; the margin
//! then only controls how much overlap is left to hide residual calibration
//! and exposure differences between real cameras.
//!
//! The work is one projection per splat and per other camera: about 60 000
//! projections per tick for the live layer of the default stage.

use glam::{Affine3A, Vec3};
#[cfg(feature = "par")]
use rayon::prelude::*;

use super::recon::PixelSplat;
use crate::types::{Camera, Splat};

/// One camera of the ring at one instant.
#[derive(Clone, Copy)]
pub struct FusionView<'a> {
    pub cam: &'a Camera,
    /// The depth map the camera measured (`0.0` = no return), `width·height`
    /// values. For the live layer this is the whole live frame, not only its
    /// foreground: the background also tells what is free space.
    pub depth: &'a [f32],
    /// The candidate splats of this camera.
    pub splats: &'a [PixelSplat],
}

/// How the best observation of a surface point is chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Selection {
    /// Drop a splat when another camera has a splat on the same surface point
    /// that is better by the margin, whether or not that one survives.
    Dominated,
    /// Best first; drop a splat only when a *kept* better splat covers it.
    Greedy,
}

/// Parameters of [`fuse`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FusionParams {
    /// Two depths are the same surface when they differ by less than this
    /// fraction; a camera "sees through" a point when it measures more than
    /// this fraction farther.
    pub tolerance: f32,
    /// Drop splats in free space (see the module documentation).
    pub carve: bool,
    /// Best-view selection; `None` keeps every observation.
    pub select: Option<Selection>,
    /// A better observation must be better by this fraction (`m`).
    pub margin: f32,
    /// The normals of two observations of the same surface point must make
    /// at least this cosine. Keeps the two faces of thin parts (an arm seen
    /// from the front and from behind is one depth within the tolerance)
    /// from eliminating each other.
    pub min_normal_dot: f32,
    /// A pixel on the rim of its camera's splats (a 4-neighbour produced no
    /// splat: image border, silhouette, mask edge) never eliminates another
    /// camera's splat: its own footprint is the least certain.
    pub edge_guard: bool,
    /// Instead of dropping a dominated splat, multiply its opacity by
    /// `(q / q_best)^soft` (for a renderer that uses opacity as a blending
    /// weight). `None` drops.
    pub soft: Option<f32>,
}

impl Default for FusionParams {
    fn default() -> Self {
        Self {
            tolerance: 0.03,
            carve: true,
            select: Some(Selection::Greedy),
            margin: 0.0,
            min_normal_dot: 0.3,
            edge_guard: true,
            soft: None,
        }
    }
}

/// What [`fuse`] did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FusionStats {
    /// Candidate splats of all cameras.
    pub input: usize,
    /// Dropped because they float in free space.
    pub carved: usize,
    /// Dropped (or down-weighted) because another camera sees the surface
    /// better.
    pub dominated: usize,
    /// Splats in the output.
    pub kept: usize,
}

/// Result of [`fuse`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Fused {
    /// The surviving splats of every camera, in their original order.
    pub per_view: Vec<Vec<Splat>>,
    pub stats: FusionStats,
}

impl Fused {
    /// All surviving splats in camera order.
    pub fn concat(&self) -> Vec<Splat> {
        self.per_view.iter().flatten().copied().collect()
    }
}

/// Samples per square meter of surface of an observation: the quality the
/// best view is chosen by.
#[inline]
pub fn quality(cam: &Camera, splat: &PixelSplat) -> f32 {
    let scale = cam.fx / splat.depth.max(1.0e-6);
    splat.cos.max(0.0) * scale * scale
}

const NONE: u32 = u32::MAX;

/// Per-camera lookup tables.
struct Table<'a> {
    view: FusionView<'a>,
    cam_from_world: Affine3A,
    position: Vec3,
    /// Pixel → index of the splat it produced, or [`NONE`].
    emitted: Vec<u32>,
    /// Pixel → false when on the rim of the emitted pixels.
    inner: Vec<bool>,
    /// Index of the camera's first splat in the global numbering.
    first: usize,
}

impl Table<'_> {
    /// `(u, v, depth)` of a world point, when in front of the camera.
    #[inline]
    fn project(&self, p: Vec3) -> Option<(f32, f32, f32)> {
        let c = self.cam_from_world.transform_point3(p);
        let depth = -c.z;
        let cam = self.view.cam;
        (depth > 1.0e-6).then(|| (cam.cx + cam.fx * c.x / depth, cam.cy - cam.fy * c.y / depth, depth))
    }
}

/// What the other cameras say about one splat.
#[derive(Default)]
struct Evidence {
    /// A facing camera sees farther through the splat.
    seen_through: bool,
    /// Some camera measured a surface at the splat.
    supported: bool,
    /// Global indices of other cameras' splats on the same surface point.
    covers: Vec<u32>,
}

fn gather(tables: &[Table], own: usize, splat: &PixelSplat, params: &FusionParams) -> Evidence {
    let mut evidence = Evidence::default();
    let p = splat.splat.position();
    let tol = params.tolerance;
    for (j, table) in tables.iter().enumerate() {
        if j == own || table.view.depth.is_empty() {
            continue;
        }
        let Some((u, v, z)) = table.project(p) else { continue };
        let cam = table.view.cam;
        let (w, h) = (cam.width as i64, cam.height as i64);
        // The four pixel centers around the projection.
        let (x0, y0) = ((u - 0.5).floor() as i64, (v - 0.5).floor() as i64);
        let (mut valid, mut farther, mut same) = (0, 0, 0);
        for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
            let (x, y) = (x0 + dx, y0 + dy);
            if x < 0 || y < 0 || x >= w || y >= h {
                continue;
            }
            let d = table.view.depth[(y * w + x) as usize];
            if d > 0.0 {
                valid += 1;
                if (d - z).abs() <= tol * z {
                    same += 1;
                } else if d > z {
                    farther += 1;
                }
            }
        }
        evidence.supported |= same > 0;
        // All four rays pass the point and go on: it is in free space,
        // unless its surface faces away from this camera (then the camera
        // may legitimately look past the back of a thin object's rim).
        if valid == 4 && farther == 4 && splat.normal.dot(table.position - p) > 0.0 {
            evidence.seen_through = true;
        }
        // The other camera's splat of the pixel that contains the projection.
        let (x, y) = (u.floor() as i64, v.floor() as i64);
        if x < 0 || y < 0 || x >= w || y >= h {
            continue;
        }
        let pixel = (y * w + x) as usize;
        let index = table.emitted[pixel];
        if index == NONE || (params.edge_guard && !table.inner[pixel]) {
            continue;
        }
        let other = &table.view.splats[index as usize];
        if (other.depth - z).abs() <= tol * z && other.normal.dot(splat.normal) >= params.min_normal_dot {
            evidence.covers.push((table.first + index as usize) as u32);
        }
    }
    evidence
}

/// Fuses the candidate splats of a camera ring; see the module documentation.
///
/// A view whose depth map does not have `width·height` values gives no
/// evidence about the splats of the others.
pub fn fuse(views: &[FusionView], params: &FusionParams) -> Fused {
    let mut first = 0;
    let tables: Vec<Table> = views
        .iter()
        .map(|view| {
            let (w, h) = (view.cam.width as usize, view.cam.height as usize);
            let usable = view.depth.len() == w * h;
            let mut emitted = vec![NONE; w * h];
            if usable {
                for (k, s) in view.splats.iter().enumerate() {
                    if let Some(slot) = emitted.get_mut(s.pixel as usize) {
                        *slot = k as u32;
                    }
                }
            }
            let has = |x: usize, y: usize| emitted[y * w + x] != NONE;
            let inner = (0..w * h)
                .map(|i| {
                    let (x, y) = (i % w, i / w);
                    x > 0 && y > 0 && x + 1 < w && y + 1 < h && has(x - 1, y) && has(x + 1, y) && has(x, y - 1) && has(x, y + 1)
                })
                .collect();
            let table = Table {
                view: FusionView { depth: if usable { view.depth } else { &[] }, ..*view },
                cam_from_world: view.cam.world_from_cam.inverse(),
                position: view.cam.position(),
                emitted,
                inner,
                first,
            };
            first += view.splats.len();
            table
        })
        .collect();
    let total = first;

    // Flat list of (camera, splat) in global order.
    let owners: Vec<(u32, u32)> =
        views.iter().enumerate().flat_map(|(i, v)| (0..v.splats.len()).map(move |k| (i as u32, k as u32))).collect();
    let splat_of = |g: usize| &views[owners[g].0 as usize].splats[owners[g].1 as usize];
    let look = |g: usize| gather(&tables, owners[g].0 as usize, splat_of(g), params);
    #[cfg(feature = "par")]
    let evidence: Vec<Evidence> = (0..total).into_par_iter().map(look).collect();
    #[cfg(not(feature = "par"))]
    let evidence: Vec<Evidence> = (0..total).map(look).collect();

    let q: Vec<f32> = (0..total).map(|g| quality(views[owners[g].0 as usize].cam, splat_of(g))).collect();
    let mut stats = FusionStats { input: total, ..FusionStats::default() };
    // Opacity factor per splat; 0 = dropped.
    let mut weight = vec![1.0f32; total];
    if params.carve {
        for g in 0..total {
            if evidence[g].seen_through && !evidence[g].supported {
                weight[g] = 0.0;
                stats.carved += 1;
            }
        }
    }
    if let Some(selection) = params.select {
        let threshold = 1.0 + params.margin.max(0.0);
        // Best first; ties broken by the global index, so that two equal
        // observations never eliminate each other.
        let better = |a: usize, b: usize| q[a] > q[b] || (q[a] == q[b] && a < b);
        let mut order: Vec<u32> = (0..total as u32).filter(|&g| weight[g as usize] > 0.0).collect();
        order.sort_unstable_by(|&a, &b| q[b as usize].total_cmp(&q[a as usize]).then(a.cmp(&b)));
        let carved = weight.clone();
        for &g in &order {
            let g = g as usize;
            let mut best = 0.0f32;
            for &c in &evidence[g].covers {
                let c = c as usize;
                let alive = match selection {
                    Selection::Dominated => carved[c] > 0.0,
                    // Decided already: `c` comes before `g` in the order.
                    Selection::Greedy => weight[c] >= 1.0,
                };
                if alive && better(c, g) && q[c] > threshold * q[g] {
                    best = best.max(q[c]);
                }
            }
            if best > 0.0 {
                stats.dominated += 1;
                weight[g] = match params.soft {
                    Some(power) => (q[g] / best).max(0.0).powf(power).min(0.999),
                    None => 0.0,
                };
            }
        }
    }

    let mut per_view: Vec<Vec<Splat>> = views.iter().map(|v| Vec::with_capacity(v.splats.len())).collect();
    for g in 0..total {
        let mut splat = splat_of(g).splat;
        if weight[g] < 1.0 {
            let alpha = (splat.rgba[3] as f32 * weight[g]).round();
            if alpha < 1.0 {
                continue;
            }
            splat.rgba[3] = alpha as u8;
        }
        per_view[owners[g].0 as usize].push(splat);
    }
    stats.kept = per_view.iter().map(Vec::len).sum();
    Fused { per_view, stats }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::experimental::recon::{frame_to_pixel_splats, ReconHooks};
    use crate::reconstruct::ReconstructParams;
    use crate::types::RgbdFrame;

    /// Renders the plane `y = 0` (a floor, where `pattern` says so) and an
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

    fn candidates(cams: &[Camera], frames: &[RgbdFrame]) -> Vec<Vec<PixelSplat>> {
        let params = ReconstructParams::default();
        cams.iter().zip(frames).map(|(c, f)| frame_to_pixel_splats(f, c, &params, &ReconHooks::default())).collect()
    }

    fn views<'a>(cams: &'a [Camera], frames: &'a [RgbdFrame], splats: &'a [Vec<PixelSplat>]) -> Vec<FusionView<'a>> {
        (0..cams.len()).map(|i| FusionView { cam: &cams[i], depth: &frames[i].depth, splats: &splats[i] }).collect()
    }

    #[test]
    fn the_frontal_camera_wins_and_the_floor_stays_closed() {
        // One camera straight above a floor patch, one at a grazing angle.
        let cams = [camera(Vec3::new(0.0, 3.0, 0.01)), camera(Vec3::new(0.0, 0.9, 3.0))];
        let disc = |p: Vec3| p.length() < 0.8;
        let frames: Vec<RgbdFrame> = cams.iter().map(|c| shoot(c, disc, None)).collect();
        let splats = candidates(&cams, &frames);
        assert!(splats[0].len() > 300 && splats[1].len() > 100, "{} {}", splats[0].len(), splats[1].len());
        for selection in [Selection::Greedy, Selection::Dominated] {
            let params = FusionParams { select: Some(selection), ..FusionParams::default() };
            let fused = fuse(&views(&cams, &frames, &splats), &params);
            assert_eq!(fused.stats.input, splats[0].len() + splats[1].len());
            assert_eq!(fused.stats.carved, 0);
            assert_eq!(fused.stats.kept, fused.stats.input - fused.stats.dominated);
            // Nothing of the frontal view is lost; of the grazing view only
            // the rim survives (where the frontal view's pixels are guarded).
            assert_eq!(fused.per_view[0].len(), splats[0].len(), "{selection:?}");
            assert!(fused.per_view[1].len() < splats[1].len() / 2, "{selection:?}: {}", fused.per_view[1].len());
            assert_eq!(fused.concat().len(), fused.stats.kept);
        }
        // Without selection nothing changes.
        let off = FusionParams { select: None, ..FusionParams::default() };
        let fused = fuse(&views(&cams, &frames, &splats), &off);
        assert_eq!(fused.stats.kept, fused.stats.input);
    }

    #[test]
    fn soft_selection_keeps_dominated_splats_with_less_opacity() {
        let cams = [camera(Vec3::new(0.0, 3.0, 0.01)), camera(Vec3::new(0.0, 0.9, 3.0))];
        let frames: Vec<RgbdFrame> = cams.iter().map(|c| shoot(c, |p| p.length() < 0.8, None)).collect();
        let splats = candidates(&cams, &frames);
        let params = FusionParams { soft: Some(1.0), ..FusionParams::default() };
        let fused = fuse(&views(&cams, &frames, &splats), &params);
        assert!(fused.stats.dominated > 50);
        assert_eq!(fused.stats.kept, fused.stats.input);
        let full = splats[1][0].splat.rgba[3];
        let faded = fused.per_view[1].iter().filter(|s| s.rgba[3] < full).count();
        assert_eq!(faded, fused.stats.dominated);
        assert!(fused.per_view[0].iter().all(|s| s.rgba[3] == full));
    }

    #[test]
    fn a_splat_in_free_space_is_carved_and_a_supported_one_is_not() {
        let cams = [camera(Vec3::new(0.0, 2.5, 2.0)), camera(Vec3::new(2.0, 2.5, -0.5)), camera(Vec3::new(-2.0, 2.5, -0.5))];
        let frames: Vec<RgbdFrame> = cams.iter().map(|c| shoot(c, |p| p.length() < 1.5, None)).collect();
        let mut splats = candidates(&cams, &frames);
        let honest: usize = splats.iter().map(Vec::len).sum();
        // An outlier of camera 0, floating 0.6 m above the floor and facing up.
        let mut ghost = splats[0][splats[0].len() / 2];
        ghost.splat.pos = [0.1, 0.6, 0.1];
        ghost.normal = Vec3::Y;
        splats[0].push(ghost);
        let params = FusionParams { select: None, ..FusionParams::default() };
        let fused = fuse(&views(&cams, &frames, &splats), &params);
        assert_eq!(fused.stats.carved, 1);
        assert_eq!(fused.stats.kept, honest);
        assert!(fused.per_view[0].iter().all(|s| s.pos[1] < 0.1));
        // With carving off it stays.
        let off = FusionParams { select: None, carve: false, ..FusionParams::default() };
        assert_eq!(fuse(&views(&cams, &frames, &splats), &off).stats.kept, honest + 1);
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
        let params = FusionParams { edge_guard: false, ..FusionParams::default() };
        let fused = fuse(&views(&cams, &frames, &splats), &params);
        assert_eq!(fused.stats.kept, fused.stats.input);
        // Without the normal test one face would go.
        let loose = FusionParams { min_normal_dot: -1.0, ..params };
        assert!(fuse(&views(&cams, &frames, &splats), &loose).stats.dominated > 0);
    }

    #[test]
    fn greedy_selection_never_drops_without_a_kept_cover() {
        // Three cameras around a floor; with a zero margin the plain rule may
        // drop chains, the greedy one must leave every dropped splat within
        // reach of a kept splat of another camera.
        let cams = [camera(Vec3::new(0.0, 2.0, 2.5)), camera(Vec3::new(2.2, 2.0, -1.2)), camera(Vec3::new(-2.2, 2.0, -1.2))];
        let frames: Vec<RgbdFrame> = cams.iter().map(|c| shoot(c, |p| p.length() < 1.2, None)).collect();
        let splats = candidates(&cams, &frames);
        let fused = fuse(&views(&cams, &frames, &splats), &FusionParams::default());
        assert!(fused.stats.dominated > fused.stats.input / 3, "{:?}", fused.stats);
        let kept: Vec<Vec3> = fused.concat().iter().map(|s| s.position()).collect();
        // Pixel footprint on the floor is about 3 m / 71 px ≈ 4 cm.
        for list in &splats {
            for s in list {
                let p = s.splat.position();
                let nearest = kept.iter().map(|k| (*k - p).length()).fold(f32::INFINITY, f32::min);
                assert!(nearest < 0.07, "a hole of {nearest} m at {p}");
            }
        }
    }
}
