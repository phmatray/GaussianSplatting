//! Gizmo scene description and its conversion to line segments.
//!
//! The UI hands the viewer plain data ([`GizmoScene`]); this module turns it
//! into a flat list of colored world-space segments that `lines` draws on top
//! of the splats. No browser types here.

use glam::{Mat4, Vec3};

/// Wireframe of a camera: a pyramid from the optical center to the image
/// rectangle at distance `size`, with a small triangle marking the image top.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrustumGizmo {
    /// Camera-to-world transform (camera looks along its −Z, +Y is up).
    pub world_from_cam: Mat4,
    /// Vertical field of view, radians.
    pub fov_y: f32,
    /// Image width / height.
    pub aspect: f32,
    /// sRGB in `0..=1`.
    pub color: [f32; 3],
    /// Length of the pyramid along the optical axis, meters.
    pub size: f32,
}

/// Square grid of lines in a horizontal plane.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridGizmo {
    /// Center of the grid; its `y` is the height of the plane.
    pub center: Vec3,
    /// Half of the side length, meters.
    pub half_extent: f32,
    /// Distance between lines, meters.
    pub step: f32,
    /// sRGB in `0..=1`.
    pub color: [f32; 3],
}

impl Default for GridGizmo {
    fn default() -> Self {
        Self { center: Vec3::ZERO, half_extent: 5.0, step: 0.5, color: [0.45, 0.5, 0.6] }
    }
}

/// Everything the viewer draws besides splats. Replace it as a whole with
/// `Viewer::set_gizmos`; the VR controllers are added by the viewer itself.
#[derive(Clone, Debug, PartialEq)]
pub struct GizmoScene {
    /// Floor grid.
    pub grid: Option<GridGizmo>,
    /// World axes at the origin with this length in meters (X red, Y green, Z blue).
    pub axes: Option<f32>,
    /// Cameras of the capture rig.
    pub frusta: Vec<FrustumGizmo>,
    /// Scan path polyline.
    pub path: Vec<Vec3>,
    /// sRGB in `0..=1`.
    pub path_color: [f32; 3],
    /// Current pose of the handheld scanning camera (drawn thicker).
    pub handheld: Option<FrustumGizmo>,
}

impl Default for GizmoScene {
    fn default() -> Self {
        Self { grid: None, axes: None, frusta: Vec::new(), path: Vec::new(), path_color: [1.0, 0.75, 0.2], handheld: None }
    }
}

/// Floats per segment in a [`LineBatch`]: `a.xyz, b.xyz, rgba, width`.
pub(crate) const FLOATS_PER_SEGMENT: usize = 11;

/// The grid never has more lines per direction than this, whatever the step.
const MAX_GRID_LINES: f32 = 400.0;

/// Flat segment list in the vertex layout of the line renderer.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct LineBatch {
    data: Vec<f32>,
}

impl LineBatch {
    pub fn clear(&mut self) {
        self.data.clear();
    }

    pub fn data(&self) -> &[f32] {
        &self.data
    }

    pub fn len(&self) -> usize {
        self.data.len() / FLOATS_PER_SEGMENT
    }

    /// Adds one segment; `width` is in CSS-like pixels. Segments with a
    /// non-finite end point are dropped.
    pub fn segment(&mut self, a: Vec3, b: Vec3, color: [f32; 3], alpha: f32, width: f32) {
        if a.is_finite() && b.is_finite() {
            self.data.extend_from_slice(&[a.x, a.y, a.z, b.x, b.y, b.z, color[0], color[1], color[2], alpha, width]);
        }
    }

    pub fn polyline(&mut self, points: &[Vec3], color: [f32; 3], alpha: f32, width: f32) {
        for pair in points.windows(2) {
            self.segment(pair[0], pair[1], color, alpha, width);
        }
    }

    pub fn grid(&mut self, grid: &GridGizmo) {
        let half = grid.half_extent;
        if !(half.is_finite() && half > 0.0 && grid.step.is_finite() && grid.step > 0.0) {
            return;
        }
        let step = grid.step.max(2.0 * half / MAX_GRID_LINES);
        let n = (half / step).floor() as i32;
        for i in -n..=n {
            let o = i as f32 * step;
            // The two center lines are a little stronger.
            let alpha = if i == 0 { 0.7 } else { 0.35 };
            let c = grid.center;
            self.segment(c + Vec3::new(o, 0.0, -half), c + Vec3::new(o, 0.0, half), grid.color, alpha, 1.0);
            self.segment(c + Vec3::new(-half, 0.0, o), c + Vec3::new(half, 0.0, o), grid.color, alpha, 1.0);
        }
    }

