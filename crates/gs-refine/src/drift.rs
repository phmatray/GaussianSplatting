//! Global correction of ARKit's pose drift.
//!
//! ARKit's poses are excellent from one frame to the next and drift over a
//! walk: on the first long real capture (1 039 frames, 48 m, 2026-11-15) a
//! place seen again after two minutes was 5 to 10 cm off, two to four fusion
//! cells, and every such place came out doubled. This module finds where the
//! phone came back ([`loop_closures`]: frames long apart, near each other,
//! looking the same way, registered by a point-to-plane ICP of their depth),
//! spreads those corrections over the whole walk with a pose graph
//! ([`optimize`]: ARKit's relative poses between consecutive keyframes hold
//! the shape, the loops close it), and gives every frame its corrected pose
//! ([`corrected_poses`]).

use std::collections::HashMap;

use glam::{Mat3, Mat4, Quat, Vec3};
use gs_core::ingest::Tracking;

use crate::dataset::Capture;

/// Settings of the drift correction.
#[derive(Clone, Copy, Debug)]
pub struct DriftParams {
    /// One keyframe every this many frames.
    pub every: usize,
    /// Frames closer in time than this are not a loop, seconds.
    pub min_gap: f64,
    /// Largest distance between the two cameras of a loop, meters.
    pub max_distance: f32,
    /// Largest angle between their viewing directions, degrees.
    pub max_angle: f32,
    /// A registration is kept when at least this share of points matched...
    pub min_inliers: f32,
    /// ...and it turns the frame by at most this many degrees.
    pub max_turn: f32,
}

impl Default for DriftParams {
    fn default() -> Self {
        Self { every: 5, min_gap: 20.0, max_distance: 1.0, max_angle: 35.0, min_inliers: 0.4, max_turn: 8.0 }
    }
}

/// A keyframe's depth as points and normals, in world coordinates.
pub struct Cloud {
    pub frame: usize,
    pub time: f64,
    pub world_from_cam: Mat4,
    pub points: Vec<Vec3>,
    pub normals: Vec<Vec3>,
}

impl Cloud {
    fn center(&self) -> Vec3 {
        self.world_from_cam.transform_point3(Vec3::ZERO)
    }
    fn forward(&self) -> Vec3 {
        self.world_from_cam.transform_vector3(Vec3::NEG_Z).normalize()
    }
}

/// Depth of `frame` as a cloud, one point every `stride` depth pixels, or
/// nothing for a frame without normal tracking or usable depth.
pub fn cloud_of(capture: &Capture, frame: usize, stride: usize) -> Option<Cloud> {
    let f = capture.frame(frame).ok()?;
    if f.tracking != Tracking::Normal || f.depth_mm.is_empty() {
        return None;
    }
    let (dw, dh) = (f.depth_width as usize, f.depth_height as usize);
    let sx = dw as f32 / f.color_width as f32;
    let sy = dh as f32 / f.color_height as f32;
    let (fx, fy, cx, cy) = (f.intrinsics[0] * sx, f.intrinsics[1] * sy, f.intrinsics[2] * sx, f.intrinsics[3] * sy);
    let m = Mat4::from_cols_array(&f.world_from_cam);
    let cam_point = |u: usize, v: usize| -> Option<Vec3> {
        let i = v * dw + u;
        let z = f.depth_mm[i] as f32 / 1000.0;
        if z <= 0.2 || z > 4.5 || f.confidence.as_ref().is_some_and(|c| c[i] < 1) {
            return None;
        }
        Some(Vec3::new((u as f32 - cx) / fx * z, -(v as f32 - cy) / fy * z, -z))
    };
    let (mut points, mut normals) = (Vec::new(), Vec::new());
    for v in (stride..dh.saturating_sub(stride)).step_by(stride) {
        for u in (stride..dw.saturating_sub(stride)).step_by(stride) {
            let (Some(p), Some(px), Some(py)) = (cam_point(u, v), cam_point(u + stride, v), cam_point(u, v + stride)) else {
                continue;
            };
            // Depth discontinuities are no surface.
            if (px - p).length() > 0.08 || (py - p).length() > 0.08 {
                continue;
            }
            let n = (px - p).cross(py - p).normalize_or_zero();
            if n == Vec3::ZERO {
                continue;
            }
            let n = if n.dot(-p) < 0.0 { -n } else { n };
            points.push(m.transform_point3(p));
            normals.push(m.transform_vector3(n).normalize());
        }
    }
    (points.len() >= 300).then_some(Cloud { frame, time: f.timestamp, world_from_cam: m, points, normals })
}

