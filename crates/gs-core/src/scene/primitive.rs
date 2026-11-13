//! Analytic primitives under affine transforms and their ray intersections.
//!
//! Every primitive is a canonical shape in its own local space, placed in the
//! world by `world_from_local` (rotation, translation and possibly non-uniform
//! scale). Rays are transformed into local space *without* renormalizing the
//! direction, so the ray parameter `t` means the same thing in both spaces.

use glam::{Affine3A, Mat3A, Quat, Vec3, Vec3A};

use super::material::Material;
use crate::types::Aabb;

/// Canonical local-space shapes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Shape {
    /// Unit sphere at the origin. Scale it to get an ellipsoid.
    Sphere,
    /// Cube `[-1, 1]^3`. Scale it by the half extents.
    Box,
    /// Capped cylinder around the Y axis: radius 1, `|y| <= 1`.
    Cylinder,
    /// Capsule around the Y axis: all points within `radius` of the segment
    /// `(0, -half_len, 0) .. (0, half_len, 0)`.
    Capsule { half_len: f32, radius: f32 },
    /// Capped cone frustum around the Y axis for `|y| <= 1`: radius `r0` at
    /// `y = -1` and `r1` at `y = +1`. Either radius may be zero (a true cone).
    Cone { r0: f32, r1: f32 },
}

/// One shape instance: transform, material and the "moves over time" flag.
#[derive(Clone, Debug)]
pub struct Primitive {
    pub shape: Shape,
    pub material: Material,
    /// True when the primitive moves over time (dancer, props). Static
    /// primitives form the "empty plate" of a scene.
    pub dynamic: bool,
    world_from_local: Affine3A,
    local_from_world: Affine3A,
    /// Lengths of the local axes in world units: local point * `tex_scale`
    /// is the object-space position in meters used by material patterns.
    tex_scale: Vec3A,
}

impl Primitive {
    /// Places `shape` with an arbitrary invertible affine transform.
    pub fn new(shape: Shape, world_from_local: Affine3A, material: Material) -> Self {
        let m = world_from_local.matrix3;
        Self {
            shape,
            material,
            dynamic: false,
            world_from_local,
            local_from_world: world_from_local.inverse(),
            tex_scale: Vec3A::new(m.x_axis.length(), m.y_axis.length(), m.z_axis.length()),
        }
    }

    /// Places `shape` in a rigid `frame` with an extra per-axis `scale`
    /// applied in the frame's local axes.
    pub fn in_frame(shape: Shape, frame: Affine3A, scale: Vec3, material: Material) -> Self {
        Self::new(shape, frame * Affine3A::from_scale(scale), material)
    }

    /// Sphere with the given center and radius.
    pub fn sphere(center: Vec3, radius: f32, material: Material) -> Self {
        Self::ellipsoid(center, Quat::IDENTITY, Vec3::splat(radius), material)
    }

    /// Ellipsoid with semi-axes `radii` along its rotated local axes.
    pub fn ellipsoid(center: Vec3, rotation: Quat, radii: Vec3, material: Material) -> Self {
        Self::new(Shape::Sphere, Affine3A::from_scale_rotation_translation(radii, rotation, center), material)
    }

    /// Axis-aligned box from its center and half extents.
    pub fn cuboid(center: Vec3, half_extents: Vec3, material: Material) -> Self {
        Self::oriented_box(center, Quat::IDENTITY, half_extents, material)
    }

    /// Axis-aligned box from two opposite corners.
    pub fn cuboid_min_max(min: Vec3, max: Vec3, material: Material) -> Self {
        Self::cuboid(0.5 * (min + max), 0.5 * (max - min), material)
    }

    /// Rotated box.
    pub fn oriented_box(center: Vec3, rotation: Quat, half_extents: Vec3, material: Material) -> Self {
        Self::new(Shape::Box, Affine3A::from_scale_rotation_translation(half_extents, rotation, center), material)
    }

    /// Capped cylinder whose axis is the rotated local Y axis.
    pub fn cylinder(center: Vec3, rotation: Quat, radius: f32, half_height: f32, material: Material) -> Self {
        let scale = Vec3::new(radius, half_height, radius);
        Self::new(Shape::Cylinder, Affine3A::from_scale_rotation_translation(scale, rotation, center), material)
    }

    /// Upright capped cylinder.
    pub fn cylinder_y(center: Vec3, radius: f32, half_height: f32, material: Material) -> Self {
        Self::cylinder(center, Quat::IDENTITY, radius, half_height, material)
    }