    pub fn axes(&mut self, length: f32) {
        self.segment(Vec3::ZERO, Vec3::X * length, [1.0, 0.25, 0.25], 1.0, 2.0);
        self.segment(Vec3::ZERO, Vec3::Y * length, [0.3, 0.9, 0.3], 1.0, 2.0);
        self.segment(Vec3::ZERO, Vec3::Z * length, [0.3, 0.5, 1.0], 1.0, 2.0);
    }

    pub fn frustum(&mut self, f: &FrustumGizmo, width: f32) {
        let half_h = (0.5 * f.fov_y).tan() * f.size;
        let half_w = half_h * f.aspect;
        if !(half_h.is_finite() && half_w.is_finite()) {
            return;
        }
        let m = &f.world_from_cam;
        let apex = m.transform_point3(Vec3::ZERO);
        // Image corners, counter-clockwise seen from the camera, starting top-left.
        let corners = [(-1.0, 1.0), (1.0, 1.0), (1.0, -1.0), (-1.0, -1.0)]
            .map(|(x, y)| m.transform_point3(Vec3::new(x * half_w, y * half_h, -f.size)));
        for i in 0..4 {
            self.segment(apex, corners[i], f.color, 0.9, width);
            self.segment(corners[i], corners[(i + 1) % 4], f.color, 0.9, width);
        }
        // "This side up" marker above the top edge.
        let tip = m.transform_point3(Vec3::new(0.0, 1.35 * half_h, -f.size));
        let (left, right) = (corners[0].lerp(corners[1], 0.3), corners[0].lerp(corners[1], 0.7));
        self.segment(left, tip, f.color, 0.9, width);
        self.segment(tip, right, f.color, 0.9, width);
    }

    /// The whole user-provided scene.
    pub fn scene(&mut self, scene: &GizmoScene) {
        if let Some(grid) = &scene.grid {
            self.grid(grid);
        }
        if let Some(length) = scene.axes {
            self.axes(length);
        }
        for f in &scene.frusta {
            self.frustum(f, 1.5);
        }
        self.polyline(&scene.path, scene.path_color, 0.9, 2.0);
        if let Some(f) = &scene.handheld {
            self.frustum(f, 2.5);
        }
    }

