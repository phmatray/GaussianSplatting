//! Camera rigs: a ring of static cameras around the stage and a handheld
//! scan path through the house.

use core::f32::consts::TAU;

use glam::{Quat, Vec3};
use serde::{Deserialize, Serialize};

use crate::scene::house;
use crate::types::Camera;

/// A ring of identical cameras around the vertical axis through `target`,
/// all looking at `target`.
///
/// Fields missing from serialized data take their [`Default`]. Out-of-range
/// values are tolerated: [`RingRig::cameras`] builds the rig described by
/// [`RingRig::sanitized`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RingRig {
    /// Number of cameras, clamped to `3..=24` when the rig is built.
    pub count: u32,
    /// Height tiers, 1 or 2. With 2 tiers the cameras alternate around the
    /// ring: even indices sit at `elevation`, odd ones at `upper_elevation`.
    pub tiers: u32,
    /// Horizontal distance from the target axis to every camera, in meters.
    pub radius: f32,
    /// Camera height above the floor (tier 1), in meters.
    pub elevation: f32,
    /// Camera height of the second tier, in meters (looking down more).
    pub upper_elevation: f32,
    /// Point all cameras look at.
    pub target: Vec3,
    /// Vertical field of view in degrees.
    pub fov_y_deg: f32,
    /// Image width in pixels.
    pub width: u32,
    /// Image height in pixels.
    pub height: u32,
}

impl Default for RingRig {
    /// Eight 160x120 cameras around the stage, alternating between 1.3 m and
    /// 2.5 m, 3.2 m from the axis, looking at the dancer's waist.
    fn default() -> Self {
        Self {
            count: 8,
            tiers: 2,
            radius: 3.2,
            elevation: 1.3,
            upper_elevation: 2.5,
            target: Vec3::new(0.0, 1.0, 0.0),
            fov_y_deg: 50.0,
            width: 160,
            height: 120,
        }
    }
}

impl RingRig {
    /// Smallest and largest supported camera counts.
    pub const MIN_COUNT: u32 = 3;
    pub const MAX_COUNT: u32 = 24;
    /// Largest supported image width and height in pixels.
    pub const MAX_IMAGE_SIZE: u32 = 4096;

    /// Number of cameras [`RingRig::cameras`] returns.
    pub fn camera_count(&self) -> usize {
        self.count.clamp(Self::MIN_COUNT, Self::MAX_COUNT) as usize
    }

    /// The rig [`RingRig::cameras`] actually builds: `count` clamped to
    /// `3..=24`, `tiers` to `1..=2`, the field of view to
    /// [`MIN_FOV_DEG`]`..=`[`MAX_FOV_DEG`], the image size to at most
    /// [`RingRig::MAX_IMAGE_SIZE`], and every number that is NaN or infinite
    /// replaced by its default. A rig that is already valid is returned
    /// unchanged.
    pub fn sanitized(&self) -> Self {
        let d = Self::default();
        let finite = |v: f32, fallback: f32| if v.is_finite() { v } else { fallback };
        Self {
            count: self.count.clamp(Self::MIN_COUNT, Self::MAX_COUNT),
            tiers: self.tiers.clamp(1, 2),
            radius: finite(self.radius, d.radius),
            elevation: finite(self.elevation, d.elevation),
            upper_elevation: finite(self.upper_elevation, d.upper_elevation),
            target: if self.target.is_finite() { self.target } else { d.target },
            fov_y_deg: clamp_fov_deg(self.fov_y_deg, d.fov_y_deg),
            width: self.width.min(Self::MAX_IMAGE_SIZE),
            height: self.height.min(Self::MAX_IMAGE_SIZE),
        }
    }

    /// The calibrated cameras of the [sanitized](RingRig::sanitized) rig,
    /// evenly spaced around the Y axis. Camera 0 stands on the +Z side of
    /// the target; indices advance toward +X.
    pub fn cameras(&self) -> Vec<Camera> {
        let rig = self.sanitized();
        let n = rig.count as usize;
        (0..n)
            .map(|i| {
                let angle = TAU * i as f32 / n as f32;
                let y = if rig.tiers == 2 && i % 2 == 1 {
                    rig.upper_elevation
                } else {
                    rig.elevation
                };
                let eye = Vec3::new(
                    rig.target.x + rig.radius * angle.sin(),
                    y,
                    rig.target.z + rig.radius * angle.cos(),
                );
                Camera::look_at(
                    eye,
                    rig.target,
                    Vec3::Y,
                    rig.fov_y_deg.to_radians(),
                    rig.width,
                    rig.height,
                )
            })
            .collect()
    }
}

/// Narrowest vertical field of view the rigs accept, in degrees.
pub const MIN_FOV_DEG: f32 = 1.0;
/// Widest vertical field of view the rigs accept, in degrees.
pub const MAX_FOV_DEG: f32 = 170.0;

/// A usable vertical field of view: clamped to the supported range, or
/// `fallback` when it is not a number. (A pinhole camera has no focal length
/// at 0 or 180 degrees.)
#[inline]
fn clamp_fov_deg(fov_y_deg: f32, fallback: f32) -> f32 {
    if fov_y_deg.is_nan() {
        fallback
    } else {
        fov_y_deg.clamp(MIN_FOV_DEG, MAX_FOV_DEG)
    }
}