    /// Capsule of the given radius around the segment `a .. b`.
    ///
    /// The roll around the segment is arbitrary; use [`Primitive::in_frame`]
    /// with [`Shape::Capsule`] when a pattern must follow a joint frame.
    pub fn capsule(a: Vec3, b: Vec3, radius: f32, material: Material) -> Self {
        let axis = b - a;
        let len = axis.length();
        let rotation = if len > 1e-9 { Quat::from_rotation_arc(Vec3::Y, axis / len) } else { Quat::IDENTITY };
        let frame = Affine3A::from_rotation_translation(rotation, 0.5 * (a + b));
        Self::new(Shape::Capsule { half_len: 0.5 * len, radius }, frame, material)
    }

    /// Capped cone frustum whose axis is the rotated local Y axis, with
    /// `bottom_radius` at `-half_height` and `top_radius` at `+half_height`.
    pub fn cone(
        center: Vec3,
        rotation: Quat,
        bottom_radius: f32,
        top_radius: f32,
        half_height: f32,
        material: Material,
    ) -> Self {
        let r = bottom_radius.max(top_radius).max(1e-6);
        let scale = Vec3::new(r, half_height, r);
        Self::new(
            Shape::Cone { r0: bottom_radius / r, r1: top_radius / r },
            Affine3A::from_scale_rotation_translation(scale, rotation, center),
            material,
        )
    }

    /// Marks the primitive as moving over time.
    pub fn with_dynamic(mut self, dynamic: bool) -> Self {
        self.dynamic = dynamic;
        self
    }

    /// Local-to-world transform.
    #[inline]
    pub fn world_from_local(&self) -> &Affine3A {
        &self.world_from_local
    }

    /// World-to-local transform.
    #[inline]
    pub fn local_from_world(&self) -> &Affine3A {
        &self.local_from_world
    }

    /// Tight world-space bounding box.
    pub fn world_aabb(&self) -> Aabb {
        let m = &self.world_from_local.matrix3;
        let (ax, ay, az) = (m.x_axis, m.y_axis, m.z_axis);
        // Extent of a transformed unit ball along each world axis: the norm of
        // the matching matrix row.
        let ball = (ax * ax + ay * ay + az * az).powf(0.5);
        let half = match self.shape {
            Shape::Sphere => ball,
            Shape::Box => ax.abs() + ay.abs() + az.abs(),
            Shape::Cylinder => ay.abs() + (ax * ax + az * az).powf(0.5),
            Shape::Cone { r0, r1 } => ay.abs() + r0.max(r1) * (ax * ax + az * az).powf(0.5),
            Shape::Capsule { half_len, radius } => half_len * ay.abs() + radius * ball,
        };
        let c = self.world_from_local.translation;
        Aabb::new((c - half).into(), (c + half).into())
    }

    /// Nearest intersection with the world-space ray `origin + t * dir` for
    /// `t` in `(t_min, t_max)`. Returns `t` and the (unnormalized, arbitrarily
    /// signed) local-space normal; convert it with [`Primitive::world_normal`].
    #[inline]
    pub(crate) fn intersect(&self, origin: Vec3A, dir: Vec3A, t_min: f32, t_max: f32) -> Option<(f32, Vec3A)> {
        let o = self.local_from_world.transform_point3a(origin);
        let d = self.local_from_world.matrix3 * dir;
        if let Shape::Box = self.shape {
            return hit_box(o, d, t_min, t_max);
        }
        // Re-origin the ray at its closest approach to the shape center. The
        // quadratics below then work on small numbers even when the camera is
        // hundreds of local units away, which avoids catastrophic cancellation.
        let dd = d.length_squared();
        if dd < 1e-30 {
            return None;
        }
        let shift = -o.dot(d) / dd;
        let o = o + d * shift;
        let (lo, hi) = (t_min - shift, t_max - shift);
        let hit = match self.shape {
            Shape::Sphere => hit_sphere(o, d, dd, lo, hi),
            Shape::Cylinder => hit_cylinder(o, d, lo, hi),
            Shape::Capsule { half_len, radius } => hit_capsule(o, d, half_len, radius, lo, hi),
            Shape::Cone { r0, r1 } => hit_cone(o, d, r0, r1, lo, hi),
            Shape::Box => unreachable!(),
        };
        hit.map(|(t, n)| (t + shift, n))
    }

    /// Unit world-space normal for a local-space normal (inverse-transpose).
    #[inline]
    pub(crate) fn world_normal(&self, local_normal: Vec3A) -> Vec3A {
        let m: &Mat3A = &self.local_from_world.matrix3;
        m.transpose().mul_vec3a(local_normal).normalize_or_zero()
    }

    /// Object-space position (meters) of a world point, for material lookup.
    #[inline]
    pub(crate) fn object_point(&self, world: Vec3A) -> Vec3 {
        (self.local_from_world.transform_point3a(world) * self.tex_scale).into()
    }

