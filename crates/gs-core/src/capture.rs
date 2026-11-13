//! Simulated RGB-D sensors.
//!
//! [`render_rgbd`] ray casts a [`SceneSnapshot`] through a calibrated
//! [`Camera`] and returns what a depth camera would deliver: an sRGB image and
//! a depth map holding the distance along the camera's forward axis (`0.0`
//! where there is no return). An optional [`NoiseModel`] degrades the result
//! the way consumer depth sensors do. [`render_reference`] produces the
//! noise-free, anti-aliased ground truth used to score reconstructions.

use glam::Vec3;
use serde::{Deserialize, Serialize};

use crate::scene::{linear_to_srgb8, srgb8_to_linear, SceneSnapshot};
use crate::types::{Camera, RgbdFrame};

/// Sensor noise. All noise is a pure function of `(seed, pixel index)`: the
/// same inputs always give the same frame, whatever the thread count.
///
/// Fields missing from serialized data take their [`Default`] (no noise).
/// Negative and NaN parameters switch the corresponding effect off.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NoiseModel {
    /// Standard deviation of the depth error at 1 m, in meters. The error
    /// grows with the square of the depth, as for structured-light and stereo
    /// sensors.
    pub depth_sigma_at_1m: f32,
    /// Standard deviation of the per-channel color noise, in 8-bit levels.
    pub color_sigma: f32,
    /// Probability that a pixel returns no depth at all.
    pub dropout: f32,
}

impl NoiseModel {
    /// A perfect sensor.
    pub const NONE: Self = Self {
        depth_sigma_at_1m: 0.0,
        color_sigma: 0.0,
        dropout: 0.0,
    };

    /// Roughly a consumer depth camera: 1.5 mm depth noise at 1 m (about
    /// 1.4 cm at 3 m), 2 levels of color noise, 1 % dropped pixels.
    pub fn realistic() -> Self {
        Self {
            depth_sigma_at_1m: 0.0015,
            color_sigma: 2.0,
            dropout: 0.01,
        }
    }

    /// True when the model does not alter the frame.
    pub fn is_none(&self) -> bool {
        self.depth_sigma_at_1m <= 0.0 && self.color_sigma <= 0.0 && self.dropout <= 0.0
    }
}

impl Default for NoiseModel {
    fn default() -> Self {
        Self::NONE
    }
}

/// Parameters of one simulated exposure.
///
/// Fields missing from serialized data take their [`Default`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CaptureOptions {
    /// Cast shadow rays toward the lights.
    pub shadows: bool,
    /// Returns closer than this forward distance are invalid (meters).
    pub min_range: f32,
    /// Returns farther than this forward distance are invalid (meters).
    pub max_range: f32,
    pub noise: NoiseModel,
    /// Noise seed. Vary it per camera and per frame for uncorrelated noise.
    pub seed: u32,
}

impl Default for CaptureOptions {
    fn default() -> Self {
        Self {
            shadows: true,
            min_range: 0.1,
            max_range: 20.0,
            noise: NoiseModel::NONE,
            seed: 0,
        }
    }
}

/// Renders one RGB-D frame: one primary ray through each pixel center.
///
/// `depth` is the forward distance (ray length times the cosine between the
/// ray and the optical axis), `0.0` where nothing is hit, the hit is outside
/// `[min_range, max_range]`, or the pixel dropped out. `rgb` is the sRGB
/// encoding of [`SceneSnapshot::shade`] for valid pixels and black otherwise.
///
/// The nearest surface decides: something closer than `min_range` blanks the
/// pixel instead of revealing what lies behind it, as on a real sensor.
/// Every valid depth is finite, positive and inside `[min_range, max_range]`,
/// with or without noise (noisy values are clamped to the range).
///
/// With the `par` feature rows are rendered in parallel; the output is
/// identical to the single-threaded one.
pub fn render_rgbd(snap: &SceneSnapshot, cam: &Camera, opts: &CaptureOptions) -> RgbdFrame {
    let mut frame = RgbdFrame::new(cam.width, cam.height);
    for_each_row(&mut frame, |y, rgb, depth| render_row(snap, cam, opts, y, rgb, depth));
    frame
}

