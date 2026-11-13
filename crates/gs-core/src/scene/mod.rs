//! Analytic scene description, ray casting and shading.
//!
//! A [`Scene`] is a procedural world that can be frozen at any time `t` into
//! an immutable [`SceneSnapshot`]: a list of analytic [`Primitive`]s, a
//! [`Lighting`] setup and a BVH. The simulated depth cameras in
//! [`crate::capture`] ray cast snapshots; nothing downstream ever sees the
//! primitives themselves.
//!
//! Shading is purely diffuse and view independent, so every camera of a rig
//! observes the same color for a given surface point (what a real multi-view
//! capture approximates after color calibration).

use std::sync::{Arc, OnceLock};

use glam::{Vec3, Vec3A};
use serde::{Deserialize, Serialize};

use crate::types::Aabb;

mod bvh;
pub mod house;
pub mod material;
pub mod primitive;
pub mod stage;

pub use house::HouseScene;
pub use material::{srgb, Axis, Material, Pattern};
pub use primitive::{Primitive, Shape};
pub use stage::StageScene;

use bvh::Bvh;

/// Rays ignore intersections closer than this (meters).
const T_MIN: f32 = 1.0e-5;
/// Shadow rays start this far above the surface (meters).
const SHADOW_BIAS: f32 = 1.0e-3;

// ---------------------------------------------------------------------------
// Color space helpers
// ---------------------------------------------------------------------------

/// sRGB transfer function (linear -> encoded), both in `[0, 1]`.
#[inline]
fn srgb_encode(x: f32) -> f32 {
    if x <= 0.003_130_8 {
        12.92 * x
    } else {
        1.055 * x.powf(1.0 / 2.4) - 0.055
    }
}

/// Inverse sRGB transfer function (encoded -> linear), both in `[0, 1]`.
#[inline]
fn srgb_decode(x: f32) -> f32 {
    if x <= 0.040_45 {
        x / 12.92
    } else {
        ((x + 0.055) / 1.055).powf(2.4)
    }
}

const ENCODE_LUT_SIZE: usize = 1024;

struct SrgbTables {
    /// Linear value of each 8-bit sRGB code.
    decode: [f32; 256],
    /// Linear value above which the encoded result rounds to `code + 1`.
    upper: [f32; 256],
    /// First guess of the code, indexed by `sqrt(linear)`.
    guess: [u8; ENCODE_LUT_SIZE],
}

fn srgb_tables() -> &'static SrgbTables {
    static TABLES: OnceLock<SrgbTables> = OnceLock::new();
    TABLES.get_or_init(|| {
        let mut t = SrgbTables { decode: [0.0; 256], upper: [f32::INFINITY; 256], guess: [0; ENCODE_LUT_SIZE] };
        for code in 0..256 {
            t.decode[code] = srgb_decode(code as f32 / 255.0);
            if code < 255 {
                t.upper[code] = srgb_decode((code as f32 + 0.5) / 255.0);
            }
        }
        for (i, g) in t.guess.iter_mut().enumerate() {
            let s = i as f32 / (ENCODE_LUT_SIZE - 1) as f32;
            *g = (srgb_encode(s * s) * 255.0 + 0.5) as u8;
        }
        t
    })
}

#[inline]
fn encode_channel(t: &SrgbTables, x: f32) -> u8 {
    // NaN and negatives clamp to black, overflows to white.
    let x = if x > 0.0 { x.min(1.0) } else { 0.0 };
    // The table lookup is within one code of the answer; the threshold table
    // then makes the rounding exact without a `powf` per pixel.
    let mut code = t.guess[(x.sqrt() * (ENCODE_LUT_SIZE - 1) as f32 + 0.5) as usize] as usize;
    while x >= t.upper[code] {
        code += 1;
    }
    while code > 0 && x < t.upper[code - 1] {
        code -= 1;
    }
    code as u8
}

/// Converts a linear RGB color to 8-bit sRGB (clamping to `[0, 1]`).
#[inline]
pub fn linear_to_srgb8(linear: Vec3) -> [u8; 3] {
    let t = srgb_tables();
    [encode_channel(t, linear.x), encode_channel(t, linear.y), encode_channel(t, linear.z)]
}