    /// Diffuse albedo (linear RGB) at a world-space surface point.
    #[inline]
    pub fn albedo_at(&self, world: Vec3) -> Vec3 {
        self.material.albedo(self.object_point(world.into()))
    }
}

/// Picks the entry of the convex interval `[enter, exit]`, or its exit when the
/// ray starts inside. Returns `(t, is_entry)`.
#[inline]
fn pick_interval(enter: f32, exit: f32, t_min: f32, t_max: f32) -> Option<(f32, bool)> {
    if enter > exit {
        None
    } else if enter > t_min {
        (enter < t_max).then_some((enter, true))
    } else if exit > t_min && exit < t_max {
        Some((exit, false))
    } else {
        None
    }
}

/// Unit sphere; `o` is already the closest point of the ray to the center.
#[inline]
fn hit_sphere(o: Vec3A, d: Vec3A, dd: f32, t_min: f32, t_max: f32) -> Option<(f32, Vec3A)> {
    let disc = 1.0 - o.length_squared();
    if disc < 0.0 {
        return None;
    }
    let h = (disc / dd).sqrt();
    let (t, _) = pick_interval(-h, h, t_min, t_max)?;
    Some((t, o + d * t))
}

/// Box `[-1, 1]^3` by slabs.
#[inline]
fn hit_box(o: Vec3A, d: Vec3A, t_min: f32, t_max: f32) -> Option<(f32, Vec3A)> {
    let inv = safe_recip(d);
    let ta = (Vec3A::NEG_ONE - o) * inv;
    let tb = (Vec3A::ONE - o) * inv;
    let near = ta.min(tb);
    let far = ta.max(tb);
    let (t, entry) = pick_interval(near.max_element(), far.min_element(), t_min, t_max)?;
    let v = if entry { near } else { far };
    // The face that produced `t`.
    let n = if v.x == t {
        Vec3A::X
    } else if v.y == t {
        Vec3A::Y
    } else {
        Vec3A::Z
    };
    Some((t, n))
}

/// Component-wise reciprocal that maps zero components to a huge finite value
/// of the right magnitude, so `0 * inf = NaN` never appears in slab tests.
#[inline]
pub(crate) fn safe_recip(d: Vec3A) -> Vec3A {
    const TINY: f32 = 1.0e-20;
    let d = Vec3A::select(d.abs().cmplt(Vec3A::splat(TINY)), Vec3A::splat(TINY), d);
    d.recip()
}

/// Ray parameters where the XZ projection of the ray crosses the circle of
/// squared radius `r2` around the Y axis. `None` when the ray misses the
/// infinite cylinder; an infinite interval when it runs parallel inside it.
#[inline]
fn circle_interval(o: Vec3A, d: Vec3A, r2: f32) -> Option<(f32, f32)> {
    let a = d.x * d.x + d.z * d.z;
    if a < 1e-24 {
        return (o.x * o.x + o.z * o.z <= r2).then_some((f32::NEG_INFINITY, f32::INFINITY));
    }
    let tc = -(o.x * d.x + o.z * d.z) / a;
    let (qx, qz) = (o.x + tc * d.x, o.z + tc * d.z);
    let disc = r2 - (qx * qx + qz * qz);
    if disc < 0.0 {
        return None;
    }
    let h = (disc / a).sqrt();
    Some((tc - h, tc + h))
}

/// Capped cylinder: radius 1, `|y| <= 1`.
#[inline]
fn hit_cylinder(o: Vec3A, d: Vec3A, t_min: f32, t_max: f32) -> Option<(f32, Vec3A)> {
    let (s0, s1) = circle_interval(o, d, 1.0)?;
    let (c0, c1) = if d.y.abs() < 1e-12 {
        if o.y.abs() > 1.0 {
            return None;
        }
        (f32::NEG_INFINITY, f32::INFINITY)
    } else {
        let inv = 1.0 / d.y;
        let (a, b) = ((-1.0 - o.y) * inv, (1.0 - o.y) * inv);
        (a.min(b), a.max(b))
    };
    let (t, entry) = pick_interval(s0.max(c0), s1.min(c1), t_min, t_max)?;
    let from_side = if entry { s0 >= c0 } else { s1 <= c1 };
    let p = o + d * t;
    Some((t, if from_side { Vec3A::new(p.x, 0.0, p.z) } else { Vec3A::Y }))
}