/// Samples per spline segment in the arc-length table.
const TABLE_STEPS: usize = 24;
/// How far ahead on the path the camera heading aims, in meters.
const LOOK_AHEAD: f32 = 0.9;
/// Spacing of the stations of the speed profile, in meters.
const STATION: f32 = 0.02;
/// The walker slows down so that the path never turns faster than this (rad/s).
const MAX_TURN_RATE: f32 = 0.75;
/// The walker also slows down so that [`ScanPath::heading_at`], the direction
/// a handheld camera faces, never swings faster than this (rad/s, about 32
/// degrees per second). The heading aims at a point ahead on the path, so it
/// starts to swing before the walker reaches a bend and can turn faster than
/// the path under his feet.
const MAX_HEADING_RATE: f32 = 0.56;
/// Slowest walking speed in tight turns, as a fraction of the cruise speed.
const MIN_SPEED_FRACTION: f32 = 0.3;

/// A "handheld filming" trajectory: a smooth spline walked at a steady pace
/// while the view direction pans left/right and tilts up/down the way a
/// person filming a house tour would.
///
/// Positions follow a uniform cubic B-spline whose control polygon is the
/// waypoint list (C2 continuous; it starts and ends exactly on the first and
/// last waypoint and rounds the corners in between), re-parameterized by arc
/// length. The walker moves at the cruise speed on straight stretches and
/// slows down in tight turns, like someone turning around in a small room.
#[derive(Clone, Debug, PartialEq)]
pub struct ScanPath {
    waypoints: Vec<Vec3>,
    /// Cumulative arc length at `TABLE_STEPS` samples per spline segment.
    table: Vec<f32>,
    /// Arrival time at stations spaced `station` meters apart along the path.
    times: Vec<f32>,
    station: f32,
    speed: f32,
    /// Peak yaw swing left/right of the walking direction, radians.
    pub pan_amplitude: f32,
    /// Peak pitch swing around `pitch_bias`, radians.
    pub tilt_amplitude: f32,
    /// Mean pitch (negative looks down), radians.
    pub pitch_bias: f32,
}

impl ScanPath {
    /// Default cruise walking speed in meters per second.
    pub const DEFAULT_SPEED: f32 = 0.8;

    /// Default field of view of the handheld camera in degrees, used when
    /// [`ScanPath::camera_at`] is given one that is not a number.
    pub const DEFAULT_FOV_DEG: f32 = 70.0;

    /// Builds a path guided by `waypoints` (at least two) and walked at a
    /// cruise speed of `speed` m/s (at least 1 mm/s).
    ///
    /// Waypoints with a NaN or infinite coordinate are ignored.
    ///
    /// # Panics
    /// Panics when fewer than two (finite) waypoints are given.
    pub fn new(mut waypoints: Vec<Vec3>, speed: f32) -> Self {
        waypoints.retain(|p| p.is_finite());
        assert!(waypoints.len() >= 2, "a scan path needs at least two waypoints");
        let mut path = Self {
            waypoints,
            table: Vec::new(),
            times: Vec::new(),
            station: STATION,
            speed: speed.max(1e-3),
            pan_amplitude: 0.85,
            // Wide enough that ceilings and floors of small rooms get filmed
            // (see `house_tour_films_most_of_every_room`).
            tilt_amplitude: 0.55,
            pitch_bias: -0.10,
        };
        let segments = path.segments();
        let mut table = Vec::with_capacity(segments * TABLE_STEPS + 1);
        let mut length = 0.0;
        let mut prev = path.waypoints[0];
        table.push(0.0);
        for seg in 0..segments {
            for k in 1..=TABLE_STEPS {
                let p = path.eval(seg, k as f32 / TABLE_STEPS as f32);
                length += (p - prev).length();
                table.push(length);
                prev = p;
            }
        }
        path.table = table;
        path.build_schedule();
        path
    }

    /// Number of spline segments. The end waypoints count three times, which
    /// clamps the curve onto them.
    fn segments(&self) -> usize {
        self.waypoints.len() + 1
    }

    /// Control point `i` of the clamped control polygon.
    #[inline]
    fn control(&self, i: usize) -> Vec3 {
        self.waypoints[i.saturating_sub(2).min(self.waypoints.len() - 1)]
    }

    /// Uniform cubic B-spline on segment `seg` at local parameter `u` in `[0, 1]`.
    fn eval(&self, seg: usize, u: f32) -> Vec3 {
        let (u2, u3) = (u * u, u * u * u);
        let w0 = (1.0 - u) * (1.0 - u) * (1.0 - u);
        let w1 = 3.0 * u3 - 6.0 * u2 + 4.0;
        let w2 = -3.0 * u3 + 3.0 * u2 + 3.0 * u + 1.0;
        (self.control(seg) * w0 + self.control(seg + 1) * w1 + self.control(seg + 2) * w2 + self.control(seg + 3) * u3)
            / 6.0
    }

