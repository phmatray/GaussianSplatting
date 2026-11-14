//! Deterministic splat clouds for the renderer checks.
//!
//! Pure Rust (no browser types): hand-built clouds that stress one aspect of
//! the splat math each, and a naive depth-map unprojection that turns
//! simulated RGB-D frames into surfels so that the test data looks like what
//! the real pipeline produces.

use std::f32::consts::{PI, TAU};

use glam::{Quat, Vec3};
use gs_core::{Camera, RgbdFrame, Splat};

const LIGHT: Vec3 = Vec3::new(0.35, 0.85, 0.4);

/// Diffuse shading of an sRGB base color, good enough for a test picture.
fn shade(base: [f32; 3], normal: Vec3) -> [u8; 4] {
    let k = 0.22 + 0.78 * normal.dot(LIGHT.normalize()).max(0.0);
    [(base[0] * k * 255.0) as u8, (base[1] * k * 255.0) as u8, (base[2] * k * 255.0) as u8, 255]
}

fn hsv(h: f32, s: f32, v: f32) -> [f32; 3] {
    let f = |n: f32| {
        let k = (n + h * 6.0) % 6.0;
        v - v * s * k.min(4.0 - k).clamp(0.0, 1.0)
    };
    [f(5.0), f(3.0), f(1.0)]
}

/// Small deterministic generator (SplitMix64), uniform in `[0, 1)`.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub fn next_f32(&mut self) -> f32 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        ((z ^ (z >> 31)) >> 40) as f32 / (1u64 << 24) as f32
    }

    /// Uniform in `[lo, hi)`.
    pub fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * self.next_f32()
    }

    /// Uniformly distributed unit vector.
    pub fn unit_vector(&mut self) -> Vec3 {
        let y = self.range(-1.0, 1.0);
        let phi = self.range(0.0, TAU);
        let r = (1.0 - y * y).max(0.0).sqrt();
        Vec3::new(r * phi.cos(), y, r * phi.sin())
    }
}

/// Sphere made of `n` surfels on a Fibonacci lattice, with colored bands.
pub fn sphere(center: Vec3, radius: f32, n: usize) -> Vec<Splat> {
    let spacing = (4.0 * PI * radius * radius / n as f32).sqrt();
    let golden = PI * (3.0 - 5f32.sqrt());
    (0..n)
        .map(|i| {
            let y = 1.0 - 2.0 * (i as f32 + 0.5) / n as f32;
            let r = (1.0 - y * y).sqrt();
            let phi = golden * i as f32;
            let normal = Vec3::new(r * phi.cos(), y, r * phi.sin());
            let longitude = normal.z.atan2(normal.x) / TAU + 0.5;
            let band = ((y * 0.5 + 0.5) * 7.0) as i32 + (longitude * 12.0) as i32;
            let base = if band % 2 == 0 { [0.95, 0.35, 0.15] } else { [0.95, 0.9, 0.8] };
            Splat::surfel(center + normal * radius, normal, 0.75 * spacing, 0.1 * spacing, shade(base, normal))
        })
        .collect()
}

/// Checkerboard floor in the plane `y = 0`.
pub fn floor(half_size: f32, step: f32) -> Vec<Splat> {
    let n = (half_size / step) as i32;
    let mut splats = Vec::new();
    for iz in -n..=n {
        for ix in -n..=n {
            let (x, z) = (ix as f32 * step, iz as f32 * step);
            let tile = ((x / 0.25).floor() + (z / 0.25).floor()) as i32;
            let base = if tile.rem_euclid(2) == 0 { [0.25, 0.35, 0.55] } else { [0.8, 0.8, 0.75] };
            splats.push(Splat::surfel(Vec3::new(x, 0.0, z), Vec3::Y, 0.75 * step, 0.1 * step, shade(base, Vec3::Y)));
        }
    }
    splats
}