/// Single-threaded [`render_rgbd`], whatever the enabled features.
#[doc(hidden)]
pub fn render_rgbd_serial(snap: &SceneSnapshot, cam: &Camera, opts: &CaptureOptions) -> RgbdFrame {
    let mut frame = RgbdFrame::new(cam.width, cam.height);
    for_each_row_serial(&mut frame, |y, rgb, depth| render_row(snap, cam, opts, y, rgb, depth));
    frame
}

/// Renders the noise-free ground truth for image-quality metrics.
///
/// Color is averaged (in linear light) over `supersample x supersample` rays
/// per pixel; rays that miss the scene contribute the flat sRGB `background`.
/// `depth` comes from the pixel-center ray alone, with no range limit.
pub fn render_reference(
    snap: &SceneSnapshot,
    cam: &Camera,
    shadows: bool,
    supersample: u32,
    background: [u8; 3],
) -> RgbdFrame {
    let n = supersample.max(1);
    let background = srgb8_to_linear(background);
    let forward = cam.forward();
    let inv_samples = 1.0 / (n * n) as f32;
    let mut frame = RgbdFrame::new(cam.width, cam.height);
    for_each_row(&mut frame, |y, rgb, depth| {
        for x in 0..cam.width as usize {
            let mut sum = Vec3::ZERO;
            for sy in 0..n {
                for sx in 0..n {
                    let px = x as f32 + (sx as f32 + 0.5) / n as f32;
                    let py = y as f32 + (sy as f32 + 0.5) / n as f32;
                    let (origin, dir) = cam.ray(px, py);
                    sum += match snap.raycast(origin, dir, f32::INFINITY) {
                        Some(hit) => snap.shade(&hit, shadows),
                        None => background,
                    };
                }
            }
            rgb[x * 3..x * 3 + 3].copy_from_slice(&linear_to_srgb8(sum * inv_samples));
            let (origin, dir) = cam.ray(x as f32 + 0.5, y as f32 + 0.5);
            depth[x] = snap
                .raycast(origin, dir, f32::INFINITY)
                .map_or(0.0, |hit| hit.t * dir.dot(forward));
        }
    });
    frame
}

/// Depth noise never pushes a return closer than this (meters), so a valid
/// depth stays distinguishable from the `0.0` "no return" marker.
const MIN_NOISY_DEPTH: f32 = 1.0e-3;

fn render_row(snap: &SceneSnapshot, cam: &Camera, opts: &CaptureOptions, y: u32, rgb: &mut [u8], depth: &mut [f32]) {
    let forward = cam.forward();
    let noisy = !opts.noise.is_none();
    for x in 0..cam.width {
        let (origin, dir) = cam.ray(x as f32 + 0.5, y as f32 + 0.5);
        let cos = dir.dot(forward);
        // Stop the ray where its forward distance reaches the range limit.
        let Some(hit) = snap.raycast(origin, dir, opts.max_range / cos.max(1e-6)) else {
            continue;
        };
        let mut z = hit.t * cos;
        if z < opts.min_range || z > opts.max_range {
            continue;
        }
        let mut color = linear_to_srgb8(snap.shade(&hit, opts.shadows));
        if noisy {
            let pixel = y * cam.width + x;
            let n = &opts.noise;
            if n.dropout > 0.0 && uniform(opts.seed, pixel, 0) < n.dropout {
                continue;
            }
            if n.depth_sigma_at_1m > 0.0 {
                let noisy = z + gaussian(opts.seed, pixel, 1) * n.depth_sigma_at_1m * z * z;
                if !noisy.is_finite() {
                    continue;
                }
                // Stay a valid return: positive and inside the sensor range.
                // (`max`/`min` rather than `clamp`, which panics on a NaN limit.)
                z = noisy.max(opts.min_range.max(MIN_NOISY_DEPTH)).min(opts.max_range);
            }
            if n.color_sigma > 0.0 {
                for (c, channel) in color.iter_mut().enumerate() {
                    let v = *channel as f32 + gaussian(opts.seed, pixel, 3 + 2 * c as u32) * n.color_sigma;
                    *channel = v.round().clamp(0.0, 255.0) as u8;
                }
            }
        }
        let i = x as usize;
        depth[i] = z;
        rgb[i * 3..i * 3 + 3].copy_from_slice(&color);
    }
}

