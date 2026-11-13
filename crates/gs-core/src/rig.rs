//! Camera rigs: a ring of static cameras around the stage and a handheld
//! scan path through the house.

use core::f32::consts::TAU;

use glam::{Quat, Vec3};
use serde::{Deserialize, Serialize};

use crate::scene::house;
use crate::types::Camera;

/// A ring of identical cameras around the vertical axis through `target`,
/// all looking at `target`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
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

    /// Number of cameras [`RingRig::cameras`] returns.
    pub fn camera_count(&self) -> usize {
        self.count.clamp(Self::MIN_COUNT, Self::MAX_COUNT) as usize
    }

    /// The calibrated cameras, evenly spaced around the Y axis. Camera 0
    /// stands on the +Z side of the target; indices advance toward +X.
    pub fn cameras(&self) -> Vec<Camera> {
        let n = self.camera_count();
        let two_tiers = self.tiers >= 2;
        (0..n)
            .map(|i| {
                let angle = TAU * i as f32 / n as f32;
                let y = if two_tiers && i % 2 == 1 { self.upper_elevation } else { self.elevation };
                let eye = Vec3::new(self.target.x + self.radius * angle.sin(), y, self.target.z + self.radius * angle.cos());
                Camera::look_at(eye, self.target, Vec3::Y, self.fov_y_deg.to_radians(), self.width, self.height)
            })
            .collect()
    }
}

/// Samples per spline segment in the arc-length table.
const TABLE_STEPS: usize = 24;
/// How far ahead on the path the camera heading aims, in meters.
const LOOK_AHEAD: f32 = 0.9;

/// A "handheld filming" trajectory: a smooth spline walked at constant speed
/// while the view direction pans left/right and tilts up/down the way a
/// person filming a house tour would.
///
/// Positions follow a centripetal Catmull-Rom spline through the waypoints
/// (C1 continuous, no overshoot loops), re-parameterized by arc length.
#[derive(Clone, Debug, PartialEq)]
pub struct ScanPath {
    waypoints: Vec<Vec3>,
    /// Hermite tangents per waypoint, per unit of the knot parameter.
    tangents: Vec<Vec3>,
    /// Knot interval of every segment.
    knots: Vec<f32>,
    /// Cumulative arc length at `TABLE_STEPS` samples per segment.
    table: Vec<f32>,
    speed: f32,
    /// Peak yaw swing left/right of the walking direction, radians.
    pub pan_amplitude: f32,
    /// Peak pitch swing around `pitch_bias`, radians.
    pub tilt_amplitude: f32,
    /// Mean pitch (negative looks down), radians.
    pub pitch_bias: f32,
}

impl ScanPath {
    /// Default walking speed in meters per second.
    pub const DEFAULT_SPEED: f32 = 0.8;

    /// Builds a path through `waypoints` (at least two) walked at `speed` m/s.
    ///
    /// # Panics
    /// Panics when fewer than two waypoints are given.
    pub fn new(waypoints: Vec<Vec3>, speed: f32) -> Self {
        assert!(waypoints.len() >= 2, "a scan path needs at least two waypoints");
        let n = waypoints.len();
        // Centripetal parameterization: knot spacing = sqrt(chord length).
        let knots: Vec<f32> = waypoints.windows(2).map(|w| (w[1] - w[0]).length().sqrt().max(1e-3)).collect();
        let mut tangents = Vec::with_capacity(n);
        for i in 0..n {
            let tangent = if i == 0 {
                (waypoints[1] - waypoints[0]) / knots[0]
            } else if i == n - 1 {
                (waypoints[n - 1] - waypoints[n - 2]) / knots[n - 2]
            } else {
                // Derivative at the middle knot of the parabola through three points.
                let (d0, d1) = (knots[i - 1], knots[i]);
                let (p0, p1, p2) = (waypoints[i - 1], waypoints[i], waypoints[i + 1]);
                (p1 - p0) / d0 - (p2 - p0) / (d0 + d1) + (p2 - p1) / d1
            };
            tangents.push(tangent);
        }
        let mut path = Self {
            waypoints,
            tangents,
            knots,
            table: Vec::new(),
            speed: speed.max(1e-3),
            pan_amplitude: 0.95,
            tilt_amplitude: 0.30,
            pitch_bias: -0.14,
        };
        let mut table = Vec::with_capacity((n - 1) * TABLE_STEPS + 1);
        let mut length = 0.0;
        let mut prev = path.waypoints[0];
        table.push(0.0);
        for seg in 0..n - 1 {
            for k in 1..=TABLE_STEPS {
                let p = path.eval(seg, k as f32 / TABLE_STEPS as f32);
                length += (p - prev).length();
                table.push(length);
                prev = p;
            }
        }
        path.table = table;
        path
    }