/// Fan of thin, strongly anisotropic Gaussians ("needles") in the plane
/// through `center` facing +Z, a ring of tilted discs around it and a few
/// needles pointing at the viewer (strong foreshortening).
pub fn needles(center: Vec3, length: f32) -> Vec<Splat> {
    let count = 16;
    let mut splats = Vec::new();
    for i in 0..count {
        let angle = PI * i as f32 / count as f32;
        let c = hsv(i as f32 / count as f32, 0.85, 1.0);
        splats.push(Splat {
            pos: center.to_array(),
            scale: [length / 3.0, length / 150.0, length / 150.0],
            rot: Quat::from_rotation_z(angle).to_array(),
            rgba: [(c[0] * 255.0) as u8, (c[1] * 255.0) as u8, (c[2] * 255.0) as u8, 230],
        });
    }
    for i in 0..2 * count {
        let angle = PI * i as f32 / count as f32;
        let pos = center + Vec3::new(angle.cos(), angle.sin(), 0.0) * (length * 1.12);
        // Flat discs seen under varying tilt.
        let normal = Vec3::new(angle.cos() * 0.8, angle.sin() * 0.8, 0.6);
        splats.push(Splat::surfel(pos, normal, length / 40.0, length / 400.0, [240, 240, 240, 255]));
    }
    for i in 0..6 {
        let angle = TAU * i as f32 / 6.0;
        let pos = center + Vec3::new(angle.cos(), angle.sin(), 0.3) * (length * 0.55);
        let c = hsv(i as f32 / 6.0 + 0.08, 0.6, 0.9);
        splats.push(Splat {
            pos: pos.to_array(),
            scale: [length / 300.0, length / 300.0, length / 4.0],
            rot: Quat::from_rotation_y(0.25 * angle.cos()).to_array(),
            rgba: [(c[0] * 255.0) as u8, (c[1] * 255.0) as u8, (c[2] * 255.0) as u8, 200],
        });
    }
    splats
}

/// Randomly placed, randomly oriented translucent ellipsoids of very
/// different sizes inside a unit-ish ball around `center`: every pixel is
/// covered by many partially transparent layers, so any error in the sort
/// order, the alpha model or the blending shows.
pub fn translucent(center: Vec3, n: usize, seed: u64) -> Vec<Splat> {
    let mut rng = Rng::new(seed);
    (0..n)
        .map(|i| {
            let pos = center + rng.unit_vector() * rng.next_f32().cbrt() * 0.8;
            let size = 0.01 * (rng.range(0.0, 3.2)).exp();
            let scale = [size * rng.range(0.3, 1.0), size * rng.range(0.3, 1.0), size * rng.range(0.05, 1.0)];
            let rot = Quat::from_axis_angle(rng.unit_vector(), rng.range(0.0, PI));
            let c = hsv(rng.next_f32(), rng.range(0.4, 1.0), rng.range(0.5, 1.0));
            // A few fully opaque ones among mostly see-through ones.
            let alpha = if i % 9 == 0 { 255 } else { rng.range(20.0, 190.0) as u8 };
            Splat {
                pos: pos.to_array(),
                scale,
                rot: rot.to_array(),
                rgba: [(c[0] * 255.0) as u8, (c[1] * 255.0) as u8, (c[2] * 255.0) as u8, alpha],
            }
        })
        .collect()
}

/// `n` small surfels scattered in a box, like a room-sized scan: the load
/// for the performance check.
pub fn random_surfels(n: usize, half_extent: Vec3, seed: u64) -> Vec<Splat> {
    let mut rng = Rng::new(seed);
    (0..n)
        .map(|_| {
            let pos = Vec3::new(rng.range(-1.0, 1.0), rng.range(0.0, 1.0), rng.range(-1.0, 1.0)) * half_extent * Vec3::new(1.0, 2.0, 1.0);
            let c = hsv(rng.next_f32(), 0.6, 0.9);
            let radius = rng.range(0.008, 0.02);
            Splat::surfel(
                pos,
                rng.unit_vector(),
                radius,
                0.1 * radius,
                [(c[0] * 255.0) as u8, (c[1] * 255.0) as u8, (c[2] * 255.0) as u8, 255],
            )
        })
        .collect()
}

/// Options of [`unproject_frame`].
#[derive(Clone, Copy, Debug)]
pub struct UnprojectOptions {
    /// Surfel radius as a multiple of the pixel footprint at the surface.
    pub radius_factor: f32,
    /// Neighbours farther apart in depth than this fraction of the depth are
    /// across an edge and not used for the normal.
    pub edge_ratio: f32,
    pub opacity: u8,
}

impl Default for UnprojectOptions {
    fn default() -> Self {
        Self { radius_factor: 0.8, edge_ratio: 0.03, opacity: 255 }
    }
}

