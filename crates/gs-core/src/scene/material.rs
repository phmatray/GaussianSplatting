//! Procedural surface materials.
//!
//! Patterns are evaluated in the primitive's *object space* (meters, in the
//! primitive's own rotated/translated frame), so a texture sticks to an
//! animated limb instead of swimming through it.

use glam::Vec3;

use super::srgb8_to_linear;

/// Linear RGB color from a `0xRRGGBB` sRGB hex code.
#[inline]
pub fn srgb(hex: u32) -> Vec3 {
    srgb8_to_linear([(hex >> 16) as u8, (hex >> 8) as u8, hex as u8])
}

/// A coordinate axis of a primitive's object space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Axis {
    X,
    Y,
    Z,
}

impl Axis {
    /// Splits `p` into the coordinate along the axis and the distance from it.
    #[inline]
    fn split(self, p: Vec3) -> (f32, f32) {
        match self {
            Axis::X => (p.x, p.y.hypot(p.z)),
            Axis::Y => (p.y, p.x.hypot(p.z)),
            Axis::Z => (p.z, p.x.hypot(p.y)),
        }
    }

    /// Angle around the axis in `[-pi, pi]`.
    #[inline]
    fn angle(self, p: Vec3) -> f32 {
        match self {
            Axis::X => p.z.atan2(p.y),
            Axis::Y => p.x.atan2(p.z),
            Axis::Z => p.y.atan2(p.x),
        }
    }
}

/// How the two colors of a [`Material`] are distributed over the surface.
///
/// All lengths are meters in object space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Pattern {
    /// Color `a` everywhere.
    Solid,
    /// Checkerboard with a per-axis cell size. An axis whose cell size is
    /// `f32::INFINITY` is ignored, which turns the 3D checker into tiles
    /// (two finite axes) or bands (one finite axis).
    Checker { cell: Vec3 },
    /// Alternating bands of `a` and `b`, each `width` thick, stacked along `axis`.
    Stripes { axis: Axis, width: f32 },
    /// Concentric alternating shells around `axis`, each `width` thick.
    Rings { axis: Axis, width: f32 },
    /// Polar checkerboard around `axis`: rings of `ring_width` cut into
    /// `sectors` angular wedges.
    Polar { axis: Axis, ring_width: f32, sectors: u32 },
    /// Balls of color `b` with the given `radius` on a cubic lattice of pitch
    /// `spacing`, in a field of `a`. Cut by a surface they read as polka dots.
    Dots { spacing: f32, radius: f32 },
    /// Linear blend from `a` (at coordinate `from`) to `b` (at `to`) along `axis`.
    Gradient { axis: Axis, from: f32, to: f32 },
    /// Every cell gets its own pseudo-random blend between `a` and `b`
    /// (planks, bricks, speckled carpet). `f32::INFINITY` ignores an axis.
    Cells { cell: Vec3 },
}

/// Diffuse surface description: two linear RGB colors and a pattern mixing them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Material {
    /// Primary color, linear RGB.
    pub a: Vec3,
    /// Secondary color, linear RGB.
    pub b: Vec3,
    pub pattern: Pattern,
    /// Self-illumination factor: `albedo * emissive` is added to the shaded
    /// color regardless of lighting (windows, screens, lamp shades).
    pub emissive: f32,
}

// Keeps cell boundaries off faces that sit exactly on a multiple of the cell size.
const CELL_BIAS: f32 = 1.0e-4;

#[inline]
fn cell_index(v: f32, size: f32) -> i32 {
    ((v + CELL_BIAS) / size).floor() as i32
}

#[inline]
fn hash3(x: i32, y: i32, z: i32) -> u32 {
    let mut h = (x as u32).wrapping_mul(0x9E37_79B1) ^ (y as u32).wrapping_mul(0x85EB_CA77) ^ (z as u32).wrapping_mul(0xC2B2_AE3D);
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^= h >> 12;
    h = h.wrapping_mul(0x297A_2D39);
    h ^ (h >> 15)
}