    /// Computes when the walker reaches each station: cruise speed, reduced
    /// where the path curves so the turn rate stays below `MAX_TURN_RATE`
    /// and the heading swings no faster than `MAX_HEADING_RATE`.
    fn build_schedule(&mut self) {
        let length = self.length();
        if length < 1e-6 {
            // All waypoints coincide: the walker stands still.
            self.station = 0.0;
            self.times = vec![0.0, 0.0];
            return;
        }
        let count = ((length / STATION).ceil() as usize).max(1);
        self.station = length / count as f32;
        let pts: Vec<Vec3> = (0..=count)
            .map(|k| self.position_at_distance(k as f32 * self.station))
            .collect();
        let headings: Vec<Vec3> = (0..=count)
            .map(|k| self.heading_at_distance(k as f32 * self.station))
            .collect();
        // Curvature, and swing of the heading per meter walked, from chords
        // spanning ~10 cm either side of each station.
        let span = ((0.1 / self.station).round() as usize).clamp(1, count);
        let mut v: Vec<f32> = (0..=count)
            .map(|k| {
                if k < span || k + span > count {
                    return self.speed;
                }
                let (a, b) = (pts[k] - pts[k - span], pts[k + span] - pts[k]);
                let walked = (a.length() + b.length()).max(1e-6);
                let curvature = a.angle_between(b) / (0.5 * walked);
                let swing = headings[k - span].angle_between(headings[k + span]) / walked;
                let limit = (MAX_TURN_RATE / curvature.max(1e-6)).min(MAX_HEADING_RATE / swing.max(1e-6));
                limit.clamp(MIN_SPEED_FRACTION * self.speed, self.speed)
            })
            .collect();
        // Ease in and out of the slow sections. Eroding first (running
        // minimum) over the full support of the two box blurs keeps the
        // smoothed speed at or below the limit everywhere.
        let radius = ((0.15 / self.station).round() as usize).clamp(1, count);
        let window = |k: usize, r: usize| (k.saturating_sub(r), (k + r).min(count));
        let limit = v.clone();
        for (k, out) in v.iter_mut().enumerate() {
            let (lo, hi) = window(k, 2 * radius);
            *out = limit[lo..=hi].iter().copied().fold(f32::INFINITY, f32::min);
        }
        for _ in 0..2 {
            let src = v.clone();
            for (k, out) in v.iter_mut().enumerate() {
                let (lo, hi) = window(k, radius);
                *out = src[lo..=hi].iter().sum::<f32>() / (hi - lo + 1) as f32;
            }
        }
        let mut times = Vec::with_capacity(count + 1);
        let mut t = 0.0;
        times.push(0.0);
        for k in 0..count {
            t += self.station / (0.5 * (v[k] + v[k + 1]));
            times.push(t);
        }
        self.times = times;
    }

    /// The default tour of the house (see [`house::tour_waypoints`]): every
    /// room, through the door openings, at eye height.
    pub fn house_default() -> Self {
        Self::new(house::tour_waypoints(), Self::DEFAULT_SPEED)
    }

    /// Returns the path with another cruise speed (m/s).
    pub fn with_speed(mut self, speed: f32) -> Self {
        self.speed = speed.max(1e-3);
        self.build_schedule();
        self
    }

    /// Cruise walking speed in meters per second.
    pub fn speed(&self) -> f32 {
        self.speed
    }

    /// Length of the path in meters.
    pub fn length(&self) -> f32 {
        *self.table.last().unwrap_or(&0.0)
    }

    /// Time needed to walk the whole path at playback speed 1, in seconds.
    /// Longer than `length / speed` because the walker slows down in turns.
    pub fn duration(&self) -> f32 {
        *self.times.last().unwrap_or(&0.0)
    }

    /// Distance walked along the path after `t` seconds (clamped to the
    /// path; a NaN time counts as the start).
    pub fn distance_at(&self, t: f32) -> f32 {
        let t = self.clamp_time(t);
        let hi = self
            .times
            .partition_point(|&time| time < t)
            .clamp(1, self.times.len() - 1);
        let (t0, t1) = (self.times[hi - 1], self.times[hi]);
        let frac = if t1 > t0 { (t - t0) / (t1 - t0) } else { 0.0 };
        ((hi - 1) as f32 + frac) * self.station
    }

    /// `t` clamped to `[0, duration]`, NaN mapped to 0.
    #[inline]
    fn clamp_time(&self, t: f32) -> f32 {
        // `max` first: it returns the other operand for a NaN.
        t.max(0.0).min(self.duration())
    }

    /// The control points guiding the spline.
    pub fn waypoints(&self) -> &[Vec3] {
        &self.waypoints
    }

    /// Position after walking `distance` meters along the path (clamped to
    /// its ends; NaN counts as the start).
    pub fn position_at_distance(&self, distance: f32) -> Vec3 {
        let s = distance.max(0.0).min(self.length());
        // First table entry >= s.
        let hi = self
            .table
            .partition_point(|&len| len < s)
            .clamp(1, self.table.len() - 1);
        let (l0, l1) = (self.table[hi - 1], self.table[hi]);
        let frac = if l1 > l0 { (s - l0) / (l1 - l0) } else { 0.0 };
        let sample = (hi - 1) as f32 + frac;
        let seg = (((hi - 1) / TABLE_STEPS).min(self.segments() - 1)) as f32;
        let u = (sample - seg * TABLE_STEPS as f32) / TABLE_STEPS as f32;
        self.eval(seg as usize, u.clamp(0.0, 1.0))
    }

    /// Position of the walker at time `t` seconds (clamped to the path).
    pub fn position_at(&self, t: f32) -> Vec3 {
        self.position_at_distance(self.distance_at(t))
    }

    /// Unit walking direction (horizontal) at time `t`, smoothed by aiming at
    /// a point a little ahead on the path.
    pub fn heading_at(&self, t: f32) -> Vec3 {
        self.heading_at_distance(self.distance_at(t))
    }

    /// [`ScanPath::heading_at`] after walking `distance` meters.
    fn heading_at_distance(&self, distance: f32) -> Vec3 {
        let length = self.length();
        let ahead = LOOK_AHEAD.min(0.5 * length);
        let s = distance.min(length - ahead);
        let d = self.position_at_distance(s + ahead) - self.position_at_distance(s);
        Vec3::new(d.x, 0.0, d.z).normalize_or(Vec3::NEG_Z)
    }