/// Converts an 8-bit sRGB color to linear RGB in `[0, 1]`.
#[inline]
pub fn srgb8_to_linear(srgb: [u8; 3]) -> Vec3 {
    let t = srgb_tables();
    Vec3::new(t.decode[srgb[0] as usize], t.decode[srgb[1] as usize], t.decode[srgb[2] as usize])
}

// ---------------------------------------------------------------------------
// Lighting
// ---------------------------------------------------------------------------

/// Geometry of a light source.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LightKind {
    /// Infinitely distant light. `to_light` is the unit direction from a
    /// surface toward the light.
    Directional { to_light: Vec3 },
    /// Point light with a soft falloff `1 / (1 + (d / reference_distance)^2)`.
    Point { pos: Vec3, reference_distance: f32 },
}

/// One light source.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Light {
    pub kind: LightKind,
    /// Linear RGB intensity at normal incidence (before falloff).
    pub color: Vec3,
    /// Whether the light casts hard shadows when shadows are enabled.
    pub casts_shadow: bool,
    /// Optional box the light is confined to (a room). Its contribution fades
    /// linearly to zero over [`Light::REGION_FADE`] meters outside the box, so
    /// a ceiling lamp does not light the rooms behind its walls even when
    /// shadows are disabled.
    pub region: Option<Aabb>,
}

impl Light {
    /// Distance over which a region-limited light fades out outside its box.
    pub const REGION_FADE: f32 = 0.5;

    /// Shadow-casting directional light shining from `to_light`.
    pub fn directional(to_light: Vec3, color: Vec3) -> Self {
        Self { kind: LightKind::Directional { to_light: to_light.normalize() }, color, casts_shadow: true, region: None }
    }

    /// Shadow-casting point light.
    pub fn point(pos: Vec3, reference_distance: f32, color: Vec3) -> Self {
        Self { kind: LightKind::Point { pos, reference_distance }, color, casts_shadow: true, region: None }
    }

    /// Confines the light to a box (see [`Light::region`]).
    pub fn with_region(mut self, region: Aabb) -> Self {
        self.region = Some(region);
        self
    }

    /// Enables or disables shadow casting.
    pub fn with_shadow(mut self, casts_shadow: bool) -> Self {
        self.casts_shadow = casts_shadow;
        self
    }
}

/// Light setup of a scene: hemispheric ambient, a few lights and the sky.
#[derive(Clone, Debug, PartialEq)]
pub struct Lighting {
    /// Ambient radiance arriving from straight above (linear RGB).
    pub ambient_sky: Vec3,
    /// Ambient radiance arriving from straight below.
    pub ambient_ground: Vec3,
    pub lights: Vec<Light>,
    /// Background color looking straight up.
    pub sky_zenith: Vec3,
    /// Background color at and below the horizon.
    pub sky_horizon: Vec3,
}

impl Default for Lighting {
    fn default() -> Self {
        Self {
            ambient_sky: Vec3::splat(0.6),
            ambient_ground: Vec3::splat(0.3),
            lights: Vec::new(),
            sky_zenith: Vec3::new(0.25, 0.45, 0.85),
            sky_horizon: Vec3::new(0.75, 0.82, 0.9),
        }
    }
}

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

/// Result of a ray cast.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Hit {
    /// Ray length to the hit (in units of the ray direction's length).
    pub t: f32,
    /// World-space hit position.
    pub pos: Vec3,
    /// Unit surface normal, flipped to face the side the ray came from.
    pub normal: Vec3,
    /// Diffuse albedo at the hit, linear RGB.
    pub albedo: Vec3,
    /// Index of the primitive in [`SceneSnapshot::primitives`].
    pub prim: u32,
    /// Whether the primitive moves over time.
    pub dynamic: bool,
}

/// An immutable scene at one instant: primitives, lights and a BVH.
///
/// Cloning is cheap (the data is shared), so a static scene can hand out the
/// same snapshot again and again.
#[derive(Clone, Debug)]
pub struct SceneSnapshot {
    inner: Arc<SnapshotData>,
}

#[derive(Debug)]
struct SnapshotData {
    prims: Vec<Primitive>,
    bvh: Bvh,
    lighting: Lighting,
    bounds: Aabb,
}

