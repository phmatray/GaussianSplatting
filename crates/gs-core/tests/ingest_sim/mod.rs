//! A simulated phone: renders the procedural scenes the way the companion
//! app would film them and encodes real capture frames.
//!
//! Shared by the ingest tests and the `gscap_*` examples. What makes the
//! frames realistic, each of it switchable:
//!
//! * a colour image several times finer than the depth map, sent as a JPEG;
//! * a depth map of 256x192 in millimeters that is the *average* of what
//!   lies under each pixel, as the phone's LiDAR pipeline delivers it: at
//!   a silhouette that gives pixels hanging between the object and what is
//!   behind it, marked with low confidence like ARKit does;
//! * depth noise growing with the square of the distance, pose jitter and a
//!   slow pose drift, exposure and white balance that wander;
//! * a world whose origin is where the phone started, looking along its
//!   first heading: not the floor, not the scene's axes;
//! * a person mask (what differs from the empty scene).

#![allow(dead_code)]

use gs_core::capture::{render_rgbd, CaptureOptions};
use gs_core::glam::{Affine3A, Mat3, Mat4, Quat, Vec3};
use gs_core::ingest::frame::{depth_meters_to_mm, CaptureFrame, Orientation, Tracking};
use gs_core::rig::ScanPath;
use gs_core::scene::SceneSnapshot;
use gs_core::{Camera, RgbdFrame, Splat};

/// Deterministic noise source (SplitMix64).
pub struct Rng(pub u64);

impl Rng {
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform in `0..1`.
    pub fn unit(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }

    /// Standard normal (sum of uniforms: good enough for sensor noise).
    pub fn normal(&mut self) -> f32 {
        (0..6).map(|_| self.unit()).sum::<f32>() * 2.0_f32.sqrt() - 3.0 * 2.0_f32.sqrt()
    }

    pub fn vec3(&mut self) -> Vec3 {
        Vec3::new(self.normal(), self.normal(), self.normal())
    }
}

/// The simulated phone.
#[derive(Clone, Copy, Debug)]
pub struct PhoneSim {
    /// Colour image as sent.
    pub color_width: u32,
    pub color_height: u32,
    /// LiDAR depth map (ARKit: 256x192).
    pub depth_width: u32,
    pub depth_height: u32,
    /// Horizontal field of view in degrees (the wide camera: about 60).
    pub fov_x_deg: f32,
    pub jpeg_quality: u8,
    /// Standard deviation of the depth noise at 1 m, meters; it grows with
    /// the square of the distance. `0` for an exact sensor.
    pub depth_noise: f32,
    /// Standard deviation of the per-frame pose error: meters, and a
    /// thousandth of that in radians.
    pub pose_jitter: f32,
    /// Size of the slow pose drift (a random walk), meters.
    pub pose_drift: f32,
    /// Amplitude of the exposure wander (`0.15`: plus or minus 15 %), with a
    /// white balance wander a third of it.
    pub exposure_drift: f32,
    /// Standard deviation of the grain added to the colour image, in 8-bit
    /// levels. The procedural scenes are flat and compress to a quarter of
    /// what a photograph does; with a grain of 6 the JPEGs are as large as
    /// a phone's (for decoding benchmarks).
    pub color_grain: f32,
    pub with_confidence: bool,
    /// Deflate the planes, as the phone does.
    pub compress: bool,
    pub shadows: bool,
    pub seed: u64,
}

impl Default for PhoneSim {
    /// An ideal phone: 960x720 colour, 256x192 depth, no noise.
    fn default() -> Self {
        Self {
            color_width: 960,
            color_height: 720,
            depth_width: 256,
            depth_height: 192,
            fov_x_deg: 60.0,
            jpeg_quality: 80,
            depth_noise: 0.0,
            pose_jitter: 0.0,
            pose_drift: 0.0,
            exposure_drift: 0.0,
            color_grain: 0.0,
            with_confidence: true,
            compress: true,
            shadows: true,
            seed: 1,
        }
    }
}

impl PhoneSim {
    /// What a real device adds: LiDAR noise of 1.5 mm at 1 m (1.4 cm at
    /// 3 m), poses off by 3 mm with a drift of 1 cm, exposure wandering by
    /// 15 %.
    pub fn realistic() -> Self {
        Self { depth_noise: 0.0015, pose_jitter: 0.003, pose_drift: 0.01, exposure_drift: 0.15, ..Self::default() }
    }