/// Naive reconstruction: one surfel per valid depth pixel of `frame`, placed
/// at the unprojected point, oriented by the depth gradient (facing the
/// camera across depth edges) and sized to cover its pixel.
///
/// `keep(x, y, depth)` selects the pixels to convert (e.g. foreground only).
/// Splats are appended to `out`.
pub fn unproject_frame(
    frame: &RgbdFrame,
    cam: &Camera,
    opts: &UnprojectOptions,
    mut keep: impl FnMut(u32, u32, f32) -> bool,
    out: &mut Vec<Splat>,
) {
    let (w, h) = (frame.width, frame.height);
    let point = |x: u32, y: u32| {
        let d = frame.depth_at(x, y);
        (d > 0.0).then(|| (cam.unproject(x as f32 + 0.5, y as f32 + 0.5, d), d))
    };
    let eye = cam.position();
    for y in 0..h {
        for x in 0..w {
            let Some((p, d)) = point(x, y) else { continue };
            if !keep(x, y, d) {
                continue;
            }
            let to_eye = (eye - p).normalize_or(Vec3::Y);
            // Central differences where both neighbours are on the same surface.
            let tangent = |a: Option<(Vec3, f32)>, b: Option<(Vec3, f32)>| match (a, b) {
                (Some((pa, da)), Some((pb, db))) if (da - d).abs() < opts.edge_ratio * d && (db - d).abs() < opts.edge_ratio * d => {
                    Some(pb - pa)
                }
                _ => None,
            };
            let tx = tangent((x > 0).then(|| point(x - 1, y)).flatten(), (x + 1 < w).then(|| point(x + 1, y)).flatten());
            let ty = tangent((y > 0).then(|| point(x, y - 1)).flatten(), (y + 1 < h).then(|| point(x, y + 1)).flatten());
            let normal = match (tx, ty) {
                (Some(tx), Some(ty)) => {
                    let n = tx.cross(ty).normalize_or(to_eye);
                    if n.dot(to_eye) < 0.0 {
                        -n
                    } else {
                        n
                    }
                }
                _ => to_eye,
            };
            // Footprint of one pixel at this depth, widened on slanted surfaces.
            let slant = normal.dot(to_eye).clamp(0.35, 1.0);
            let radius = opts.radius_factor * d / cam.fx / slant.sqrt();
            let [r, g, b] = frame.rgb_at(x, y);
            out.push(Splat::surfel(p, normal, radius, 0.15 * radius, [r, g, b, opts.opacity]));
        }
    }
}

#[cfg(test)]
mod tests {
    use gs_core::capture::{render_rgbd, CaptureOptions};
    use gs_core::raster::{psnr, rasterize, Image, RasterOptions};
    use gs_core::scene::{Scene, StageScene};

    use super::*;

    #[test]
    fn generators_are_deterministic_and_finite() {
        for cloud in [
            sphere(Vec3::ZERO, 0.5, 500),
            floor(1.0, 0.1),
            needles(Vec3::ZERO, 0.5),
            translucent(Vec3::ZERO, 300, 7),
            random_surfels(300, Vec3::splat(2.0), 3),
        ] {
            assert!(!cloud.is_empty());
            assert!(cloud.iter().all(|s| s.pos.iter().chain(&s.scale).chain(&s.rot).all(|v| v.is_finite())));
            assert!(cloud.iter().all(|s| s.scale.iter().all(|v| *v > 0.0)));
        }
        assert_eq!(translucent(Vec3::ZERO, 50, 1), translucent(Vec3::ZERO, 50, 1));
        assert_ne!(translucent(Vec3::ZERO, 50, 1), translucent(Vec3::ZERO, 50, 2));
    }

    #[test]
    fn unprojected_frame_reproduces_its_own_view() {
        let snap = StageScene::new().snapshot(1.0);
        let cam = Camera::look_at(Vec3::new(0.5, 1.5, 3.4), Vec3::new(0.0, 0.9, 0.0), Vec3::Y, 50f32.to_radians(), 160, 120);
        let frame = render_rgbd(&snap, &cam, &CaptureOptions::default());
        let mut splats = Vec::new();
        unproject_frame(&frame, &cam, &UnprojectOptions::default(), |_, _, _| true, &mut splats);
        assert!(splats.len() > 5_000, "{} splats", splats.len());
        // Seen from the capturing camera the surfels give a (blurred) copy of the image.
        let image = rasterize(&splats, &cam, &RasterOptions::default());
        let quality = psnr(&image, &Image::from_frame(&frame));
        assert!(quality > 18.0, "PSNR {quality}");
        // The filter is honoured.
        let mut half = Vec::new();
        unproject_frame(&frame, &cam, &UnprojectOptions::default(), |x, _, _| x < 80, &mut half);
        assert!(half.len() < splats.len() && !half.is_empty());
    }
}