impl Material {
    /// Single-color material.
    pub const fn solid(color: Vec3) -> Self {
        Self { a: color, b: color, pattern: Pattern::Solid, emissive: 0.0 }
    }

    /// Two-color material with an arbitrary pattern.
    pub const fn new(a: Vec3, b: Vec3, pattern: Pattern) -> Self {
        Self { a, b, pattern, emissive: 0.0 }
    }

    /// 3D checkerboard with cubic cells.
    pub fn checker(a: Vec3, b: Vec3, size: f32) -> Self {
        Self::new(a, b, Pattern::Checker { cell: Vec3::splat(size) })
    }

    /// Square tiles in the object-space XZ plane (floors).
    pub fn tiles(a: Vec3, b: Vec3, size: f32) -> Self {
        Self::new(a, b, Pattern::Checker { cell: Vec3::new(size, f32::INFINITY, size) })
    }

    /// Alternating bands along `axis`.
    pub fn stripes(a: Vec3, b: Vec3, axis: Axis, width: f32) -> Self {
        Self::new(a, b, Pattern::Stripes { axis, width })
    }

    /// Concentric shells around `axis`.
    pub fn rings(a: Vec3, b: Vec3, axis: Axis, width: f32) -> Self {
        Self::new(a, b, Pattern::Rings { axis, width })
    }

    /// Polar checkerboard around `axis`.
    pub fn polar(a: Vec3, b: Vec3, axis: Axis, ring_width: f32, sectors: u32) -> Self {
        Self::new(a, b, Pattern::Polar { axis, ring_width, sectors })
    }

    /// Polka dots of color `b` on `a`.
    pub fn dots(a: Vec3, b: Vec3, spacing: f32, radius: f32) -> Self {
        Self::new(a, b, Pattern::Dots { spacing, radius })
    }

    /// Linear gradient along `axis` between two object-space coordinates.
    pub fn gradient(a: Vec3, b: Vec3, axis: Axis, from: f32, to: f32) -> Self {
        Self::new(a, b, Pattern::Gradient { axis, from, to })
    }

    /// Randomly toned cells (planks, bricks, speckle).
    pub fn cells(a: Vec3, b: Vec3, cell: Vec3) -> Self {
        Self::new(a, b, Pattern::Cells { cell })
    }

    /// Returns the material with a self-illumination factor.
    pub fn with_emissive(mut self, emissive: f32) -> Self {
        self.emissive = emissive;
        self
    }

    /// Diffuse albedo (linear RGB) at object-space point `p`.
    pub fn albedo(&self, p: Vec3) -> Vec3 {
        match self.pattern {
            Pattern::Solid => self.a,
            Pattern::Checker { cell } => {
                let parity = cell_index(p.x, cell.x) + cell_index(p.y, cell.y) + cell_index(p.z, cell.z);
                self.pick(parity)
            }
            Pattern::Stripes { axis, width } => self.pick(cell_index(axis.split(p).0, width)),
            Pattern::Rings { axis, width } => self.pick(cell_index(axis.split(p).1, width)),
            Pattern::Polar { axis, ring_width, sectors } => {
                let ring = cell_index(axis.split(p).1, ring_width);
                let turn = axis.angle(p) / core::f32::consts::TAU + 0.5;
                let sector = (turn * sectors as f32).floor() as i32;
                self.pick(ring + sector)
            }
            Pattern::Dots { spacing, radius } => {
                let q = p / spacing;
                let d = (q - q.round()) * spacing;
                if d.length_squared() < radius * radius {
                    self.b
                } else {
                    self.a
                }
            }
            Pattern::Gradient { axis, from, to } => {
                let s = ((axis.split(p).0 - from) / (to - from)).clamp(0.0, 1.0);
                self.a.lerp(self.b, s)
            }
            Pattern::Cells { cell } => {
                let h = hash3(cell_index(p.x, cell.x), cell_index(p.y, cell.y), cell_index(p.z, cell.z));
                self.a.lerp(self.b, (h >> 8) as f32 / (1u32 << 24) as f32)
            }
        }
    }

    #[inline]
    fn pick(&self, parity: i32) -> Vec3 {
        if parity & 1 == 0 {
            self.a
        } else {
            self.b
        }
    }
}