/// Capsule around the Y axis.
#[inline]
fn hit_capsule(o: Vec3A, d: Vec3A, half_len: f32, radius: f32, t_min: f32, t_max: f32) -> Option<(f32, Vec3A)> {
    let r2 = radius * radius;
    // The capsule lies inside the infinite cylinder: missing it misses both.
    let (s0, s1) = circle_interval(o, d, r2)?;
    let normal = |t: f32| {
        let p = o + d * t;
        Vec3A::new(p.x, p.y - p.y.clamp(-half_len, half_len), p.z)
    };
    let in_range = |t: f32| t > t_min && t < t_max;

    // Entering the cylinder wall between the caps is entering the capsule.
    if in_range(s0) && (o.y + s0 * d.y).abs() <= half_len {
        return Some((s0, normal(s0)));
    }

    let mut best = f32::INFINITY;
    if in_range(s1) && (o.y + s1 * d.y).abs() <= half_len {
        best = s1;
    }
    let dd = d.length_squared();
    for cy in [-half_len, half_len] {
        // Sphere around the segment end; only its outer hemisphere is surface.
        let oc = Vec3A::new(o.x, o.y - cy, o.z);
        let tc = -oc.dot(d) / dd;
        let q = oc + d * tc;
        let disc = r2 - q.length_squared();
        if disc < 0.0 {
            continue;
        }
        let h = (disc / dd).sqrt();
        for t in [tc - h, tc + h] {
            if in_range(t) && t < best && (oc.y + t * d.y) * cy >= 0.0 {
                best = t;
            }
        }
    }
    (best < f32::INFINITY).then(|| (best, normal(best)))
}