    /// The colour camera at a pose (scene coordinates).
    pub fn camera(&self, world_from_cam: Affine3A) -> Camera {
        let f = 0.5 * self.color_width as f32 / (0.5 * self.fov_x_deg.to_radians()).tan();
        Camera {
            width: self.color_width,
            height: self.color_height,
            fx: f,
            fy: f,
            cx: 0.5 * self.color_width as f32,
            cy: 0.5 * self.color_height as f32,
            world_from_cam,
        }
    }

    /// The camera at `eye` looking at `target`.
    pub fn look_at(&self, eye: Vec3, target: Vec3) -> Camera {
        self.camera(Camera::look_at(eye, target, Vec3::Y, 1.0, 4, 3).world_from_cam)
    }
}

/// The world of a capture: ARKit puts the origin where the phone was when
/// the session started, with -Z along its first heading and +Y up.
#[derive(Clone, Copy, Debug)]
pub struct CaptureWorld {
    /// Maps scene coordinates to capture coordinates.
    pub capture_from_scene: Affine3A,
}

impl CaptureWorld {
    /// The world a session started at the pose of `first` has.
    pub fn starting_at(first: &Camera) -> Self {
        let forward = first.forward();
        let yaw = (-forward.x).atan2(-forward.z);
        let scene_from_capture = Affine3A::from_rotation_translation(Quat::from_rotation_y(yaw), first.position());
        Self { capture_from_scene: scene_from_capture.inverse() }
    }

    /// Height of the scene's floor (`y = 0`) in capture coordinates: what
    /// the phone would report in its `floor` message.
    pub fn floor_y(self) -> f32 {
        self.capture_from_scene.transform_point3(Vec3::ZERO).y
    }

    /// A point of the scene in capture coordinates.
    pub fn to_capture(self, p: Vec3) -> Vec3 {
        self.capture_from_scene.transform_point3(p)
    }

    /// Splats as a viewer of the capture holds them (capture coordinates
    /// with the floor moved to `y = 0` by `floor_y`) brought back into the
    /// scene, for comparison with the ground truth.
    pub fn to_scene(self, splats: &[Splat], floor_y: f32) -> Vec<Splat> {
        let back = self.capture_from_scene.inverse();
        let turn = Quat::from_mat3(&Mat3::from(back.matrix3));
        splats
            .iter()
            .map(|s| Splat {
                pos: back.transform_point3(s.position() + Vec3::new(0.0, floor_y, 0.0)).to_array(),
                rot: (turn * s.rotation()).to_array(),
                ..*s
            })
            .collect()
    }
}

/// One exposure of the simulated phone, with what the test may know and the
/// phone does not.
pub struct Shot {
    pub frame: CaptureFrame,
    /// The true colour camera, scene coordinates.
    pub cam: Camera,
    /// The true depth at colour resolution, scene coordinates (for
    /// `observed_mask`).
    pub truth: RgbdFrame,
}

/// Films scenes frame after frame; carries the state of the drifts.
pub struct Phone {
    pub sim: PhoneSim,
    pub world: CaptureWorld,
    rng: Rng,
    drift: Vec3,
    next_index: u32,
}

impl Phone {
    /// A phone whose AR session starts at the pose of `first`.
    pub fn new(sim: PhoneSim, first: &Camera) -> Self {
        Self { sim, world: CaptureWorld::starting_at(first), rng: Rng(sim.seed), drift: Vec3::ZERO, next_index: 0 }
    }