impl SceneSnapshot {
    /// Freezes a set of primitives and builds the acceleration structure.
    pub fn new(prims: Vec<Primitive>, lighting: Lighting) -> Self {
        let boxes: Vec<Aabb> = prims.iter().map(Primitive::world_aabb).collect();
        let bounds = boxes.iter().fold(Aabb::EMPTY, |acc, b| acc.union(b));
        Self { inner: Arc::new(SnapshotData { bvh: Bvh::build(&boxes), prims, lighting, bounds }) }
    }

    /// Nearest surface along the ray `origin + t * dir` with `t < t_max`.
    ///
    /// `dir` should be a unit vector; `t` is then a distance in meters.
    pub fn raycast(&self, origin: Vec3, dir: Vec3, t_max: f32) -> Option<Hit> {
        let (o, d) = (Vec3A::from(origin), Vec3A::from(dir));
        let mut best: Option<(u32, Vec3A)> = None;
        let mut t_hit = t_max;
        self.inner.bvh.traverse(o, d, t_max, |i, t_far| {
            if let Some((t, n)) = self.inner.prims[i as usize].intersect(o, d, T_MIN, *t_far) {
                *t_far = t;
                t_hit = t;
                best = Some((i, n));
            }
            false
        });
        best.map(|(i, n)| self.finish_hit(o, d, t_hit, i, n))
    }

    #[inline]
    fn finish_hit(&self, o: Vec3A, d: Vec3A, t: f32, prim: u32, local_normal: Vec3A) -> Hit {
        let p = &self.inner.prims[prim as usize];
        let pos = o + d * t;
        let mut normal = p.world_normal(local_normal);
        if normal.dot(d) > 0.0 {
            normal = -normal;
        }
        Hit {
            t,
            pos: pos.into(),
            normal: normal.into(),
            albedo: p.material.albedo(p.object_point(pos)),
            prim,
            dynamic: p.dynamic,
        }
    }

    /// True when anything blocks the segment `origin .. origin + t_max * dir`.
    pub fn occluded(&self, origin: Vec3, dir: Vec3, t_max: f32) -> bool {
        let (o, d) = (Vec3A::from(origin), Vec3A::from(dir));
        let mut blocked = false;
        self.inner.bvh.traverse(o, d, t_max, |i, t_far| {
            blocked = self.inner.prims[i as usize].intersect(o, d, T_MIN, *t_far).is_some();
            blocked
        });
        blocked
    }

    /// Outgoing radiance (linear RGB) at a hit.
    ///
    /// Diffuse only and independent of the viewing direction: hemispheric
    /// ambient plus the Lambert term of every light, optionally with hard
    /// shadows, plus the material's self-illumination.
    pub fn shade(&self, hit: &Hit, shadows: bool) -> Vec3 {
        let l = &self.inner.lighting;
        let n = hit.normal;
        let mut irradiance = l.ambient_ground.lerp(l.ambient_sky, 0.5 + 0.5 * n.y);
        let shadow_origin = hit.pos + n * SHADOW_BIAS;
        for light in &l.lights {
            let mut weight = 1.0;
            if let Some(region) = &light.region {
                let outside = (region.min - hit.pos).max(hit.pos - region.max).max(Vec3::ZERO).length();
                weight = 1.0 - outside / Light::REGION_FADE;
                if weight <= 0.0 {
                    continue;
                }
            }
            let (to_light, distance) = match light.kind {
                LightKind::Directional { to_light } => (to_light, f32::INFINITY),
                LightKind::Point { pos, reference_distance } => {
                    let v = pos - hit.pos;
                    let d = v.length();
                    let q = d / reference_distance;
                    weight /= 1.0 + q * q;
                    (v / d.max(1e-6), d)
                }
            };
            let cos = n.dot(to_light);
            if cos <= 0.0 {
                continue;
            }
            if shadows && light.casts_shadow && self.occluded(shadow_origin, to_light, distance - SHADOW_BIAS) {
                continue;
            }
            irradiance += light.color * (cos * weight);
        }
        let emissive = self.inner.prims[hit.prim as usize].material.emissive;
        hit.albedo * (irradiance + Vec3::splat(emissive))
    }

