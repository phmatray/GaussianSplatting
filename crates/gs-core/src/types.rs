//! Shared value types. These are the contract between every module and crate;
//! change them only together with `docs/ARCHITECTURE.md`.

use bytemuck::{Pod, Zeroable};
use glam::{Affine3A, Mat3, Mat4, Quat, Vec3};
use serde::{Deserialize, Serialize};

/// Calibrated pinhole camera.
///
/// Camera space is OpenGL/WebXR style: +X right, +Y up, -Z forward.
/// Pixel `(0, 0)` is the top-left corner of the image, pixel centers sit at
/// `+0.5`, and image `v` grows downward.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Camera {
    pub width: u32,
    pub height: u32,
    /// Focal lengths in pixels.
    pub fx: f32,
    pub fy: f32,
    /// Principal point in pixels.
    pub cx: f32,
    pub cy: f32,
    /// Camera-to-world rigid transform.
    pub world_from_cam: Affine3A,
}

impl Camera {
    /// Camera at `eye` looking at `target`, with a vertical field of view.
    pub fn look_at(eye: Vec3, target: Vec3, up: Vec3, fov_y: f32, width: u32, height: u32) -> Self {
        let forward = (target - eye).normalize_or(Vec3::NEG_Z);
        let mut right = forward.cross(up);
        if right.length_squared() < 1e-12 {
            // Looking straight along `up`: pick any perpendicular axis.
            right = forward.cross(Vec3::X);
            if right.length_squared() < 1e-12 {
                right = forward.cross(Vec3::Z);
            }
        }
        let right = right.normalize();
        let true_up = right.cross(forward);
        let world_from_cam = Affine3A::from_mat3_translation(Mat3::from_cols(right, true_up, -forward), eye);
        Self::from_pose(world_from_cam, fov_y, width, height)
    }

    /// Camera with a given pose and a vertical field of view (square pixels,
    /// centered principal point).
    pub fn from_pose(world_from_cam: Affine3A, fov_y: f32, width: u32, height: u32) -> Self {
        let f = 0.5 * height as f32 / (0.5 * fov_y).tan();
        Self {
            width,
            height,
            fx: f,
            fy: f,
            cx: 0.5 * width as f32,
            cy: 0.5 * height as f32,
            world_from_cam,
        }
    }

    /// Same pose and field of view at another resolution.
    pub fn resized(&self, width: u32, height: u32) -> Self {
        let sx = width as f32 / self.width as f32;
        let sy = height as f32 / self.height as f32;
        Self {
            width,
            height,
            fx: self.fx * sx,
            fy: self.fy * sy,
            cx: self.cx * sx,
            cy: self.cy * sy,
            world_from_cam: self.world_from_cam,
        }
    }

    #[inline]
    pub fn position(&self) -> Vec3 {
        self.world_from_cam.translation.into()
    }

    /// Unit vector the camera looks along, in world space.
    #[inline]
    pub fn forward(&self) -> Vec3 {
        -Vec3::from(self.world_from_cam.matrix3.z_axis)
    }

    #[inline]
    pub fn fov_y(&self) -> f32 {
        2.0 * (0.5 * self.height as f32 / self.fy).atan()
    }

    /// Camera-space direction through pixel coordinates `(px, py)`, scaled so
    /// that its forward component is 1 (`z = -1`). Multiply by a depth to get
    /// the camera-space point at that depth.
    #[inline]
    pub fn pixel_dir_cam(&self, px: f32, py: f32) -> Vec3 {
        Vec3::new((px - self.cx) / self.fx, -(py - self.cy) / self.fy, -1.0)
    }

    /// World-space ray through pixel coordinates `(px, py)`.
    /// Returns `(origin, unit direction)`.
    #[inline]
    pub fn ray(&self, px: f32, py: f32) -> (Vec3, Vec3) {
        let dir = self.world_from_cam.transform_vector3(self.pixel_dir_cam(px, py));
        (self.position(), dir.normalize())
    }