struct Grid {
    cell: f32,
    map: HashMap<(i32, i32, i32), Vec<u32>>,
}

impl Grid {
    fn new(points: &[Vec3], cell: f32) -> Self {
        let mut map: HashMap<(i32, i32, i32), Vec<u32>> = HashMap::new();
        for (i, p) in points.iter().enumerate() {
            map.entry(Self::key(*p, cell)).or_default().push(i as u32);
        }
        Self { cell, map }
    }
    fn key(p: Vec3, cell: f32) -> (i32, i32, i32) {
        ((p.x / cell).floor() as i32, (p.y / cell).floor() as i32, (p.z / cell).floor() as i32)
    }
    fn nearest(&self, points: &[Vec3], q: Vec3, max: f32) -> Option<usize> {
        let (kx, ky, kz) = Self::key(q, self.cell);
        let mut best = (max * max, None);
        for dx in -1..=1 {
            for dy in -1..=1 {
                for dz in -1..=1 {
                    for &i in self.map.get(&(kx + dx, ky + dy, kz + dz)).map(Vec::as_slice).unwrap_or(&[]) {
                        let d = (points[i as usize] - q).length_squared();
                        if d < best.0 {
                            best = (d, Some(i as usize));
                        }
                    }
                }
            }
        }
        best.1
    }
}

/// Where the phone came back: `source`'s pose should have been
/// `correction * source`, seen from `target`.
#[derive(Clone, Copy, Debug)]
pub struct Loop {
    pub target: usize,
    pub source: usize,
    /// Rigid correction in world coordinates that brings the source cloud
    /// onto the target's.
    pub correction: Mat4,
    pub inliers: f32,
    pub rms_before: f32,
    pub rms_after: f32,
}

/// Point-to-plane ICP of `source` onto `target`, from where ARKit put them.
fn register(source: &Cloud, target: &Cloud, grid: &Grid) -> Option<(Mat4, f32, f32, f32)> {
    let (mut rot, mut trans) = (Mat3::IDENTITY, Vec3::ZERO);
    let (mut first, mut last) = (None, (0.0f32, 0usize));
    for iteration in 0..25 {
        let max = if iteration < 5 { 0.15 } else { 0.06 };
        let mut ata = [[0.0f64; 6]; 6];
        let mut atb = [0.0f64; 6];
        let (mut sum, mut count) = (0.0f64, 0usize);
        for (p, n) in source.points.iter().zip(&source.normals) {
            let q = rot * *p + trans;
            let Some(j) = grid.nearest(&target.points, q, max) else { continue };
            let tn = target.normals[j];
            if tn.dot(rot * *n) < 0.8 {
                continue;
            }
            let r = (q - target.points[j]).dot(tn) as f64;
            let c = q.cross(tn);
            let row = [c.x as f64, c.y as f64, c.z as f64, tn.x as f64, tn.y as f64, tn.z as f64];
            for a in 0..6 {
                for b in 0..6 {
                    ata[a][b] += row[a] * row[b];
                }
                atb[a] -= row[a] * r;
            }
            sum += r * r;
            count += 1;
        }
        if count < 200 {
            return None;
        }
        let rms = (sum / count as f64).sqrt() as f32;
        first.get_or_insert(rms);
        last = (rms, count);
        let x = solve_dense(&ata.iter().flatten().copied().collect::<Vec<_>>(), &atb, 6)?;
        let w = Vec3::new(x[0] as f32, x[1] as f32, x[2] as f32);
        let dr = Mat3::from_axis_angle(w.normalize_or(Vec3::X), w.length());
        let dt = Vec3::new(x[3] as f32, x[4] as f32, x[5] as f32);
        rot = dr * rot;
        trans = dr * trans + dt;
        if dt.length() < 1e-5 && w.length() < 1e-6 {
            break;
        }
    }
    let correction = Mat4::from_mat3_translation(rot, trans);
    Some((correction, last.1 as f32 / source.points.len() as f32, first?, last.0))
}

