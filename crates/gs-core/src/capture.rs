//! Simulated RGB-D sensors.
//!
//! [`render_rgbd`] ray casts a [`SceneSnapshot`] through a calibrated
//! [`Camera`] and returns what a depth camera would deliver: an sRGB image and
//! a depth map holding the distance along the camera's forward axis (`0.0`
//! where there is no return). An optional [`NoiseModel`] degrades the result
//! the way consumer depth sensors do. [`render_reference`] produces the
//! noise-free, anti-aliased ground truth used to score reconstructions.
//!
//! # Partial capture
//!
//! A fixed camera looking at a mostly static scene need not cast every ray
//! again for every frame. [`StaticRender`] keeps what the camera sees of the
//! static part of the scene; [`PartialCapture`] then casts only the pixels
//! that the moving objects of an instant can change and takes the others
//! from it. The frame is *bit-identical* to [`render_rgbd`] of the whole
//! scene, sensor noise included.
//!
//! A pixel can differ from the static render in two ways only:
//!
//! * its primary ray meets a moving object no farther than the static
//!   surface it used to end on (or before it leaves the sensor range);
//! * a moving object blocks one of the shadow rays that reach their light
//!   from that static surface point (a new shadow). Lights that never
//!   reached the point stay away whatever moves.
//!
//! Both are tested per pixel against the moving primitives alone, a handful
//! of them in a hierarchy of their own. Most pixels never get that far: the
//! primary test only runs inside the screen boxes of the moving primitives,
//! and the shadow test of a directional light only where the point lies in
//! the light's view of those boxes (`ShadowMask`). On the stage about 6 %
//! of the pixels of a ring camera change from the empty stage; one
//! screen-space bounding box around them would still hold 42 % of the
//! image, which is why the tests are per pixel and per primitive.

use glam::Vec3;
use serde::{Deserialize, Serialize};

use crate::scene::{linear_to_srgb8, srgb8_to_linear, LightKind, SceneSnapshot, ShadowRay};
use crate::types::{Aabb, Camera, RgbdFrame};

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

/// What a perfect sensor returns for one pixel.
#[derive(Clone, Copy)]
struct CleanSample {
    /// Forward depth, inside the sensor range.
    z: f32,
    color: [u8; 3],
}

/// The primary ray of pixel `(x, y)`: origin, direction, the cosine between
/// the ray and the optical axis and the ray length at which the forward
/// distance reaches the range limit.
#[inline]
fn pixel_ray(cam: &Camera, forward: Vec3, max_range: f32, x: u32, y: u32) -> (Vec3, Vec3, f32, f32) {
    let (origin, dir) = cam.ray(x as f32 + 0.5, y as f32 + 0.5);
    let cos = dir.dot(forward);
    (origin, dir, cos, max_range / cos.max(1e-6))
}

/// Casts the primary ray of a pixel and shades what it hits. `None` when
/// nothing is hit or the hit is outside the sensor range.
#[inline]
fn clean_sample(snap: &SceneSnapshot, cam: &Camera, forward: Vec3, opts: &CaptureOptions, x: u32, y: u32) -> Option<CleanSample> {
    let (origin, dir, cos, reach) = pixel_ray(cam, forward, opts.max_range, x, y);
    let hit = snap.raycast(origin, dir, reach)?;
    let z = hit.t * cos;
    if z < opts.min_range || z > opts.max_range {
        return None;
    }
    Some(CleanSample { z, color: linear_to_srgb8(snap.shade(&hit, opts.shadows)) })
}