    /// Takes one frame of `snap` from `cam` (a [`PhoneSim::camera`], scene
    /// coordinates) at `time` seconds. With `empty`, the scene without the
    /// person, the frame carries a person mask.
    pub fn shoot(&mut self, snap: &SceneSnapshot, empty: Option<&SceneSnapshot>, cam: &Camera, time: f64) -> Shot {
        let sim = self.sim;
        let opts = CaptureOptions { shadows: sim.shadows, min_range: 0.05, max_range: 50.0, ..CaptureOptions::default() };
        let truth = render_rgbd(snap, cam, &opts);
        let index = self.next_index;
        self.next_index += 1;

        // Colour: exposure and white balance wander slowly and independently.
        let mut rgb = truth.rgb.clone();
        if sim.exposure_drift > 0.0 {
            let t = time as f32;
            let exposure = 1.0 + sim.exposure_drift * (0.9 * t).sin();
            let warmth = sim.exposure_drift / 3.0 * (0.37 * t + 1.0).sin();
            let gain = [exposure * (1.0 + warmth), exposure, exposure * (1.0 - warmth)];
            for pixel in rgb.chunks_exact_mut(3) {
                for (value, g) in pixel.iter_mut().zip(gain) {
                    *value = (*value as f32 * g + 0.5).clamp(0.0, 255.0) as u8;
                }
            }
        }
        if sim.color_grain > 0.0 {
            for value in rgb.iter_mut() {
                *value = (*value as f32 + sim.color_grain * self.rng.normal() + 0.5).clamp(0.0, 255.0) as u8;
            }
        }
        let mut color = Vec::new();
        jpeg_encoder::Encoder::new(&mut color, sim.jpeg_quality)
            .encode(&rgb, sim.color_width as u16, sim.color_height as u16, jpeg_encoder::ColorType::Rgb)
            .expect("JPEG encoding failed");

        // Depth: the mean of what lies under each LiDAR pixel.
        let (dw, dh) = (sim.depth_width as usize, sim.depth_height as usize);
        let (cw, ch) = (sim.color_width as usize, sim.color_height as usize);
        let mut depth_mm = vec![0u16; dw * dh];
        let mut confidence = vec![0u8; dw * dh];
        for y in 0..dh {
            let (y0, y1) = (y * ch / dh, ((y + 1) * ch).div_ceil(dh).min(ch));
            for x in 0..dw {
                let (x0, x1) = (x * cw / dw, ((x + 1) * cw).div_ceil(dw).min(cw));
                let (mut sum, mut count, mut lo, mut hi) = (0.0f32, 0u32, f32::INFINITY, 0.0f32);
                for sy in y0..y1 {
                    for sx in x0..x1 {
                        let z = truth.depth[sy * cw + sx];
                        if z > 0.0 {
                            sum += z;
                            count += 1;
                            lo = lo.min(z);
                            hi = hi.max(z);
                        }
                    }
                }
                let cells = ((y1 - y0) * (x1 - x0)) as u32;
                if count * 2 < cells {
                    continue;
                }
                let mut z = sum / count as f32;
                if sim.depth_noise > 0.0 {
                    z += sim.depth_noise * z * z * self.rng.normal();
                }
                let spread = (hi - lo) / lo;
                depth_mm[y * dw + x] = depth_meters_to_mm(z);
                // ARKit: low at silhouettes, medium when far or unsure.
                confidence[y * dw + x] = if spread > 0.08 || count < cells {
                    0
                } else if spread > 0.03 || z > 3.5 {
                    1
                } else {
                    2
                };
            }
        }

        // Person mask: where the scene differs from the empty one.
        let mask = empty.map(|empty| {
            let small = Camera {
                width: sim.depth_width,
                height: sim.depth_height,
                fx: cam.fx * dw as f32 / cw as f32,
                fy: cam.fy * dh as f32 / ch as f32,
                cx: cam.cx * dw as f32 / cw as f32,
                cy: cam.cy * dh as f32 / ch as f32,
                world_from_cam: cam.world_from_cam,
            };
            let quick = CaptureOptions { shadows: false, ..opts };
            let (with, without) = (render_rgbd(snap, &small, &quick), render_rgbd(empty, &small, &quick));
            with.depth.iter().zip(&without.depth).map(|(a, b)| if (a - b).abs() > 0.02 { 255 } else { 0 }).collect()
        });

        // Pose: capture coordinates, with jitter and drift.
        let mut pose = self.world.capture_from_scene * cam.world_from_cam;
        if sim.pose_drift > 0.0 {
            // A random walk pulled back toward zero: wanders by about
            // `pose_drift` over hundreds of frames.
            self.drift = self.drift * 0.99 + self.rng.vec3() * (sim.pose_drift * 0.14);
            pose.translation += gs_core::glam::Vec3A::from(self.drift);
        }
        if sim.pose_jitter > 0.0 {
            pose.translation += gs_core::glam::Vec3A::from(self.rng.vec3() * sim.pose_jitter);
            let wobble = self.rng.vec3() * sim.pose_jitter;
            let turn = Quat::from_scaled_axis(wobble);
            pose.matrix3 = gs_core::glam::Mat3A::from_quat(turn) * pose.matrix3;
        }

        let frame = CaptureFrame {
            index,
            timestamp: 1000.0 + time,
            world_from_cam: Mat4::from(pose).to_cols_array(),
            intrinsics: [cam.fx, cam.fy, cam.cx, cam.cy],
            tracking: Tracking::Normal,
            orientation: Orientation::LandscapeNative,
            color_width: sim.color_width as u16,
            color_height: sim.color_height as u16,
            color,
            depth_width: sim.depth_width as u16,
            depth_height: sim.depth_height as u16,
            depth_mm,
            confidence: sim.with_confidence.then_some(confidence),
            mask,
            compressed: sim.compress,
            reserved: 0,
        };
        Shot { frame, cam: *cam, truth }
    }
}