    /// The default tour of the house (see [`house::tour_waypoints`]): every
    /// room, through the door openings, at eye height.
    pub fn house_default() -> Self {
        Self::new(house::tour_waypoints(), Self::DEFAULT_SPEED)
    }

    /// Returns the path with another walking speed (m/s).
    pub fn with_speed(mut self, speed: f32) -> Self {
        self.speed = speed.max(1e-3);
        self
    }

    /// Walking speed in meters per second.
    pub fn speed(&self) -> f32 {
        self.speed
    }

    /// Length of the path in meters.
    pub fn length(&self) -> f32 {
        *self.table.last().unwrap_or(&0.0)
    }

    /// Time needed to walk the whole path at playback speed 1, in seconds.
    pub fn duration(&self) -> f32 {
        self.length() / self.speed
    }

    /// The control points the spline passes through.
    pub fn waypoints(&self) -> &[Vec3] {
        &self.waypoints
    }

    /// Hermite evaluation on segment `seg` at local parameter `u` in `[0, 1]`.
    fn eval(&self, seg: usize, u: f32) -> Vec3 {
        let (p0, p1) = (self.waypoints[seg], self.waypoints[seg + 1]);
        let d = self.knots[seg];
        let (m0, m1) = (self.tangents[seg] * d, self.tangents[seg + 1] * d);
        let (u2, u3) = (u * u, u * u * u);
        p0 * (2.0 * u3 - 3.0 * u2 + 1.0) + m0 * (u3 - 2.0 * u2 + u) + p1 * (-2.0 * u3 + 3.0 * u2) + m1 * (u3 - u2)
    }

    /// Position after walking `distance` meters along the path (clamped to
    /// its ends).
    pub fn position_at_distance(&self, distance: f32) -> Vec3 {
        let s = distance.clamp(0.0, self.length());
        // First table entry >= s.
        let hi = self.table.partition_point(|&len| len < s).clamp(1, self.table.len() - 1);
        let (l0, l1) = (self.table[hi - 1], self.table[hi]);
        let frac = if l1 > l0 { (s - l0) / (l1 - l0) } else { 0.0 };
        let sample = (hi - 1) as f32 + frac;
        let seg = (((hi - 1) / TABLE_STEPS).min(self.waypoints.len() - 2)) as f32;
        let u = (sample - seg * TABLE_STEPS as f32) / TABLE_STEPS as f32;
        self.eval(seg as usize, u.clamp(0.0, 1.0))
    }

    /// Position of the walker at time `t` seconds (clamped to the path).
    pub fn position_at(&self, t: f32) -> Vec3 {
        self.position_at_distance(t * self.speed)
    }

    /// Unit walking direction (horizontal) at time `t`, smoothed by aiming at
    /// a point a little ahead on the path.
    pub fn heading_at(&self, t: f32) -> Vec3 {
        let length = self.length();
        let ahead = LOOK_AHEAD.min(0.5 * length);
        let s = (t * self.speed).clamp(0.0, length - ahead);
        let d = self.position_at_distance(s + ahead) - self.position_at_distance(s);
        Vec3::new(d.x, 0.0, d.z).normalize_or(Vec3::NEG_Z)
    }