    /// World-space point seen at pixel `(px, py)` with forward depth `depth`.
    #[inline]
    pub fn unproject(&self, px: f32, py: f32, depth: f32) -> Vec3 {
        self.world_from_cam.transform_point3(self.pixel_dir_cam(px, py) * depth)
    }

    /// Projects a world point. Returns `(px, py, depth)` or `None` when the
    /// point is behind the camera. The pixel may lie outside the image.
    #[inline]
    pub fn project(&self, world: Vec3) -> Option<(f32, f32, f32)> {
        let c = self.world_from_cam.inverse().transform_point3(world);
        let depth = -c.z;
        if depth <= 1e-6 {
            return None;
        }
        Some((self.cx + self.fx * c.x / depth, self.cy - self.fy * c.y / depth, depth))
    }

    /// World-to-camera matrix.
    pub fn view_matrix(&self) -> Mat4 {
        Mat4::from(self.world_from_cam.inverse())
    }

    /// OpenGL clip-space projection matching the intrinsics (NDC z in [-1, 1]).
    pub fn projection_matrix(&self, near: f32, far: f32) -> Mat4 {
        let w = self.width as f32;
        let h = self.height as f32;
        Mat4::from_cols_array(&[
            2.0 * self.fx / w,
            0.0,
            0.0,
            0.0,
            0.0,
            2.0 * self.fy / h,
            0.0,
            0.0,
            1.0 - 2.0 * self.cx / w,
            2.0 * self.cy / h - 1.0,
            -(far + near) / (far - near),
            -1.0,
            0.0,
            0.0,
            -2.0 * far * near / (far - near),
            0.0,
        ])
    }
}

/// One RGB-D image as delivered by a (simulated) depth camera.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RgbdFrame {
    pub width: u32,
    pub height: u32,
    /// sRGB, 3 bytes per pixel, row-major, top row first.
    pub rgb: Vec<u8>,
    /// Distance along the camera forward axis in meters. `0.0` = no return.
    pub depth: Vec<f32>,
}

impl RgbdFrame {
    pub fn new(width: u32, height: u32) -> Self {
        let n = (width * height) as usize;
        Self { width, height, rgb: vec![0; n * 3], depth: vec![0.0; n] }
    }

    #[inline]
    pub fn index(&self, x: u32, y: u32) -> usize {
        (y * self.width + x) as usize
    }

    #[inline]
    pub fn depth_at(&self, x: u32, y: u32) -> f32 {
        self.depth[self.index(x, y)]
    }

    #[inline]
    pub fn rgb_at(&self, x: u32, y: u32) -> [u8; 3] {
        let i = self.index(x, y) * 3;
        [self.rgb[i], self.rgb[i + 1], self.rgb[i + 2]]
    }
}

/// One 3D Gaussian. 44 bytes, plain old data.
///
/// Covariance is `R * diag(scale^2) * R^T` with `R` from `rot`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct Splat {
    /// Center, world space, meters.
    pub pos: [f32; 3],
    /// Standard deviation along each local axis, meters.
    pub scale: [f32; 3],
    /// Unit quaternion `[x, y, z, w]`, local-to-world.
    pub rot: [f32; 4],
    /// sRGB color and opacity.
    pub rgba: [u8; 4],
}

impl Splat {
    #[inline]
    pub fn position(&self) -> Vec3 {
        Vec3::from_array(self.pos)
    }

    #[inline]
    pub fn rotation(&self) -> Quat {
        Quat::from_array(self.rot)
    }

    /// World-space covariance matrix.
    pub fn covariance(&self) -> Mat3 {
        let r = Mat3::from_quat(self.rotation().normalize());
        let s = Vec3::from_array(self.scale);
        let m = Mat3::from_cols(r.x_axis * s.x, r.y_axis * s.y, r.z_axis * s.z);
        m * m.transpose()
    }

    /// Flat disc-like Gaussian ("surfel") lying in the plane with unit normal
    /// `normal`: standard deviation `radius` in the plane, `thickness` along
    /// the normal.
    pub fn surfel(pos: Vec3, normal: Vec3, radius: f32, thickness: f32, rgba: [u8; 4]) -> Self {
        let rot = Quat::from_rotation_arc(Vec3::Z, normal.normalize_or(Vec3::Z));
        Self { pos: pos.to_array(), scale: [radius, radius, thickness], rot: rot.to_array(), rgba }
    }
}