    /// The handheld camera at time `t` seconds (clamped to the walk).
    ///
    /// The camera sits on the path (plus a slight walking bob) and looks
    /// along the walking direction, panned and tilted by slow oscillations
    /// so that walls, floor, ceiling and furniture all come into view. The
    /// horizon stays level. `fov_y_deg` is clamped to
    /// [`MIN_FOV_DEG`]`..=`[`MAX_FOV_DEG`].
    pub fn camera_at(&self, t: f32, fov_y_deg: f32, width: u32, height: u32) -> Camera {
        let t = self.clamp_time(t);
        let fov_y_deg = clamp_fov_deg(fov_y_deg, Self::DEFAULT_FOV_DEG);
        let heading = self.heading_at(t);
        // Two incommensurate sines per axis: sweeping but not mechanical.
        let pan = self.pan_amplitude * (0.85 * (TAU * t / 9.0).sin() + 0.15 * (TAU * t / 3.3 + 1.0).sin());
        let tilt =
            self.pitch_bias + self.tilt_amplitude * (0.8 * (TAU * t / 6.3 + 0.6).sin() + 0.2 * (TAU * t / 2.7).sin());
        let yaw = heading.x.atan2(heading.z) + pan;
        // Forward for yaw 0 / tilt 0 is +Z; positive tilt looks up.
        let forward = Quat::from_rotation_y(yaw) * Quat::from_rotation_x(-tilt) * Vec3::Z;
        // Step bob: about two steps per second, one centimeter.
        let bob = Vec3::Y * (0.01 * (TAU * 1.9 * t).sin());
        let eye = self.position_at(t) + bob;
        Camera::look_at(eye, eye + forward, Vec3::Y, fov_y_deg.to_radians(), width, height)
    }