    /// Background radiance (linear RGB) seen along `dir` when nothing is hit.
    pub fn sky(&self, dir: Vec3) -> Vec3 {
        let up = dir.normalize_or_zero().y.clamp(0.0, 1.0);
        self.inner.lighting.sky_horizon.lerp(self.inner.lighting.sky_zenith, up.sqrt())
    }

    /// World bounds of all primitives ([`Aabb::EMPTY`] for an empty snapshot).
    pub fn bounds(&self) -> Aabb {
        self.inner.bounds
    }

    /// Number of primitives.
    pub fn len(&self) -> usize {
        self.inner.prims.len()
    }

    /// True when the snapshot holds no primitive.
    pub fn is_empty(&self) -> bool {
        self.inner.prims.is_empty()
    }

    /// The primitives, indexed by [`Hit::prim`].
    pub fn primitives(&self) -> &[Primitive] {
        &self.inner.prims
    }

    /// The light setup.
    pub fn lighting(&self) -> &Lighting {
        &self.inner.lighting
    }

    /// Reference ray cast testing every primitive; must agree with
    /// [`SceneSnapshot::raycast`]. For tests and diagnostics.
    #[doc(hidden)]
    pub fn raycast_brute_force(&self, origin: Vec3, dir: Vec3, t_max: f32) -> Option<Hit> {
        let (o, d) = (Vec3A::from(origin), Vec3A::from(dir));
        let mut best: Option<(u32, Vec3A)> = None;
        let mut t_hit = t_max;
        for (i, p) in self.inner.prims.iter().enumerate() {
            if let Some((t, n)) = p.intersect(o, d, T_MIN, t_hit) {
                t_hit = t;
                best = Some((i as u32, n));
            }
        }
        best.map(|(i, n)| self.finish_hit(o, d, t_hit, i, n))
    }
}

// ---------------------------------------------------------------------------
// Scenes
// ---------------------------------------------------------------------------

/// A procedural, possibly animated world.
pub trait Scene: Send + Sync {
    /// Short identifier (`"stage"`, `"house"`).
    fn name(&self) -> &'static str;

    /// The whole scene at time `t` (seconds).
    fn snapshot(&self, t: f32) -> SceneSnapshot;

    /// Only the primitives that never move: the "empty plate".
    fn snapshot_static(&self) -> SceneSnapshot;

    /// Box containing the scene at every instant.
    fn bounds(&self) -> Aabb;

    /// Period of the animation in seconds, `None` for a static scene.
    fn loop_duration(&self) -> Option<f32>;

    /// A good first viewpoint for a free viewer: `(eye, look target)`.
    fn viewer_start(&self) -> (Vec3, Vec3);
}

/// The built-in scenes.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub enum SceneKind {
    /// Volumetric-capture stage with an animated dancer.
    Stage,
    /// Furnished house interior for the walk-through scan.
    House,
}

/// Instantiates a built-in scene.
pub fn make_scene(kind: SceneKind) -> Box<dyn Scene> {
    match kind {
        SceneKind::Stage => Box::new(StageScene::new()),
        SceneKind::House => Box::new(HouseScene::new()),
    }
}

#[cfg(test)]
pub(crate) mod test_util {
    use glam::Vec3;

    /// Small deterministic generator for randomized tests.
    pub struct Lcg(pub u64);