/// The loops of a set of keyframe clouds (in time order).
pub fn loop_closures(clouds: &[Cloud], params: &DriftParams) -> Vec<Loop> {
    let cos = params.max_angle.to_radians().cos();
    let mut loops = Vec::new();
    for (t, target) in clouds.iter().enumerate() {
        let mut grid = None;
        for (s, source) in clouds.iter().enumerate().skip(t + 1) {
            if source.time - target.time < params.min_gap
                || (source.center() - target.center()).length() > params.max_distance
                || source.forward().dot(target.forward()) < cos
            {
                continue;
            }
            let grid = grid.get_or_insert_with(|| Grid::new(&target.points, 0.06));
            let Some((correction, inliers, rms_before, rms_after)) = register(source, target, grid) else { continue };
            let turn = Quat::from_mat4(&correction).to_axis_angle().1.to_degrees();
            if inliers >= params.min_inliers && turn.min(360.0 - turn) <= params.max_turn {
                loops.push(Loop { target: t, source: s, correction, inliers, rms_before, rms_after });
            }
        }
    }
    loops
}

/// 6-vector of a rigid transform near the identity: rotation vector, then
/// translation.
fn log6(m: &Mat4) -> [f64; 6] {
    let (_, rot, t) = m.to_scale_rotation_translation();
    let (axis, angle) = rot.to_axis_angle();
    let angle = if angle > std::f32::consts::PI { angle - std::f32::consts::TAU } else { angle };
    let w = axis * angle;
    [w.x as f64, w.y as f64, w.z as f64, t.x as f64, t.y as f64, t.z as f64]
}

fn exp6(x: &[f64]) -> Mat4 {
    let w = Vec3::new(x[0] as f32, x[1] as f32, x[2] as f32);
    let rot = Quat::from_axis_angle(w.normalize_or(Vec3::X), w.length());
    Mat4::from_rotation_translation(rot, Vec3::new(x[3] as f32, x[4] as f32, x[5] as f32))
}

/// An edge of the pose graph: `nodes[b]` should be `nodes[a] * relative`.
struct Edge {
    a: usize,
    b: usize,
    relative: Mat4,
    /// Inverse standard deviations of the rotation (per radian) and the
    /// translation (per meter).
    weight: (f64, f64),
}

/// Odometry: ARKit between consecutive keyframes, a few millimeters and a
/// fraction of a degree. Loops: the ICP, about a centimeter and half a degree.
const ODOMETRY_WEIGHT: (f64, f64) = (1.0 / 0.003, 1.0 / 0.004);
const LOOP_WEIGHT: (f64, f64) = (1.0 / 0.009, 1.0 / 0.012);
/// A loop whose residual after a first solve exceeds this many standard
/// deviations is a false registration and is dropped.
const OUTLIER_SIGMAS: f64 = 3.0;

fn residual(nodes: &[Mat4], e: &Edge) -> [f64; 6] {
    let error = e.relative.inverse() * nodes[e.a].inverse() * nodes[e.b];
    let mut r = log6(&error);
    for (k, v) in r.iter_mut().enumerate() {
        *v *= if k < 3 { e.weight.0 } else { e.weight.1 };
    }
    r
}

/// Gauss-Newton on the poses (node 0 fixed), perturbing each node on the
/// left: `exp(ξ) · T`.
fn solve_graph(mut nodes: Vec<Mat4>, edges: &[Edge], iterations: usize) -> Vec<Mat4> {
    let n = nodes.len();
    if n < 2 {
        return nodes;
    }
    let dim = 6 * (n - 1);
    for _ in 0..iterations {
        let mut h = vec![0.0f64; dim * dim];
        let mut g = vec![0.0f64; dim];
        for e in edges {
            let r0 = residual(&nodes, e);
            // Numeric Jacobian with respect to the 12 perturbations.
            let mut jac: Vec<(usize, [f64; 6])> = Vec::with_capacity(12);
            for node in [e.a, e.b] {
                if node == 0 {
                    continue;
                }
                for k in 0..6 {
                    let mut x = [0.0f64; 6];
                    x[k] = 1e-6;
                    let saved = nodes[node];
                    nodes[node] = exp6(&x) * saved;
                    let r = residual(&nodes, e);
                    nodes[node] = saved;
                    let mut col = [0.0f64; 6];
                    for i in 0..6 {
                        col[i] = (r[i] - r0[i]) / 1e-6;
                    }
                    jac.push((6 * (node - 1) + k, col));
                }
            }
            for (i, ci) in &jac {
                g[*i] += (0..6).map(|q| ci[q] * r0[q]).sum::<f64>();
                for (j, cj) in &jac {
                    h[i * dim + j] += (0..6).map(|q| ci[q] * cj[q]).sum::<f64>();
                }
            }
        }
        for i in 0..dim {
            h[i * dim + i] += 1e-6;
            g[i] = -g[i];
        }
        let Some(dx) = solve_dense(&h, &g, dim) else { break };
        let mut step = 0.0f64;
        for k in 1..n {
            let x = &dx[6 * (k - 1)..6 * k];
            nodes[k] = exp6(x) * nodes[k];
            step = step.max(x.iter().fold(0.0, |m, v| m.max(v.abs())));
        }
        if step < 1e-7 {
            break;
        }
    }
    nodes
}