/// Capped cone frustum: radius `r0` at `y = -1`, `r1` at `y = +1`.
#[inline]
fn hit_cone(o: Vec3A, d: Vec3A, r0: f32, r1: f32, t_min: f32, t_max: f32) -> Option<(f32, Vec3A)> {
    // Radius as a function of height: r(y) = k * y + m.
    let k = 0.5 * (r1 - r0);
    let m = 0.5 * (r0 + r1);
    let mut best = f32::INFINITY;
    let mut best_n = Vec3A::Y;
    let mut consider = |t: f32, n: Vec3A| {
        if t > t_min && t < t_max && t < best {
            best = t;
            best_n = n;
        }
    };

    // Caps.
    if d.y.abs() > 1e-12 {
        for (y, r) in [(-1.0f32, r0), (1.0, r1)] {
            let t = (y - o.y) / d.y;
            let (px, pz) = (o.x + t * d.x, o.z + t * d.z);
            if px * px + pz * pz <= r * r {
                consider(t, Vec3A::Y);
            }
        }
    }

    // Side: x^2 + z^2 = r(y)^2, a quadratic a t^2 + 2 b t + c = 0.
    let w = k * o.y + m;
    let a = d.x * d.x + d.z * d.z - k * k * d.y * d.y;
    let b = o.x * d.x + o.z * d.z - k * w * d.y;
    let c = o.x * o.x + o.z * o.z - w * w;
    let mut side = |t: f32| {
        let p = o + d * t;
        let r = k * p.y + m;
        // Reject the mirror nappe (negative radius) and points beyond the caps.
        if p.y.abs() <= 1.0 && r >= 0.0 {
            consider(t, Vec3A::new(p.x, -k * r, p.z));
        }
    };
    if a.abs() < 1e-9 * d.length_squared() {
        // Ray parallel to the cone surface: a single crossing.
        if b.abs() > 1e-12 {
            side(-0.5 * c / b);
        }
    } else {
        let disc = b * b - a * c;
        if disc >= 0.0 {
            let q = -(b + b.signum() * disc.sqrt());
            side(q / a);
            if q.abs() > 1e-30 {
                side(c / q);
            }
        }
    }
    (best < f32::INFINITY).then_some((best, best_n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::f32::consts::FRAC_PI_2;

    fn mat() -> Material {
        Material::default()
    }

    /// Intersects and returns `(t, unit world normal facing the ray)`.
    fn cast(p: &Primitive, origin: Vec3, dir: Vec3) -> Option<(f32, Vec3)> {
        let dir = dir.normalize();
        let (t, n) = p.intersect(origin.into(), dir.into(), 1e-4, f32::INFINITY)?;
        let mut n: Vec3 = p.world_normal(n).into();
        if n.dot(dir) > 0.0 {
            n = -n;
        }
        Some((t, n))
    }

    fn assert_close(a: Vec3, b: Vec3, eps: f32) {
        assert!((a - b).length() < eps, "{a:?} != {b:?}");
    }

    #[test]
    fn sphere_hit_miss_and_inside() {
        let s = Primitive::sphere(Vec3::new(1.0, 2.0, 3.0), 0.5, mat());
        let (t, n) = cast(&s, Vec3::new(1.0, 2.0, 10.0), Vec3::NEG_Z).unwrap();
        assert!((t - 6.5).abs() < 1e-5);
        assert_close(n, Vec3::Z, 1e-5);
        assert!(cast(&s, Vec3::new(1.0, 2.6, 10.0), Vec3::NEG_Z).is_none());
        assert!(cast(&s, Vec3::new(1.0, 2.0, 10.0), Vec3::Z).is_none(), "behind the ray");
        // From the center the exit point is hit and the normal faces back in.
        let (t, n) = cast(&s, Vec3::new(1.0, 2.0, 3.0), Vec3::X).unwrap();
        assert!((t - 0.5).abs() < 1e-5);
        assert_close(n, Vec3::NEG_X, 1e-5);
    }

    #[test]
    fn tiny_far_sphere_is_precise() {
        // 1 cm ball seen from 40 m: 4000 local units away.
        let s = Primitive::sphere(Vec3::new(40.0, 0.0, 0.0), 0.01, mat());
        let (t, n) = cast(&s, Vec3::ZERO, Vec3::X).unwrap();
        assert!((t - 39.99).abs() < 1e-4, "{t}");
        assert_close(n, Vec3::NEG_X, 1e-3);
        let (t, _) = cast(&s, Vec3::ZERO, Vec3::new(40.0, 0.009, 0.0)).unwrap();
        let expect = 40.0 - (0.01f32 * 0.01 - 0.009 * 0.009).sqrt();
        assert!((t - expect).abs() < 2e-4, "{t} vs {expect}");
        assert!(cast(&s, Vec3::ZERO, Vec3::new(40.0, 0.011, 0.0)).is_none());
    }

    #[test]
    fn ellipsoid_non_uniform_scale_normal_uses_inverse_transpose() {
        let radii = Vec3::new(2.0, 0.5, 1.0);
        let e = Primitive::ellipsoid(Vec3::ZERO, Quat::IDENTITY, radii, mat());
        // Hit point chosen on the surface, approached along an arbitrary ray.
        let u = Vec3::new(0.6, 0.64, 0.48); // unit
        let p = u * radii;
        let origin = Vec3::new(3.0, 4.0, 5.0);
        let (t, n) = cast(&e, origin, p - origin).unwrap();
        assert!((t - (p - origin).length()).abs() < 1e-4);
        let expect = (p / (radii * radii)).normalize(); // gradient of the implicit
        assert_close(n, expect, 1e-4);
        assert!((n.length() - 1.0).abs() < 1e-5);
        assert!(n.dot((p - origin).normalize()) < 0.0);
    }

    #[test]
    fn rotated_ellipsoid_extent() {
        // Long axis (local X, 2 m) rotated onto world Z.
        let e = Primitive::ellipsoid(Vec3::new(0.0, 1.0, 0.0), Quat::from_rotation_y(FRAC_PI_2), Vec3::new(2.0, 0.5, 1.0), mat());
        let (t, n) = cast(&e, Vec3::new(0.0, 1.0, 10.0), Vec3::NEG_Z).unwrap();
        assert!((t - 8.0).abs() < 1e-4);
        assert_close(n, Vec3::Z, 1e-4);
        let (t, _) = cast(&e, Vec3::new(10.0, 1.0, 0.0), Vec3::NEG_X).unwrap();
        assert!((t - 9.0).abs() < 1e-4);
        let b = e.world_aabb();
        assert_close(b.min, Vec3::new(-1.0, 0.5, -2.0), 1e-4);
        assert_close(b.max, Vec3::new(1.0, 1.5, 2.0), 1e-4);
    }

    #[test]
    fn box_faces_edges_and_inside() {
        let b = Primitive::cuboid(Vec3::new(0.0, 1.0, 0.0), Vec3::new(1.0, 0.5, 2.0), mat());
        let (t, n) = cast(&b, Vec3::new(0.2, 5.0, 0.3), Vec3::NEG_Y).unwrap();
        assert!((t - 3.5).abs() < 1e-5);
        assert_close(n, Vec3::Y, 1e-6);
        let (t, n) = cast(&b, Vec3::new(-4.0, 1.2, 1.5), Vec3::X).unwrap();
        assert!((t - 3.0).abs() < 1e-5);
        assert_close(n, Vec3::NEG_X, 1e-6);
        assert!(cast(&b, Vec3::new(-4.0, 1.6, 1.5), Vec3::X).is_none());
        // Axis-parallel ray lying exactly in a face plane must not produce NaN.
        let grazing = cast(&b, Vec3::new(-4.0, 1.5, 0.0), Vec3::X);
        assert!(grazing.is_none_or(|(t, _)| t.is_finite()));
        // From inside: exit face, normal turned toward the origin.
        let (t, n) = cast(&b, Vec3::new(0.0, 1.0, 0.0), Vec3::Z).unwrap();
        assert!((t - 2.0).abs() < 1e-5);
        assert_close(n, Vec3::NEG_Z, 1e-6);
    }

    #[test]
    fn oriented_box_diagonal() {
        // Unit cube rotated 45 degrees about Y: its corner points at +X.
        let b = Primitive::oriented_box(Vec3::ZERO, Quat::from_rotation_y(core::f32::consts::FRAC_PI_4), Vec3::ONE, mat());
        let (t, _) = cast(&b, Vec3::new(5.0, 0.0, 0.0), Vec3::NEG_X).unwrap();
        assert!((t - (5.0 - 2f32.sqrt())).abs() < 1e-4);
        let (t, n) = cast(&b, Vec3::new(5.0, 0.0, 0.5), Vec3::NEG_X).unwrap();
        assert!((t - (5.0 - 2f32.sqrt() + 0.5)).abs() < 1e-4);
        assert_close(n, Vec3::new(1.0, 0.0, 1.0).normalize(), 1e-4);
        let bb = b.world_aabb();
        assert!((bb.max.x - 2f32.sqrt()).abs() < 1e-4 && (bb.max.y - 1.0).abs() < 1e-5);
    }

    #[test]
    fn cylinder_side_caps_and_axis() {
        let c = Primitive::cylinder_y(Vec3::new(0.0, 1.0, 0.0), 0.5, 1.0, mat());
        let (t, n) = cast(&c, Vec3::new(3.0, 1.2, 0.0), Vec3::NEG_X).unwrap();
        assert!((t - 2.5).abs() < 1e-5);
        assert_close(n, Vec3::X, 1e-5);
        let (t, n) = cast(&c, Vec3::new(0.1, 5.0, 0.2), Vec3::NEG_Y).unwrap();
        assert!((t - 3.0).abs() < 1e-5);
        assert_close(n, Vec3::Y, 1e-6);
        let (t, n) = cast(&c, Vec3::new(0.1, -5.0, 0.2), Vec3::Y).unwrap();
        assert!((t - 5.0).abs() < 1e-5);
        assert_close(n, Vec3::NEG_Y, 1e-6);
        assert!(cast(&c, Vec3::new(0.6, 5.0, 0.0), Vec3::NEG_Y).is_none());
        assert!(cast(&c, Vec3::new(3.0, 2.1, 0.0), Vec3::NEG_X).is_none());
        // Oblique ray entering through the top cap.
        let target = Vec3::new(0.2, 2.0, 0.1);
        let origin = Vec3::new(1.2, 4.0, 0.6);
        let (t, n) = cast(&c, origin, target - origin).unwrap();
        assert!((t - (target - origin).length()).abs() < 1e-4);
        assert_close(n, Vec3::Y, 1e-6);
    }

    #[test]
    fn rotated_cylinder() {
        // Axis along world X, length 4, radius 0.25.
        let c = Primitive::cylinder(Vec3::ZERO, Quat::from_rotation_z(-FRAC_PI_2), 0.25, 2.0, mat());
        let (t, n) = cast(&c, Vec3::new(1.5, 3.0, 0.0), Vec3::NEG_Y).unwrap();
        assert!((t - 2.75).abs() < 1e-5);
        assert_close(n, Vec3::Y, 1e-5);
        let (t, n) = cast(&c, Vec3::new(6.0, 0.1, 0.0), Vec3::NEG_X).unwrap();
        assert!((t - 4.0).abs() < 1e-5);
        assert_close(n, Vec3::X, 1e-5);
        let bb = c.world_aabb();
        assert_close(bb.max, Vec3::new(2.0, 0.25, 0.25), 1e-5);
    }

    #[test]
    fn capsule_side_and_caps() {
        let c = Primitive::capsule(Vec3::new(0.0, 0.0, 0.0), Vec3::new(0.0, 2.0, 0.0), 0.5, mat());
        let (t, n) = cast(&c, Vec3::new(3.0, 1.0, 0.0), Vec3::NEG_X).unwrap();
        assert!((t - 2.5).abs() < 1e-5);
        assert_close(n, Vec3::X, 1e-5);
        // Top hemisphere from above.
        let (t, n) = cast(&c, Vec3::new(0.0, 5.0, 0.0), Vec3::NEG_Y).unwrap();
        assert!((t - 2.5).abs() < 1e-5);
        assert_close(n, Vec3::Y, 1e-5);
        // Hemisphere hit off-axis: compare with the equivalent sphere.
        let origin = Vec3::new(2.0, 3.5, 0.3);
        let dir = Vec3::new(0.1, 2.2, 0.05) - origin;
        let sphere = Primitive::sphere(Vec3::new(0.0, 2.0, 0.0), 0.5, mat());
        let (tc, nc) = cast(&c, origin, dir).unwrap();
        let (ts, ns) = cast(&sphere, origin, dir).unwrap();
        assert!((tc - ts).abs() < 1e-4);
        assert_close(nc, ns, 1e-3);
        // Misses beyond the round end and beside the body.
        assert!(cast(&c, Vec3::new(3.0, 2.6, 0.0), Vec3::NEG_X).is_none());
        assert!(cast(&c, Vec3::new(3.0, 1.0, 0.6), Vec3::NEG_X).is_none());
        // Along the axis from below.
        let (t, n) = cast(&c, Vec3::new(0.0, -4.0, 0.0), Vec3::Y).unwrap();
        assert!((t - 3.5).abs() < 1e-5);
        assert_close(n, Vec3::NEG_Y, 1e-5);
        // From inside: exits through the side.
        let (t, n) = cast(&c, Vec3::new(0.0, 1.0, 0.0), Vec3::X).unwrap();
        assert!((t - 0.5).abs() < 1e-5);
        assert_close(n, Vec3::NEG_X, 1e-5);
        let bb = c.world_aabb();
        assert_close(bb.min, Vec3::new(-0.5, -0.5, -0.5), 1e-5);
        assert_close(bb.max, Vec3::new(0.5, 2.5, 0.5), 1e-5);
    }

    #[test]
    fn slanted_capsule_matches_point_segment_distance() {
        let (a, b, r) = (Vec3::new(-0.4, 0.2, 0.1), Vec3::new(0.7, 1.3, -0.5), 0.15);
        let c = Primitive::capsule(a, b, r, mat());
        let mut hits = 0;
        for i in 0..400 {
            let f = i as f32;
            let origin = Vec3::new((f * 0.37).sin() * 3.0, 0.7 + (f * 0.11).cos() * 2.0, (f * 0.23).cos() * 3.0);
            let target = a.lerp(b, (f * 0.619).fract()) + 0.25 * Vec3::new((f * 1.3).sin(), (f * 2.1).cos(), (f * 0.7).sin());
            if let Some((t, n)) = cast(&c, origin, target - origin) {
                hits += 1;
                let p = origin + (target - origin).normalize() * t;
                let s = ((p - a).dot(b - a) / (b - a).length_squared()).clamp(0.0, 1.0);
                let closest = a.lerp(b, s);
                assert!(((p - closest).length() - r).abs() < 1e-4, "hit point on the surface");
                assert_close(n, (p - closest).normalize(), 2e-3);
            }
        }
        assert!(hits > 100);
    }

    #[test]
    fn cone_frustum_side_caps_and_apex() {
        // Bottom radius 1 at y = 0, top radius 0.5 at y = 2.
        let c = Primitive::cone(Vec3::new(0.0, 1.0, 0.0), Quat::IDENTITY, 1.0, 0.5, 1.0, mat());
        // Horizontal ray at mid height: radius there is 0.75.
        let (t, n) = cast(&c, Vec3::new(5.0, 1.0, 0.0), Vec3::NEG_X).unwrap();
        assert!((t - 4.25).abs() < 1e-5);
        // Side slope: radius shrinks 0.25 per unit height, so n ~ (1, 0.25, 0).
        assert_close(n, Vec3::new(1.0, 0.25, 0.0).normalize(), 1e-4);
        let (t, n) = cast(&c, Vec3::new(0.3, 5.0, 0.0), Vec3::NEG_Y).unwrap();
        assert!((t - 3.0).abs() < 1e-5);
        assert_close(n, Vec3::Y, 1e-6);
        // Down the side outside the top cap: hits the slanted wall.
        let (t, _) = cast(&c, Vec3::new(0.75, 5.0, 0.0), Vec3::NEG_Y).unwrap();
        assert!((t - 4.0).abs() < 1e-4);
        assert!(cast(&c, Vec3::new(1.01, 5.0, 0.0), Vec3::NEG_Y).is_none());
        let (t, n) = cast(&c, Vec3::new(0.9, -3.0, 0.0), Vec3::Y).unwrap();
        assert!((t - 3.0).abs() < 1e-5);
        assert_close(n, Vec3::NEG_Y, 1e-6);

        // True cone with the apex up; a ray above the apex must not hit the mirror nappe.
        let cone = Primitive::cone(Vec3::new(0.0, 1.0, 0.0), Quat::IDENTITY, 1.0, 0.0, 1.0, mat());
        assert!(cast(&cone, Vec3::new(5.0, 2.5, 0.0), Vec3::NEG_X).is_none());
        let (t, n) = cast(&cone, Vec3::new(5.0, 1.0, 0.0), Vec3::NEG_X).unwrap();
        assert!((t - 4.5).abs() < 1e-5);
        assert_close(n, Vec3::new(1.0, 0.5, 0.0).normalize(), 1e-4);
        // Along the axis from below: bottom cap.
        let (t, _) = cast(&cone, Vec3::new(0.0, -2.0, 0.0), Vec3::Y).unwrap();
        assert!((t - 2.0).abs() < 1e-5);
    }

    #[test]
    fn cone_with_unit_radii_equals_cylinder() {
        let frame = Affine3A::from_scale_rotation_translation(
            Vec3::new(0.4, 0.9, 0.7),
            Quat::from_euler(glam::EulerRot::XYZ, 0.3, -0.8, 1.1),
            Vec3::new(0.2, 1.0, -0.3),
        );
        let cyl = Primitive::new(Shape::Cylinder, frame, mat());
        let cone = Primitive::new(Shape::Cone { r0: 1.0, r1: 1.0 }, frame, mat());
        let mut hits = 0;
        for i in 0..500 {
            let f = i as f32;
            let origin = Vec3::new((f * 0.31).sin() * 4.0, 1.0 + (f * 0.17).cos() * 3.0, (f * 0.13).cos() * 4.0);
            let target = Vec3::new(0.2, 1.0, -0.3) + 0.8 * Vec3::new((f * 1.7).sin(), (f * 0.9).cos(), (f * 2.3).sin());
            let (a, b) = (cast(&cyl, origin, target - origin), cast(&cone, origin, target - origin));
            assert_eq!(a.is_some(), b.is_some(), "ray {i}");
            if let (Some((ta, na)), Some((tb, nb))) = (a, b) {
                hits += 1;
                assert!((ta - tb).abs() < 1e-3, "ray {i}: {ta} vs {tb}");
                // Normals may legitimately differ exactly on the rim.
                if na.dot(nb) < 0.999 {
                    let p = cyl.local_from_world().transform_point3(origin + (target - origin).normalize() * ta);
                    assert!((p.y.abs() - 1.0).abs() < 1e-2, "ray {i}");
                }
            }
        }
        assert!(hits > 150);
    }

    #[test]
    fn world_aabb_contains_surface_points() {
        let frame = Affine3A::from_scale_rotation_translation(
            Vec3::new(0.5, 1.4, 0.8),
            Quat::from_euler(glam::EulerRot::XYZ, 0.7, 0.2, -1.1),
            Vec3::new(1.0, 2.0, 3.0),
        );
        let shapes = [
            Shape::Sphere,
            Shape::Box,
            Shape::Cylinder,
            Shape::Capsule { half_len: 0.8, radius: 0.6 },
            Shape::Cone { r0: 1.0, r1: 0.3 },
        ];
        for shape in shapes {
            let p = Primitive::new(shape, frame, mat());
            let bb = p.world_aabb();
            let mut touched = Aabb::EMPTY;
            for i in 0..3000 {
                let f = i as f32;
                let dir = Vec3::new((f * 0.731).sin(), (f * 0.317).cos(), (f * 1.113).sin()).normalize();
                let origin = Vec3::new(1.0, 2.0, 3.0) - dir * 10.0 + dir.any_orthonormal_vector() * ((f * 0.05).sin() * 1.5);
                if let Some((t, _)) = cast(&p, origin, dir) {
                    let hit = origin + dir * t;
                    touched.expand(hit);
                    assert!(hit.cmpge(bb.min - 1e-3).all() && hit.cmple(bb.max + 1e-3).all(), "{shape:?} {hit:?} outside {bb:?}");
                }
            }
            // The box is reasonably tight, not wildly conservative.
            assert!((bb.size() - touched.size()).max_element() < 0.6, "{shape:?}: {bb:?} vs {touched:?}");
        }
    }

    #[test]
    fn object_space_is_metric_and_follows_the_primitive() {
        let rot = Quat::from_rotation_z(FRAC_PI_2);
        let p = Primitive::oriented_box(Vec3::new(5.0, 0.0, 0.0), rot, Vec3::new(2.0, 0.1, 0.1), mat());
        // Local +X (2 m long) points along world +Y after the rotation.
        let q = p.object_point(Vec3A::new(5.0, 1.5, 0.0));
        assert_close(q, Vec3::new(1.5, 0.0, 0.0), 1e-5);
    }
}