    impl Lcg {
        pub fn next_f32(&mut self) -> f32 {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.0 >> 40) as f32) / (1u64 << 24) as f32
        }

        pub fn range(&mut self, lo: f32, hi: f32) -> f32 {
            lo + (hi - lo) * self.next_f32()
        }

        pub fn unit_vec(&mut self) -> Vec3 {
            loop {
                let v = Vec3::new(self.range(-1.0, 1.0), self.range(-1.0, 1.0), self.range(-1.0, 1.0));
                let l = v.length_squared();
                if l > 1e-3 && l <= 1.0 {
                    return v / l.sqrt();
                }
            }
        }

        pub fn in_box(&mut self, min: Vec3, max: Vec3) -> Vec3 {
            Vec3::new(self.range(min.x, max.x), self.range(min.y, max.y), self.range(min.z, max.z))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_util::Lcg;
    use super::*;

    #[test]
    fn srgb_round_trip_is_exact() {
        for code in 0..=255u8 {
            let lin = srgb8_to_linear([code; 3]);
            assert_eq!(linear_to_srgb8(lin), [code; 3]);
        }
    }

    #[test]
    fn srgb_encode_matches_formula_and_clamps() {
        let mut rng = Lcg(7);
        for i in 0..20000 {
            let x = if i < 2000 { rng.range(0.0, 0.01) } else { rng.range(0.0, 1.0) };
            let exact = srgb_encode(x) * 255.0;
            let got = linear_to_srgb8(Vec3::splat(x))[0] as f32;
            assert!((got - exact).abs() <= 0.5 + 2e-3, "{x}: {got} vs {exact}");
        }
        assert_eq!(linear_to_srgb8(Vec3::new(-1.0, 2.0, f32::NAN)), [0, 255, 0]);
        assert_eq!(linear_to_srgb8(Vec3::new(0.0, 1.0, 0.5)), [0, 255, 188]);
    }

    #[test]
    fn scene_kind_serializes_snake_case() {
        assert_eq!(serde_json::to_string(&SceneKind::Stage).unwrap(), "\"stage\"");
        assert_eq!(serde_json::from_str::<SceneKind>("\"house\"").unwrap(), SceneKind::House);
        assert_eq!(make_scene(SceneKind::Stage).name(), "stage");
        assert_eq!(make_scene(SceneKind::House).name(), "house");
    }

    fn two_spheres() -> SceneSnapshot {
        let floor = Primitive::cuboid(Vec3::new(0.0, -0.5, 0.0), Vec3::new(10.0, 0.5, 10.0), Material::solid(Vec3::splat(0.8)));
        let ball = Primitive::sphere(Vec3::new(0.0, 1.0, 0.0), 0.5, Material::solid(Vec3::new(1.0, 0.0, 0.0))).with_dynamic(true);
        let lighting = Lighting {
            ambient_sky: Vec3::splat(0.2),
            ambient_ground: Vec3::splat(0.2),
            lights: vec![Light::directional(Vec3::Y, Vec3::splat(0.5))],
            ..Lighting::default()
        };
        SceneSnapshot::new(vec![floor, ball], lighting)
    }

    #[test]
    fn empty_snapshot() {
        let s = SceneSnapshot::new(Vec::new(), Lighting::default());
        assert!(s.is_empty() && s.len() == 0 && s.bounds().is_empty());
        assert!(s.raycast(Vec3::ZERO, Vec3::X, 100.0).is_none());
        assert!(!s.occluded(Vec3::ZERO, Vec3::X, 100.0));
    }

    #[test]
    fn raycast_reports_nearest_hit_and_respects_t_max() {
        let s = two_spheres();
        let hit = s.raycast(Vec3::new(0.0, 5.0, 0.0), Vec3::NEG_Y, 100.0).unwrap();
        assert!((hit.t - 3.5).abs() < 1e-5);
        assert_eq!(hit.prim, 1);
        assert!(hit.dynamic);
        assert!((hit.normal - Vec3::Y).length() < 1e-5);
        assert!((hit.pos - Vec3::new(0.0, 1.5, 0.0)).length() < 1e-5);
        assert_eq!(hit.albedo, Vec3::new(1.0, 0.0, 0.0));
        assert!(s.raycast(Vec3::new(0.0, 5.0, 0.0), Vec3::NEG_Y, 3.4).is_none());
        let floor = s.raycast(Vec3::new(3.0, 5.0, 0.0), Vec3::NEG_Y, 100.0).unwrap();
        assert_eq!(floor.prim, 0);
        assert!(!floor.dynamic);
        assert!(s.raycast(Vec3::new(0.0, 5.0, 0.0), Vec3::Y, 100.0).is_none());
    }

    #[test]
    fn occlusion_and_shadows() {
        let s = two_spheres();
        assert!(s.occluded(Vec3::new(0.0, 5.0, 0.0), Vec3::NEG_Y, 100.0));
        assert!(!s.occluded(Vec3::new(0.0, 5.0, 0.0), Vec3::NEG_Y, 3.0));
        assert!(!s.occluded(Vec3::new(3.0, 5.0, 0.0), Vec3::X, 100.0));

        // Under the ball the floor only receives ambient light when shadows are on.
        let under = s.raycast(Vec3::new(0.1, 0.2, 0.0), Vec3::NEG_Y, 10.0).unwrap();
        let open = s.raycast(Vec3::new(3.0, 0.2, 0.0), Vec3::NEG_Y, 10.0).unwrap();
        let lit = s.shade(&open, true);
        assert!((lit - Vec3::splat(0.8 * 0.7)).abs().max_element() < 1e-5);
        assert!((s.shade(&under, true) - Vec3::splat(0.8 * 0.2)).abs().max_element() < 1e-5);
        assert!((s.shade(&under, false) - lit).abs().max_element() < 1e-5);
    }

    #[test]
    fn shading_is_view_independent() {
        let s = two_spheres();
        let target = Vec3::new(0.2, 1.0, 0.1).normalize() * 0.5 + Vec3::new(0.0, 1.0, 0.0);
        let mut colors = Vec::new();
        for eye in [Vec3::new(3.0, 3.0, 2.0), Vec3::new(1.0, 4.0, -0.5), Vec3::new(2.0, 2.2, 3.0)] {
            let dir = (target - eye).normalize();
            let hit = s.raycast(eye, dir, 100.0).unwrap();
            assert!((hit.pos - target).length() < 1e-3);
            assert!(hit.normal.dot(dir) < 0.0);
            colors.push(s.shade(&hit, true));
        }
        assert!((colors[0] - colors[1]).abs().max_element() < 2e-3);
        assert!((colors[0] - colors[2]).abs().max_element() < 2e-3);
    }

    #[test]
    fn region_limited_point_light_and_emissive() {
        let floor = Primitive::cuboid(Vec3::new(0.0, -0.5, 0.0), Vec3::new(10.0, 0.5, 10.0), Material::solid(Vec3::ONE));
        let glow = Primitive::cuboid(Vec3::new(0.0, 0.5, -3.0), Vec3::splat(0.5), Material::solid(Vec3::ONE).with_emissive(2.0));
        let region = Aabb::new(Vec3::new(-2.0, 0.0, -2.0), Vec3::new(2.0, 3.0, 2.0));
        let light = Light::point(Vec3::new(0.0, 2.0, 0.0), 2.0, Vec3::splat(1.0)).with_region(region);
        let lighting = Lighting { ambient_sky: Vec3::ZERO, ambient_ground: Vec3::ZERO, lights: vec![light], ..Lighting::default() };
        let s = SceneSnapshot::new(vec![floor, glow], lighting);
        let shade_at = |x: f32| {
            let h = s.raycast(Vec3::new(x, 1.0, 0.0), Vec3::NEG_Y, 10.0).unwrap();
            s.shade(&h, false).x
        };
        // Directly below: cos = 1, falloff 1 / (1 + 1).
        assert!((shade_at(0.0) - 0.5).abs() < 1e-4);
        assert!(shade_at(1.9) > 0.0);
        assert!(shade_at(2.25) > 0.0 && shade_at(2.25) < shade_at(1.9) * 0.6);
        assert_eq!(shade_at(2.6), 0.0);
        // Emissive surfaces glow without any light reaching them.
        let h = s.raycast(Vec3::new(0.0, 0.5, -10.0), Vec3::Z, 100.0).unwrap();
        assert!((s.shade(&h, true) - Vec3::splat(2.0)).abs().max_element() < 1e-5);
    }

    #[test]
    fn sky_gradient() {
        let s = two_spheres();
        assert_eq!(s.sky(Vec3::Y), s.lighting().sky_zenith);
        assert_eq!(s.sky(Vec3::NEG_Y), s.lighting().sky_horizon);
        assert_eq!(s.sky(Vec3::X), s.lighting().sky_horizon);
    }

    #[test]
    fn snapshot_is_send_sync() {
        fn check<T: Send + Sync>() {}
        check::<SceneSnapshot>();
        check::<Box<dyn Scene>>();
    }
}