/// Corrected keyframe poses: the shape of ARKit's walk, closed by the
/// loops. Loops that disagree with the rest after a first solve are dropped.
/// Returns the poses and the loops kept.
pub fn optimize(keyframes: &[Mat4], loops: &[Loop]) -> (Vec<Mat4>, Vec<Loop>) {
    let mut edges: Vec<Edge> = keyframes
        .windows(2)
        .enumerate()
        .map(|(i, w)| Edge { a: i, b: i + 1, relative: w[0].inverse() * w[1], weight: ODOMETRY_WEIGHT })
        .collect();
    let loop_edge = |l: &Loop| {
        let corrected = l.correction * keyframes[l.source];
        Edge { a: l.target, b: l.source, relative: keyframes[l.target].inverse() * corrected, weight: LOOP_WEIGHT }
    };
    let odometry = edges.len();
    edges.extend(loops.iter().map(loop_edge));
    let first = solve_graph(keyframes.to_vec(), &edges, 10);
    let kept: Vec<Loop> = loops
        .iter()
        .zip(&edges[odometry..])
        .filter(|(_, e)| residual(&first, e).iter().all(|v| v.abs() <= OUTLIER_SIGMAS))
        .map(|(l, _)| *l)
        .collect();
    if kept.len() == loops.len() {
        return (first, kept);
    }
    edges.truncate(odometry);
    edges.extend(kept.iter().map(loop_edge));
    (solve_graph(keyframes.to_vec(), &edges, 10), kept)
}

/// Corrected `world_from_cam` of every frame of `capture`, and a report.
/// Frames between two keyframes take a blend of their corrections.
pub fn corrected_poses(capture: &Capture, params: &DriftParams) -> (Vec<[f32; 16]>, DriftReport) {
    let frames = capture.frames.len();
    let clouds: Vec<Cloud> = (0..frames).step_by(params.every.max(1)).filter_map(|i| cloud_of(capture, i, 4)).collect();
    let loops = loop_closures(&clouds, params);
    let keyframes: Vec<Mat4> = clouds.iter().map(|c| c.world_from_cam).collect();
    let (corrected, kept) = optimize(&keyframes, &loops);
    let corrections: Vec<Mat4> = corrected.iter().zip(&keyframes).map(|(c, k)| *c * k.inverse()).collect();
    let originals: Vec<Mat4> = capture.frames.iter().map(|f| Mat4::from_cols_array(&f.header.world_from_cam)).collect();
    let correction_at = |frame: usize| -> Mat4 {
        if corrections.is_empty() {
            return Mat4::IDENTITY;
        }
        let next = clouds.partition_point(|c| c.frame <= frame);
        if next == 0 {
            return corrections[0];
        }
        if next == clouds.len() {
            return corrections[next - 1];
        }
        let (a, b) = (&clouds[next - 1], &clouds[next]);
        let t = (frame - a.frame) as f32 / (b.frame - a.frame) as f32;
        let (_, ra, ta) = corrections[next - 1].to_scale_rotation_translation();
        let (_, rb, tb) = corrections[next].to_scale_rotation_translation();
        Mat4::from_rotation_translation(ra.slerp(rb, t), ta.lerp(tb, t))
    };
    let poses: Vec<[f32; 16]> = originals.iter().enumerate().map(|(i, m)| (correction_at(i) * *m).to_cols_array()).collect();
    let moved: Vec<f32> = corrected.iter().zip(&keyframes).map(|(c, k)| (c.w_axis - k.w_axis).truncate().length()).collect();
    let report = DriftReport {
        keyframes: clouds.len(),
        loops_found: loops.len(),
        loops_kept: kept.len(),
        largest_shift_m: moved.iter().copied().fold(0.0, f32::max),
        mean_shift_m: moved.iter().sum::<f32>() / moved.len().max(1) as f32,
    };
    (poses, report)
}