/// Runs `f(row, rgb_row, depth_row)` for every image row, in parallel when
/// the `par` feature is enabled.
fn for_each_row(frame: &mut RgbdFrame, f: impl Fn(u32, &mut [u8], &mut [f32]) + Sync) {
    #[cfg(feature = "par")]
    {
        use rayon::prelude::*;
        let w = frame.width as usize;
        if w == 0 {
            return;
        }
        frame
            .rgb
            .par_chunks_mut(w * 3)
            .zip(frame.depth.par_chunks_mut(w))
            .enumerate()
            .for_each(|(y, (rgb, depth))| f(y as u32, rgb, depth));
    }
    #[cfg(not(feature = "par"))]
    for_each_row_serial(frame, f);
}

fn for_each_row_serial(frame: &mut RgbdFrame, f: impl Fn(u32, &mut [u8], &mut [f32])) {
    let w = frame.width as usize;
    if w == 0 {
        return;
    }
    for (y, (rgb, depth)) in frame.rgb.chunks_mut(w * 3).zip(frame.depth.chunks_mut(w)).enumerate() {
        f(y as u32, rgb, depth);
    }
}

/// Integer hash of `(seed, pixel, stream)` (lowbias32-style avalanche).
#[inline]
fn hash(seed: u32, pixel: u32, stream: u32) -> u32 {
    let mut h = seed.wrapping_mul(0x9E37_79B9) ^ pixel.wrapping_mul(0x85EB_CA6B) ^ stream.wrapping_mul(0xC2B2_AE35);
    h ^= h >> 16;
    h = h.wrapping_mul(0x7FEB_352D);
    h ^= h >> 15;
    h = h.wrapping_mul(0x846C_A68B);
    h ^= h >> 16;
    h = h.wrapping_add(seed.rotate_left(13)).wrapping_mul(0x9E37_79B9);
    h ^ (h >> 16)
}

/// Uniform sample in `[0, 1)`.
#[inline]
fn uniform(seed: u32, pixel: u32, stream: u32) -> f32 {
    (hash(seed, pixel, stream) >> 8) as f32 / (1u32 << 24) as f32
}

