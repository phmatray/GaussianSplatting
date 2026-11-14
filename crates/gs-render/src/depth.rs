//! Outlier-robust depth keys for the sorter.
//!
//! `gs_core::sort::DepthSorter` quantizes depths to 16 bits over the range it
//! is given. One stray splat far away from everything else (a "floater" in an
//! imported file, a corrupt record) stretches that range, and the splats that
//! matter end up sharing a handful of buckets: surfaces blend in storage
//! order and the back of an object shows through its front.
//!
//! The fix is to hand the sorter *warped* depths: unchanged inside the range
//! where the bulk of the splats lives ([`RobustBounds`]), compressed outside
//! of it ([`DepthWarp`]). The warp is monotonic, so the order is the same as
//! for the true depths; only the resolution is spent where the splats are.
//! Whatever the outliers, the scene keeps at least a ninth of the sorter's
//! resolution (a splat a million scene radii away costs it a factor of six,
//! not a million).

use glam::Vec3;

/// Positions sampled per layer for the percentile estimate.
const MAX_SAMPLES: usize = 2048;
/// The robust box spans this central fraction of the samples on every axis.
const INLIER_FRACTION: f32 = 0.98;

/// The full-resolution sphere is the circumsphere of the robust box, enlarged
/// by this factor so that it also holds the 2 % the percentiles cut off.
const SPHERE_MARGIN: f32 = 1.5;

/// Box around the bulk of a set of positions: per axis, from the 1st to the
/// 99th percentile of an evenly strided sample. Up to about 1 % of far
/// outliers do not move it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct RobustBounds {
    pub min: Vec3,
    pub max: Vec3,
}

impl RobustBounds {
    /// Bounds of `positions`, or `None` when no position is finite.
    /// O(`MAX_SAMPLES`), whatever the number of positions.
    pub fn of(positions: &[[f32; 3]]) -> Option<Self> {
        let count = positions.len().min(MAX_SAMPLES);
        let samples: Vec<Vec3> = (0..count)
            // Evenly spread over the whole slice, first and last included.
            // (64-bit: the product overflows `usize` on wasm32 for millions of splats.)
            .map(|i| {
                Vec3::from(positions[if count > 1 { (i as u64 * (positions.len() - 1) as u64 / (count - 1) as u64) as usize } else { 0 }])
            })
            .filter(|p| p.is_finite())
            .collect();
        if samples.is_empty() {
            return None;
        }
        let tail = (0.5 * (1.0 - INLIER_FRACTION) * (samples.len() - 1) as f32) as usize;
        let mut axis = Vec::with_capacity(samples.len());
        let mut percentiles = |component: fn(&Vec3) -> f32| {
            axis.clear();
            axis.extend(samples.iter().map(component));
            let last = axis.len() - 1;
            let lo = *axis.select_nth_unstable_by(tail, f32::total_cmp).1;
            let hi = *axis.select_nth_unstable_by(last - tail, f32::total_cmp).1;
            (lo, hi)
        };
        let (x, y, z) = (percentiles(|p| p.x), percentiles(|p| p.y), percentiles(|p| p.z));
        Some(Self { min: Vec3::new(x.0, y.0, z.0), max: Vec3::new(x.1, y.1, z.1) })
    }

    /// Smallest box containing both (either may be absent).
    pub fn union(a: Option<Self>, b: Option<Self>) -> Option<Self> {
        match (a, b) {
            (Some(a), Some(b)) => Some(Self { min: a.min.min(b.min), max: a.max.max(b.max) }),
            (a, b) => a.or(b),
        }
    }
}

/// Compression of the logarithmic distance beyond the sphere saturates at
/// this many radii: the warped depth range is at most `2·(1 + 8)` radii.
const MAX_OUTSIDE_RADII: f32 = 8.0;

/// Monotonic remapping of view depths: identity within `radius` of `center`,
/// compressed beyond: logarithmic at first (slope 1 at the boundary, so
/// nearby surroundings lose little), approaching `1 + MAX_OUTSIDE_RADII`
/// radii for astronomically distant splats.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct DepthWarp {
    center: f32,
    radius: f32,
    /// `ln(radius)`.
    ln_radius: f32,
}

impl DepthWarp {
    /// The identity.
    pub const NONE: Self = Self { center: 0.0, radius: f32::INFINITY, ln_radius: 0.0 };

    /// Warp for depths computed as `forward · p + offset`, keeping full
    /// resolution inside the sphere around `bounds`. `forward` need not be
    /// normalized (a view matrix with a world scale).
    pub fn new(bounds: Option<RobustBounds>, forward: Vec3, offset: f32) -> Self {
        let Some(bounds) = bounds else { return Self::NONE };
        let center = forward.dot(0.5 * (bounds.min + bounds.max)) + offset;
        let radius = SPHERE_MARGIN * 0.5 * (bounds.max - bounds.min).length() * forward.length();
        // A degenerate box (all samples in one point) says nothing about the
        // scale of the scene: leave the depths alone.
        if center.is_finite() && radius.is_finite() && radius > f32::MIN_POSITIVE {
            Self { center, radius, ln_radius: radius.ln() }
        } else {
            Self::NONE
        }
    }

    /// The key to sort `depth` by. Non-finite depths stay non-finite.
    #[inline]
    pub fn apply(&self, depth: f32) -> f32 {
        let t = depth - self.center;
        let distance = t.abs();
        if distance <= self.radius {
            depth
        } else {
            // l = ln(distance / radius) > 0, as a difference of logarithms
            // because the quotient could overflow. l·M/(l + M) grows like l
            // near the boundary and saturates at M.
            let l = distance.ln() - self.ln_radius;
            self.center + self.radius.copysign(t) * (1.0 + MAX_OUTSIDE_RADII * l / (l + MAX_OUTSIDE_RADII))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic points in the unit cube.
    fn cube(n: usize) -> Vec<[f32; 3]> {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 40) as f32 / (1u64 << 24) as f32
        };
        (0..n).map(|_| [next(), next(), next()]).collect()
    }