impl Default for Material {
    fn default() -> Self {
        Self::solid(Vec3::splat(0.5))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Vec3 = Vec3::new(1.0, 0.0, 0.0);
    const B: Vec3 = Vec3::new(0.0, 0.0, 1.0);

    #[test]
    fn srgb_hex_endpoints() {
        assert_eq!(srgb(0x000000), Vec3::ZERO);
        assert!((srgb(0xFFFFFF) - Vec3::ONE).abs().max_element() < 1e-6);
        let c = srgb(0xFF0000);
        assert!(c.x > 0.999 && c.y == 0.0 && c.z == 0.0);
    }

    #[test]
    fn checker_alternates_along_each_axis() {
        let m = Material::checker(A, B, 0.5);
        assert_eq!(m.albedo(Vec3::new(0.25, 0.25, 0.25)), A);
        assert_eq!(m.albedo(Vec3::new(0.75, 0.25, 0.25)), B);
        assert_eq!(m.albedo(Vec3::new(0.75, 0.75, 0.25)), A);
        assert_eq!(m.albedo(Vec3::new(0.75, 0.75, 0.75)), B);
        assert_eq!(m.albedo(Vec3::new(-0.25, 0.25, 0.25)), B);
    }

    #[test]
    fn tiles_ignore_height() {
        let m = Material::tiles(A, B, 0.5);
        for y in [-3.0, -0.1, 0.0, 0.7, 12.0] {
            assert_eq!(m.albedo(Vec3::new(0.25, y, 0.25)), A);
            assert_eq!(m.albedo(Vec3::new(0.75, y, 0.25)), B);
        }
    }

    #[test]
    fn stripes_rings_and_polar() {
        let s = Material::stripes(A, B, Axis::Y, 0.1);
        assert_eq!(s.albedo(Vec3::new(5.0, 0.05, -2.0)), A);
        assert_eq!(s.albedo(Vec3::new(5.0, 0.15, -2.0)), B);

        let r = Material::rings(A, B, Axis::Y, 1.0);
        assert_eq!(r.albedo(Vec3::new(0.3, 9.0, 0.4)), A);
        assert_eq!(r.albedo(Vec3::new(1.2, 9.0, 0.9)), B);

        let p = Material::polar(A, B, Axis::Y, 1.0, 4);
        let c0 = p.albedo(Vec3::new(0.3, 0.0, 0.3));
        let c1 = p.albedo(Vec3::new(-0.3, 0.0, 0.3));
        let c2 = p.albedo(Vec3::new(0.9, 0.0, 0.9));
        assert_ne!(c0, c1, "adjacent sectors differ");
        assert_ne!(c0, c2, "adjacent rings differ");
    }

    #[test]
    fn dots_gradient_cells() {
        let d = Material::dots(A, B, 0.2, 0.05);
        assert_eq!(d.albedo(Vec3::new(0.2, 0.4, -0.2)), B);
        assert_eq!(d.albedo(Vec3::new(0.3, 0.5, -0.1)), A);

        let g = Material::gradient(A, B, Axis::Y, 1.0, 3.0);
        assert_eq!(g.albedo(Vec3::new(0.0, 0.0, 0.0)), A);
        assert_eq!(g.albedo(Vec3::new(0.0, 4.0, 0.0)), B);
        assert!((g.albedo(Vec3::new(0.0, 2.0, 0.0)) - Vec3::new(0.5, 0.0, 0.5)).abs().max_element() < 1e-6);

        let c = Material::cells(A, B, Vec3::new(0.1, f32::INFINITY, 1.0));
        let v0 = c.albedo(Vec3::new(0.05, 0.0, 0.5));
        assert_eq!(v0, c.albedo(Vec3::new(0.07, 3.0, 0.9)), "constant inside a cell");
        let distinct = (0..32).map(|i| c.albedo(Vec3::new(0.05 + 0.1 * i as f32, 0.0, 0.5)).x).filter(|x| (x - v0.x).abs() > 1e-3).count();
        assert!(distinct > 20, "cells vary");
    }
}