/// Writes a pixel as the sensor of `opts` delivers it: the clean sample
/// degraded by the noise model, or nothing when the pixel drops out.
#[inline]
fn store_sample(opts: &CaptureOptions, pixel: u32, x: usize, sample: CleanSample, rgb: &mut [u8], depth: &mut [f32]) {
    let CleanSample { mut z, mut color } = sample;
    let n = &opts.noise;
    if !n.is_none() {
        if n.dropout > 0.0 && uniform(opts.seed, pixel, 0) < n.dropout {
            return;
        }
        if n.depth_sigma_at_1m > 0.0 {
            let noisy = z + gaussian(opts.seed, pixel, 1) * n.depth_sigma_at_1m * z * z;
            if !noisy.is_finite() {
                return;
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
    depth[x] = z;
    rgb[x * 3..x * 3 + 3].copy_from_slice(&color);
}

fn render_row(snap: &SceneSnapshot, cam: &Camera, opts: &CaptureOptions, y: u32, rgb: &mut [u8], depth: &mut [f32]) {
    let forward = cam.forward();
    for x in 0..cam.width {
        if let Some(sample) = clean_sample(snap, cam, forward, opts, x, y) {
            store_sample(opts, y * cam.width + x, x as usize, sample, rgb, depth);
        }
    }
}

// ---------------------------------------------------------------------------
// Partial capture
// ---------------------------------------------------------------------------

/// What a fixed camera sees of the static part of a scene, noise-free, with
/// everything needed to tell which pixels a moving object can change. See
/// the module documentation.
#[derive(Clone, Debug)]
pub struct StaticRender {
    scene: SceneSnapshot,
    cam: Camera,
    shadows: bool,
    min_range: f32,
    max_range: f32,
    /// Clean forward depth per pixel, `0.0` where the sensor has no return.
    depth: Vec<f32>,
    /// Clean color per pixel.
    rgb: Vec<u8>,
    /// Per pixel: a moving object changes the pixel when its primary ray
    /// meets it within this ray length (just past the static surface, or
    /// the range limit where nothing static is hit).
    reach: Vec<f32>,
    /// Per pixel: the index of its first shadow ray in `shadow_rays`; one
    /// more entry closes the last pixel.
    first_ray: Vec<u32>,
    /// The shadow rays that reach their light from the static surface.
    shadow_rays: Vec<LightRay>,
}

/// A shadow ray and the light it leads to.
#[derive(Clone, Copy, Debug)]
struct LightRay {
    ray: ShadowRay,
    /// Index of the light in the scene's lighting.
    light: u32,
}

/// A static row under construction.
struct StaticRow {
    reach: Vec<f32>,
    ray_counts: Vec<u32>,
    shadow_rays: Vec<LightRay>,
}

impl StaticRender {
    /// A moving surface at the same ray length as the static one may win the
    /// pixel (see the tie rule of the ray caster): the primary test reaches
    /// this much farther than the static hit.
    const REACH_SLACK: f32 = 8.0 * f32::EPSILON;

    /// Renders `scene` (the static part of a scene, as
    /// [`crate::scene::Scene::snapshot_static`] returns it) through `cam`.
    /// Of `opts`, the shadows and the range limits are used; the noise is
    /// applied per exposure.
    pub fn new(scene: &SceneSnapshot, cam: &Camera, opts: &CaptureOptions) -> Self {
        let clean = CaptureOptions { noise: NoiseModel::NONE, ..*opts };
        let forward = cam.forward();
        let width = cam.width as usize;
        let mut frame = RgbdFrame::new(cam.width, cam.height);
        let rows = for_each_row_collect(&mut frame, |y, rgb, depth| {
            let mut row = StaticRow { reach: Vec::with_capacity(width), ray_counts: Vec::with_capacity(width), shadow_rays: Vec::new() };
            for x in 0..cam.width {
                let (origin, dir, cos, reach) = pixel_ray(cam, forward, clean.max_range, x, y);
                let before = row.shadow_rays.len();
                let hit = scene.raycast(origin, dir, reach);
                row.reach.push(hit.map_or(reach, |hit| hit.t * (1.0 + Self::REACH_SLACK)));
                if let Some(hit) = hit {
                    let z = hit.t * cos;
                    if z >= clean.min_range && z <= clean.max_range {
                        depth[x as usize] = z;
                        rgb[x as usize * 3..x as usize * 3 + 3].copy_from_slice(&linear_to_srgb8(scene.shade(&hit, clean.shadows)));
                        if clean.shadows {
                            scene.open_shadow_rays(&hit, |light, ray| row.shadow_rays.push(LightRay { ray, light: light as u32 }));
                        }
                    }
                }
                row.ray_counts.push((row.shadow_rays.len() - before) as u32);
            }
            row
        });
        let mut reach = Vec::with_capacity(frame.depth.len());
        let mut first_ray = Vec::with_capacity(frame.depth.len() + 1);
        let mut shadow_rays = Vec::new();
        for row in rows {
            reach.extend_from_slice(&row.reach);
            let mut next = shadow_rays.len() as u32;
            for count in row.ray_counts {
                first_ray.push(next);
                next += count;
            }
            shadow_rays.extend_from_slice(&row.shadow_rays);
        }
        first_ray.push(shadow_rays.len() as u32);
        Self {
            scene: scene.clone(),
            cam: *cam,
            shadows: clean.shadows,
            min_range: clean.min_range,
            max_range: clean.max_range,
            depth: frame.depth,
            rgb: frame.rgb,
            reach,
            first_ray,
            shadow_rays,
        }
    }

    /// The static scene this render shows.
    pub fn scene(&self) -> &SceneSnapshot {
        &self.scene
    }

    /// The camera this render was taken with.
    pub fn camera(&self) -> &Camera {
        &self.cam
    }

    /// True when a capture through `cam` with `opts` can build on this
    /// render: same camera, same shadows, same range limits.
    pub(crate) fn serves(&self, cam: &Camera, opts: &CaptureOptions) -> bool {
        // Bit patterns, so that a NaN limit matches itself.
        let same = |a: f32, b: f32| a.to_bits() == b.to_bits();
        self.cam == *cam && self.shadows == opts.shadows && same(self.min_range, opts.min_range) && same(self.max_range, opts.max_range)
    }

    /// The clean sample of pixel `i`, if the sensor has a return there.
    #[inline]
    fn sample(&self, i: usize) -> Option<CleanSample> {
        let z = self.depth[i];
        (z != 0.0).then(|| CleanSample { z, color: [self.rgb[i * 3], self.rgb[i * 3 + 1], self.rgb[i * 3 + 2]] })
    }

    /// One exposure of the static scene alone: exactly
    /// [`render_rgbd`]`(self.scene(), self.camera(), opts)` for options that
    /// only differ in noise and seed from those this render was built with.
    /// With other shadows or range limits the scene is rendered afresh.
    pub fn expose(&self, opts: &CaptureOptions) -> RgbdFrame {
        if !self.serves(&self.cam, opts) {
            return render_rgbd(&self.scene, &self.cam, opts);
        }
        let mut frame = RgbdFrame::new(self.cam.width, self.cam.height);
        let width = self.cam.width as usize;
        for_each_row(&mut frame, |y, rgb, depth| {
            for x in 0..width {
                let i = y as usize * width + x;
                if let Some(sample) = self.sample(i) {
                    store_sample(opts, i as u32, x, sample, rgb, depth);
                }
            }
        });
        frame
    }
}

/// One instant of a scene, prepared for capturing it through cameras that
/// have a [`StaticRender`] of its static part. See the module documentation.
#[derive(Clone, Debug)]
pub struct PartialCapture<'a> {
    snap: &'a SceneSnapshot,
    /// The static part the split was made against.
    base: SceneSnapshot,
    /// The moving primitives alone.
    moving: SceneSnapshot,
    /// Their boxes (conservative, see [`SceneSnapshot::padded_boxes`]).
    boxes: Vec<Aabb>,
    /// Per light of the scene: where its shadow rays can meet a moving
    /// primitive at all (directional lights only).
    shadow_masks: Vec<Option<ShadowMask>>,
}

/// Screen-space boxes are widened by this many pixels: far more than the
/// rounding of a projection.
const SCREEN_MARGIN: f32 = 1.5;

/// The moving primitives as a directional light sees them: a grid on the
/// plane perpendicular to the light, marked where the box of a moving
/// primitive projects to. A shadow ray toward the light runs along the
/// light direction, so it projects to one point of that plane and can only
/// be blocked when that point is marked.
#[derive(Clone, Debug)]
struct ShadowMask {
    /// Orthonormal axes of the plane.
    axes: [Vec3; 2],
    /// Plane coordinates of the grid's corner.
    origin: [f32; 2],
    /// Cells per meter.
    scale: [f32; 2],
    cells: Vec<bool>,
}

impl ShadowMask {
    /// Cells along each axis.
    const SIZE: usize = 128;
    /// The boxes are widened by this much on the plane (meters): a ray is
    /// only as parallel to the light as floats allow.
    const MARGIN: f32 = 2.0e-3;

    fn new(to_light: Vec3, boxes: &[Aabb]) -> Self {
        let (a, b) = to_light.any_orthonormal_pair();
        let rect = |bounds: &Aabb| {
            let (mut min, mut max) = ([f32::INFINITY; 2], [f32::NEG_INFINITY; 2]);
            for corner in 0..8 {
                let p = box_corner(bounds, corner);
                let uv = [p.dot(a), p.dot(b)];
                // Farther from the origin the coordinates are coarser.
                let margin = Self::MARGIN + 1.0e-5 * p.abs().max_element();
                for k in 0..2 {
                    min[k] = min[k].min(uv[k] - margin);
                    max[k] = max[k].max(uv[k] + margin);
                }
            }
            (min, max)
        };
        let rects: Vec<_> = boxes.iter().map(rect).collect();
        let (mut origin, mut end) = ([f32::INFINITY; 2], [f32::NEG_INFINITY; 2]);
        for (min, max) in &rects {
            for k in 0..2 {
                origin[k] = origin[k].min(min[k]);
                end[k] = end[k].max(max[k]);
            }
        }
        let scale = [0, 1].map(|k| Self::SIZE as f32 / (end[k] - origin[k]).max(1.0e-6));
        let mut cells = vec![false; Self::SIZE * Self::SIZE];
        // The cell of a coordinate, clamped into the grid (the cast saturates).
        let cell = |value: f32, k: usize| (((value - origin[k]) * scale[k]) as usize).min(Self::SIZE - 1);
        for (min, max) in &rects {
            for y in cell(min[1], 1)..=cell(max[1], 1) {
                cells[y * Self::SIZE + cell(min[0], 0)..=y * Self::SIZE + cell(max[0], 0)].fill(true);
            }
        }
        Self { axes: [a, b], origin, scale, cells }
    }

    /// Can a ray from `point` toward the light meet a moving primitive?
    #[inline]
    fn covers(&self, point: Vec3) -> bool {
        let x = (point.dot(self.axes[0]) - self.origin[0]) * self.scale[0];
        let y = (point.dot(self.axes[1]) - self.origin[1]) * self.scale[1];
        let size = Self::SIZE as f32;
        // Also false for coordinates that are not numbers.
        x >= 0.0 && y >= 0.0 && x < size && y < size && self.cells[y as usize * Self::SIZE + x as usize]
    }
}

/// Corner `index` (0..8) of a box.
#[inline]
fn box_corner(bounds: &Aabb, index: usize) -> Vec3 {
    let pick = |axis: usize| if index >> axis & 1 == 0 { bounds.min[axis] } else { bounds.max[axis] };
    Vec3::new(pick(0), pick(1), pick(2))
}

impl<'a> PartialCapture<'a> {
    /// Splits `snap` into `base`, its static part, and what moves in front
    /// of it. `None` when `snap` is not `base` plus moving primitives (see
    /// [`SceneSnapshot::moving_parts`]): such a snapshot has to be rendered
    /// in full.
    pub fn new(snap: &'a SceneSnapshot, base: &SceneSnapshot) -> Option<Self> {
        let moving = snap.moving_parts(base)?;
        let boxes: Vec<Aabb> = moving.padded_boxes().filter(|b| b.min.is_finite() && b.max.is_finite()).collect();
        let shadow_masks = snap
            .lighting()
            .lights
            .iter()
            .map(|light| match light.kind {
                LightKind::Directional { to_light } if light.casts_shadow && !boxes.is_empty() => Some(ShadowMask::new(to_light, &boxes)),
                _ => None,
            })
            .collect();
        Some(Self { snap, base: base.clone(), moving, boxes, shadow_masks })
    }

    /// Captures the instant through `cam`: the same frame, bit for bit, as
    /// [`render_rgbd`]`(snap, cam, opts)`.
    ///
    /// `fixed` must be a [`StaticRender`] of the static part this capture
    /// was split against, through the same camera and with the same shadows
    /// and range limits as `opts`; otherwise the frame is rendered in full.
    /// With the `par` feature rows are rendered in parallel.
    pub fn render(&self, cam: &Camera, opts: &CaptureOptions, fixed: &StaticRender) -> RgbdFrame {
        if !(fixed.scene.is_same(&self.base) && fixed.serves(cam, opts)) {
            return render_rgbd(self.snap, cam, opts);
        }
        let mut frame = RgbdFrame::new(cam.width, cam.height);
        let width = cam.width as usize;
        let screen = self.screen_mask(cam);
        let forward = cam.forward();
        for_each_row(&mut frame, |y, rgb, depth| {
            for x in 0..width {
                let i = y as usize * width + x;
                let sample = if self.changes(cam, fixed, &screen, x as u32, y) {
                    clean_sample(self.snap, cam, forward, opts, x as u32, y)
                } else {
                    fixed.sample(i)
                };
                if let Some(sample) = sample {
                    store_sample(opts, i as u32, x, sample, rgb, depth);
                }
            }
        });
        frame
    }

    /// Can a moving primitive change pixel `(x, y)` of `fixed`? `screen` is
    /// the [`Self::screen_mask`] of its camera.
    #[inline]
    fn changes(&self, cam: &Camera, fixed: &StaticRender, screen: &[bool], x: u32, y: u32) -> bool {
        let i = (y * cam.width + x) as usize;
        let in_front = screen[i] && {
            let (origin, dir) = cam.ray(x as f32 + 0.5, y as f32 + 0.5);
            self.moving.occluded(origin, dir, fixed.reach[i])
        };
        let rays = &fixed.shadow_rays[fixed.first_ray[i] as usize..fixed.first_ray[i + 1] as usize];
        in_front
            || rays.iter().any(|LightRay { ray, light }| {
                // A light the mask knows nothing about is always tested.
                let reachable = self.shadow_masks.get(*light as usize).and_then(Option::as_ref).is_none_or(|mask| mask.covers(ray.origin));
                reachable && self.moving.occluded(ray.origin, ray.dir, ray.t_max)
            })
    }

    /// Number of pixels [`PartialCapture::render`] casts again through the
    /// camera of `fixed`, or `None` when it would render the frame in full.
    /// For benchmarks and tests.
    #[doc(hidden)]
    pub fn recast_pixels(&self, fixed: &StaticRender) -> Option<usize> {
        if !fixed.scene.is_same(&self.base) {
            return None;
        }
        let cam = &fixed.cam;
        let screen = self.screen_mask(cam);
        let count = (0..cam.height).map(|y| (0..cam.width).filter(|&x| self.changes(cam, fixed, &screen, x, y)).count());
        Some(count.sum())
    }

    /// Per pixel of `cam`: can its primary ray meet a moving primitive? The
    /// union of the screen boxes of their bounds.
    fn screen_mask(&self, cam: &Camera) -> Vec<bool> {
        let (width, height) = (cam.width, cam.height);
        let mut mask = vec![false; (width * height) as usize];
        for bounds in &self.boxes {
            let (mut min, mut max) = ([f32::INFINITY; 2], [f32::NEG_INFINITY; 2]);
            let mut behind = false;
            for corner in 0..8 {
                match cam.project(box_corner(bounds, corner)) {
                    // A corner at or behind the camera plane projects nowhere
                    // useful: the box may cover anything.
                    Some((u, v, z)) if z > 1.0e-4 && u.is_finite() && v.is_finite() => {
                        min = [min[0].min(u), min[1].min(v)];
                        max = [max[0].max(u), max[1].max(v)];
                    }
                    _ => behind = true,
                }
            }
            if behind {
                (min, max) = ([0.0; 2], [width as f32, height as f32]);
            }
            // Pixel `x` has its center at `x + 0.5`; the casts saturate.
            let first = |lo: f32| (lo - SCREEN_MARGIN - 0.5).floor().max(0.0) as u32;
            let end = |hi: f32, size: u32| ((hi + SCREEN_MARGIN + 0.5).ceil().max(0.0) as u32).min(size);
            let (x0, x1) = (first(min[0]), end(max[0], width));
            let (y0, y1) = (first(min[1]), end(max[1], height));
            if x0 >= x1 {
                continue;
            }
            for y in y0..y1 {
                mask[(y * width + x0) as usize..(y * width + x1) as usize].fill(true);
            }
        }
        mask
    }
}

/// Captures `snap` through the camera of `fixed`, re-casting only the pixels
/// that its moving primitives can change: the same frame, bit for bit, as
/// [`render_rgbd`]`(snap, fixed.camera(), opts)`.
///
/// For several cameras at the same instant, split the snapshot once with
/// [`PartialCapture::new`] instead.
pub fn render_rgbd_partial(snap: &SceneSnapshot, opts: &CaptureOptions, fixed: &StaticRender) -> RgbdFrame {
    match PartialCapture::new(snap, &fixed.scene) {
        Some(capture) => capture.render(&fixed.cam, opts, fixed),
        None => render_rgbd(snap, &fixed.cam, opts),
    }
}

/// Runs `f(row, rgb_row, depth_row)` for every image row, in parallel when
/// the `par` feature is enabled.
fn for_each_row(frame: &mut RgbdFrame, f: impl Fn(u32, &mut [u8], &mut [f32]) + Sync) {
    for_each_row_collect(frame, f);
}

/// [`for_each_row`] that also returns what `f` returned, in row order.
fn for_each_row_collect<T: Send>(frame: &mut RgbdFrame, f: impl Fn(u32, &mut [u8], &mut [f32]) -> T + Sync) -> Vec<T> {
    #[cfg(feature = "par")]
    {
        use rayon::prelude::*;
        let w = frame.width as usize;
        if w == 0 {
            return Vec::new();
        }
        frame
            .rgb
            .par_chunks_mut(w * 3)
            .zip(frame.depth.par_chunks_mut(w))
            .enumerate()
            .map(|(y, (rgb, depth))| f(y as u32, rgb, depth))
            .collect()
    }
    #[cfg(not(feature = "par"))]
    for_each_row_serial(frame, f)
}

fn for_each_row_serial<T>(frame: &mut RgbdFrame, f: impl Fn(u32, &mut [u8], &mut [f32]) -> T) -> Vec<T> {
    let w = frame.width as usize;
    if w == 0 {
        return Vec::new();
    }
    frame
        .rgb
        .chunks_mut(w * 3)
        .zip(frame.depth.chunks_mut(w))
        .enumerate()
        .map(|(y, (rgb, depth))| f(y as u32, rgb, depth))
        .collect()
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
    use crate::types::Aabb;

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

    /// A static floor under a point light confined to a room, with moving
    /// things chosen to sit on every edge case of the partial capture.
    fn tricky_scene(t: f32) -> (SceneSnapshot, SceneSnapshot) {
        let gray = Material::solid(Vec3::splat(0.6));
        let fixed = vec![
            Primitive::cuboid(Vec3::new(0.0, -0.5, -3.0), Vec3::new(4.0, 0.5, 4.0), Material::checker(Vec3::X, Vec3::ONE, 0.5)),
            Primitive::cuboid(Vec3::new(0.0, 1.0, -6.5), Vec3::new(4.0, 2.0, 0.5), gray),
            // Two coincident static boxes: a tie between static primitives.
            Primitive::cuboid(Vec3::new(-1.5, 0.3, -3.0), Vec3::splat(0.3), Material::solid(Vec3::X)),
            Primitive::cuboid(Vec3::new(-1.5, 0.3, -3.0), Vec3::splat(0.3), Material::solid(Vec3::Y)),
        ];
        let moving = [
            // Rests exactly on the floor and slides across it.
            Primitive::cuboid(Vec3::new(t.sin(), 0.2, -3.0), Vec3::splat(0.2), Material::solid(Vec3::Z)).with_dynamic(true),
            // A slab flush with the floor surface (coplanar with it).
            Primitive::cuboid(Vec3::new(1.2, -0.25, -2.0 - t.cos()), Vec3::new(0.3, 0.25, 0.3), gray).with_dynamic(true),
            // Passes behind the wall, behind the camera and right in front of it
            // (nearer than the sensor's minimum range).
            Primitive::sphere(Vec3::new(2.0 * t.cos(), 1.0, -7.5), 0.4, gray).with_dynamic(true),
            Primitive::sphere(Vec3::new(0.3, 1.2, 2.0 + t.sin()), 0.5, gray).with_dynamic(true),
            Primitive::sphere(Vec3::new(0.05 * t.sin(), 1.0, -0.12), 0.03, gray).with_dynamic(true),
            // High up: only its shadow is in view.
            Primitive::sphere(Vec3::new(1.5 * t.sin(), 4.5, -3.0 + t.cos()), 0.5, gray).with_dynamic(true),
        ];
        let room = Aabb::new(Vec3::new(-3.0, 0.0, -6.0), Vec3::new(3.0, 5.0, 0.0));
        let lighting = Lighting {
            lights: vec![
                Light::directional(Vec3::new(0.2, 1.0, 0.3), Vec3::splat(0.4)),
                Light::point(Vec3::new(0.0, 6.0, -3.0), 4.0, Vec3::splat(0.8)).with_region(room),
                Light::directional(Vec3::new(-0.5, 0.6, 0.7), Vec3::splat(0.1)).with_shadow(false),
            ],
            ..Lighting::default()
        };
        let base = SceneSnapshot::new(fixed.clone(), lighting.clone());
        // Moving primitives in between the static ones: the order of the
        // static ones is what counts.
        let mut all = vec![moving[0].clone(), fixed[0].clone(), fixed[1].clone()];
        all.extend_from_slice(&moving[1..4]);
        all.extend_from_slice(&fixed[2..]);
        all.extend_from_slice(&moving[4..]);
        (base, SceneSnapshot::new(all, lighting))
    }

    #[test]
    fn partial_capture_is_bit_identical_on_the_stage() {
        let scene = make_scene(SceneKind::Stage);
        let empty = scene.snapshot_static();
        let period = scene.loop_duration().unwrap();
        let rig = crate::rig::RingRig { width: 120, height: 90, ..Default::default() };
        let mut cameras = rig.cameras();
        // One camera inside the capture volume, where the dancer passes
        // closer than the minimum range and behind the camera.
        cameras.push(Camera::look_at(Vec3::new(0.3, 1.2, 0.5), Vec3::new(-0.4, 0.6, -0.8), Vec3::Y, 1.2, 96, 72));
        let instants = 40;
        let (mut recast, mut pixels) = (0usize, 0usize);
        for shadows in [true, false] {
            for noise in [NoiseModel::NONE, NoiseModel::realistic()] {
                let base = CaptureOptions { shadows, noise, ..Default::default() };
                let fixed: Vec<StaticRender> = cameras.iter().map(|cam| StaticRender::new(&empty, cam, &base)).collect();
                for k in 0..instants {
                    let t = period * (k as f32 + 0.37) / instants as f32;
                    let snap = scene.snapshot(t);
                    let capture = PartialCapture::new(&snap, &empty).expect("the stage is its static part plus what moves");
                    for (i, cam) in cameras.iter().enumerate() {
                        let opts = CaptureOptions { seed: (k * 31 + i) as u32, ..base };
                        let partial = capture.render(cam, &opts, &fixed[i]);
                        assert!(partial == render_rgbd(&snap, cam, &opts), "camera {i} at t = {t}, shadows {shadows}, {noise:?}");
                        if shadows && noise.is_none() && i < 8 {
                            recast += capture.recast_pixels(&fixed[i]).unwrap();
                            pixels += (cam.width * cam.height) as usize;
                        }
                    }
                }
                // The empty stage itself, exposure by exposure.
                for (i, cam) in cameras.iter().enumerate() {
                    let opts = CaptureOptions { seed: 1000 + i as u32, ..base };
                    assert!(fixed[i].expose(&opts) == render_rgbd(&empty, cam, &opts), "plate of camera {i}");
                    assert!(render_rgbd_partial(&empty, &opts, &fixed[i]) == render_rgbd(&empty, cam, &opts));
                }
            }
        }
        // Measured: 5.96 % of the pixels of the ring are cast again, which
        // is the share that really differs from the empty stage.
        let share = recast as f32 / pixels as f32;
        println!("partial capture of the stage casts {:.2} % of the pixels again", 100.0 * share);
        assert!((0.03..0.12).contains(&share), "{share}");
    }

    #[test]
    fn partial_capture_is_bit_identical_in_awkward_scenes() {
        let cameras = [
            Camera::look_at(Vec3::new(0.0, 1.0, 0.0), Vec3::new(0.0, 0.5, -3.0), Vec3::Y, 1.2, 96, 72),
            Camera::look_at(Vec3::new(2.5, 3.0, -0.5), Vec3::new(0.0, 0.0, -3.0), Vec3::Y, 0.9, 80, 60),
            // Straight down the floor, at a grazing angle.
            Camera::look_at(Vec3::new(-3.5, 0.05, -3.0), Vec3::new(3.0, 0.0, -3.0), Vec3::Y, 1.0, 96, 48),
        ];
        let (base, _) = tricky_scene(0.0);
        for shadows in [true, false] {
            for (min_range, max_range) in [(0.1, 20.0), (0.5, 4.0), (0.1, f32::INFINITY), (0.1, f32::NAN)] {
                let opts = CaptureOptions { shadows, min_range, max_range, noise: NoiseModel::realistic(), seed: 9 };
                let fixed: Vec<StaticRender> = cameras.iter().map(|cam| StaticRender::new(&base, cam, &opts)).collect();
                for k in 0..24 {
                    // The same static part in a snapshot of its own every time.
                    let (_, snap) = tricky_scene(k as f32 * 0.29);
                    let capture = PartialCapture::new(&snap, &base).expect("static part plus moving primitives");
                    for (cam, fixed) in cameras.iter().zip(&fixed) {
                        let opts = CaptureOptions { seed: k, ..opts };
                        let full = render_rgbd(&snap, cam, &opts);
                        assert!(capture.render(cam, &opts, fixed) == full, "t {k}, shadows {shadows}, range {min_range}..{max_range}");
                        assert!(render_rgbd_partial(&snap, &opts, fixed) == full);
                    }
                }
            }
        }
    }

    #[test]
    fn partial_capture_falls_back_to_a_full_render_when_it_cannot_be_exact() {
        let (base, snap) = tricky_scene(0.4);
        let cam = Camera::look_at(Vec3::new(0.0, 1.0, 0.0), Vec3::new(0.0, 0.5, -3.0), Vec3::Y, 1.2, 64, 48);
        let opts = CaptureOptions::default();
        let fixed = StaticRender::new(&base, &cam, &opts);
        let full = render_rgbd(&snap, &cam, &opts);
        let capture = PartialCapture::new(&snap, &base).unwrap();
        assert!(capture.recast_pixels(&fixed).is_some_and(|n| n > 0 && n < 64 * 48 / 2));
        // Other options or another camera than the static render was made for.
        for other in [
            CaptureOptions { shadows: false, ..opts },
            CaptureOptions { max_range: 5.0, ..opts },
            CaptureOptions { min_range: 1.5, ..opts },
        ] {
            assert_eq!(capture.render(&cam, &other, &fixed), render_rgbd(&snap, &cam, &other));
            assert_eq!(fixed.expose(&other), render_rgbd(&base, &cam, &other));
        }
        let wider = cam.resized(80, 60);
        assert_eq!(capture.render(&wider, &opts, &fixed), render_rgbd(&snap, &wider, &opts));
        // A static render of an equal but separate scene.
        let (twin, _) = tricky_scene(0.0);
        let foreign = StaticRender::new(&twin, &cam, &opts);
        assert!(capture.recast_pixels(&foreign).is_none());
        assert_eq!(capture.render(&cam, &opts, &foreign), full);
        // A snapshot that is not "the static part plus moving primitives".
        let (_, later) = tricky_scene(1.0);
        assert!(PartialCapture::new(&base, &later).is_none(), "the base holds moving primitives");
        let stage = make_scene(SceneKind::Stage).snapshot(1.0);
        assert!(PartialCapture::new(&stage, &base).is_none(), "another scene");
        assert_eq!(render_rgbd_partial(&stage, &opts, &fixed), render_rgbd(&stage, &cam, &opts));
        // A scene without anything moving is all static render.
        let house = make_scene(SceneKind::House).snapshot(0.0);
        let still = StaticRender::new(&house, &cam, &opts);
        assert_eq!(PartialCapture::new(&house, &house).unwrap().recast_pixels(&still), Some(0));
        assert_eq!(render_rgbd_partial(&house, &opts, &still), render_rgbd(&house, &cam, &opts));
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