    /// `n` points evenly spaced along the path, for drawing it.
    pub fn polyline(&self, n: usize) -> Vec<Vec3> {
        let length = self.length();
        match n {
            0 => Vec::new(),
            1 => vec![self.position_at_distance(0.0)],
            _ => (0..n)
                .map(|i| self.position_at_distance(length * i as f32 / (n - 1) as f32))
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scene::house::{HouseScene, CEILING_HEIGHT, DOORS, EYE_HEIGHT, ROOMS};
    use crate::scene::Scene;

    #[test]
    fn ring_rig_default_geometry() {
        let rig = RingRig::default();
        let cams = rig.cameras();
        assert_eq!(cams.len(), 8);
        for (i, cam) in cams.iter().enumerate() {
            assert_eq!((cam.width, cam.height), (160, 120));
            assert!((cam.fov_y().to_degrees() - 50.0).abs() < 1e-3);
            let p = cam.position();
            assert!((Vec3::new(p.x, 0.0, p.z).length() - 3.2).abs() < 1e-4);
            let expected_y = if i % 2 == 0 { 1.3 } else { 2.5 };
            assert!((p.y - expected_y).abs() < 1e-5);
            // The target projects to the image center.
            let (px, py, depth) = cam.project(rig.target).unwrap();
            assert!((px - 80.0).abs() < 1e-2 && (py - 60.0).abs() < 1e-2);
            assert!(depth > 3.0);
            // Upright: world up maps to image up.
            let (_, py_up, _) = cam.project(rig.target + Vec3::Y * 0.5).unwrap();
            assert!(py_up < py);
            // Evenly spaced azimuths.
            let az = p.x.atan2(p.z).rem_euclid(TAU);
            let expect = TAU * i as f32 / 8.0;
            assert!((az - expect).abs() < 1e-4 || (az - expect).abs() > TAU - 1e-4);
        }
        assert!((cams[0].position() - Vec3::new(0.0, 1.3, 3.2)).length() < 1e-5);
        assert!(cams[2].position().x > 3.0, "indices advance toward +X");
    }

    #[test]
    fn ring_rig_count_and_tiers_are_clamped() {
        let one_tier = RingRig {
            count: 5,
            tiers: 1,
            ..Default::default()
        };
        assert!(one_tier.cameras().iter().all(|c| (c.position().y - 1.3).abs() < 1e-5));
        assert_eq!(
            RingRig {
                count: 0,
                ..Default::default()
            }
            .cameras()
            .len(),
            3
        );
        assert_eq!(
            RingRig {
                count: 100,
                ..Default::default()
            }
            .cameras()
            .len(),
            24
        );
        assert_eq!(
            RingRig {
                count: 100,
                ..Default::default()
            }
            .camera_count(),
            24
        );
        let weird = RingRig {
            tiers: 7,
            ..Default::default()
        };
        assert_eq!(weird.cameras(), RingRig::default().cameras());
        let offset = RingRig {
            target: Vec3::new(1.0, 0.8, -2.0),
            ..Default::default()
        };
        for cam in offset.cameras() {
            let p = cam.position() - offset.target;
            assert!((Vec3::new(p.x, 0.0, p.z).length() - 3.2).abs() < 1e-4);
            assert!(cam.forward().dot((offset.target - cam.position()).normalize()) > 0.9999);
        }
    }

    #[test]
    fn ring_rig_serde_round_trip() {
        let rig = RingRig {
            count: 12,
            tiers: 1,
            ..Default::default()
        };
        let json = serde_json::to_string(&rig).unwrap();
        assert_eq!(serde_json::from_str::<RingRig>(&json).unwrap(), rig);
    }

    #[test]
    fn ring_rig_covers_the_capture_volume() {
        // Every point of the capture cylinder's axis is seen by all cameras,
        // and points on its surface by at least three.
        use crate::scene::stage::{CAPTURE_HEIGHT, CAPTURE_RADIUS};
        let cams = RingRig::default().cameras();
        let sees = |cam: &Camera, p: Vec3| {
            cam.project(p)
                .is_some_and(|(px, py, _)| px >= 0.0 && py >= 0.0 && px <= cam.width as f32 && py <= cam.height as f32)
        };
        for k in 0..=10 {
            let y = 0.02 + (CAPTURE_HEIGHT - 0.3) * k as f32 / 10.0;
            assert!(
                cams.iter().all(|c| sees(c, Vec3::new(0.0, y, 0.0))),
                "axis point at y = {y}"
            );
            for a in 0..16 {
                let ang = TAU * a as f32 / 16.0;
                let p = Vec3::new(CAPTURE_RADIUS * ang.sin(), y, CAPTURE_RADIUS * ang.cos());
                let n = cams.iter().filter(|c| sees(c, p)).count();
                assert!(n >= 3, "surface point {p:?} seen by {n} cameras");
            }
        }
    }

    #[test]
    fn spline_passes_through_waypoints() {
        let pts = vec![
            Vec3::new(0.0, 1.0, 0.0),
            Vec3::new(2.0, 1.0, 0.0),
            Vec3::new(2.0, 1.0, 3.0),
            Vec3::new(-1.0, 1.0, 3.5),
        ];
        let path = ScanPath::new(pts.clone(), 0.5);
        assert!((path.position_at(0.0) - pts[0]).length() < 1e-5);
        assert!((path.position_at(path.duration()) - pts[3]).length() < 1e-4);
        assert!((path.position_at(1e9) - pts[3]).length() < 1e-4, "clamped past the end");
        assert!(
            (path.position_at(-5.0) - pts[0]).length() < 1e-5,
            "clamped before the start"
        );
        // Shorter than the control polygon (2 + 3 + 3.04 m): corners are cut.
        assert!(path.length() > 6.0 && path.length() < 8.04, "length {}", path.length());
        // The curve rounds the corners but stays close to every waypoint.
        let dense = path.polyline(4000);
        for p in &pts {
            let d = dense.iter().map(|q| (*q - *p).length()).fold(f32::INFINITY, f32::min);
            assert!(d < 0.7, "waypoint {p:?} missed by {d}");
        }
        // Collinear waypoints are followed exactly.
        let line = ScanPath::new((0..6).map(|i| Vec3::new(i as f32, 1.0, 2.0 * i as f32)).collect(), 1.0);
        for p in line.polyline(100) {
            assert!((p.z - 2.0 * p.x).abs() < 1e-4 && (p.y - 1.0).abs() < 1e-5);
        }
        assert!((line.length() - 5.0 * 5f32.sqrt()).abs() < 1e-2);
        // The polyline is evenly spaced in arc length.
        let coarse = path.polyline(400);
        let step = path.length() / 399.0;
        for w in coarse.windows(2) {
            assert!(
                ((w[1] - w[0]).length() - step).abs() < 0.03 * step,
                "{} vs {step}",
                (w[1] - w[0]).length()
            );
        }
    }

    #[test]
    fn walker_cruises_on_straights_and_slows_in_turns() {
        // A straight line is walked at exactly the cruise speed.
        let line = ScanPath::new(
            vec![Vec3::ZERO, Vec3::new(2.0, 0.0, 0.0), Vec3::new(5.0, 0.0, 0.0)],
            0.8,
        );
        assert!((line.duration() - 5.0 / 0.8).abs() < 1e-3);
        assert!((line.position_at(2.5) - Vec3::new(2.0, 0.0, 0.0)).length() < 1e-3);
        assert!((line.distance_at(1.0) - 0.8).abs() < 1e-4);
        assert_eq!(line.clone().with_speed(1.6).speed(), 1.6);
        assert!((line.with_speed(1.6).duration() - 5.0 / 1.6).abs() < 1e-3);

        // The house tour: never faster than cruise, never slower than the
        // floor, and the direction of travel turns at a bounded rate.
        let path = ScanPath::house_default();
        assert!(path.duration() > path.length() / path.speed());
        let dt = 0.05;
        let steps = (path.duration() / dt) as usize;
        let (mut slowest, mut fastest, mut max_turn) = (f32::INFINITY, 0.0f32, 0.0f32);
        let mut prev_dir: Option<Vec3> = None;
        for i in 0..steps {
            let d = path.position_at((i + 1) as f32 * dt) - path.position_at(i as f32 * dt);
            let v = d.length() / dt;
            slowest = slowest.min(v);
            fastest = fastest.max(v);
            if let Some(p) = prev_dir {
                max_turn = max_turn.max(p.angle_between(d) / dt);
            }
            prev_dir = Some(d);
        }
        assert!(fastest <= 0.8 * 1.01, "fastest {fastest}");
        assert!(fastest > 0.8 * 0.98, "cruise speed is reached on straights ({fastest})");
        assert!(slowest >= 0.8 * MIN_SPEED_FRACTION * 0.95, "slowest {slowest}");
        assert!(slowest < 0.6, "the walker slows down in the tight turns ({slowest})");
        assert!(max_turn < 1.1 * MAX_TURN_RATE, "turn rate {max_turn} rad/s");
    }

    #[test]
    fn spline_is_smooth() {
        // The direction of travel never jumps, including across segments.
        let path = ScanPath::house_default();
        let pts = path.polyline(6000);
        let mut worst = 0.0f32;
        for w in pts.windows(3) {
            let (a, b) = ((w[1] - w[0]).normalize(), (w[2] - w[1]).normalize());
            worst = worst.max(a.angle_between(b));
        }
        // 6000 samples over ~35 m is about 6 mm per step: no turn of the
        // tour is tighter than a 0.25 m radius.
        let step = path.length() / 5999.0;
        assert!(worst < step / 0.25, "direction turns {worst} rad in {step} m");
    }

    #[test]
    fn polyline_edge_cases() {
        let path = ScanPath::house_default();
        assert!(path.polyline(0).is_empty());
        let single = path.polyline(1);
        assert!(single.len() == 1 && (single[0] - path.waypoints()[0]).length() < 1e-5);
        let p = path.polyline(50);
        assert_eq!(p.len(), 50);
        assert!((p[0] - path.waypoints()[0]).length() < 1e-5);
        assert!((p[49] - *path.waypoints().last().unwrap()).length() < 1e-3);
    }

    #[test]
    fn house_tour_is_a_plausible_walk() {
        let path = ScanPath::house_default();
        assert_eq!(path.speed(), ScanPath::DEFAULT_SPEED);
        assert!(
            (35.0..120.0).contains(&path.duration()),
            "tour lasts {} s",
            path.duration()
        );
        // Eye height throughout.
        for p in path.polyline(500) {
            assert!((p.y - EYE_HEIGHT).abs() < 1e-3);
        }
        // Visits every room, spending some time well inside each.
        let pts = path.polyline(2000);
        for room in ROOMS {
            let inside = pts
                .iter()
                .filter(|p| {
                    p.x > room.min[0] + 0.3
                        && p.x < room.max[0] - 0.3
                        && p.z > room.min[1] + 0.3
                        && p.z < room.max[1] - 0.3
                })
                .count();
            let meters = path.length() * inside as f32 / pts.len() as f32;
            assert!(meters > 0.5, "only {meters} m walked inside the {}", room.name);
        }
        // Goes through every door.
        for door in DOORS {
            let near = pts
                .iter()
                .any(|p| (Vec3::new(p.x, 0.0, p.z) - door.center).length() < 0.5 * door.width);
            assert!(near, "tour never passes the {:?} door", door.rooms);
        }
    }

    #[test]
    fn house_tour_never_crosses_geometry_and_keeps_clearance() {
        const CLEARANCE: f32 = 0.25;
        let house = HouseScene::new();
        let snap = house.snapshot(0.0);
        let path = ScanPath::house_default();
        // One sample every ~1.5 cm.
        let n = (path.length() / 0.015) as usize;
        let pts = path.polyline(n);
        // 26 probe directions: axes, face diagonals, corners.
        let mut dirs = Vec::new();
        for x in -1..=1 {
            for y in -1..=1 {
                for z in -1..=1 {
                    if (x, y, z) != (0, 0, 0) {
                        dirs.push(Vec3::new(x as f32, y as f32, z as f32).normalize());
                    }
                }
            }
        }
        // Finer fan in the horizontal plane, where walls and furniture are.
        for k in 0..32 {
            let a = TAU * k as f32 / 32.0;
            dirs.push(Vec3::new(a.cos(), 0.0, a.sin()));
            dirs.push(Vec3::new(a.cos(), -0.6, a.sin()).normalize());
        }
        for (i, w) in pts.windows(2).enumerate() {
            let seg = w[1] - w[0];
            let len = seg.length();
            if len > 1e-6 {
                assert!(
                    !snap.occluded(w[0], seg / len, len),
                    "path crosses geometry near {:?} (sample {i})",
                    w[0]
                );
            }
            for d in &dirs {
                if let Some(hit) = snap.raycast(w[0], *d, CLEARANCE) {
                    panic!(
                        "clearance {:.3} m < {CLEARANCE} m at {:?} (sample {i} of {n}) toward {:?}, primitive {}",
                        hit.t, w[0], d, hit.prim
                    );
                }
            }
        }
        // Head room and floor.
        for p in pts.iter().step_by(20) {
            let up = snap.raycast(*p, Vec3::Y, 10.0).unwrap();
            let down = snap.raycast(*p, Vec3::NEG_Y, 10.0).unwrap();
            assert!(up.t > 0.5 && up.pos.y <= CEILING_HEIGHT + 1e-3);
            assert!(down.pos.y < 0.05, "walking over furniture at {p:?}");
        }
    }

    #[test]
    fn handheld_camera_pans_and_tilts_smoothly() {
        let path = ScanPath::house_default();
        let dt = 1.0 / 30.0;
        let frames = (path.duration() / dt) as usize;
        let (mut min_pitch, mut max_pitch) = (f32::INFINITY, f32::NEG_INFINITY);
        let (mut left, mut right) = (0usize, 0usize);
        let mut prev = path.camera_at(0.0, 70.0, 160, 120);
        assert_eq!((prev.width, prev.height), (160, 120));
        assert!((prev.fov_y().to_degrees() - 70.0).abs() < 1e-3);
        for i in 1..frames {
            let t = i as f32 * dt;
            let cam = path.camera_at(t, 70.0, 160, 120);
            let f = cam.forward();
            let pitch = f.y.asin();
            min_pitch = min_pitch.min(pitch);
            max_pitch = max_pitch.max(pitch);
            let side = path.heading_at(t).cross(f).y;
            left += (side > 0.3) as usize;
            right += (side < -0.3) as usize;
            // Smooth footage: under 3.5 degrees of rotation and 4 cm of travel per frame.
            assert!(prev.forward().angle_between(f) < 0.06, "camera jerks at t = {t}");
            assert!((cam.position() - prev.position()).length() < 0.04);
            // Level horizon.
            assert!(cam.world_from_cam.matrix3.x_axis.y.abs() < 1e-4);
            // Stays near the path at eye height.
            assert!((cam.position() - path.position_at(t)).length() < 0.02);
            prev = cam;
        }
        assert!(
            min_pitch < -0.3 && max_pitch > 0.1,
            "pitch range {min_pitch}..{max_pitch}"
        );
        assert!(
            left > frames / 12 && right > frames / 12,
            "pans left {left} / right {right} of {frames}"
        );
        // Clamped outside the walk.
        assert_eq!(path.camera_at(-3.0, 70.0, 64, 48), path.camera_at(0.0, 70.0, 64, 48));
        let end = path.duration();
        assert_eq!(
            path.camera_at(end + 10.0, 70.0, 64, 48),
            path.camera_at(end, 70.0, 64, 48)
        );
    }

    #[test]
    fn ring_rig_tolerates_hostile_values() {
        let d = RingRig::default();
        assert_eq!(d.sanitized(), d, "a valid rig is left alone");
        let bad = RingRig {
            count: u32::MAX,
            tiers: 0,
            radius: f32::NAN,
            elevation: f32::INFINITY,
            upper_elevation: f32::NEG_INFINITY,
            target: Vec3::new(0.0, f32::NAN, 0.0),
            fov_y_deg: f32::NAN,
            width: u32::MAX,
            height: 100_000,
        };
        let fixed = bad.sanitized();
        assert_eq!(
            fixed,
            RingRig {
                count: 24,
                tiers: 1,
                width: RingRig::MAX_IMAGE_SIZE,
                height: RingRig::MAX_IMAGE_SIZE,
                ..d
            }
        );
        let cams = bad.cameras();
        assert_eq!(cams.len(), bad.camera_count());
        assert!(cams
            .iter()
            .all(|c| c.world_from_cam.is_finite() && c.fx.is_finite() && c.fx > 0.0));
        // The field of view is clamped to something a pinhole can do.
        for (fov, expect) in [
            (0.0, MIN_FOV_DEG),
            (-30.0, MIN_FOV_DEG),
            (180.0, MAX_FOV_DEG),
            (1e9, MAX_FOV_DEG),
        ] {
            let rig = RingRig { fov_y_deg: fov, ..d };
            for cam in rig.cameras() {
                assert!(cam.fx.is_finite() && cam.fx > 0.0 && cam.fy > 0.0);
                assert!((cam.fov_y().to_degrees() - expect).abs() < 1e-2, "{fov}");
            }
        }
        // Degenerate but finite geometry still yields finite, orthonormal poses.
        for rig in [
            RingRig { radius: 0.0, ..d },
            RingRig { radius: -2.0, ..d },
            RingRig {
                radius: 0.0,
                elevation: 1.0,
                upper_elevation: 1.0,
                ..d
            },
            RingRig {
                width: 0,
                height: 0,
                ..d
            },
        ] {
            for cam in rig.cameras() {
                let m = cam.world_from_cam.matrix3;
                assert!(cam.world_from_cam.is_finite());
                assert!((m.determinant() - 1.0).abs() < 1e-4, "{rig:?}");
            }
        }
        // Partial JSON falls back to the defaults field by field.
        let partial: RingRig = serde_json::from_str(r#"{"count":6,"radius":2.5}"#).unwrap();
        assert_eq!(
            partial,
            RingRig {
                count: 6,
                radius: 2.5,
                ..d
            }
        );
        assert_eq!(serde_json::from_str::<RingRig>("{}").unwrap(), d);
    }

    #[test]
    fn ring_cameras_are_not_mirrored() {
        // Seen from camera 0 (on +Z looking toward -Z), world +X is on the
        // right of the image and the next camera (toward +X) is too.
        let rig = RingRig::default();
        let cams = rig.cameras();
        let (cx, cy, _) = cams[0].project(rig.target).unwrap();
        let (px, _, _) = cams[0].project(rig.target + Vec3::X * 0.5).unwrap();
        assert!(px > cx + 10.0);
        let (px1, _, _) = cams[0].project(cams[1].position()).unwrap();
        assert!(px1 > cx);
        // For every camera: its right-hand neighbour on the ring appears on
        // the right, and the floor below the target appears below it.
        for (i, cam) in cams.iter().enumerate() {
            let right = Vec3::from(cam.world_from_cam.matrix3.x_axis);
            let (px, py, _) = cam.project(rig.target + right * 0.3 - Vec3::Y * 0.3).unwrap();
            assert!(px > cx && py > cy, "camera {i}");
            // Right-handed pose with -Z forward.
            let m = cam.world_from_cam.matrix3;
            assert!((m.x_axis.cross(m.y_axis) - m.z_axis).length() < 1e-5);
            assert!((cam.forward() + Vec3::from(m.z_axis)).length() < 1e-6);
        }
    }

    #[test]
    fn scan_path_tolerates_hostile_inputs() {
        // Coincident waypoints: a path of zero length, standing still.
        for p in [Vec3::ZERO, Vec3::new(1.0, 1.4, -2.0)] {
            let still = ScanPath::new(vec![p; 3], 0.8);
            assert!(still.length() < 1e-4 && still.duration() < 1e-3);
            assert!((still.position_at(5.0) - p).length() < 1e-4);
            let cam = still.camera_at(2.0, 70.0, 32, 24);
            assert!(cam.world_from_cam.is_finite());
            assert_eq!(still.polyline(4).len(), 4);
            let faster = still.with_speed(2.0);
            assert!(faster.duration() < 1e-3);
        }
        // Non-finite waypoints are dropped.
        let pts = vec![
            Vec3::ZERO,
            Vec3::splat(f32::NAN),
            Vec3::new(2.0, 0.0, 0.0),
            Vec3::new(f32::INFINITY, 0.0, 0.0),
            Vec3::new(2.0, 0.0, 3.0),
        ];
        let path = ScanPath::new(pts, 0.8);
        assert_eq!(path.waypoints().len(), 3);
        assert!(path.length().is_finite() && path.duration().is_finite() && path.length() > 4.0);
        // Absurd speeds are clamped to a crawl, not rejected.
        for speed in [0.0, -3.0, f32::NAN] {
            let slow = path.clone().with_speed(speed);
            assert!(slow.speed() > 0.0 && slow.duration().is_finite());
            assert!((ScanPath::new(path.waypoints().to_vec(), speed).duration() - slow.duration()).abs() < 1e-2);
        }
        // Times and distances that are not numbers count as the start.
        let tour = ScanPath::house_default();
        let start = tour.camera_at(0.0, 70.0, 64, 48);
        assert_eq!(tour.camera_at(f32::NAN, 70.0, 64, 48), start);
        assert_eq!(tour.camera_at(f32::NEG_INFINITY, 70.0, 64, 48), start);
        assert_eq!(
            tour.camera_at(f32::INFINITY, 70.0, 64, 48),
            tour.camera_at(tour.duration(), 70.0, 64, 48)
        );
        assert_eq!(tour.distance_at(f32::NAN), 0.0);
        assert_eq!(tour.position_at_distance(f32::NAN), tour.position_at_distance(0.0));
        assert_eq!(
            tour.position_at_distance(f32::INFINITY),
            tour.position_at_distance(tour.length())
        );
        assert!(tour.heading_at(f32::NAN).is_finite());
        // Fields of view a pinhole cannot do are clamped.
        for fov in [0.0, -20.0, 180.0, 400.0, f32::NAN] {
            let cam = tour.camera_at(3.0, fov, 64, 48);
            assert!(cam.fx.is_finite() && cam.fx > 0.0, "fov {fov}: fx {}", cam.fx);
            let deg = cam.fov_y().to_degrees();
            assert!((MIN_FOV_DEG - 1e-3..=MAX_FOV_DEG + 1e-3).contains(&deg));
        }
        assert!((tour.camera_at(3.0, f32::NAN, 64, 48).fov_y().to_degrees() - ScanPath::DEFAULT_FOV_DEG).abs() < 1e-3);
    }

    #[test]
    fn handheld_camera_is_upright_and_not_mirrored() {
        let path = ScanPath::house_default();
        let mut t = 0.0;
        while t < path.duration() {
            let cam = path.camera_at(t, 70.0, 160, 120);
            let f = cam.forward();
            // The point the walker's left hand points at shows up on the left.
            let left = Vec3::Y.cross(f).normalize();
            let ahead = cam.position() + f * 2.0;
            let (cx, cy, depth) = cam.project(ahead).unwrap();
            assert!((cx - 80.0).abs() < 1e-2 && (cy - 60.0).abs() < 1e-2 && (depth - 2.0).abs() < 1e-4);
            let (lx, _, _) = cam.project(ahead + left * 0.5).unwrap();
            let (_, uy, _) = cam.project(ahead + Vec3::Y * 0.5).unwrap();
            assert!(lx < cx - 5.0, "t = {t}: left is not left");
            assert!(uy < cy - 5.0, "t = {t}: up is not up");
            t += 0.37;
        }
    }

    #[test]
    fn house_tour_films_most_of_every_room() {
        // Everything a visitor could see from the path should be captured by
        // some frame of the footage: sample surface points visible from the
        // path, then check which ones fall inside the frustum, in range and
        // unoccluded, at a sensible incidence, in at least one frame.
        use crate::scene::test_util::Lcg;
        let snap = HouseScene::new().snapshot(0.0);
        let path = ScanPath::house_default();
        let mut rng = Lcg(4242);
        let mut samples = Vec::new();
        for p in path.polyline(240) {
            for _ in 0..40 {
                if let Some(hit) = snap.raycast(p, rng.unit_vec(), 50.0) {
                    samples.push((hit.pos, hit.normal));
                }
            }
        }
        let fps = 4.0;
        let frames = (path.duration() * fps) as usize;
        let mut seen = vec![false; samples.len()];
        for i in 0..=frames {
            let cam = path.camera_at(i as f32 / fps, 70.0, 160, 120);
            let eye = cam.position();
            for (k, (p, n)) in samples.iter().enumerate() {
                if seen[k] {
                    continue;
                }
                let Some((px, py, z)) = cam.project(*p) else {
                    continue;
                };
                if !(0.0..160.0).contains(&px) || !(0.0..120.0).contains(&py) || !(0.1..=20.0).contains(&z) {
                    continue;
                }
                let to_point = *p - eye;
                let dist = to_point.length();
                let dir = to_point / dist;
                if n.dot(-dir) > 0.15 && !snap.occluded(eye, dir, dist - 2e-3) {
                    seen[k] = true;
                }
            }
        }
        let ratio = |filter: &dyn Fn(Vec3, Vec3) -> bool| {
            let (mut total, mut hit) = (0u32, 0u32);
            for (k, (p, n)) in samples.iter().enumerate() {
                if filter(*p, *n) {
                    total += 1;
                    hit += seen[k] as u32;
                }
            }
            assert!(total >= 25, "only {total} samples");
            hit as f32 / total as f32
        };
        let overall = ratio(&|_, _| true);
        assert!(
            overall > 0.88,
            "the footage covers only {:.1} % of what is visible",
            overall * 100.0
        );
        for room in ROOMS {
            let all = ratio(&|p, _| room.contains(p));
            let floor = ratio(&|p, n| room.contains(p) && p.y < 0.02 && n.y > 0.5);
            let ceiling = ratio(&|p, n| room.contains(p) && p.y > CEILING_HEIGHT - 0.14 && n.y < -0.5);
            assert!(all > 0.82, "{}: {:.1} % covered", room.name, all * 100.0);
            assert!(floor > 0.72, "{}: floor {:.1} % covered", room.name, floor * 100.0);
            assert!(
                ceiling > 0.60,
                "{}: ceiling {:.1} % covered",
                room.name,
                ceiling * 100.0
            );
        }
    }

    #[test]
    #[should_panic(expected = "at least two waypoints")]
    fn only_non_finite_waypoints_are_rejected() {
        let _ = ScanPath::new(vec![Vec3::ZERO, Vec3::splat(f32::NAN), Vec3::splat(f32::INFINITY)], 1.0);
    }

    #[test]
    #[should_panic(expected = "at least two waypoints")]
    fn single_waypoint_is_rejected() {
        let _ = ScanPath::new(vec![Vec3::ZERO], 1.0);
    }
}