/// What [`corrected_poses`] did.
#[derive(Clone, Copy, Debug, Default)]
pub struct DriftReport {
    pub keyframes: usize,
    pub loops_found: usize,
    pub loops_kept: usize,
    pub largest_shift_m: f32,
    pub mean_shift_m: f32,
}

/// Solves the dense symmetric positive system `a · x = b` (row-major `n × n`)
/// by Cholesky.
fn solve_dense(a: &[f64], b: &[f64], n: usize) -> Option<Vec<f64>> {
    let mut l = vec![0.0f64; n * n];
    for i in 0..n {
        for j in 0..=i {
            let mut s = a[i * n + j];
            for k in 0..j {
                s -= l[i * n + k] * l[j * n + k];
            }
            if i == j {
                if s <= 0.0 {
                    return None;
                }
                l[i * n + i] = s.sqrt();
            } else {
                l[i * n + j] = s / l[j * n + j];
            }
        }
    }
    let mut y = vec![0.0f64; n];
    for i in 0..n {
        let s: f64 = (0..i).map(|k| l[i * n + k] * y[k]).sum();
        y[i] = (b[i] - s) / l[i * n + i];
    }
    let mut x = vec![0.0f64; n];
    for i in (0..n).rev() {
        let s: f64 = (i + 1..n).map(|k| l[k * n + i] * x[k]).sum();
        x[i] = (y[i] - s) / l[i * n + i];
    }
    Some(x)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A square walk whose poses drift a little at every step; one loop
    /// measures the truth between the last pose and the first. The graph
    /// brings the last pose back close to the truth and spreads the rest.
    #[test]
    fn a_loop_spreads_the_drift_over_the_walk() {
        let step = |i: usize| {
            let angle = (i as f32 / 40.0) * std::f32::consts::TAU;
            Mat4::from_rotation_translation(Quat::from_rotation_y(-angle), Vec3::new(angle.cos() * 2.0, 0.0, angle.sin() * 2.0))
        };
        let truth: Vec<Mat4> = (0..=40).map(step).collect();
        // Drift: 2 mm and 0.05 degree more at every step.
        let mut drifted = vec![truth[0]];
        for i in 1..truth.len() {
            let rel = truth[i - 1].inverse() * truth[i];
            let noise = Mat4::from_rotation_translation(Quat::from_rotation_y(0.05f32.to_radians()), Vec3::new(0.002, 0.0, 0.0));
            drifted.push(drifted[i - 1] * rel * noise);
        }
        let last = truth.len() - 1;
        let error = |poses: &[Mat4]| (poses[last].w_axis - truth[last].w_axis).truncate().length();
        // The loop says: the last frame is where the truth puts it.
        let correction = truth[last] * drifted[last].inverse();
        let l = Loop { target: 0, source: last, correction, inliers: 1.0, rms_before: 0.0, rms_after: 0.0 };
        let (fixed, kept) = optimize(&drifted, &[l]);
        assert_eq!(kept.len(), 1);
        assert!(error(&drifted) > 0.05, "the walk should have drifted, {}", error(&drifted));
        assert!(error(&fixed) < 0.2 * error(&drifted), "{} -> {}", error(&drifted), error(&fixed));
        // The middle moves too, by less than the end.
        let mid = (fixed[20].w_axis - drifted[20].w_axis).truncate().length();
        assert!(mid > 0.0 && mid < (fixed[last].w_axis - drifted[last].w_axis).truncate().length());
    }

    #[test]
    fn cholesky_solves_a_small_system() {
        let a = [4.0, 2.0, 2.0, 3.0];
        let x = solve_dense(&a, &[2.0, 1.0], 2).unwrap();
        assert!((x[0] - 0.5).abs() < 1e-12 && x[1].abs() < 1e-12);
    }
}