    /// The handheld camera at time `t` seconds.
    ///
    /// The camera sits on the path (plus a slight walking bob) and looks
    /// along the walking direction, panned and tilted by slow oscillations
    /// so that walls, floor, ceiling and furniture all come into view.
    pub fn camera_at(&self, t: f32, fov_y_deg: f32, width: u32, height: u32) -> Camera {
        let t = t.clamp(0.0, self.duration());
        let heading = self.heading_at(t);
        // Two incommensurate sines per axis: sweeping but not mechanical.
        let pan = self.pan_amplitude * (0.72 * (TAU * t / 6.8).sin() + 0.28 * (TAU * t / 2.9 + 1.0).sin());
        let tilt = self.pitch_bias + self.tilt_amplitude * (0.75 * (TAU * t / 5.1 + 0.6).sin() + 0.25 * (TAU * t / 2.3).sin());
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
            _ => (0..n).map(|i| self.position_at_distance(length * i as f32 / (n - 1) as f32)).collect(),
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
        let one_tier = RingRig { count: 5, tiers: 1, ..Default::default() };
        assert!(one_tier.cameras().iter().all(|c| (c.position().y - 1.3).abs() < 1e-5));
        assert_eq!(RingRig { count: 0, ..Default::default() }.cameras().len(), 3);
        assert_eq!(RingRig { count: 100, ..Default::default() }.cameras().len(), 24);
        assert_eq!(RingRig { count: 100, ..Default::default() }.camera_count(), 24);
        let weird = RingRig { tiers: 7, ..Default::default() };
        assert_eq!(weird.cameras(), RingRig::default().cameras());
        let offset = RingRig { target: Vec3::new(1.0, 0.8, -2.0), ..Default::default() };
        for cam in offset.cameras() {
            let p = cam.position() - offset.target;
            assert!((Vec3::new(p.x, 0.0, p.z).length() - 3.2).abs() < 1e-4);
            assert!(cam.forward().dot((offset.target - cam.position()).normalize()) > 0.9999);
        }
    }