    /// A tracked VR controller: a short pointing ray and a small cross at the
    /// grip. `world_from_grip` carries the rig scale, so the sizes below are
    /// real meters.
    pub fn controller(&mut self, world_from_grip: &Mat4, left: bool) {
        let color = if left { [0.3, 1.0, 0.6] } else { [1.0, 0.85, 0.3] };
        let p = |v: Vec3| world_from_grip.transform_point3(v);
        self.segment(p(Vec3::ZERO), p(Vec3::new(0.0, 0.0, -0.12)), color, 1.0, 3.0);
        for axis in [Vec3::X, Vec3::Y] {
            self.segment(p(axis * -0.025), p(axis * 0.025), color, 1.0, 2.0);
        }
        // Small square around the ray so that the roll of the hand is visible.
        let ring = [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)].map(|(x, y)| p(Vec3::new(x * 0.018, y * 0.018, -0.04)));
        for i in 0..4 {
            self.segment(ring[i], ring[(i + 1) % 4], color, 1.0, 2.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segments(batch: &LineBatch) -> Vec<(Vec3, Vec3)> {
        batch.data().chunks_exact(FLOATS_PER_SEGMENT).map(|s| (Vec3::new(s[0], s[1], s[2]), Vec3::new(s[3], s[4], s[5]))).collect()
    }

    #[test]
    fn frustum_respects_pose_fov_and_aspect() {
        let pose = Mat4::from_translation(Vec3::new(1.0, 2.0, 3.0)) * Mat4::from_rotation_y(0.5);
        let f = FrustumGizmo { world_from_cam: pose, fov_y: 60f32.to_radians(), aspect: 1.5, color: [1.0; 3], size: 0.4 };
        let mut batch = LineBatch::default();
        batch.frustum(&f, 1.5);
        assert_eq!(batch.len(), 10);
        let cam_from_world = pose.inverse();
        let segs = segments(&batch);
        let half_h = 0.4 * 30f32.to_radians().tan();
        // The first eight segments alternate apex→corner and corner→next corner.
        for (i, (a, b)) in segs.iter().take(8).enumerate() {
            let (a, b) = (cam_from_world.transform_point3(*a), cam_from_world.transform_point3(*b));
            if i % 2 == 0 {
                assert!(a.length() < 1e-5, "starts at the optical center");
            } else {
                assert!((a.z + 0.4).abs() < 1e-5);
            }
            assert!((b.z + 0.4).abs() < 1e-5, "image plane is in front, along −Z");
            assert!((b.x.abs() - 1.5 * half_h).abs() < 1e-5 && (b.y.abs() - half_h).abs() < 1e-5);
        }
        // The marker points up in camera space.
        let tip = cam_from_world.transform_point3(segs[8].1);
        assert!(tip.y > half_h && tip.x.abs() < 1e-5);
    }

    #[test]
    fn grid_is_square_and_bounded() {
        let mut batch = LineBatch::default();
        batch.grid(&GridGizmo { center: Vec3::new(1.0, 0.5, -1.0), half_extent: 2.0, step: 0.5, color: [0.5; 3] });
        assert_eq!(batch.len(), 2 * 9);
        for (a, b) in segments(&batch) {
            assert!((a.y - 0.5).abs() < 1e-6 && (b.y - 0.5).abs() < 1e-6);
            assert!(((a - b).length() - 4.0).abs() < 1e-5);
            for p in [a, b] {
                assert!((p.x - 1.0).abs() <= 2.0 + 1e-5 && (p.z + 1.0).abs() <= 2.0 + 1e-5);
            }
        }
        // A tiny step does not explode the line count; nonsense draws nothing.
        batch.clear();
        batch.grid(&GridGizmo { step: 1e-6, ..Default::default() });
        assert!(batch.len() <= 2 * (MAX_GRID_LINES as usize + 1));
        batch.clear();
        batch.grid(&GridGizmo { step: 0.0, ..Default::default() });
        batch.grid(&GridGizmo { half_extent: f32::NAN, ..Default::default() });
        assert_eq!(batch.len(), 0);
    }

    #[test]
    fn scene_collects_everything_and_drops_bad_points() {
        let frustum = FrustumGizmo { world_from_cam: Mat4::IDENTITY, fov_y: 1.0, aspect: 1.0, color: [1.0; 3], size: 0.3 };
        let scene = GizmoScene {
            grid: Some(GridGizmo { half_extent: 1.0, step: 1.0, ..Default::default() }),
            axes: Some(1.0),
            frusta: vec![frustum; 3],
            path: vec![Vec3::ZERO, Vec3::X, Vec3::new(f32::NAN, 0.0, 0.0), Vec3::Z, Vec3::Y],
            handheld: Some(frustum),
            ..Default::default()
        };
        let mut batch = LineBatch::default();
        batch.scene(&scene);
        // grid 6 + axes 3 + 3 frusta + path (4 segments, 2 touch the NaN point) + handheld.
        assert_eq!(batch.len(), 6 + 3 + 30 + 2 + 10);
        assert!(batch.data().iter().all(|v| v.is_finite()));
        let mut empty = LineBatch::default();
        empty.scene(&GizmoScene::default());
        assert_eq!(empty.len(), 0);
    }

    #[test]
    fn controller_gizmo_follows_the_grip_pose() {
        let pose = Mat4::from_translation(Vec3::new(0.0, 1.0, 0.0)) * Mat4::from_scale(Vec3::splat(15.0));
        let mut batch = LineBatch::default();
        batch.controller(&pose, true);
        let segs = segments(&batch);
        assert_eq!(segs.len(), 7);
        // The ray is 12 cm long in real meters, i.e. 1.8 m at world scale 15.
        assert!(((segs[0].0 - segs[0].1).length() - 1.8).abs() < 1e-4);
        assert!((segs[0].0 - Vec3::new(0.0, 1.0, 0.0)).length() < 1e-6);
    }
}