    #[test]
    fn bounds_cover_the_bulk_and_ignore_outliers() {
        let mut points = cube(50_000);
        let clean = RobustBounds::of(&points).unwrap();
        assert!(clean.min.max_element() < 0.05 && clean.max.min_element() > 0.95, "{clean:?}");
        assert!(clean.min.min_element() >= 0.0 && clean.max.max_element() <= 1.0);
        // A few hundred far floaters, a NaN and an infinity change nothing much.
        for i in 0..200 {
            points[i * 211] = [1.0e6, -3.0e7, 5.0e5];
        }
        points[17] = [f32::NAN, 0.0, 0.0];
        points[18] = [0.0, f32::INFINITY, 0.0];
        let dirty = RobustBounds::of(&points).unwrap();
        assert!(dirty.min.min_element() >= 0.0 && dirty.max.max_element() <= 1.0, "{dirty:?}");
        assert!((dirty.max - dirty.min).min_element() > 0.9);
    }

    #[test]
    fn bounds_of_small_and_degenerate_sets() {
        assert_eq!(RobustBounds::of(&[]), None);
        assert_eq!(RobustBounds::of(&[[f32::NAN; 3], [f32::INFINITY, 0.0, 0.0]]), None);
        let one = RobustBounds::of(&[[1.0, 2.0, 3.0]]).unwrap();
        assert_eq!((one.min, one.max), (Vec3::new(1.0, 2.0, 3.0), Vec3::new(1.0, 2.0, 3.0)));
        // Too few points to call anything an outlier: plain bounding box.
        let few = RobustBounds::of(&[[0.0, 5.0, 0.0], [1.0, -1.0, 2.0], [0.5, 0.0, -3.0]]).unwrap();
        assert_eq!((few.min, few.max), (Vec3::new(0.0, -1.0, -3.0), Vec3::new(1.0, 5.0, 2.0)));
        assert_eq!(RobustBounds::union(None, Some(one)), Some(one));
        assert_eq!(RobustBounds::union(None, None), None);
        let both = RobustBounds::union(Some(one), Some(few)).unwrap();
        assert_eq!((both.min, both.max), (Vec3::new(0.0, -1.0, -3.0), Vec3::new(1.0, 5.0, 3.0)));
    }

    #[test]
    fn warp_is_identity_inside_and_monotonic_everywhere() {
        let bounds = RobustBounds { min: Vec3::new(-1.0, 0.0, -1.0), max: Vec3::new(1.0, 2.0, 1.0) };
        let forward = Vec3::new(0.0, 0.0, -1.0);
        let warp = DepthWarp::new(Some(bounds), forward, 5.0);
        // Center depth 5, radius = 1.5 × half diagonal = 1.5·sqrt(3) ≈ 2.6.
        let radius = SPHERE_MARGIN * 3f32.sqrt();
        for d in [2.5, 4.0, 5.0, 6.0, 7.5] {
            assert_eq!(warp.apply(d), d);
        }
        let depths: Vec<f32> = vec![-3.0e38, -1.0e20, -1.0e6, -50.0, 0.0, 2.3, 2.5, 5.0, 7.5, 7.7, 9.0, 1.0e3, 1.0e6, 1.0e20, 3.0e38];
        let warped: Vec<f32> = depths.iter().map(|&d| warp.apply(d)).collect();
        assert!(warped.windows(2).all(|w| w[0] < w[1]), "{warped:?}");
        assert!(warped.iter().all(|w| w.is_finite()));
        // Everything representable lands within nine radii.
        assert!(warped[0] > 5.0 - 9.0 * radius && *warped.last().unwrap() < 5.0 + 9.0 * radius);
        // A splat a million radii away takes about six radii, not a million.
        assert!(warp.apply(5.0 + 1.0e6 * radius) < 5.0 + 6.2 * radius);
        // Just outside, the warp is still nearly the identity.
        let near = 5.0 + 1.2 * radius;
        assert!((warp.apply(near) - near).abs() < 0.03 * radius);
        assert!(warp.apply(f32::NAN).is_nan());
        assert!(!warp.apply(f32::INFINITY).is_finite());
        assert!(!warp.apply(f32::NEG_INFINITY).is_finite());
    }

    #[test]
    fn warp_follows_the_view_scale_and_degenerate_bounds_disable_it() {
        let bounds = RobustBounds { min: Vec3::splat(-1.0), max: Vec3::splat(1.0) };
        // A miniature view: depths are 1/15 of the world distances.
        let warp = DepthWarp::new(Some(bounds), Vec3::new(0.0, 0.0, -1.0 / 15.0), 0.2);
        let edge = 0.2 + SPHERE_MARGIN * 3f32.sqrt() / 15.0;
        assert_eq!(warp.apply(edge - 1e-3), edge - 1e-3);
        assert!(warp.apply(edge + 10.0) < edge + 1.0);

        let point = RobustBounds { min: Vec3::ONE, max: Vec3::ONE };
        for warp in
            [DepthWarp::new(Some(point), Vec3::Z, 0.0), DepthWarp::new(None, Vec3::Z, 0.0), DepthWarp::new(Some(bounds), Vec3::ZERO, 0.0)]
        {
            assert_eq!(warp, DepthWarp::NONE);
            assert_eq!(warp.apply(1.0e30), 1.0e30);
            assert!(warp.apply(f32::NAN).is_nan());
        }
    }
}