/// Approximately standard normal sample; consumes streams `stream` and
/// `stream + 1`.
///
/// Sum of eight uniform bytes (Irwin-Hall): cheap, no transcendental
/// functions, tails bounded at about 4.9 sigma, which suits sensor noise.
#[inline]
fn gaussian(seed: u32, pixel: u32, stream: u32) -> f32 {
    #[inline]
    fn byte_sum(x: u32) -> u32 {
        let pairs = (x & 0x00FF_00FF) + ((x >> 8) & 0x00FF_00FF);
        (pairs & 0xFFFF) + (pairs >> 16)
    }
    let sum = byte_sum(hash(seed, pixel, stream)) + byte_sum(hash(seed, pixel, stream + 1));
    // Each byte has mean 127.5 and variance (256^2 - 1) / 12.
    const MEAN: f32 = 8.0 * 127.5;
    const INV_SIGMA: f32 = 1.0 / 209.021_53;
    (sum as f32 - MEAN) * INV_SIGMA
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::{make_scene, Light, Lighting, Material, Primitive, SceneKind};

    /// A wall facing the camera at z = -4 and a ball in front of it.
    fn wall_scene() -> SceneSnapshot {
        let wall = Primitive::cuboid(
            Vec3::new(0.0, 0.0, -4.5),
            Vec3::new(1.2, 50.0, 0.5),
            Material::solid(Vec3::splat(0.5)),
        );
        let ball = Primitive::sphere(Vec3::new(0.0, 0.0, -2.0), 0.3, Material::checker(Vec3::X, Vec3::Y, 0.1));
        let lighting = Lighting {
            lights: vec![Light::directional(Vec3::new(0.3, 1.0, 0.8), Vec3::splat(0.5))],
            ..Lighting::default()
        };
        SceneSnapshot::new(vec![wall, ball], lighting)
    }

    fn cam(w: u32, h: u32) -> Camera {
        Camera::look_at(Vec3::ZERO, Vec3::NEG_Z, Vec3::Y, 60f32.to_radians(), w, h)
    }

    #[test]
    fn depth_is_forward_distance_not_ray_length() {
        let snap = wall_scene();
        let cam = cam(64, 48);
        let frame = render_rgbd(&snap, &cam, &CaptureOptions::default());
        // The wall is a plane perpendicular to the optical axis: every wall
        // pixel has the same forward depth although ray lengths differ.
        let mut wall_pixels = 0;
        for y in 0..48 {
            for x in 0..64 {
                let z = frame.depth_at(x, y);
                if z > 3.0 {
                    wall_pixels += 1;
                    assert!((z - 4.0).abs() < 1e-3, "({x},{y}) depth {z}");
                }
            }
        }
        assert!(wall_pixels > 800, "{wall_pixels} wall pixels");
        // The top image row still sees the tall wall.
        assert!((frame.depth_at(30, 0) - 4.0).abs() < 1e-3);
        // Center pixel sees the ball.
        assert!((frame.depth_at(32, 24) - 1.7).abs() < 0.01);
    }

    #[test]
    fn unprojected_depth_lands_on_the_hit_point() {
        let scene = make_scene(SceneKind::Stage);
        let snap = scene.snapshot(1.3);
        let cam = Camera::look_at(
            Vec3::new(2.5, 1.7, 2.0),
            Vec3::new(0.0, 1.0, 0.0),
            Vec3::Y,
            50f32.to_radians(),
            80,
            60,
        );
        let frame = render_rgbd(&snap, &cam, &CaptureOptions::default());
        let mut checked = 0;
        for y in 0..60 {
            for x in 0..80 {
                let z = frame.depth_at(x, y);
                let (origin, dir) = cam.ray(x as f32 + 0.5, y as f32 + 0.5);
                match snap.raycast(origin, dir, f32::INFINITY) {
                    Some(hit) if z > 0.0 => {
                        let p = cam.unproject(x as f32 + 0.5, y as f32 + 0.5, z);
                        assert!((p - hit.pos).length() < 1e-3, "({x},{y}): {p:?} vs {:?}", hit.pos);
                        let (px, py, pz) = cam.project(hit.pos).unwrap();
                        assert!((px - x as f32 - 0.5).abs() < 0.02 && (py - y as f32 - 0.5).abs() < 0.02);
                        assert!((pz - z).abs() < 1e-3);
                        checked += 1;
                    }
                    Some(_) => panic!("hit without depth at ({x},{y})"),
                    None => assert_eq!(z, 0.0),
                }
            }
        }
        assert!(checked > 2000, "only {checked} valid pixels");
    }

    #[test]
    fn invalid_pixels_are_black_with_zero_depth() {
        let snap = wall_scene();
        let cam = cam(64, 48);
        let frame = render_rgbd(&snap, &cam, &CaptureOptions::default());
        // The wall is only 2.4 m wide: the image borders see nothing.
        assert_eq!(frame.depth_at(0, 24), 0.0);
        assert_eq!(frame.rgb_at(0, 24), [0, 0, 0]);
        assert!(frame.depth_at(32, 2) > 0.0);
        assert_ne!(frame.rgb_at(32, 2), [0, 0, 0]);
        for i in 0..frame.depth.len() {
            if frame.depth[i] == 0.0 {
                assert_eq!(&frame.rgb[i * 3..i * 3 + 3], &[0, 0, 0]);
            }
        }
    }

    #[test]
    fn range_limits() {
        let snap = wall_scene();
        let cam = cam(64, 48);
        let near_only = render_rgbd(
            &snap,
            &cam,
            &CaptureOptions {
                max_range: 3.0,
                ..Default::default()
            },
        );
        assert!(near_only.depth.iter().all(|&z| z <= 3.0));
        assert!(near_only.depth_at(32, 24) > 0.0, "ball kept");
        assert_eq!(near_only.depth_at(32, 2), 0.0, "wall dropped");
        let far_only = render_rgbd(
            &snap,
            &cam,
            &CaptureOptions {
                min_range: 3.0,
                ..Default::default()
            },
        );
        assert_eq!(
            far_only.depth_at(32, 24),
            0.0,
            "ball dropped, wall behind it is occluded"
        );
        assert_eq!(far_only.rgb_at(32, 24), [0, 0, 0]);
        assert!((far_only.depth_at(32, 2) - 4.0).abs() < 1e-3);
    }

    #[test]
    fn color_matches_shade() {
        let snap = wall_scene();
        let cam = cam(64, 48);
        for shadows in [false, true] {
            let frame = render_rgbd(
                &snap,
                &cam,
                &CaptureOptions {
                    shadows,
                    ..Default::default()
                },
            );
            for (x, y) in [(32, 24), (30, 20), (32, 2), (40, 40)] {
                let (o, d) = cam.ray(x as f32 + 0.5, y as f32 + 0.5);
                let hit = snap.raycast(o, d, 100.0).unwrap();
                assert_eq!(frame.rgb_at(x, y), linear_to_srgb8(snap.shade(&hit, shadows)));
            }
        }
    }

    #[test]
    fn noise_is_deterministic_and_seeded() {
        let snap = wall_scene();
        let cam = cam(64, 48);
        let opts = CaptureOptions {
            noise: NoiseModel::realistic(),
            seed: 42,
            ..Default::default()
        };
        let a = render_rgbd(&snap, &cam, &opts);
        let b = render_rgbd(&snap, &cam, &opts);
        assert_eq!(a, b);
        let c = render_rgbd(&snap, &cam, &CaptureOptions { seed: 43, ..opts });
        assert_ne!(a, c);
        let clean = render_rgbd(&snap, &cam, &CaptureOptions::default());
        assert_ne!(a, clean);
    }

    #[test]
    fn noise_statistics() {
        let snap = wall_scene();
        let cam = cam(160, 120);
        let clean = render_rgbd(&snap, &cam, &CaptureOptions::default());
        let noise = NoiseModel {
            depth_sigma_at_1m: 0.002,
            color_sigma: 3.0,
            dropout: 0.05,
        };
        let noisy = render_rgbd(
            &snap,
            &cam,
            &CaptureOptions {
                noise,
                seed: 7,
                ..Default::default()
            },
        );

        let (mut n, mut dropped, mut sum, mut sum2, mut csum, mut csum2) = (0u32, 0u32, 0f64, 0f64, 0f64, 0f64);
        for i in 0..clean.depth.len() {
            // Wall pixels only: constant depth 4 m, constant mid-gray color.
            if (clean.depth[i] - 4.0).abs() > 1e-2 {
                continue;
            }
            n += 1;
            if noisy.depth[i] == 0.0 {
                dropped += 1;
                assert_eq!(&noisy.rgb[i * 3..i * 3 + 3], &[0, 0, 0]);
                continue;
            }
            let e = (noisy.depth[i] - clean.depth[i]) as f64;
            sum += e;
            sum2 += e * e;
            let ce = noisy.rgb[i * 3 + 1] as f64 - clean.rgb[i * 3 + 1] as f64;
            csum += ce;
            csum2 += ce * ce;
        }
        assert!(n > 5000);
        let kept = (n - dropped) as f64;
        let drop_rate = dropped as f64 / n as f64;
        assert!((drop_rate - 0.05).abs() < 0.012, "dropout {drop_rate}");
        let (mean, sigma) = (sum / kept, (sum2 / kept).sqrt());
        // sigma(z) = 0.002 * 4^2 = 0.032 m.
        assert!(mean.abs() < 0.003, "depth bias {mean}");
        assert!((sigma - 0.032).abs() < 0.003, "depth sigma {sigma}");
        let (cmean, csigma) = (csum / kept, (csum2 / kept).sqrt());
        assert!(cmean.abs() < 0.2, "color bias {cmean}");
        assert!((csigma - 3.0).abs() < 0.3, "color sigma {csigma}");
    }

    #[test]
    fn noisy_depth_stays_a_valid_return_inside_the_range() {
        let snap = wall_scene();
        let cam = cam(160, 120);
        // The wall sits at 4 m: with the limit right behind it, a third of
        // the noisy samples would overshoot without clamping.
        let heavy = NoiseModel {
            depth_sigma_at_1m: 0.02,
            color_sigma: 0.0,
            dropout: 0.0,
        };
        let opts = CaptureOptions {
            noise: heavy,
            min_range: 1.6,
            max_range: 4.1,
            seed: 3,
            ..Default::default()
        };
        let frame = render_rgbd(&snap, &cam, &opts);
        let valid: Vec<f32> = frame.depth.iter().copied().filter(|&z| z != 0.0).collect();
        assert!(valid.len() > 5000);
        assert!(valid.iter().all(|z| z.is_finite() && (1.6..=4.1).contains(z)));
        assert!(valid.contains(&4.1), "overshoots are clamped to the limit");
        assert!(valid.contains(&1.6), "undershoots too (the ball at 1.7 m)");

        // Absurd parameters never produce NaN, infinite or negative depth,
        // and never panic.
        let absurd = [
            NoiseModel {
                depth_sigma_at_1m: f32::INFINITY,
                color_sigma: f32::INFINITY,
                dropout: 0.5,
            },
            NoiseModel {
                depth_sigma_at_1m: 1.0e30,
                color_sigma: 1.0e30,
                dropout: 0.0,
            },
            NoiseModel {
                depth_sigma_at_1m: f32::NAN,
                color_sigma: f32::NAN,
                dropout: f32::NAN,
            },
            NoiseModel {
                depth_sigma_at_1m: -1.0,
                color_sigma: -1.0,
                dropout: -1.0,
            },
        ];
        for noise in absurd {
            for max_range in [20.0, f32::INFINITY, f32::NAN] {
                let opts = CaptureOptions {
                    noise,
                    max_range,
                    ..Default::default()
                };
                let frame = render_rgbd(&snap, &cam, &opts);
                assert!(
                    frame.depth.iter().all(|z| z.is_finite() && *z >= 0.0),
                    "{noise:?}, max_range {max_range}"
                );
                for i in 0..frame.depth.len() {
                    if frame.depth[i] == 0.0 {
                        assert_eq!(&frame.rgb[i * 3..i * 3 + 3], &[0, 0, 0]);
                    }
                }
            }
        }
        // NaN and negative parameters mean "off": the frame is the clean one.
        let clean = render_rgbd(&snap, &cam, &CaptureOptions::default());
        for noise in &absurd[2..] {
            let opts = CaptureOptions {
                noise: *noise,
                ..Default::default()
            };
            assert_eq!(render_rgbd(&snap, &cam, &opts), clean);
        }
        // Certain dropout removes everything.
        let all_dropped = CaptureOptions {
            noise: NoiseModel {
                dropout: 1.0,
                ..NoiseModel::NONE
            },
            ..Default::default()
        };
        assert!(render_rgbd(&snap, &cam, &all_dropped).depth.iter().all(|&z| z == 0.0));
    }

    #[test]
    fn options_deserialize_with_defaults() {
        let opts: CaptureOptions = serde_json::from_str(r#"{"shadows":false,"noise":{"dropout":0.25}}"#).unwrap();
        assert_eq!(
            opts,
            CaptureOptions {
                shadows: false,
                noise: NoiseModel {
                    dropout: 0.25,
                    ..NoiseModel::NONE
                },
                ..Default::default()
            }
        );
        assert_eq!(
            serde_json::from_str::<CaptureOptions>("{}").unwrap(),
            CaptureOptions::default()
        );
        let full = CaptureOptions {
            noise: NoiseModel::realistic(),
            seed: 77,
            max_range: 6.5,
            ..Default::default()
        };
        let json = serde_json::to_string(&full).unwrap();
        assert_eq!(serde_json::from_str::<CaptureOptions>(&json).unwrap(), full);
    }

    #[test]
    fn image_orientation_follows_the_conventions() {
        // An asymmetric scene checked against hand-written pinhole math: a
        // red ball to the right (+X), a green one above (+Y) and a blue one
        // nearer, to the lower left. Catches mirrored or upside-down images.
        let glow = |c: Vec3| Material::solid(c).with_emissive(1.0);
        let balls = [
            (Vec3::new(1.0, 0.0, 0.0), 0.3, Vec3::X),
            (Vec3::new(0.0, 1.0, 0.0), 0.3, Vec3::Y),
            (Vec3::new(-0.5, -0.5, 2.0), 0.2, Vec3::Z),
        ];
        let prims = balls
            .iter()
            .map(|&(center, radius, color)| Primitive::sphere(center, radius, glow(color)))
            .collect();
        let snap = SceneSnapshot::new(prims, Lighting::default());
        let eye = Vec3::new(0.0, 0.0, 5.0);
        let cam = Camera::look_at(eye, Vec3::ZERO, Vec3::Y, 50f32.to_radians(), 160, 120);
        let frame = render_rgbd(&snap, &cam, &CaptureOptions::default());
        let focal = 60.0 / 25f32.to_radians().tan();
        for (channel, &(center, radius, _)) in balls.iter().enumerate() {
            let (mut sx, mut sy, mut n) = (0.0, 0.0, 0.0);
            let mut nearest = f32::INFINITY;
            for y in 0..120 {
                for x in 0..160 {
                    let c = frame.rgb_at(x, y);
                    if frame.depth_at(x, y) > 0.0 && c[channel] == *c.iter().max().unwrap() && c[channel] > 100 {
                        sx += x as f32 + 0.5;
                        sy += y as f32 + 0.5;
                        n += 1.0;
                        nearest = nearest.min(frame.depth_at(x, y));
                    }
                }
            }
            assert!(n > 100.0, "ball {channel} not found");
            // Camera looks down -Z from z = 5: u grows with x, v shrinks with y.
            let forward = eye.z - center.z;
            let u = 80.0 + focal * center.x / forward;
            let v = 60.0 - focal * center.y / forward;
            assert!(
                (sx / n - u).abs() < 1.0 && (sy / n - v).abs() < 1.0,
                "ball {channel}: centroid ({}, {}) expected ({u}, {v})",
                sx / n,
                sy / n
            );
            // Nearest depth of a ball: its front, measured along the axis.
            assert!((nearest - (forward - radius)).abs() < 0.03, "ball {channel}: {nearest}");
        }
        // Red is in the right half, green in the top half, blue bottom left.
        assert_eq!(frame.rgb_at(106, 60)[0], *frame.rgb_at(106, 60).iter().max().unwrap());
        assert!(frame.rgb_at(80, 34)[1] > 100 && frame.rgb_at(58, 81)[2] > 100);
        assert_eq!(frame.depth_at(54, 60), 0.0, "nothing to the left at eye level");
    }

    #[test]
    fn gaussian_generator_is_standard_normal() {
        let n = 200_000u32;
        let (mut sum, mut sum2, mut within_one, mut max) = (0f64, 0f64, 0u32, 0f32);
        for i in 0..n {
            let g = gaussian(9, i, 4);
            sum += g as f64;
            sum2 += (g * g) as f64;
            within_one += (g.abs() < 1.0) as u32;
            max = max.max(g.abs());
        }
        let mean = sum / n as f64;
        let var = sum2 / n as f64 - mean * mean;
        assert!(mean.abs() < 0.01, "mean {mean}");
        assert!((var - 1.0).abs() < 0.02, "variance {var}");
        let frac = within_one as f64 / n as f64;
        assert!((frac - 0.6827).abs() < 0.01, "{frac} within one sigma");
        assert!(max > 3.0 && max < 5.0);
        // Streams are independent enough: no correlation between neighbours.
        let corr: f64 = (0..n)
            .map(|i| (gaussian(9, i, 4) * gaussian(9, i + 1, 4)) as f64)
            .sum::<f64>()
            / n as f64;
        assert!(corr.abs() < 0.01, "neighbour correlation {corr}");
        let cross: f64 = (0..n)
            .map(|i| (gaussian(9, i, 1) * gaussian(9, i, 3)) as f64)
            .sum::<f64>()
            / n as f64;
        assert!(cross.abs() < 0.01, "stream correlation {cross}");
    }

    #[test]
    fn parallel_and_serial_renders_are_identical() {
        for kind in [SceneKind::Stage, SceneKind::House] {
            let scene = make_scene(kind);
            let snap = scene.snapshot(2.0);
            let (eye, target) = scene.viewer_start();
            let cam = Camera::look_at(eye, target, Vec3::Y, 60f32.to_radians(), 96, 72);
            for noise in [NoiseModel::NONE, NoiseModel::realistic()] {
                let opts = CaptureOptions {
                    noise,
                    seed: 5,
                    ..Default::default()
                };
                assert_eq!(
                    render_rgbd(&snap, &cam, &opts),
                    render_rgbd_serial(&snap, &cam, &opts),
                    "{kind:?}"
                );
            }
        }
    }

    #[test]
    fn reference_fills_background_and_antialiases() {
        let snap = wall_scene();
        let cam = cam(64, 48);
        let bg = [10, 200, 30];
        let reference = render_reference(&snap, &cam, true, 3, bg);
        assert_eq!(reference.rgb_at(0, 24), bg);
        assert_eq!(reference.depth_at(0, 24), 0.0);
        // Same depth as the sensor for the pixel-center ray.
        let sensor = render_rgbd(&snap, &cam, &CaptureOptions::default());
        assert_eq!(reference.depth, sensor.depth);
        // Supersample 1 reproduces the sensor colors wherever something is hit.
        let single = render_reference(&snap, &cam, true, 1, bg);
        for i in 0..sensor.depth.len() {
            if sensor.depth[i] > 0.0 {
                assert_eq!(&single.rgb[i * 3..i * 3 + 3], &sensor.rgb[i * 3..i * 3 + 3]);
            }
        }
        // Along the wall's vertical edge, supersampling yields blended colors
        // that are neither the wall nor the background.
        let wall = sensor.rgb_at(32, 2);
        let blended = (0..64).filter(|&x| {
            let c = reference.rgb_at(x, 2);
            c != wall && c != bg
        });
        assert!(blended.count() >= 1);
    }

    #[test]
    fn empty_camera_and_empty_scene() {
        let snap = wall_scene();
        let frame = render_rgbd(&snap, &cam(0, 0), &CaptureOptions::default());
        assert!(frame.rgb.is_empty() && frame.depth.is_empty());
        let empty = SceneSnapshot::new(Vec::new(), Lighting::default());
        let frame = render_rgbd(&empty, &cam(8, 8), &CaptureOptions::default());
        assert!(frame.depth.iter().all(|&z| z == 0.0) && frame.rgb.iter().all(|&c| c == 0));
    }
}