/// Axis-aligned bounding box.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Aabb {
    pub min: Vec3,
    pub max: Vec3,
}

impl Aabb {
    /// Inverted box that any point will expand.
    pub const EMPTY: Self = Self { min: Vec3::splat(f32::INFINITY), max: Vec3::splat(f32::NEG_INFINITY) };

    pub fn new(min: Vec3, max: Vec3) -> Self {
        Self { min, max }
    }

    pub fn from_points<I: IntoIterator<Item = Vec3>>(points: I) -> Self {
        let mut b = Self::EMPTY;
        for p in points {
            b.expand(p);
        }
        b
    }

    #[inline]
    pub fn expand(&mut self, p: Vec3) {
        self.min = self.min.min(p);
        self.max = self.max.max(p);
    }

    pub fn union(&self, other: &Aabb) -> Aabb {
        Aabb { min: self.min.min(other.min), max: self.max.max(other.max) }
    }

    pub fn is_empty(&self) -> bool {
        self.min.cmpgt(self.max).any()
    }

    pub fn center(&self) -> Vec3 {
        0.5 * (self.min + self.max)
    }

    pub fn size(&self) -> Vec3 {
        self.max - self.min
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cam() -> Camera {
        Camera::look_at(Vec3::new(2.0, 1.5, 3.0), Vec3::new(0.0, 1.0, 0.0), Vec3::Y, 60f32.to_radians(), 160, 120)
    }

    #[test]
    fn project_unproject_roundtrip() {
        let c = cam();
        for &(px, py, z) in &[(0.5, 0.5, 1.0), (80.0, 60.0, 3.5), (159.5, 119.5, 7.0), (10.2, 100.7, 0.3)] {
            let w = c.unproject(px, py, z);
            let (qx, qy, qz) = c.project(w).unwrap();
            assert!((qx - px).abs() < 1e-2 && (qy - py).abs() < 1e-2 && (qz - z).abs() < 1e-4, "{px} {py} {z}");
        }
    }

    #[test]
    fn center_pixel_looks_at_target_and_up_is_up() {
        let c = cam();
        let (px, py, depth) = c.project(Vec3::new(0.0, 1.0, 0.0)).unwrap();
        assert!((px - 80.0).abs() < 1e-3 && (py - 60.0).abs() < 1e-3);
        assert!((depth - (Vec3::new(2.0, 1.5, 3.0) - Vec3::new(0.0, 1.0, 0.0)).length()).abs() < 1e-4);
        // A point above the target projects to a smaller v (higher in the image).
        let (_, py_up, _) = c.project(Vec3::new(0.0, 1.5, 0.0)).unwrap();
        assert!(py_up < py);
        // Behind the camera.
        assert!(c.project(c.position() - c.forward()).is_none());
    }

    #[test]
    fn projection_matrix_matches_project() {
        let c = cam();
        let vp = c.projection_matrix(0.1, 100.0) * c.view_matrix();
        let w = Vec3::new(0.3, 0.8, -0.4);
        let clip = vp * w.extend(1.0);
        let ndc = clip.truncate() / clip.w;
        let (px, py, _) = c.project(w).unwrap();
        let px2 = (ndc.x * 0.5 + 0.5) * c.width as f32;
        let py2 = (1.0 - (ndc.y * 0.5 + 0.5)) * c.height as f32;
        assert!((px - px2).abs() < 1e-2 && (py - py2).abs() < 1e-2);
    }

    #[test]
    fn surfel_covariance_is_flat_along_normal() {
        let n = Vec3::new(1.0, 2.0, -0.5).normalize();
        let s = Splat::surfel(Vec3::ZERO, n, 0.1, 0.001, [255; 4]);
        let cov = s.covariance();
        assert!((n.dot(cov * n) - 1e-6).abs() < 1e-7);
        let t = n.any_orthonormal_vector();
        assert!((t.dot(cov * t) - 0.01).abs() < 1e-5);
    }
}