    #[test]
    fn ring_rig_serde_round_trip() {
        let rig = RingRig { count: 12, tiers: 1, ..Default::default() };
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
            cam.project(p).is_some_and(|(px, py, _)| px >= 0.0 && py >= 0.0 && px <= cam.width as f32 && py <= cam.height as f32)
        };
        for k in 0..=10 {
            let y = 0.02 + (CAPTURE_HEIGHT - 0.3) * k as f32 / 10.0;
            assert!(cams.iter().all(|c| sees(c, Vec3::new(0.0, y, 0.0))), "axis point at y = {y}");
            for a in 0..16 {
                let ang = TAU * a as f32 / 16.0;
                let p = Vec3::new(CAPTURE_RADIUS * ang.sin(), y, CAPTURE_RADIUS * ang.cos());
                let n = cams.iter().filter(|c| sees(c, p)).count();
                assert!(n >= 3, "surface point {p:?} seen by {n} cameras");
            }
        }
    }

    #[test]
    fn spline_passes_through_waypoints_at_constant_speed() {
        let pts = vec![Vec3::new(0.0, 1.0, 0.0), Vec3::new(2.0, 1.0, 0.0), Vec3::new(2.0, 1.0, 3.0), Vec3::new(-1.0, 1.0, 3.5)];
        let path = ScanPath::new(pts.clone(), 0.5);
        assert!((path.position_at(0.0) - pts[0]).length() < 1e-5);
        assert!((path.position_at(path.duration()) - pts[3]).length() < 1e-4);
        assert!((path.position_at(1e9) - pts[3]).length() < 1e-4, "clamped past the end");
        assert!((path.position_at(-5.0) - pts[0]).length() < 1e-5, "clamped before the start");
        assert!(path.length() > 2.0 + 3.0 + 3.0 && path.length() < 10.0);
        assert!((path.duration() - path.length() / 0.5).abs() < 1e-4);
        // Each interior waypoint is on the curve.
        let dense = path.polyline(4000);
        for p in &pts {
            let d = dense.iter().map(|q| (*q - *p).length()).fold(f32::INFINITY, f32::min);
            assert!(d < 5e-3, "waypoint {p:?} missed by {d}");
        }
        // Constant speed: equal time steps cover equal distances.
        let dt = 0.05;
        let steps = (path.duration() / dt) as usize - 1;
        for i in 0..steps {
            let d = (path.position_at((i + 1) as f32 * dt) - path.position_at(i as f32 * dt)).length();
            assert!((d - 0.5 * dt).abs() < 0.05 * 0.5 * dt, "step {i}: {d}");
        }
        assert_eq!(path.with_speed(1.0).speed(), 1.0);
    }

    #[test]
    fn spline_is_c1() {
        // The direction of travel never jumps, including across waypoints.
        let path = ScanPath::house_default();
        let pts = path.polyline(6000);
        let mut worst = 0.0f32;
        for w in pts.windows(3) {
            let (a, b) = ((w[1] - w[0]).normalize(), (w[2] - w[1]).normalize());
            worst = worst.max(a.angle_between(b));
        }
        // 6000 samples over ~45 m is 7.5 mm per step: even the tightest turn
        // (radius ~0.3 m) bends less than 3 degrees per step.
        assert!(worst < 0.05, "direction jumps by {worst} rad");
    }

    #[test]
    fn polyline_edge_cases() {
        let path = ScanPath::house_default();
        assert!(path.polyline(0).is_empty());
        assert_eq!(path.polyline(1), vec![path.waypoints()[0]]);
        let p = path.polyline(50);
        assert_eq!(p.len(), 50);
        assert!((p[0] - path.waypoints()[0]).length() < 1e-5);
        assert!((p[49] - *path.waypoints().last().unwrap()).length() < 1e-3);
    }

    #[test]
    fn house_tour_is_a_plausible_walk() {
        let path = ScanPath::house_default();
        assert_eq!(path.speed(), ScanPath::DEFAULT_SPEED);
        assert!((30.0..90.0).contains(&path.duration()), "tour lasts {} s", path.duration());
        // Eye height throughout.
        for p in path.polyline(500) {
            assert!((p.y - EYE_HEIGHT).abs() < 1e-3);
        }
        // Visits every room, spending some time well inside each.
        let pts = path.polyline(2000);
        for room in ROOMS {
            let inside = pts
                .iter()
                .filter(|p| p.x > room.min[0] + 0.3 && p.x < room.max[0] - 0.3 && p.z > room.min[1] + 0.3 && p.z < room.max[1] - 0.3)
                .count();
            let meters = path.length() * inside as f32 / pts.len() as f32;
            assert!(meters > 0.5, "only {meters} m walked inside the {}", room.name);
        }
        // Goes through every door.
        for door in DOORS {
            let near = pts.iter().any(|p| (Vec3::new(p.x, 0.0, p.z) - door.center).length() < 0.5 * door.width);
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
                assert!(!snap.occluded(w[0], seg / len, len), "path crosses geometry near {:?} (sample {i})", w[0]);
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
            // Smooth footage: under 4 degrees of rotation and 4 cm of travel per frame.
            assert!(prev.forward().angle_between(f) < 0.07, "camera jerks at t = {t}");
            assert!((cam.position() - prev.position()).length() < 0.04);
            // Level horizon.
            assert!(cam.world_from_cam.matrix3.x_axis.y.abs() < 1e-4);
            // Stays near the path at eye height.
            assert!((cam.position() - path.position_at(t)).length() < 0.02);
            prev = cam;
        }
        assert!(min_pitch < -0.3 && max_pitch > 0.1, "pitch range {min_pitch}..{max_pitch}");
        assert!(left > frames / 12 && right > frames / 12, "pans left {left} / right {right} of {frames}");
        // Clamped outside the walk.
        assert_eq!(path.camera_at(-3.0, 70.0, 64, 48), path.camera_at(0.0, 70.0, 64, 48));
        let end = path.duration();
        assert_eq!(path.camera_at(end + 10.0, 70.0, 64, 48), path.camera_at(end, 70.0, 64, 48));
    }

    #[test]
    #[should_panic(expected = "at least two waypoints")]
    fn single_waypoint_is_rejected() {
        let _ = ScanPath::new(vec![Vec3::ZERO], 1.0);
    }
}