/// A person filming a house: walks the scan path at eye height, turning
/// the phone around and tilting it up and down, as one does to cover a room.
pub struct HouseWalk {
    pub path: ScanPath,
    /// Frames per second of walking time.
    pub fps: f32,
}

impl HouseWalk {
    /// The walk at 0.4 m/s, half the pace of the simulated scan head: one
    /// phone has to look everywhere itself.
    pub fn new(fps: f32) -> Self {
        Self { path: ScanPath::house_default().with_speed(0.4), fps }
    }

    pub fn frame_count(&self) -> usize {
        (self.path.duration() * self.fps).floor() as usize + 1
    }

    pub fn time(&self, index: usize) -> f32 {
        index as f32 / self.fps
    }

    /// Pose of the phone at walking time `t` (scene coordinates).
    pub fn pose(&self, t: f32) -> Affine3A {
        spiral_pose(self.path.position_at(t), t / 6.0, t / 14.0)
    }
}

/// A phone at `position` in the middle of a spiral sweep: `turns` full
/// turns around the vertical so far, its tilt `cycles` times from level
/// down to 55 degrees, up to 50 and back. Turning while tilting paints
/// floor, walls and ceiling from wherever the person stands.
pub fn spiral_pose(position: Vec3, turns: f32, cycles: f32) -> Affine3A {
    let yaw = std::f32::consts::TAU * turns;
    let triangle = 4.0 * ((cycles + 0.25).fract() - 0.5).abs() - 1.0;
    let pitch = (-2.5f32 - 52.5 * triangle).to_radians();
    Affine3A::from_rotation_translation(Quat::from_rotation_y(yaw) * Quat::from_rotation_x(pitch), position)
}

/// A person filming one room: walks slowly along `waypoints` (floor plan
/// `[x, z]`, phone 1.4 m up) and sweeps the phone in a spiral, a full turn
/// every `frames_per_turn` frames. Returns the pose of frame `i` of `n`.
pub fn room_sweep(waypoints: &[[f32; 2]], i: usize, n: usize, frames_per_turn: f32) -> Affine3A {
    let points: Vec<Vec3> = waypoints.iter().map(|p| Vec3::new(p[0], 1.4, p[1])).collect();
    let lengths: Vec<f32> = points.windows(2).map(|w| w[0].distance(w[1])).collect();
    let total: f32 = lengths.iter().sum();
    let phase = i as f32 / (n.max(2) - 1) as f32;
    let mut left = phase * total;
    let mut position = *points.last().unwrap();
    for (w, &length) in points.windows(2).zip(&lengths) {
        if left <= length {
            position = w[0].lerp(w[1], left / length.max(1e-6));
            break;
        }
        left -= length;
    }
    spiral_pose(position, i as f32 / frames_per_turn, 1.5 * phase)
}

/// A phone carried around a subject, always looking at it: `turns` times
/// around `center` at `radius`, bobbing between `low` and `high`.
pub fn orbit_pose(center: Vec3, radius: f32, low: f32, high: f32, phase: f32, turns: f32) -> (Vec3, Vec3) {
    let angle = std::f32::consts::TAU * turns * phase;
    let height = 0.5 * (low + high) + 0.5 * (high - low) * (std::f32::consts::TAU * 1.5 * turns * phase).sin();
    (Vec3::new(center.x + radius * angle.sin(), height, center.z + radius * angle.cos()), center)
}
