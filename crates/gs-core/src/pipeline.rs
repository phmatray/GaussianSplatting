//! The live capture and reconstruction pipeline.
//!
//! [`Pipeline`] owns a scene, the simulated sensors and the reconstruction
//! state, and turns time into wire messages: call [`Pipeline::tick`] at the
//! configured rate and forward every [`Outgoing`] to the viewers. It never
//! sleeps, spawns nothing and does no I/O, so the same code runs on a native
//! thread behind a WebSocket server and inside a Web Worker.
//!
//! Downstream of the simulated sensors the pipeline only uses what a real
//! rig delivers: RGB-D frames and camera calibration.
//!
//! # Layers
//!
//! * Layer 0 is static. Live stage: the empty stage, reconstructed once from
//!   the background plates and sent as `Replace`. Home scan: the fused map,
//!   grown by `Append` messages that carry only the new splats, and sent once
//!   more as a whole (`Replace`) when the scan ends or is paused, because
//!   splats keep being refined from better views after they were appended.
//! * Layer 1 is live: the foreground of the stage, `Replace`d every tick.
//!   It stays empty in the house.
//!
//! # Delivery
//!
//! [`Outgoing::reliable`] messages (State, everything on layer 0) must reach
//! every client, in order. The others are "latest wins" per
//! [`Outgoing::slot`]: when a client is slow, a newer message of the same
//! slot may replace an older one that has not been sent yet.
//!
//! | Slot | Content |
//! |---|---|
//! | [`SLOT_RELIABLE`] (0) | every reliable message |
//! | [`SLOT_DYNAMIC`] (1) | layer 1 `Replace` |
//! | [`SLOT_STATS`] (2) | Stats |
//! | [`SLOT_PREVIEW_BASE`]` + id` (16…) | Preview of camera `id`: a ring camera of the stage, or a sensor of the scan head (the one looking ahead is 0) |
//!
//! A client that connects late needs [`Pipeline::snapshot`], taken between
//! two ticks, followed by everything later ticks emit.

use std::sync::atomic::{AtomicU32, Ordering};

use glam::{Affine3A, Quat, Vec3};
#[cfg(feature = "par")]
use rayon::prelude::*;
use web_time::Instant;

use crate::capture::{render_rgbd, CaptureOptions, PartialCapture, StaticRender};
use crate::codec::{self, Body, LayerMode, Message};
use crate::protocol::{
    CameraInfo, ControlMsg, PipelineConfig, ScanConfig, ScanHead, ScanInfo, ScanState, ScanStats, SessionState, Stats,
    ViewerStart, PROTOCOL_VERSION,
};
use crate::reconstruct::{foreground_mask, frame_to_splats, BackgroundPlate, VoxelFusion};
use crate::rig::ScanPath;
use crate::scene::{house, make_scene, Scene, SceneKind, SceneSnapshot};
use crate::types::{Camera, RgbdFrame, Splat};

/// Slot of every reliable message (they are never dropped, the slot is
/// informative only).
pub const SLOT_RELIABLE: u8 = 0;
/// Slot of the live layer.
pub const SLOT_DYNAMIC: u8 = 1;
/// Slot of the Stats message.
pub const SLOT_STATS: u8 = 2;
/// Slot of the preview of camera 0; camera `id` uses `SLOT_PREVIEW_BASE + id`.
/// In the house the ids are those of the sensors of the scan head: 0 is the
/// one looking ahead.
pub const SLOT_PREVIEW_BASE: u8 = 16;

/// Wire id of the static layer.
pub const LAYER_STATIC: u8 = 0;
/// Wire id of the live layer.
pub const LAYER_DYNAMIC: u8 = 1;

/// Longest accepted [`Pipeline::handle_control_json`] text in bytes.
pub const MAX_CONTROL_JSON: usize = 64 * 1024;

/// Wall-clock time one tick may spend on scan frames, in milliseconds. A
/// tick stops taking frames when the next one would probably exceed it (it
/// always takes at least one frame that is due).
pub const SCAN_TICK_BUDGET_MS: f64 = 25.0;
/// Exposures averaged into a background plate when the sensor is noisy.
const PLATE_EXPOSURES: u32 = 4;
/// Points of the scan path polyline in [`SessionState`].
const SCAN_PATH_POINTS: usize = 240;

/// One encoded [`crate::codec`] message and how to deliver it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Outgoing {
    /// The message, ready to be sent as one WebSocket frame or `postMessage`.
    pub bytes: Vec<u8>,
    /// Must reach every client, in order. Otherwise latest-wins per `slot`.
    pub reliable: bool,
    /// Latest-wins key of non-reliable messages (see the module docs).
    pub slot: u8,
}

/// Live stage state.
struct Stage {
    cameras: Vec<Camera>,
    /// The simulated sensors' view of the empty stage, one per camera: what
    /// makes capturing a tick cost the moving pixels only.
    fixed: Vec<StaticRender>,
    /// One plate per camera; empty without background subtraction.
    plates: Vec<BackgroundPlate>,
    static_splats: Vec<Splat>,
    dynamic_splats: Vec<Splat>,
    /// The latest exposure of every camera (for the previews).
    frames: Vec<RgbdFrame>,
    /// Position in the dance loop, seconds.
    time: f64,
    /// Exposure counter; seeds the sensor noise.
    exposures: u32,
    preview_cursor: usize,
}

/// Home scan state.
struct House {
    snapshot: SceneSnapshot,
    path: ScanPath,
    fusion: VoxelFusion,
    state: ScanState,
    /// Fuse the rest of the path as fast as the tick budget allows.
    finishing: bool,
    /// Walking time along the path, seconds.
    time: f64,
    /// Index of the next frame to capture (see [`scan_frame`]).
    next_frame: u64,
    /// [`VoxelFusion::refined`] when layer 0 was last broadcast as a whole.
    published_refined: u64,
    /// Smoothed cost of capturing and fusing one frame, milliseconds.
    frame_ms: f64,
}

// The stage is the common case and lives as long as the pipeline; boxing it
// would only add an indirection.
#[allow(clippy::large_enum_variant)]
enum Mode {
    Stage(Stage),
    House(House),
}

/// Capture → reconstruct → encode, one tick at a time.
///
/// `Send + Sync`: move it to a pipeline thread, or keep it behind a mutex.
pub struct Pipeline {
    config: PipelineConfig,
    scene: Box<dyn Scene>,
    mode: Mode,
    /// Message counter. Atomic so that [`Pipeline::snapshot`] can number its
    /// messages through a shared reference.
    seq: AtomicU32,
    tick: u64,
    last_tick: Option<Instant>,
    tick_hz: f32,
    last_stats: Stats,
}

/// Timings and counters gathered during one tick.
#[derive(Default)]
struct TickWork {
    capture_ms: f64,
    reconstruct_ms: f64,
    encode_ms: f64,
    pixels: u64,
}

fn ms_since(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1e3
}

fn saturating_u32(value: usize) -> u32 {
    value.min(u32::MAX as usize) as u32
}

fn capture_options(config: &PipelineConfig, seed: u32) -> CaptureOptions {
    CaptureOptions { shadows: config.shadows, noise: config.noise, seed, ..CaptureOptions::default() }
}

/// Captures every camera at the same scene instant.
///
/// `fixed` holds what each camera sees of the static part of the scene:
/// only the pixels that the moving objects of `snap` can change are cast
/// again, the frames are the same bit for bit (see [`PartialCapture`]).
/// Cameras without one, or a snapshot that is not that static part plus
/// moving objects, are rendered in full.
fn capture_ring(
    snap: &SceneSnapshot,
    cameras: &[Camera],
    fixed: &[StaticRender],
    config: &PipelineConfig,
    exposure: u32,
) -> Vec<RgbdFrame> {
    let partial = fixed.first().and_then(|first| PartialCapture::new(snap, first.scene()));
    let shoot = |(i, cam): (usize, &Camera)| {
        // Distinct noise per camera and per exposure.
        let seed = exposure.wrapping_mul(0x0001_0001).wrapping_add(i as u32).wrapping_mul(0x9E37_79B1);
        let opts = capture_options(config, seed);
        match (&partial, fixed.get(i)) {
            (Some(partial), Some(fixed)) => partial.render(cam, &opts, fixed),
            _ => render_rgbd(snap, cam, &opts),
        }
    };
    #[cfg(feature = "par")]
    return cameras.par_iter().enumerate().map(shoot).collect();
    #[cfg(not(feature = "par"))]
    cameras.iter().enumerate().map(shoot).collect()
}

/// Runs `per_camera(index, out)` for every camera and concatenates the
/// results in camera order, whatever the thread schedule.
fn reconstruct_ring(count: usize, out: &mut Vec<Splat>, per_camera: impl Fn(usize, &mut Vec<Splat>) + Sync) {
    out.clear();
    #[cfg(feature = "par")]
    {
        let parts: Vec<Vec<Splat>> = (0..count)
            .into_par_iter()
            .map(|i| {
                let mut part = Vec::new();
                per_camera(i, &mut part);
                part
            })
            .collect();
        out.reserve(parts.iter().map(Vec::len).sum());
        for part in &parts {
            out.extend_from_slice(part);
        }
    }
    #[cfg(not(feature = "par"))]
    for i in 0..count {
        per_camera(i, out);
    }
}

/// `frame` reduced by an integer factor so that it is at most `max_width`
/// wide (center sample of every block).
fn downsample(frame: &RgbdFrame, max_width: u32) -> RgbdFrame {
    let step = frame.width.div_ceil(max_width.max(1)).max(1);
    if step == 1 {
        return frame.clone();
    }
    let (w, h) = ((frame.width / step).max(1), (frame.height / step).max(1));
    let mut out = RgbdFrame::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let sx = (x * step + step / 2).min(frame.width - 1);
            let sy = (y * step + step / 2).min(frame.height - 1);
            let (src, dst) = (frame.index(sx, sy), out.index(x, y));
            out.depth[dst] = frame.depth[src];
            out.rgb[dst * 3..dst * 3 + 3].copy_from_slice(&frame.rgb[src * 3..src * 3 + 3]);
        }
    }
    out
}

/// Recursively merges `patch` into `target`: objects merge key by key,
/// anything else replaces.
fn merge_json(target: &mut serde_json::Value, patch: serde_json::Value) {
    match (target, patch) {
        (serde_json::Value::Object(target), serde_json::Value::Object(patch)) => {
            for (key, value) in patch {
                merge_json(target.entry(key).or_insert(serde_json::Value::Null), value);
            }
        }
        (target, patch) => *target = patch,
    }
}

/// Peak yaw sway of the scan head around the walking direction, radians
/// (10 degrees).
pub const SCAN_PAN_AMPLITUDE: f32 = 0.175;
/// Period of the yaw sway in seconds of walking time.
pub const SCAN_PAN_PERIOD: f32 = 11.0;
/// Peak pitch sway of the scan head around the horizon, radians (3 degrees).
pub const SCAN_TILT_AMPLITUDE: f32 = 0.052;
/// Period of the pitch sway in seconds of walking time.
pub const SCAN_TILT_PERIOD: f32 = 7.0;
/// Phase of the pitch sway at the start of the walk, radians.
const SCAN_TILT_PHASE: f32 = 0.6;
/// The scan head faces the walking direction averaged over this many seconds
/// of walking time, half of them ahead and half behind: hands do not follow
/// every change of direction at once.
const SCAN_HEADING_WINDOW: f32 = 1.0;
/// Spacing of the heading samples that are averaged, seconds.
const SCAN_HEADING_STEP: f32 = 0.125;

/// When, and by which sensor of the head, a frame of the home scan is taken.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScanFrame {
    /// Id of the sensor that fires (an index into [`ScanHead::mounts`]).
    pub sensor: u32,
    /// Walking time in seconds.
    pub time: f32,
}

/// Number of sensor frames of the whole walk along `path`.
///
/// The frame rate counts as [`ScanConfig::sanitized`] leaves it, which is
/// how the pipeline uses it: the count is bounded whatever `scan` holds.
pub fn scan_frame_count(path: &ScanPath, scan: &ScanConfig) -> u64 {
    last_scan_frame(path.duration() as f64, scan).saturating_add(1)
}

/// Frame `index` of the home scan: the sensors fire one at a time in
/// [`ScanHead::firing_order`], frame `i` at walking time
/// `i / frames_per_second`, and the last one at the end of the path. The
/// pipeline fuses exactly the frames `0..`[`scan_frame_count`], each seen
/// from [`scan_sensor`] at its time.
pub fn scan_frame(path: &ScanPath, scan: &ScanConfig, index: u64) -> ScanFrame {
    ScanFrame {
        sensor: firing_sensor(scan.head, index),
        time: scan_frame_time(path.duration() as f64, scan, index) as f32,
    }
}

/// Sensor frames per second of walking time. Sanitized, so that a
/// configuration that never went through a pipeline (zero, negative or
/// infinite rate, not a number) cannot ask for frames without end.
fn scan_frame_rate(scan: &ScanConfig) -> f64 {
    scan.sanitized().frames_per_second as f64
}

/// Index of the last frame of a walk of `duration` seconds.
fn last_scan_frame(duration: f64, scan: &ScanConfig) -> u64 {
    // The cast saturates; a duration that is not a number gives frame 0.
    (duration * scan_frame_rate(scan)).ceil() as u64
}

/// Walking time of frame `index` of a walk of `duration` seconds.
fn scan_frame_time(duration: f64, scan: &ScanConfig, index: u64) -> f64 {
    (index as f64 / scan_frame_rate(scan)).min(duration)
}

/// The sensor that takes frame `index`.
fn firing_sensor(head: ScanHead, index: u64) -> u32 {
    let order = head.firing_order();
    order[(index % order.len() as u64) as usize] as u32
}

/// Pose of the main sensor of the scan head, the one looking ahead, at
/// walking time `time`: [`scan_sensor`] 0.
pub fn scan_camera(path: &ScanPath, time: f32, scan: &ScanConfig) -> Camera {
    scan_sensor(path, time, scan, 0)
}

/// Pose of sensor `sensor` of the scan head at walking time `time` (seconds,
/// clamped to the path; a time that is not a number counts as 0, a sensor
/// the head does not have as sensor 0). Field of view and image size are
/// taken from `scan` as they are.
///
/// The head rides along `path` at eye height and faces the walking direction
/// ([`ScanPath::heading_at`], which the walker keeps from swinging faster
/// than about 35 degrees per second, averaged over one second so that the
/// head also starts and stops turning gently), with the slow, small sway of
/// something carried by hand: [`SCAN_PAN_AMPLITUDE`] of yaw and
/// [`SCAN_TILT_AMPLITUDE`] of pitch. Every sensor is rigidly mounted on it
/// at its [`ScanHead::mounts`] angles, so all of them turn at the same, calm
/// rate: 20 degrees per second on average and never more than 37 on the
/// house tour, and that rate changes by at most 43 degrees per second in a
/// second.
///
/// The coverage comes from the number of sensors, not from sweeping one
/// camera about: [`ScanHead::Surround`] sees floor, ceiling and both side
/// walls at every step of the walk. A single camera needs a look-around
/// sweep of some 200 degrees per second to scan as much, which no one
/// filming a house would do.
pub fn scan_sensor(path: &ScanPath, time: f32, scan: &ScanConfig, sensor: u32) -> Camera {
    ScanHeadPose::at(path, time).sensor(scan, sensor)
}

/// Where the scan head is and which way it faces.
#[derive(Clone, Copy)]
struct ScanHeadPose {
    position: Vec3,
    /// Head-to-world rotation. The head looks along its -Z axis, like a
    /// camera.
    rotation: Quat,
}

impl ScanHeadPose {
    /// The head at walking time `time` (see [`scan_sensor`]).
    fn at(path: &ScanPath, time: f32) -> Self {
        use std::f32::consts::{PI, TAU};
        // `max` and `min` rather than `clamp`, which panics on a path whose
        // duration is not a number.
        let t = if time.is_finite() { time.max(0.0).min(path.duration()) } else { 0.0 };
        let heading = smoothed_heading(path, t);
        let pan = SCAN_PAN_AMPLITUDE * (TAU * t / SCAN_PAN_PERIOD).sin();
        let tilt = SCAN_TILT_AMPLITUDE * (TAU * t / SCAN_TILT_PERIOD + SCAN_TILT_PHASE).sin();
        // A camera looks along its -Z axis; half a turn brings that to +Z,
        // where `atan2(x, z)` measures the heading from. Positive tilt looks
        // up.
        let rotation = Quat::from_rotation_y(heading.x.atan2(heading.z) + pan + PI) * Quat::from_rotation_x(tilt);
        Self { position: path.position_at(t), rotation }
    }

    /// Sensor `sensor` of the head (sensor 0 for one it does not have).
    fn sensor(&self, scan: &ScanConfig, sensor: u32) -> Camera {
        let mounts = scan.head.mounts();
        let mount = mounts.get(sensor as usize).unwrap_or(&mounts[0]);
        let rotation = self.rotation
            * Quat::from_rotation_y(mount.yaw_deg.to_radians())
            * Quat::from_rotation_x(mount.pitch_deg.to_radians());
        let world_from_cam = Affine3A::from_rotation_translation(rotation, self.position);
        Camera::from_pose(world_from_cam, scan.fov_y_deg.to_radians(), scan.width, scan.height)
    }
}

/// [`ScanPath::heading_at`] averaged over [`SCAN_HEADING_WINDOW`] seconds
/// centered on `t` (the path counts as standing still before its start and
/// after its end).
///
/// The heading of the path has kinks: it freezes all at once a step before
/// the end of the walk, for one. Averaging turns every sudden change of the
/// turning rate into a ramp over the window.
///
/// The average is the exact integral of the heading interpolated linearly
/// between samples at fixed times (multiples of [`SCAN_HEADING_STEP`]), not
/// a sum of samples that move along with `t`: such a sum would jump a
/// little whenever one of its samples passes a kink.
fn smoothed_heading(path: &ScanPath, t: f32) -> Vec3 {
    let (from, to) = (t - 0.5 * SCAN_HEADING_WINDOW, t + 0.5 * SCAN_HEADING_WINDOW);
    let first = (from / SCAN_HEADING_STEP).floor() as i32;
    let last = (to / SCAN_HEADING_STEP).ceil() as i32;
    let sample = |cell: i32| path.heading_at(cell as f32 * SCAN_HEADING_STEP);
    let mut sum = Vec3::ZERO;
    let mut left = sample(first);
    for cell in first..last {
        let right = sample(cell + 1);
        let start = cell as f32 * SCAN_HEADING_STEP;
        let (lo, hi) = (start.max(from), (start + SCAN_HEADING_STEP).min(to));
        if hi > lo {
            // A linear piece integrates to its length times its midpoint.
            let mid = (0.5 * (lo + hi) - start) / SCAN_HEADING_STEP;
            sum += (hi - lo) * left.lerp(right, mid);
        }
        left = right;
    }
    // The sum only vanishes if the path turns back on itself within the
    // window, which the walker's turn limit rules out.
    Vec3::new(sum.x, 0.0, sum.z).try_normalize().unwrap_or_else(|| path.heading_at(t))
}

impl Stage {
    /// Captures the empty stage, builds the plates and the static layer, and
    /// reconstructs the live layer at `time`.
    fn new(scene: &dyn Scene, config: &PipelineConfig, time: f64) -> Self {
        let cameras = config.rig.cameras();
        let empty = scene.snapshot_static();
        let render = |cam: &Camera| StaticRender::new(&empty, cam, &capture_options(config, 0));
        #[cfg(feature = "par")]
        let fixed = cameras.par_iter().map(render).collect();
        #[cfg(not(feature = "par"))]
        let fixed = cameras.iter().map(render).collect();
        let mut stage = Self {
            cameras,
            fixed,
            plates: Vec::new(),
            static_splats: Vec::new(),
            dynamic_splats: Vec::new(),
            frames: Vec::new(),
            time,
            exposures: 0,
            preview_cursor: 0,
        };
        if config.background_subtraction {
            // A noisy sensor gets a plate averaged over several exposures.
            let exposures = if config.noise.is_none() { 1 } else { PLATE_EXPOSURES };
            let mut per_camera: Vec<Vec<RgbdFrame>> = stage.cameras.iter().map(|_| Vec::new()).collect();
            for _ in 0..exposures {
                let frames = capture_ring(&empty, &stage.cameras, &stage.fixed, config, stage.exposures);
                stage.exposures = stage.exposures.wrapping_add(1);
                for (list, frame) in per_camera.iter_mut().zip(frames) {
                    list.push(frame);
                }
            }
            stage.plates = per_camera.iter().map(|frames| BackgroundPlate::from_frames(frames)).collect();
            let (cameras, plates) = (&stage.cameras, &stage.plates);
            reconstruct_ring(cameras.len(), &mut stage.static_splats, |i, out| {
                frame_to_splats(plates[i].frame(), &cameras[i], &config.recon, None, out);
            });
        }
        stage.refresh(scene, config, &mut TickWork::default());
        stage
    }

    /// Captures all cameras at the current time and rebuilds the live layer.
    fn refresh(&mut self, scene: &dyn Scene, config: &PipelineConfig, work: &mut TickWork) {
        let start = Instant::now();
        let snap = scene.snapshot(self.time as f32);
        self.frames = capture_ring(&snap, &self.cameras, &self.fixed, config, self.exposures);
        self.exposures = self.exposures.wrapping_add(1);
        work.capture_ms += ms_since(start);
        work.pixels += self.frames.iter().map(|f| f.width as u64 * f.height as u64).sum::<u64>();

        let start = Instant::now();
        let (cameras, plates, frames) = (&self.cameras, &self.plates, &self.frames);
        reconstruct_ring(cameras.len(), &mut self.dynamic_splats, |i, out| match plates.get(i) {
            Some(plate) => {
                let mask = foreground_mask(&frames[i], plate, &config.recon);
                frame_to_splats(&frames[i], &cameras[i], &config.recon, Some(&mask), out);
            }
            None => {
                frame_to_splats(&frames[i], &cameras[i], &config.recon, None, out);
            }
        });
        work.reconstruct_ms += ms_since(start);
    }
}

impl House {
    fn new(scene: &dyn Scene, config: &PipelineConfig) -> Self {
        let mut fusion = VoxelFusion::new(config.scan.voxel_size);
        fusion.max_range = config.scan.max_range;
        Self {
            snapshot: scene.snapshot_static(),
            path: ScanPath::house_default().with_speed(config.scan.speed),
            fusion,
            state: if config.scan.autostart { ScanState::Scanning } else { ScanState::Idle },
            finishing: false,
            time: 0.0,
            next_frame: 0,
            published_refined: 0,
            frame_ms: 0.0,
        }
    }

    /// Discards the map and rewinds the walk.
    fn reset(&mut self, state: ScanState) {
        self.fusion.clear();
        self.state = state;
        self.finishing = false;
        self.time = 0.0;
        self.next_frame = 0;
        self.published_refined = 0;
    }

    fn duration(&self) -> f64 {
        self.path.duration() as f64
    }

    /// Index of the last frame of the walk.
    fn last_frame(&self, config: &PipelineConfig) -> u64 {
        last_scan_frame(self.duration(), &config.scan)
    }

    /// Walking time of frame `index`.
    fn frame_time(&self, index: u64, config: &PipelineConfig) -> f64 {
        scan_frame_time(self.duration(), &config.scan, index)
    }

    /// Number of frames that are due at the current walking time.
    fn frames_due(&self, config: &PipelineConfig) -> u64 {
        if self.time >= self.duration() {
            self.last_frame(config) + 1
        } else {
            (self.time * config.scan.frames_per_second as f64).floor() as u64 + 1
        }
    }

    /// Walking time up to which the path has been fused: the walker's own
    /// time, or the time of the next frame while the fusion lags behind (a
    /// tick that ran out of budget, `ScanFinish`). It never steps back, so
    /// the head it places moves steadily even then.
    fn cursor(&self, config: &PipelineConfig) -> f64 {
        self.time.min(self.frame_time(self.next_frame, config))
    }

    /// The scan head at walking time `time`.
    fn head(&self, time: f64) -> ScanHeadPose {
        ScanHeadPose::at(&self.path, time as f32)
    }

    /// Splats were rewritten in place since layer 0 was last sent whole.
    fn needs_polish(&self) -> bool {
        self.fusion.refined() != self.published_refined
    }

    fn stats(&self, config: &PipelineConfig) -> ScanStats {
        let cursor = self.cursor(config);
        let progress = match self.state {
            ScanState::Done => 1.0,
            _ => (cursor / self.duration().max(1e-9)).clamp(0.0, 1.0) as f32,
        };
        let head = self.head(cursor);
        let info = |sensor: u32| CameraInfo::from_camera(sensor, &head.sensor(&config.scan, sensor));
        ScanStats {
            state: self.state,
            progress,
            camera: info(0),
            extra_cameras: (1..config.scan.head.sensor_count()).map(info).collect(),
        }
    }
}

impl Pipeline {
    /// Builds the pipeline for `config` (after [`PipelineConfig::sanitized`]).
    /// For the stage this already captures the empty stage and reconstructs
    /// the first live frame.
    pub fn new(config: PipelineConfig) -> Self {
        let config = config.sanitized();
        let scene = make_scene(config.scene);
        let mode = Self::build_mode(scene.as_ref(), &config, 0.0);
        Self {
            config,
            scene,
            mode,
            seq: AtomicU32::new(0),
            tick: 0,
            last_tick: None,
            tick_hz: 0.0,
            last_stats: Stats::default(),
        }
    }

    fn build_mode(scene: &dyn Scene, config: &PipelineConfig, time: f64) -> Mode {
        match config.scene {
            SceneKind::Stage => Mode::Stage(Stage::new(scene, config, time)),
            SceneKind::House => Mode::House(House::new(scene, config)),
        }
    }

    /// The configuration in effect (sanitized).
    pub fn config(&self) -> &PipelineConfig {
        &self.config
    }

    /// Scene time in seconds: the position in the dance loop (stage) or the
    /// walking time along the scan path (house).
    pub fn scene_time(&self) -> f32 {
        match &self.mode {
            Mode::Stage(stage) => stage.time as f32,
            Mode::House(house) => house.time as f32,
        }
    }

    /// Progress of the home scan, `None` on the stage.
    pub fn scan_state(&self) -> Option<ScanState> {
        match &self.mode {
            Mode::Stage(_) => None,
            Mode::House(house) => Some(house.state),
        }
    }

    /// The current splats of the static layer 0, unquantized.
    pub fn static_splats(&self) -> &[Splat] {
        match &self.mode {
            Mode::Stage(stage) => &stage.static_splats,
            Mode::House(house) => house.fusion.splats(),
        }
    }

    /// The current splats of the live layer 1, unquantized.
    pub fn dynamic_splats(&self) -> &[Splat] {
        match &self.mode {
            Mode::Stage(stage) => &stage.dynamic_splats,
            Mode::House(_) => &[],
        }
    }

    /// The session description viewers need to interpret the layers.
    pub fn state(&self) -> SessionState {
        let (eye, target) = self.scene.viewer_start();
        let (cameras, scan, ceiling_height) = match &self.mode {
            Mode::Stage(stage) => {
                let cameras =
                    stage.cameras.iter().enumerate().map(|(i, c)| CameraInfo::from_camera(i as u32, c)).collect();
                (cameras, None, None)
            }
            Mode::House(h) => {
                let path = h.path.polyline(SCAN_PATH_POINTS).iter().map(Vec3::to_array).collect();
                (Vec::new(), Some(ScanInfo { path, duration: h.path.duration() }), Some(house::CEILING_HEIGHT))
            }
        };
        SessionState {
            version: PROTOCOL_VERSION,
            config: self.config,
            scene: self.scene.name().to_string(),
            cameras,
            bounds: self.scene.bounds(),
            viewer_start: ViewerStart { eye: eye.to_array(), target: target.to_array() },
            loop_duration: self.scene.loop_duration(),
            ceiling_height,
            scan,
        }
    }

    // -- encoding ----------------------------------------------------------

    fn next_seq(&self) -> u32 {
        self.seq.fetch_add(1, Ordering::Relaxed).wrapping_add(1)
    }

    /// Scene time as the wire header carries it (wrapping milliseconds).
    fn time_ms(&self) -> u32 {
        (self.scene_time() as f64 * 1e3) as u64 as u32
    }

    fn encode_state(&self) -> Outgoing {
        let json = serde_json::to_string(&self.state()).unwrap_or_else(|_| "{}".to_string());
        let msg = Message { seq: self.next_seq(), time_ms: self.time_ms(), body: Body::State(json) };
        Outgoing { bytes: codec::encode(&msg), reliable: true, slot: SLOT_RELIABLE }
    }

    fn encode_stats(&self, stats: &Stats) -> Outgoing {
        let json = serde_json::to_string(stats).unwrap_or_else(|_| "{}".to_string());
        let msg = Message { seq: self.next_seq(), time_ms: self.time_ms(), body: Body::Stats(json) };
        Outgoing { bytes: codec::encode(&msg), reliable: false, slot: SLOT_STATS }
    }

    /// Encodes splats for a layer: everything on layer 0 is reliable, layer
    /// 1 is latest-wins.
    fn encode_splats(&self, layer: u8, mode: LayerMode, splats: &[Splat]) -> Outgoing {
        let mut bytes = Vec::new();
        codec::encode_layer(self.next_seq(), self.time_ms(), layer, mode, splats, &mut bytes);
        let reliable = layer == LAYER_STATIC;
        Outgoing { bytes, reliable, slot: if reliable { SLOT_RELIABLE } else { SLOT_DYNAMIC } }
    }

    /// Encodes the current content of a layer as `Replace`.
    fn encode_layer(&self, layer: u8) -> Outgoing {
        let splats = if layer == LAYER_STATIC { self.static_splats() } else { self.dynamic_splats() };
        self.encode_splats(layer, LayerMode::Replace, splats)
    }

    fn encode_preview(&self, camera_id: u8, frame: &RgbdFrame) -> Outgoing {
        let small = downsample(frame, self.config.preview.max_width);
        let mut bytes = Vec::new();
        codec::encode_preview(self.next_seq(), self.time_ms(), camera_id, &small, &mut bytes);
        Outgoing { bytes, reliable: false, slot: SLOT_PREVIEW_BASE.saturating_add(camera_id) }
    }

    /// State plus both layers as `Replace`: a full resynchronization.
    fn encode_everything(&self) -> Vec<Outgoing> {
        vec![self.encode_state(), self.encode_layer(LAYER_STATIC), self.encode_layer(LAYER_DYNAMIC)]
    }

    fn current_stats(&self, work: &TickWork, total_ms: f64, bytes: usize) -> Stats {
        Stats {
            tick: self.tick,
            scene_time: self.scene_time(),
            capture_ms: work.capture_ms as f32,
            reconstruct_ms: work.reconstruct_ms as f32,
            encode_ms: work.encode_ms as f32,
            total_ms: total_ms as f32,
            tick_hz: self.tick_hz,
            splats_static: saturating_u32(self.static_splats().len()),
            splats_dynamic: saturating_u32(self.dynamic_splats().len()),
            bytes: saturating_u32(bytes),
            pixels: work.pixels.min(u32::MAX as u64) as u32,
            scan: match &self.mode {
                Mode::Stage(_) => None,
                Mode::House(house) => Some(house.stats(&self.config)),
            },
        }
    }

    /// The last Stats with everything a control message can change
    /// refreshed, so that its effect shows without waiting for a tick.
    fn encode_fresh_stats(&self) -> Outgoing {
        let mut stats = self.last_stats;
        stats.scene_time = self.scene_time();
        stats.splats_static = saturating_u32(self.static_splats().len());
        stats.splats_dynamic = saturating_u32(self.dynamic_splats().len());
        stats.scan = match &self.mode {
            Mode::Stage(_) => None,
            Mode::House(house) => Some(house.stats(&self.config)),
        };
        self.encode_stats(&stats)
    }

    // -- ticking -----------------------------------------------------------

    /// Advances the pipeline by `dt` real seconds and returns the messages to
    /// broadcast.
    ///
    /// Scene time advances by `dt · time_scale` unless paused. Stage: all
    /// ring cameras capture the same instant, the foreground is
    /// reconstructed and layer 1 is replaced; one or more camera previews
    /// follow in round-robin order. House: the scan head walks on, the
    /// sensor frames that became due are fused (as many as fit into
    /// [`SCAN_TICK_BUDGET_MS`], at least one), their new splats are appended
    /// to layer 0, and every sensor that fired sends a preview. A Stats
    /// message is emitted on every tick, paused or not, always last.
    ///
    /// A `dt` that is negative or not a number counts as zero; any positive
    /// value is accepted (the dance loops, the scan stops at the end of the
    /// path).
    pub fn tick(&mut self, dt: f32) -> Vec<Outgoing> {
        let start = Instant::now();
        if let Some(last) = self.last_tick {
            let interval = start.duration_since(last).as_secs_f64();
            if interval > 1e-6 {
                let hz = (1.0 / interval) as f32;
                self.tick_hz = if self.tick_hz > 0.0 { 0.9 * self.tick_hz + 0.1 * hz } else { hz };
            }
        }
        self.last_tick = Some(start);
        self.tick += 1;

        let advance = if dt.is_finite() && dt > 0.0 { dt as f64 * self.config.time_scale as f64 } else { 0.0 };
        let mut work = TickWork::default();
        let mut out = Vec::new();
        if !self.config.paused {
            match self.config.scene {
                SceneKind::Stage => self.tick_stage(advance, &mut work, &mut out),
                SceneKind::House => self.tick_house(advance, start, &mut work, &mut out),
            }
        }

        let bytes = out.iter().map(|o| o.bytes.len()).sum();
        let stats = self.current_stats(&work, ms_since(start), bytes);
        self.last_stats = stats;
        out.push(self.encode_stats(&stats));
        out
    }

    fn tick_stage(&mut self, advance: f64, work: &mut TickWork, out: &mut Vec<Outgoing>) {
        let Mode::Stage(stage) = &mut self.mode else { return };
        stage.time += advance;
        if let Some(period) = self.scene.loop_duration().filter(|p| *p > 0.0) {
            stage.time %= period as f64;
        }
        if !stage.time.is_finite() {
            stage.time = 0.0;
        }
        stage.refresh(self.scene.as_ref(), &self.config, work);

        let count = stage.frames.len();
        let preview = &self.config.preview;
        let per_tick = if preview.enabled { (preview.cameras_per_tick as usize).min(count) } else { 0 };
        let first = stage.preview_cursor % count.max(1);
        stage.preview_cursor = (first + per_tick) % count.max(1);

        let encode = Instant::now();
        out.push(self.encode_layer(LAYER_DYNAMIC));
        let Mode::Stage(stage) = &self.mode else { return };
        for k in 0..per_tick {
            let id = (first + k) % count;
            out.push(self.encode_preview(id as u8, &stage.frames[id]));
        }
        work.encode_ms += ms_since(encode);
    }

    fn tick_house(&mut self, advance: f64, start: Instant, work: &mut TickWork, out: &mut Vec<Outgoing>) {
        let config = self.config;
        let Mode::House(h) = &mut self.mode else { return };
        if h.state != ScanState::Scanning {
            return;
        }
        let duration = h.duration();
        h.time = if h.finishing { duration } else { (h.time + advance).min(duration) };

        let first_new = h.fusion.len();
        let due = h.frames_due(&config);
        // The latest frame of every sensor that fired during this tick.
        let mut latest: [Option<RgbdFrame>; ScanHead::MAX_SENSORS] = Default::default();
        let mut fired = false;
        while h.next_frame < due {
            let frame_start = Instant::now();
            let time = h.frame_time(h.next_frame, &config);
            let sensor = firing_sensor(config.scan.head, h.next_frame);
            let cam = h.head(time).sensor(&config.scan, sensor);
            let frame = render_rgbd(&h.snapshot, &cam, &capture_options(&config, h.next_frame as u32));
            let captured = ms_since(frame_start);
            work.capture_ms += captured;
            work.pixels += frame.width as u64 * frame.height as u64;
            h.fusion.integrate(&frame, &cam, &config.recon);
            let spent = ms_since(frame_start);
            work.reconstruct_ms += spent - captured;
            h.frame_ms = if h.frame_ms > 0.0 { 0.7 * h.frame_ms + 0.3 * spent } else { spent };
            h.next_frame += 1;
            fired = true;
            if let Some(slot) = latest.get_mut(sensor as usize) {
                *slot = Some(frame);
            }
            if ms_since(start) + h.frame_ms > SCAN_TICK_BUDGET_MS {
                break;
            }
        }

        // The walk ends with a tick of its own that does no capture work:
        // it publishes the splats that were rewritten from better views
        // since they were appended, and announces `Done`.
        let walked = h.next_frame > h.last_frame(&config);
        let finished = walked && !fired;
        if finished {
            h.state = ScanState::Done;
            h.finishing = false;
        }
        let polish = finished && h.needs_polish();
        if polish {
            h.published_refined = h.fusion.refined();
        }

        let encode = Instant::now();
        let Mode::House(h) = &self.mode else { return };
        if polish {
            out.push(self.encode_layer(LAYER_STATIC));
        } else if h.fusion.len() > first_new {
            out.push(self.encode_splats(LAYER_STATIC, LayerMode::Append, &h.fusion.splats()[first_new..]));
        }
        if config.preview.enabled {
            for (sensor, frame) in latest.iter().enumerate() {
                if let Some(frame) = frame {
                    out.push(self.encode_preview(sensor as u8, frame));
                }
            }
        }
        work.encode_ms += ms_since(encode);
    }

    /// Everything a client that connects now needs, in order: the State, the
    /// whole static layer as `Replace` (background plate splats or the scan
    /// so far), the latest live layer and the latest Stats.
    ///
    /// Take it between two ticks and deliver it before anything a later tick
    /// emits; `Append` messages only make sense on top of it.
    pub fn snapshot(&self) -> Vec<Outgoing> {
        let mut out = self.encode_everything();
        out.push(self.encode_fresh_stats());
        out
    }

    // -- control -----------------------------------------------------------

    /// Applies a control message and returns the messages to broadcast right
    /// away to every client: a new State whenever the configuration may
    /// have changed, `Replace` messages for the layers it invalidated, and a
    /// Stats message after anything that changes the scan or the layers.
    ///
    /// Changing the scene, the rig, the sensor or the reconstruction
    /// parameters of the stage rebuilds the background plates and both
    /// layers. In the house, changing the voxel size, the walking speed, the
    /// frame rate or the sensor head restarts the scan; other changes apply
    /// to the frames still to come. A [`ControlMsg::PatchConfig`] that does
    /// not describe a configuration is ignored.
    pub fn handle_control(&mut self, msg: ControlMsg) -> Vec<Outgoing> {
        self.try_control(msg).unwrap_or_default()
    }

    /// Parses a JSON [`ControlMsg`] and applies it like
    /// [`Pipeline::handle_control`]. Texts longer than [`MAX_CONTROL_JSON`]
    /// and anything that is not a valid message are rejected with a
    /// description; the pipeline is left untouched.
    pub fn handle_control_json(&mut self, text: &str) -> Result<Vec<Outgoing>, String> {
        if text.len() > MAX_CONTROL_JSON {
            return Err(format!("control message of {} bytes exceeds {MAX_CONTROL_JSON}", text.len()));
        }
        let msg: ControlMsg = serde_json::from_str(text).map_err(|e| format!("invalid control message: {e}"))?;
        self.try_control(msg)
    }

    fn try_control(&mut self, msg: ControlMsg) -> Result<Vec<Outgoing>, String> {
        match msg {
            ControlMsg::SetConfig { config } => Ok(self.apply_config(config)),
            ControlMsg::PatchConfig { patch } => {
                if !patch.is_object() {
                    return Err("config patch must be a JSON object".to_string());
                }
                let mut value = serde_json::to_value(self.config).map_err(|e| e.to_string())?;
                merge_json(&mut value, patch);
                let config: PipelineConfig =
                    serde_json::from_value(value).map_err(|e| format!("invalid config patch: {e}"))?;
                Ok(self.apply_config(config))
            }
            ControlMsg::SetScene { scene } => Ok(self.apply_config(PipelineConfig { scene, ..self.config })),
            ControlMsg::SetPaused { paused } => Ok(self.apply_config(PipelineConfig { paused, ..self.config })),
            ControlMsg::SetTimeScale { time_scale } => {
                Ok(self.apply_config(PipelineConfig { time_scale, ..self.config }))
            }
            ControlMsg::Restart => {
                let config = self.config;
                let layer = match &mut self.mode {
                    Mode::Stage(stage) => {
                        stage.time = 0.0;
                        stage.refresh(self.scene.as_ref(), &config, &mut TickWork::default());
                        LAYER_DYNAMIC
                    }
                    Mode::House(house) => {
                        house.reset(if config.scan.autostart { ScanState::Scanning } else { ScanState::Idle });
                        LAYER_STATIC
                    }
                };
                Ok(vec![self.encode_layer(layer), self.encode_fresh_stats()])
            }
            ControlMsg::ScanStart => Ok(self.scan_command(|house| match house.state {
                ScanState::Idle | ScanState::Paused => {
                    house.state = ScanState::Scanning;
                    false
                }
                ScanState::Done => {
                    house.reset(ScanState::Scanning);
                    true
                }
                ScanState::Scanning => false,
            })),
            ControlMsg::ScanPause => Ok(self.scan_command(|house| {
                if house.state != ScanState::Scanning {
                    return false;
                }
                house.state = ScanState::Paused;
                house.finishing = false;
                // A pause is a good moment to show the refined splats.
                let polish = house.needs_polish();
                house.published_refined = house.fusion.refined();
                polish
            })),
            ControlMsg::ScanReset => Ok(self.scan_command(|house| {
                house.reset(ScanState::Idle);
                true
            })),
            ControlMsg::ScanFinish => Ok(self.scan_command(|house| {
                if house.state != ScanState::Done {
                    house.state = ScanState::Scanning;
                    house.finishing = true;
                }
                false
            })),
        }
    }

    /// Runs a scan command on the house (a no-op on the stage). `command`
    /// returns whether layer 0 has to be sent again as a whole.
    fn scan_command(&mut self, command: impl FnOnce(&mut House) -> bool) -> Vec<Outgoing> {
        let Mode::House(house) = &mut self.mode else { return Vec::new() };
        let resend = command(house);
        let mut out = Vec::new();
        if resend {
            out.push(self.encode_layer(LAYER_STATIC));
        }
        out.push(self.encode_fresh_stats());
        out
    }

    fn apply_config(&mut self, config: PipelineConfig) -> Vec<Outgoing> {
        let new = config.sanitized();
        let old = std::mem::replace(&mut self.config, new);
        if new == old {
            return vec![self.encode_state()];
        }
        if new.scene != old.scene {
            self.scene = make_scene(new.scene);
            self.mode = Self::build_mode(self.scene.as_ref(), &new, 0.0);
            let mut out = self.encode_everything();
            out.push(self.encode_fresh_stats());
            return out;
        }
        match &mut self.mode {
            Mode::Stage(stage) => {
                let rebuild = new.rig != old.rig
                    || new.shadows != old.shadows
                    || new.noise != old.noise
                    || new.recon != old.recon
                    || new.background_subtraction != old.background_subtraction;
                if !rebuild {
                    return vec![self.encode_state()];
                }
                let time = stage.time;
                self.mode = Mode::Stage(Stage::new(self.scene.as_ref(), &new, time));
                let mut out = self.encode_everything();
                out.push(self.encode_fresh_stats());
                out
            }
            Mode::House(house) => {
                house.fusion.max_range = new.scan.max_range;
                let restart = new.scan.voxel_size != old.scan.voxel_size
                    || new.scan.speed != old.scan.speed
                    || new.scan.frames_per_second != old.scan.frames_per_second
                    || new.scan.head != old.scan.head;
                if !restart {
                    return vec![self.encode_state()];
                }
                // Keep walking if a scan was under way, from the start.
                let state = match house.state {
                    ScanState::Scanning | ScanState::Done => ScanState::Scanning,
                    ScanState::Paused | ScanState::Idle => ScanState::Idle,
                };
                let mut fresh = House::new(self.scene.as_ref(), &new);
                fresh.state = state;
                self.mode = Mode::House(fresh);
                vec![self.encode_state(), self.encode_layer(LAYER_STATIC), self.encode_fresh_stats()]
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::NoiseModel;
    use crate::codec::Kind;
    use crate::rig::RingRig;

    /// A small, fast stage.
    fn small_stage() -> PipelineConfig {
        PipelineConfig {
            rig: RingRig { count: 4, width: 64, height: 48, ..RingRig::default() },
            ..PipelineConfig::for_scene(SceneKind::Stage)
        }
    }

    /// A small, fast house: about thirty coarse frames for the whole walk.
    fn small_house() -> PipelineConfig {
        let mut config = PipelineConfig::for_scene(SceneKind::House);
        config.scan.width = 64;
        config.scan.height = 48;
        config.scan.voxel_size = 0.1;
        config.scan.speed = 5.0;
        config.scan.frames_per_second = 2.0;
        config.scan.autostart = false;
        config
    }

    fn kinds(messages: &[Outgoing]) -> Vec<Kind> {
        messages.iter().map(|m| codec::peek_kind(&m.bytes).unwrap()).collect()
    }

    fn layer_of(out: &Outgoing) -> codec::LayerMsg {
        match codec::decode(&out.bytes).unwrap().body {
            Body::Layer(layer) => layer,
            other => panic!("expected a layer, got {:?}", other.kind()),
        }
    }

    fn stats_of(messages: &[Outgoing]) -> Stats {
        let last = messages.last().expect("no messages");
        match codec::decode(&last.bytes).unwrap().body {
            Body::Stats(json) => serde_json::from_str(&json).unwrap(),
            other => panic!("the last message must be Stats, got {:?}", other.kind()),
        }
    }

    fn state_of(out: &Outgoing) -> SessionState {
        match codec::decode(&out.bytes).unwrap().body {
            Body::State(json) => serde_json::from_str(&json).unwrap(),
            other => panic!("expected State, got {:?}", other.kind()),
        }
    }

    /// Drives the scan to its end and returns layer 0 as a client holds it.
    fn finish_scan(pipeline: &mut Pipeline, mut layer: Vec<Splat>) -> Vec<Splat> {
        pipeline.handle_control(ControlMsg::ScanFinish);
        for _ in 0..10_000 {
            if pipeline.scan_state() == Some(ScanState::Done) {
                return layer;
            }
            for out in pipeline.tick(1.0 / 30.0) {
                if codec::peek_kind(&out.bytes) == Ok(Kind::Layer) {
                    let msg = layer_of(&out);
                    assert_eq!(msg.layer_id, LAYER_STATIC);
                    assert!(out.reliable);
                    match msg.mode {
                        LayerMode::Replace => layer = msg.splats,
                        LayerMode::Append => layer.extend_from_slice(&msg.splats),
                    }
                }
            }
        }
        panic!("the scan never finished");
    }

    #[test]
    fn pipeline_is_send_and_sync() {
        fn check<T: Send + Sync>() {}
        check::<Pipeline>();
    }

    #[test]
    fn stage_tick_emits_live_layer_preview_and_stats() {
        let mut pipeline = Pipeline::new(small_stage());
        assert!(!pipeline.static_splats().is_empty());
        let out = pipeline.tick(1.0 / 30.0);
        assert_eq!(kinds(&out), [Kind::Layer, Kind::Preview, Kind::Stats]);
        assert_eq!(
            out.iter().map(|m| (m.reliable, m.slot)).collect::<Vec<_>>(),
            [(false, SLOT_DYNAMIC), (false, SLOT_PREVIEW_BASE), (false, SLOT_STATS)]
        );
        let layer = layer_of(&out[0]);
        assert_eq!((layer.layer_id, layer.mode), (LAYER_DYNAMIC, LayerMode::Replace));
        assert_eq!(layer.splats.len(), pipeline.dynamic_splats().len());
        assert!(!layer.splats.is_empty(), "the dancer must produce foreground");
        // Far fewer foreground splats than background ones.
        assert!(layer.splats.len() * 2 < pipeline.static_splats().len());

        let stats = stats_of(&out);
        assert_eq!(stats.tick, 1);
        assert!((stats.scene_time - 1.0 / 30.0).abs() < 1e-4);
        assert_eq!(stats.pixels, 4 * 64 * 48);
        assert_eq!(stats.splats_dynamic as usize, layer.splats.len());
        assert_eq!(stats.splats_static as usize, pipeline.static_splats().len());
        assert_eq!(stats.bytes as usize, out[0].bytes.len() + out[1].bytes.len());
        assert!(stats.total_ms >= stats.capture_ms && stats.capture_ms > 0.0 && stats.reconstruct_ms > 0.0);
        assert!(stats.scan.is_none());

        // Previews go round the ring, one camera per tick.
        let ids: Vec<u8> = (0..5).map(|_| pipeline.tick(0.01)[1].slot - SLOT_PREVIEW_BASE).collect();
        assert_eq!(ids, [1, 2, 3, 0, 1]);
        // The second tick has a measured rate.
        assert!(stats_of(&pipeline.tick(0.01)).tick_hz > 0.0);
    }

    #[test]
    fn sequence_numbers_increase_across_all_messages() {
        let mut pipeline = Pipeline::new(small_stage());
        let mut all = pipeline.snapshot();
        all.extend(pipeline.tick(0.1));
        all.extend(pipeline.handle_control(ControlMsg::SetPaused { paused: true }));
        all.extend(pipeline.snapshot());
        let seqs: Vec<u32> = all.iter().map(|m| codec::peek_header(&m.bytes).unwrap().seq).collect();
        assert!(seqs.windows(2).all(|w| w[1] > w[0]), "{seqs:?}");
    }

    #[test]
    fn snapshot_brings_a_late_client_up_to_date() {
        let mut pipeline = Pipeline::new(small_stage());
        pipeline.tick(0.5);
        let snapshot = pipeline.snapshot();
        assert_eq!(kinds(&snapshot), [Kind::State, Kind::Layer, Kind::Layer, Kind::Stats]);
        assert!(snapshot[0].reliable && snapshot[1].reliable && !snapshot[2].reliable && !snapshot[3].reliable);
        let state = state_of(&snapshot[0]);
        assert_eq!(state, pipeline.state());
        assert_eq!(state.version, PROTOCOL_VERSION);
        assert_eq!(state.scene, "stage");
        assert_eq!(state.cameras.len(), 4);
        assert_eq!(state.loop_duration, Some(9.6));
        assert!(state.scan.is_none() && state.ceiling_height.is_none());
        let (fixed, live) = (layer_of(&snapshot[1]), layer_of(&snapshot[2]));
        assert_eq!((fixed.layer_id, fixed.mode), (LAYER_STATIC, LayerMode::Replace));
        assert_eq!((live.layer_id, live.mode), (LAYER_DYNAMIC, LayerMode::Replace));
        assert_eq!(fixed.splats.len(), pipeline.static_splats().len());
        assert_eq!(live.splats.len(), pipeline.dynamic_splats().len());
        // Taking a snapshot does not disturb the pipeline.
        let stats = stats_of(&snapshot);
        assert_eq!(stats.tick, 1);
        assert!((pipeline.scene_time() - 0.5).abs() < 1e-5);
    }

    #[test]
    fn output_is_deterministic() {
        let run = || {
            let mut pipeline = Pipeline::new(PipelineConfig { noise: NoiseModel::realistic(), ..small_stage() });
            let mut layers = vec![pipeline.snapshot()[1].bytes.clone()];
            for _ in 0..3 {
                layers.push(pipeline.tick(0.2)[0].bytes.clone());
            }
            layers
        };
        assert_eq!(run(), run());
    }

    #[test]
    fn noise_changes_from_tick_to_tick_but_the_plate_is_averaged() {
        let mut pipeline =
            Pipeline::new(PipelineConfig { noise: NoiseModel::realistic(), time_scale: 0.0, ..small_stage() });
        let a = pipeline.tick(0.1)[0].bytes.clone();
        let b = pipeline.tick(0.1)[0].bytes.clone();
        // The scene is frozen, the sensor noise is not.
        assert_ne!(a[codec::HEADER_SIZE..], b[codec::HEADER_SIZE..]);
        let Mode::Stage(stage) = &pipeline.mode else { panic!() };
        assert!(stage.plates.iter().all(|p| p.samples() == PLATE_EXPOSURES));
    }

    #[test]
    fn without_background_subtraction_everything_is_live() {
        let mut pipeline = Pipeline::new(PipelineConfig { background_subtraction: false, ..small_stage() });
        assert!(pipeline.static_splats().is_empty());
        let with = Pipeline::new(small_stage());
        let out = pipeline.tick(0.1);
        let live = layer_of(&out[0]).splats.len();
        assert!(live > with.static_splats().len(), "{live}");
        assert!(layer_of(&pipeline.snapshot()[1]).splats.is_empty());
    }

    #[test]
    fn time_follows_dt_time_scale_and_pause() {
        let mut pipeline = Pipeline::new(PipelineConfig { time_scale: 2.0, ..small_stage() });
        pipeline.tick(0.25);
        assert!((pipeline.scene_time() - 0.5).abs() < 1e-5);
        // The dance loops.
        pipeline.tick(4.8);
        assert!((pipeline.scene_time() - 0.5).abs() < 1e-3);

        let out = pipeline.handle_control(ControlMsg::SetPaused { paused: true });
        assert_eq!(kinds(&out), [Kind::State]);
        assert!(state_of(&out[0]).config.paused);
        let before = pipeline.scene_time();
        let out = pipeline.tick(1.0);
        assert_eq!(kinds(&out), [Kind::Stats], "a paused pipeline only reports");
        assert_eq!(pipeline.scene_time(), before);
        assert_eq!(stats_of(&out).pixels, 0);

        pipeline.handle_control(ControlMsg::SetPaused { paused: false });
        pipeline.handle_control(ControlMsg::SetTimeScale { time_scale: 0.0 });
        assert_eq!(kinds(&pipeline.tick(1.0)), [Kind::Layer, Kind::Preview, Kind::Stats]);
        assert_eq!(pipeline.scene_time(), before);

        let out = pipeline.handle_control(ControlMsg::Restart);
        assert_eq!(kinds(&out), [Kind::Layer, Kind::Stats]);
        assert_eq!(pipeline.scene_time(), 0.0);
    }

    #[test]
    fn odd_time_steps_are_safe() {
        let mut pipeline = Pipeline::new(small_stage());
        for dt in [0.0, -1.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1.0e30, f32::MAX, f32::MIN_POSITIVE] {
            let out = pipeline.tick(dt);
            assert_eq!(codec::peek_kind(&out.last().unwrap().bytes), Ok(Kind::Stats), "{dt}");
            let time = pipeline.scene_time();
            assert!(time.is_finite() && (0.0..9.6).contains(&time), "dt {dt} gave time {time}");
            assert!(!pipeline.dynamic_splats().is_empty(), "{dt}");
        }
        let mut config = small_house();
        config.scan.autostart = true;
        let mut pipeline = Pipeline::new(config);
        for dt in [0.0, -1.0, f32::NAN, f32::INFINITY, 1.0e30] {
            pipeline.tick(dt);
            assert!(pipeline.scene_time().is_finite());
        }
        // A huge step makes the whole path due; it is then worked off.
        let layer = finish_scan(&mut pipeline, Vec::new());
        assert!(!layer.is_empty());
    }

    #[test]
    fn config_changes_rebuild_what_they_invalidate() {
        let mut pipeline = Pipeline::new(small_stage());
        pipeline.tick(1.0);
        // Preview settings do not touch the layers.
        let out = pipeline
            .handle_control_json(r#"{"type":"patch_config","patch":{"preview":{"enabled":false},"target_hz":15}}"#)
            .unwrap();
        assert_eq!(kinds(&out), [Kind::State]);
        assert!(!pipeline.config().preview.enabled);
        assert_eq!(pipeline.config().target_hz, 15.0);
        assert_eq!(pipeline.config().rig.count, 4, "a patch keeps everything else");
        assert_eq!(kinds(&pipeline.tick(0.1)), [Kind::Layer, Kind::Stats]);

        // A new rig: new State with the new cameras, both layers replaced,
        // the dance goes on where it was.
        let before = pipeline.scene_time();
        let out = pipeline.handle_control_json(r#"{"type":"patch_config","patch":{"rig":{"count":5}}}"#).unwrap();
        assert_eq!(kinds(&out), [Kind::State, Kind::Layer, Kind::Layer, Kind::Stats]);
        assert_eq!(state_of(&out[0]).cameras.len(), 5);
        assert_eq!(layer_of(&out[1]).splats.len(), pipeline.static_splats().len());
        assert_eq!(pipeline.scene_time(), before);

        // An identical configuration is acknowledged and nothing else.
        let same = *pipeline.config();
        assert_eq!(kinds(&pipeline.handle_control(ControlMsg::SetConfig { config: same })), [Kind::State]);

        // Hostile values are clamped, not obeyed.
        let mut hostile = same;
        hostile.rig.count = u32::MAX;
        hostile.rig.width = 16;
        hostile.rig.height = 16;
        hostile.target_hz = f32::NAN;
        pipeline.handle_control(ControlMsg::SetConfig { config: hostile });
        assert_eq!(pipeline.config().rig.count, RingRig::MAX_COUNT);
        assert_eq!(pipeline.config().target_hz, PipelineConfig::default().target_hz);
    }

    #[test]
    fn bad_control_json_is_rejected_and_changes_nothing() {
        let mut pipeline = Pipeline::new(small_stage());
        let before = *pipeline.config();
        for text in [
            "",
            "{",
            "null",
            r#"{"type":"nope"}"#,
            r#"{"type":"set_paused"}"#,
            r#"{"type":"set_paused","paused":"yes"}"#,
            r#"{"type":"patch_config","patch":7}"#,
            r#"{"type":"patch_config","patch":{"rig":{"count":"many"}}}"#,
            r#"{"type":"set_scene","scene":"moon"}"#,
        ] {
            assert!(pipeline.handle_control_json(text).is_err(), "{text:?} was accepted");
        }
        let huge = format!(r#"{{"type":"restart","pad":"{}"}}"#, "x".repeat(MAX_CONTROL_JSON));
        assert!(pipeline.handle_control_json(&huge).unwrap_err().contains("exceeds"));
        assert_eq!(*pipeline.config(), before);
        // The typed entry point ignores what the JSON one reports.
        assert!(pipeline.handle_control(ControlMsg::PatchConfig { patch: serde_json::json!(7) }).is_empty());
        // Scan commands mean nothing on the stage.
        assert!(pipeline.handle_control(ControlMsg::ScanStart).is_empty());
        assert!(pipeline.handle_control_json(r#"{"type":"scan_finish"}"#).unwrap().is_empty());
    }

    #[test]
    fn house_scan_state_machine() {
        let mut pipeline = Pipeline::new(small_house());
        assert_eq!(pipeline.scan_state(), Some(ScanState::Idle));
        let state = pipeline.state();
        assert_eq!(state.scene, "house");
        assert!(state.cameras.is_empty());
        assert_eq!(state.ceiling_height, Some(house::CEILING_HEIGHT));
        let scan = state.scan.expect("the house describes its scan path");
        assert_eq!(scan.path.len(), SCAN_PATH_POINTS);
        assert!(scan.duration > 1.0);

        // Idle: nothing happens.
        let out = pipeline.tick(1.0);
        assert_eq!(kinds(&out), [Kind::Stats]);
        let stats = stats_of(&out).scan.unwrap();
        assert_eq!((stats.state, stats.progress), (ScanState::Idle, 0.0));
        assert!(pipeline.static_splats().is_empty());

        // Scanning: appended chunks add up to the map.
        let out = pipeline.handle_control(ControlMsg::ScanStart);
        assert_eq!(kinds(&out), [Kind::Stats]);
        assert_eq!(stats_of(&out).scan.unwrap().state, ScanState::Scanning);
        let mut layer = Vec::new();
        let mut frames = 0;
        for _ in 0..4 {
            let out = pipeline.tick(0.5);
            let stats = stats_of(&out);
            // One or two frames became due, each from another sensor of the
            // head, which fire in order.
            let fired = (stats.pixels / (64 * 48)) as usize;
            assert!(stats.pixels as usize == fired * 64 * 48 && (1..=2).contains(&fired), "{}", stats.pixels);
            assert_eq!(kinds(&out)[0], Kind::Layer);
            assert_eq!(kinds(&out)[1..], [vec![Kind::Preview; fired], vec![Kind::Stats]].concat());
            assert!(out[0].reliable && out[1..].iter().all(|m| !m.reliable));
            let order = ScanHead::Surround.firing_order();
            let mut expected: Vec<u8> = (frames..frames + fired).map(|i| SLOT_PREVIEW_BASE + order[i % order.len()]).collect();
            expected.sort_unstable();
            assert_eq!(out[1..=fired].iter().map(|m| m.slot).collect::<Vec<_>>(), expected);
            frames += fired;
            let chunk = layer_of(&out[0]);
            assert_eq!((chunk.layer_id, chunk.mode), (LAYER_STATIC, LayerMode::Append));
            layer.extend_from_slice(&chunk.splats);
            assert_eq!(stats.splats_static as usize, layer.len());
            assert_eq!(stats.splats_dynamic, 0);
        }
        assert_eq!(layer.len(), pipeline.static_splats().len());
        let progress = stats_of(&pipeline.snapshot()).scan.unwrap().progress;
        assert!(progress > 0.0 && progress < 1.0, "{progress}");

        // A late client gets the whole map so far.
        let snapshot = pipeline.snapshot();
        assert_eq!(kinds(&snapshot), [Kind::State, Kind::Layer, Kind::Layer, Kind::Stats]);
        assert_eq!(layer_of(&snapshot[1]).splats.len(), layer.len());
        assert!(layer_of(&snapshot[2]).splats.is_empty());

        // Paused: time stands still, the map is kept (and sent again whole
        // if splats were refined since they were appended).
        let out = pipeline.handle_control(ControlMsg::ScanPause);
        assert_eq!(stats_of(&out).scan.unwrap().state, ScanState::Paused);
        if out.len() == 2 {
            layer = layer_of(&out[0]).splats;
        }
        assert_eq!(layer.len(), pipeline.static_splats().len());
        let time = pipeline.scene_time();
        assert_eq!(kinds(&pipeline.tick(1.0)), [Kind::Stats]);
        assert_eq!(pipeline.scene_time(), time);

        // Finish: the rest of the path, then `Done` with the polished map.
        let layer = finish_scan(&mut pipeline, layer);
        assert_eq!(pipeline.scan_state(), Some(ScanState::Done));
        assert_eq!(layer.len(), pipeline.static_splats().len());
        let stats = stats_of(&pipeline.tick(1.0)).scan.unwrap();
        assert_eq!((stats.state, stats.progress), (ScanState::Done, 1.0));
        assert_eq!(kinds(&pipeline.tick(1.0)), [Kind::Stats], "a finished scan is quiet");

        // What the client holds is the pipeline's map, up to quantization.
        let truth = pipeline.static_splats();
        let worst = layer.iter().zip(truth).map(|(a, b)| (a.position() - b.position()).length()).fold(0.0, f32::max);
        assert!(worst < 2e-3, "client and pipeline disagree by {worst} m");
        let same_color = layer.iter().zip(truth).filter(|(a, b)| a.rgba == b.rgba).count();
        assert_eq!(same_color, layer.len(), "the final Replace carries the refined colors");

        // Start after Done scans again from scratch; reset goes back to idle.
        let out = pipeline.handle_control(ControlMsg::ScanStart);
        assert_eq!(kinds(&out), [Kind::Layer, Kind::Stats]);
        assert!(layer_of(&out[0]).splats.is_empty());
        assert_eq!(pipeline.scan_state(), Some(ScanState::Scanning));
        pipeline.tick(0.5);
        assert!(!pipeline.static_splats().is_empty());
        let out = pipeline.handle_control(ControlMsg::ScanReset);
        assert_eq!(kinds(&out), [Kind::Layer, Kind::Stats]);
        assert_eq!(pipeline.scan_state(), Some(ScanState::Idle));
        assert!(pipeline.static_splats().is_empty());
        assert_eq!(pipeline.scene_time(), 0.0);
    }

    #[test]
    fn scan_is_tied_to_the_path_not_to_the_tick_rate() {
        let scan = |dt: f32| {
            let mut pipeline = Pipeline::new(small_house());
            pipeline.handle_control(ControlMsg::ScanStart);
            let mut ticks = 0;
            while pipeline.scan_state() == Some(ScanState::Scanning) {
                pipeline.tick(dt);
                ticks += 1;
                assert!(ticks < 100_000);
            }
            pipeline.static_splats().to_vec()
        };
        let (coarse, fine) = (scan(0.7), scan(0.05));
        assert!(!coarse.is_empty());
        assert_eq!(coarse, fine);
        // ScanFinish gives the same map as walking in real time.
        let mut pipeline = Pipeline::new(small_house());
        finish_scan(&mut pipeline, Vec::new());
        assert_eq!(pipeline.static_splats(), &coarse[..]);
    }

    #[test]
    fn scan_progress_never_steps_back() {
        // Coarse ticks, each with several frames due, then `ScanFinish`:
        // whether a tick fuses all its frames or runs out of budget, the
        // reported progress and the path position of the head only advance.
        let mut pipeline = Pipeline::new(small_house());
        pipeline.handle_control(ControlMsg::ScanStart);
        let path = ScanPath::house_default().with_speed(pipeline.config().scan.speed);
        let (mut progress, mut walked) = (0.0f32, 0.0f32);
        let mut check = |stats: Stats| {
            let scan = stats.scan.unwrap();
            assert!(scan.progress >= progress, "{} after {progress}", scan.progress);
            progress = scan.progress;
            // The head stands on the path at the reported progress.
            let at = path.duration() * scan.progress;
            let eye = Vec3::from_slice(&scan.camera.world_from_cam[12..15]);
            assert!((eye - path.position_at(at)).length() < 2e-3, "progress {progress}");
            assert!(path.distance_at(at) >= walked);
            walked = path.distance_at(at);
        };
        for _ in 0..3 {
            check(stats_of(&pipeline.tick(1.3)));
        }
        check(stats_of(&pipeline.handle_control(ControlMsg::ScanFinish)));
        while pipeline.scan_state() == Some(ScanState::Scanning) {
            check(stats_of(&pipeline.tick(1.0 / 30.0)));
        }
        assert_eq!(progress, 1.0);
    }

    #[test]
    fn autostart_and_restart() {
        let mut config = small_house();
        config.scan.autostart = true;
        let mut pipeline = Pipeline::new(config);
        assert_eq!(pipeline.scan_state(), Some(ScanState::Scanning));
        pipeline.tick(1.0);
        assert!(!pipeline.static_splats().is_empty());
        let out = pipeline.handle_control(ControlMsg::Restart);
        assert_eq!(kinds(&out), [Kind::Layer, Kind::Stats]);
        assert!(layer_of(&out[0]).splats.is_empty());
        assert_eq!(pipeline.scan_state(), Some(ScanState::Scanning));
        assert_eq!(pipeline.scene_time(), 0.0);

        // A new voxel size restarts the scan; a new range does not.
        pipeline.tick(1.0);
        let out =
            pipeline.handle_control_json(r#"{"type":"patch_config","patch":{"scan":{"max_range":3.0}}}"#).unwrap();
        assert_eq!(kinds(&out), [Kind::State]);
        assert!(!pipeline.static_splats().is_empty());
        let out =
            pipeline.handle_control_json(r#"{"type":"patch_config","patch":{"scan":{"voxel_size":0.2}}}"#).unwrap();
        assert_eq!(kinds(&out), [Kind::State, Kind::Layer, Kind::Stats]);
        assert!(pipeline.static_splats().is_empty());
        assert_eq!(pipeline.scan_state(), Some(ScanState::Scanning));
        // So does another sensor head; another image size does not.
        pipeline.tick(1.0);
        let out = pipeline.handle_control_json(r#"{"type":"patch_config","patch":{"scan":{"width":48}}}"#).unwrap();
        assert_eq!(kinds(&out), [Kind::State]);
        assert!(!pipeline.static_splats().is_empty());
        let out =
            pipeline.handle_control_json(r#"{"type":"patch_config","patch":{"scan":{"head":"single"}}}"#).unwrap();
        assert_eq!(kinds(&out), [Kind::State, Kind::Layer, Kind::Stats]);
        assert_eq!(state_of(&out[0]).config.scan.head, ScanHead::Single);
        assert!(pipeline.static_splats().is_empty());
        assert!(pipeline.handle_control_json(r#"{"type":"patch_config","patch":{"scan":{"head":"fisheye"}}}"#).is_err());
        assert_eq!(pipeline.config().scan.head, ScanHead::Single);
    }

    #[test]
    fn switching_scenes_mid_scan_is_safe() {
        let mut config = small_house();
        config.rig = small_stage().rig;
        config.scan.autostart = true;
        let mut pipeline = Pipeline::new(config);
        pipeline.tick(1.0);
        pipeline.handle_control(ControlMsg::ScanFinish);
        pipeline.tick(0.1);

        let out = pipeline.handle_control(ControlMsg::SetScene { scene: SceneKind::Stage });
        assert_eq!(kinds(&out), [Kind::State, Kind::Layer, Kind::Layer, Kind::Stats]);
        assert_eq!(state_of(&out[0]).scene, "stage");
        assert_eq!(pipeline.scan_state(), None);
        assert!(stats_of(&out).scan.is_none());
        assert!(!layer_of(&out[2]).splats.is_empty());
        assert_eq!(kinds(&pipeline.tick(0.1)), [Kind::Layer, Kind::Preview, Kind::Stats]);

        // And back: a fresh scan, the stage layers are replaced by empty ones.
        let out = pipeline.handle_control_json(r#"{"type":"set_scene","scene":"house"}"#).unwrap();
        assert_eq!(kinds(&out), [Kind::State, Kind::Layer, Kind::Layer, Kind::Stats]);
        assert!(layer_of(&out[1]).splats.is_empty() && layer_of(&out[2]).splats.is_empty());
        assert_eq!(pipeline.scan_state(), Some(ScanState::Scanning));
        assert_eq!(pipeline.scene_time(), 0.0);
        let layer = finish_scan(&mut pipeline, Vec::new());
        assert_eq!(layer.len(), pipeline.static_splats().len());
    }

    #[test]
    fn scan_head_faces_the_walk_and_its_sensors_look_all_around() {
        let path = ScanPath::house_default();
        let scan = ScanConfig::default();
        assert_eq!(scan.head, ScanHead::Surround);
        let mounts = scan.head.mounts();
        // The head follows the heading averaged over a second: up to 2.4
        // degrees off the momentary one on this tour, on top of the sway.
        let sway = (SCAN_PAN_AMPLITUDE + SCAN_TILT_AMPLITUDE + 3.0f32.to_radians()).cos();
        let steps = 2000;
        for i in 0..=steps {
            let t = path.duration() * i as f32 / steps as f32;
            let main = scan_camera(&path, t, &scan);
            assert_eq!(main, scan_sensor(&path, t, &scan, 0));
            let heading = path.heading_at(t);
            assert!(main.forward().dot(heading) > sway, "the head looks away from the walk at t = {t}");
            assert!(Vec3::from(main.world_from_cam.matrix3.x_axis).y.abs() < 1e-4, "level horizon at t = {t}");
            let (left, right, up, down) = (
                scan_sensor(&path, t, &scan, 1).forward(),
                scan_sensor(&path, t, &scan, 2).forward(),
                scan_sensor(&path, t, &scan, 3).forward(),
                scan_sensor(&path, t, &scan, 4).forward(),
            );
            // `heading x up` points to the walker's right. The side sensors
            // look 30 degrees back from straight sideways.
            let to_right = heading.cross(Vec3::Y);
            let (back_left, back_right) = (-0.866 * to_right - 0.5 * heading, 0.866 * to_right - 0.5 * heading);
            assert!(left.dot(back_left) > sway && right.dot(back_right) > sway, "side sensors at t = {t}");
            assert!(up.y > 0.8 && down.y < -0.8 && up.dot(heading) > 0.3 && down.dot(heading) > 0.3, "t = {t}");
            for (id, mount) in mounts.iter().enumerate() {
                let cam = scan_sensor(&path, t, &scan, id as u32);
                assert!((cam.position() - path.position_at(t)).length() < 1e-4, "sensor {id} rides on the path");
                assert_eq!((cam.width, cam.height), (scan.width, scan.height));
                assert!((cam.fov_y().to_degrees() - scan.fov_y_deg).abs() < 1e-2);
                // Rigidly mounted: always at the same angle to the main sensor.
                let angle = cam.forward().angle_between(main.forward()).to_degrees();
                let expected = (mount.yaw_deg.to_radians().cos() * mount.pitch_deg.to_radians().cos()).acos().to_degrees();
                assert!((angle - expected).abs() < 0.05, "sensor {id} at {angle} degrees instead of {expected}");
            }
        }
        // Any time and any sensor index is safe.
        for t in [f32::NAN, f32::INFINITY, -5.0, 1.0e30] {
            for sensor in [0, 4, 5, u32::MAX] {
                let cam = scan_sensor(&path, t, &scan, sensor);
                assert!(cam.position().is_finite() && cam.forward().is_finite(), "{t} {sensor}");
            }
        }
        let (odd, start) = (scan_camera(&path, f32::NAN, &scan), scan_camera(&path, 0.0, &scan));
        assert!((odd.forward() - start.forward()).length() < 1e-5 && odd.position() == start.position());
        assert_eq!(scan_sensor(&path, 3.0, &scan, 99), scan_sensor(&path, 3.0, &scan, 0));
    }

    #[test]
    fn scan_frames_follow_the_firing_order() {
        let scan = ScanConfig::default();
        let path = ScanPath::house_default().with_speed(scan.speed);
        let count = scan_frame_count(&path, &scan);
        assert_eq!(count, (path.duration() * scan.frames_per_second).ceil() as u64 + 1);
        let order = scan.head.firing_order();
        let mut per_sensor = [0u32; ScanHead::MAX_SENSORS];
        for i in 0..count {
            let frame = scan_frame(&path, &scan, i);
            assert_eq!(frame.sensor, order[i as usize % order.len()] as u32);
            let expected = (i as f32 / scan.frames_per_second).min(path.duration());
            assert!((frame.time - expected).abs() < 1e-4, "frame {i} at {}", frame.time);
            per_sensor[frame.sensor as usize] += 1;
        }
        assert_eq!(scan_frame(&path, &scan, count - 1).time, path.duration());
        assert_eq!(scan_frame(&path, &scan, u64::MAX).time, path.duration());
        // The forward sensor takes every other frame, the others share the rest.
        assert!(per_sensor[0].abs_diff(count as u32 / 2) <= 1, "{per_sensor:?}");
        assert!(per_sensor[1..].iter().all(|&n| n.abs_diff(count as u32 / 8) <= 1), "{per_sensor:?}");
        for head in [ScanHead::Single, ScanHead::Surround] {
            assert_eq!(head.mounts().len(), head.sensor_count() as usize);
            assert!(head.mounts().len() <= ScanHead::MAX_SENSORS);
            assert_eq!((head.mounts()[0].yaw_deg, head.mounts()[0].pitch_deg), (0.0, 0.0), "sensor 0 looks ahead");
            let order = head.firing_order();
            assert!((0..head.sensor_count() as u8).all(|id| order.contains(&id)), "{head:?}: a sensor never fires");
            assert!(order.iter().all(|&id| (id as u32) < head.sensor_count()));
        }
        let single = ScanConfig { head: ScanHead::Single, ..scan };
        assert!((0..40).all(|i| scan_frame(&path, &single, i).sensor == 0));
    }

    #[test]
    fn every_sensor_of_the_head_is_reported_and_previewed() {
        let mut pipeline = Pipeline::new(small_house());
        pipeline.handle_control(ControlMsg::ScanStart);
        let config = *pipeline.config();
        let path = ScanPath::house_default().with_speed(config.scan.speed);
        let mut previewed = [0u32; ScanHead::MAX_SENSORS];
        while pipeline.scan_state() == Some(ScanState::Scanning) {
            let out = pipeline.tick(0.1);
            for message in out.iter().filter(|m| codec::peek_kind(&m.bytes) == Ok(Kind::Preview)) {
                let Body::Preview(preview) = codec::decode(&message.bytes).unwrap().body else { panic!() };
                assert_eq!(message.slot, SLOT_PREVIEW_BASE + preview.camera_id);
                assert_eq!((preview.width, preview.height), (64, 48));
                previewed[preview.camera_id as usize] += 1;
            }
            // The Stats carry the pose of every sensor at the current
            // walking time: the main one first, as before.
            let scan = stats_of(&out).scan.unwrap();
            let time = pipeline.scene_time();
            assert_eq!(scan.camera, CameraInfo::from_camera(0, &scan_camera(&path, time, &config.scan)));
            assert_eq!(scan.extra_cameras.len(), 4);
            for (k, info) in scan.extra_cameras.iter().enumerate() {
                let id = k as u32 + 1;
                assert_eq!(*info, CameraInfo::from_camera(id, &scan_sensor(&path, time, &config.scan, id)));
            }
        }
        let frames = scan_frame_count(&path, &config.scan) as u32;
        assert!(previewed[0].abs_diff(frames / 2) <= 1 && previewed[1..].iter().all(|&n| n >= frames / 8), "{previewed:?}");
        assert_eq!(previewed.iter().sum::<u32>(), frames, "one preview per sensor frame at this tick rate");

        // A single sensor: one preview stream, no extra cameras on the wire.
        let mut config = small_house();
        config.scan.head = ScanHead::Single;
        let mut pipeline = Pipeline::new(config);
        pipeline.handle_control(ControlMsg::ScanStart);
        for _ in 0..5 {
            let out = pipeline.tick(0.5);
            assert_eq!(kinds(&out), [Kind::Layer, Kind::Preview, Kind::Stats]);
            assert_eq!(out[1].slot, SLOT_PREVIEW_BASE);
            assert!(stats_of(&out).scan.unwrap().extra_cameras.is_empty());
            let Body::Stats(json) = codec::decode(&out[2].bytes).unwrap().body else { panic!() };
            assert!(!json.contains("extra_cameras"), "{json}");
        }
    }

    #[test]
    fn scan_head_starts_and_stops_turning_gently() {
        // The heading of the path has a kink: it freezes all at once when
        // the walker is one look-ahead (0.9 m) short of the end, a good
        // second before the walk is over, and the turning rate drops from
        // about 9 degrees per second to nothing. The head must not pass
        // that jolt on, nor any other: its angular velocity never changes
        // by more than 60 degrees per second in a second (measured: 43, and
        // 15 in the last three seconds; 612 before the head averaged the
        // heading), sampled far finer than any tick. The last tenth of a
        // second is left out: there the walk itself stops dead.
        let path = ScanPath::house_default();
        let scan = ScanConfig::default();
        let end = path.duration();
        let dt = 1.0 / 240.0;
        let velocity = |t: f32| {
            let (a, b) = (scan_sensor(&path, t, &scan, 0), scan_sensor(&path, t + dt, &scan, 0));
            let facing = |cam: &Camera| Quat::from_mat3a(&cam.world_from_cam.matrix3);
            let turn = facing(&b) * facing(&a).conjugate();
            // `turn` and `-turn` are the same rotation; take the short way.
            (turn * turn.w.signum()).to_scaled_axis() / dt
        };
        let (mut worst, mut at, mut near_end) = (0.0f32, 0.0f32, 0.0f32);
        let mut before = velocity(0.0);
        for i in 1..((end - 0.1) / dt) as usize {
            let t = i as f32 * dt;
            let now = velocity(t);
            let change = (now - before).length().to_degrees() / dt;
            if change > worst {
                (worst, at) = (change, t);
            }
            if t > end - 3.0 {
                near_end = near_end.max(change);
            }
            before = now;
        }
        println!("scan head: angular acceleration at most {worst:.1} deg/s2 (t = {at:.2} s), {near_end:.1} in the last 3 s");
        assert!(worst <= 60.0, "the head jerks by {worst} deg/s2 at t = {at}");
        assert!(near_end <= 30.0, "the head jerks by {near_end} deg/s2 near the end of the walk");

        // It still faces the walk: never more than 3 degrees of yaw off the
        // momentary heading plus the sway.
        for i in 0..=2000 {
            let t = end * i as f32 / 2000.0;
            let forward = scan_camera(&path, t, &scan).forward();
            let level = Vec3::new(forward.x, 0.0, forward.z).normalize();
            let off = level.angle_between(path.heading_at(t)).to_degrees();
            assert!(off <= SCAN_PAN_AMPLITUDE.to_degrees() + 3.0, "{off} degrees off the walk at t = {t}");
        }
        // A path too short to average over is no problem.
        let short = ScanPath::new(vec![Vec3::new(0.0, 1.4, 0.0), Vec3::new(0.05, 1.4, 0.0)], 0.8);
        assert!(short.duration() < 0.5 * SCAN_HEADING_WINDOW);
        for t in [0.0, 0.03, short.duration(), 5.0] {
            let cam = scan_camera(&short, t, &scan);
            assert!(cam.forward().is_finite() && cam.forward().x > 0.9, "{:?}", cam.forward());
        }
    }

    #[test]
    fn scan_frames_are_bounded_whatever_the_frame_rate() {
        // These functions are public and may be given a configuration that
        // never went through a pipeline.
        let path = ScanPath::house_default();
        let most = (path.duration() * 60.0).ceil() as u64 + 1;
        for rate in [0.0, -3.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 1.0e30, 1.0e-30, f32::MAX] {
            let scan = ScanConfig { frames_per_second: rate, ..ScanConfig::default() };
            let count = scan_frame_count(&path, &scan);
            assert!((2..=most).contains(&count), "{rate} frames per second: {count} frames");
            // What the pipeline would do with it.
            assert_eq!(count, scan_frame_count(&path, &scan.sanitized()), "{rate}");
            assert_eq!(scan_frame(&path, &scan, 0).time, 0.0, "{rate}");
            assert_eq!(scan_frame(&path, &scan, count - 1).time, path.duration(), "{rate}");
            let mut before = 0.0;
            for index in 0..count {
                let frame = scan_frame(&path, &scan, index);
                assert_eq!(frame, scan_frame(&path, &scan.sanitized(), index));
                assert!(frame.time >= before && frame.time <= path.duration() && frame.sensor < 5, "{rate}: {frame:?}");
                before = frame.time;
            }
        }
        // Odd optics are the caller's business, but never a panic.
        let odd = ScanConfig { fov_y_deg: f32::NAN, width: 0, height: 0, ..ScanConfig::default() };
        let cam = scan_sensor(&path, 1.0, &odd, 3);
        assert!(cam.position().is_finite() && cam.forward().is_finite());
    }

    #[test]
    fn extreme_scan_settings_run_to_the_end() {
        // The corners of what `sanitized` lets through, with the five-sensor
        // head: the scan must end, stay within its memory bound and report
        // finite poses all the way.
        #[rustfmt::skip]
        let corners = [
            // voxel, speed, fov, range, frames per second
            (1.0e-9, 1.0e9, 1.0e9, 1.0e-9, 1.0e9),
            (1.0e9, 1.0e-9, 1.0e-9, 1.0e9, 1.0e-9),
            (1.0e9, 1.0e9, 179.9, 1.0e9, 1.0e9),
        ];
        for (voxel_size, speed, fov_y_deg, max_range, frames_per_second) in corners {
            let mut config = PipelineConfig::for_scene(SceneKind::House);
            config.scan =
                ScanConfig { voxel_size, speed, fov_y_deg, max_range, frames_per_second, width: 1, height: 1, ..config.scan };
            let mut pipeline = Pipeline::new(config);
            let scan = pipeline.config().scan;
            assert_eq!((scan.width, scan.height, scan.head), (16, 16, ScanHead::Surround));
            let frames = scan_frame_count(&ScanPath::house_default().with_speed(scan.speed), &scan);
            assert!(frames < 30_000, "{frames} frames");
            let mut client_splats = 0;
            let mut ticks = 0u64;
            pipeline.handle_control(ControlMsg::ScanFinish);
            while pipeline.scan_state() == Some(ScanState::Scanning) {
                let out = pipeline.tick(0.05);
                let stats = stats_of(&out);
                let head = stats.scan.unwrap();
                assert!(head.progress.is_finite() && head.extra_cameras.len() == 4);
                for camera in std::iter::once(&head.camera).chain(head.extra_cameras.iter()) {
                    assert!(camera.world_from_cam.iter().all(|v| v.is_finite()) && camera.fov_y > 0.0, "{camera:?}");
                }
                assert!(preview_ids(&out).len() <= ScanHead::MAX_SENSORS);
                client_splats = stats.splats_static;
                ticks += 1;
                assert!(ticks <= frames + 1, "the scan never finishes");
            }
            assert_eq!(client_splats as usize, pipeline.static_splats().len());
            assert!(pipeline.static_splats().len() <= VoxelFusion::DEFAULT_MAX_VOXELS);
        }
    }

    /// The camera ids of the previews among `messages`.
    fn preview_ids(messages: &[Outgoing]) -> Vec<u8> {
        messages
            .iter()
            .filter(|m| codec::peek_kind(&m.bytes) == Ok(Kind::Preview))
            .map(|m| m.slot - SLOT_PREVIEW_BASE)
            .collect()
    }

    #[test]
    fn interruptions_in_the_middle_of_a_multi_sensor_scan_change_nothing() {
        let walk = |pipeline: &mut Pipeline| {
            let mut ticks = 0;
            while pipeline.scan_state() == Some(ScanState::Scanning) {
                pipeline.tick(0.1);
                ticks += 1;
                assert!(ticks < 10_000, "the scan never finishes");
            }
            pipeline.static_splats().to_vec()
        };
        let mut pipeline = Pipeline::new(small_house());
        assert_eq!(pipeline.config().scan.head, ScanHead::Surround);
        pipeline.handle_control(ControlMsg::ScanStart);
        let reference = walk(&mut pipeline);
        assert!(!reference.is_empty());
        let order = ScanHead::Surround.firing_order();

        // Pause, global pause and resume, at a point where the next sensor
        // to fire is not the first of the order.
        let mut pipeline = Pipeline::new(small_house());
        pipeline.handle_control(ControlMsg::ScanStart);
        let mut ids = Vec::new();
        for _ in 0..7 {
            ids.extend(preview_ids(&pipeline.tick(0.25)));
        }
        let before_pause = ids.len();
        assert!(before_pause % order.len() != 0, "{ids:?}");
        let (time, splats) = (pipeline.scene_time(), pipeline.static_splats().len());
        let poses = stats_of(&pipeline.snapshot()).scan.unwrap();
        pipeline.handle_control(ControlMsg::ScanPause);
        for _ in 0..3 {
            let out = pipeline.tick(0.5);
            assert_eq!(kinds(&out), [Kind::Stats], "a paused scan captures nothing");
            let scan = stats_of(&out).scan.unwrap();
            // The head, all five sensors of it, waits where it was.
            assert_eq!(scan.state, ScanState::Paused);
            assert_eq!((scan.camera, scan.extra_cameras), (poses.camera, poses.extra_cameras));
        }
        pipeline.handle_control(ControlMsg::ScanStart);
        pipeline.handle_control(ControlMsg::SetPaused { paused: true });
        assert_eq!(kinds(&pipeline.tick(0.5)), [Kind::Stats]);
        assert_eq!((pipeline.scene_time(), pipeline.static_splats().len()), (time, splats));
        pipeline.handle_control(ControlMsg::SetPaused { paused: false });
        // Switching the previews off and on drops previews, not frames.
        pipeline.handle_control_json(r#"{"type":"patch_config","patch":{"preview":{"enabled":false}}}"#).unwrap();
        let mut unseen = 0;
        for _ in 0..4 {
            let out = pipeline.tick(0.25);
            assert!(preview_ids(&out).is_empty());
            unseen += (stats_of(&out).pixels / (64 * 48)) as usize;
        }
        assert!(unseen >= 1);
        pipeline.handle_control_json(r#"{"type":"patch_config","patch":{"preview":{"enabled":true}}}"#).unwrap();
        for _ in 0..12 {
            ids.extend(preview_ids(&pipeline.tick(0.25)));
        }
        // The sensors went on firing in their order, as if nothing happened.
        let fired = ids.len() + unseen;
        assert!(fired > order.len(), "{ids:?}");
        let expected: Vec<u8> = (0..fired).map(|i| order[i % order.len()]).collect();
        assert_eq!(ids[..before_pause], expected[..before_pause]);
        assert_eq!(ids[before_pause..], expected[before_pause + unseen..]);
        // Finish from the middle: the same map as the uninterrupted walk.
        pipeline.handle_control(ControlMsg::ScanFinish);
        assert!(walk(&mut pipeline) == reference, "pausing and finishing changed the map");

        // Reset in the middle: back to an empty map and to the first sensor.
        let mut pipeline = Pipeline::new(small_house());
        pipeline.handle_control(ControlMsg::ScanStart);
        for _ in 0..9 {
            pipeline.tick(0.1);
        }
        let out = pipeline.handle_control(ControlMsg::ScanReset);
        assert!(layer_of(&out[0]).splats.is_empty());
        let scan = stats_of(&out).scan.unwrap();
        assert_eq!((scan.state, scan.progress, scan.extra_cameras.len()), (ScanState::Idle, 0.0, 4));
        pipeline.handle_control(ControlMsg::ScanStart);
        assert_eq!(preview_ids(&pipeline.tick(0.1)), [order[0]]);
        assert!(walk(&mut pipeline) == reference, "a reset left something behind");

        // To the stage and back in the middle, then another head and back.
        let mut config = small_house();
        config.rig = small_stage().rig;
        let mut pipeline = Pipeline::new(config);
        pipeline.handle_control(ControlMsg::ScanStart);
        for _ in 0..9 {
            pipeline.tick(0.1);
        }
        pipeline.handle_control(ControlMsg::SetScene { scene: SceneKind::Stage });
        assert_eq!(preview_ids(&pipeline.tick(0.1)), [0], "the stage previews its ring cameras again");
        pipeline.handle_control(ControlMsg::SetScene { scene: SceneKind::House });
        assert_eq!(pipeline.scan_state(), Some(ScanState::Idle), "autostart is off");
        pipeline.handle_control(ControlMsg::ScanStart);
        for _ in 0..9 {
            pipeline.tick(0.1);
        }
        let head = |name: &str| format!(r#"{{"type":"patch_config","patch":{{"scan":{{"head":"{name}"}}}}}}"#);
        let out = pipeline.handle_control_json(&head("single")).unwrap();
        assert!(stats_of(&out).scan.unwrap().extra_cameras.is_empty());
        assert_eq!(pipeline.scan_state(), Some(ScanState::Scanning), "a new head starts the walk over");
        let mut single_ids = Vec::new();
        for _ in 0..9 {
            single_ids.extend(preview_ids(&pipeline.tick(0.1)));
        }
        assert!(!single_ids.is_empty() && single_ids.iter().all(|&id| id == 0), "{single_ids:?}");
        let out = pipeline.handle_control_json(&head("surround")).unwrap();
        assert!(layer_of(&out[1]).splats.is_empty());
        assert_eq!(stats_of(&out).scan.unwrap().extra_cameras.len(), 4);
        assert!(walk(&mut pipeline) == reference, "the scene or head switch left something behind");
    }

    #[test]
    fn hostile_head_settings_are_rejected_and_change_nothing() {
        let mut pipeline = Pipeline::new(small_house());
        pipeline.handle_control(ControlMsg::ScanStart);
        pipeline.tick(0.5);
        let (config, splats) = (*pipeline.config(), pipeline.static_splats().len());
        #[rustfmt::skip]
        let heads = [
            "null", "0", "5", "1000", "-1", "1e400", "true", r#""""#, r#""Surround""#, r#""quad""#, "[]", "{}",
            r#"["surround"]"#, r#"{"sensors":1000}"#,
        ];
        for head in heads {
            let patch = format!(r#"{{"type":"patch_config","patch":{{"scan":{{"head":{head}}}}}}}"#);
            assert!(pipeline.handle_control_json(&patch).is_err(), "head {head} was accepted");
            let set = format!(r#"{{"type":"set_config","config":{{"scene":"house","scan":{{"head":{head}}}}}}}"#);
            assert!(pipeline.handle_control_json(&set).is_err(), "head {head} was accepted");
        }
        assert_eq!((*pipeline.config(), pipeline.static_splats().len()), (config, splats));
        assert_eq!(pipeline.scan_state(), Some(ScanState::Scanning));
        // A client written before the head existed sends none: the scan
        // keeps its head and goes on.
        let old = r#"{"type":"patch_config","patch":{"scan":{"max_range":4.0},"preview":{"max_width":64}}}"#;
        assert_eq!(kinds(&pipeline.handle_control_json(old).unwrap()), [Kind::State]);
        assert_eq!((pipeline.config().scan.head, pipeline.static_splats().len()), (ScanHead::Surround, splats));
        // With `set_config` the missing field takes its default, like every
        // other one.
        let mut single = config;
        single.scan.head = ScanHead::Single;
        pipeline.handle_control(ControlMsg::SetConfig { config: single });
        let old = r#"{"type":"set_config","config":{"scene":"house","scan":{"width":64,"height":48,"voxel_size":0.1}}}"#;
        pipeline.handle_control_json(old).unwrap();
        assert_eq!(pipeline.config().scan.head, ScanHead::Surround);
    }

    #[test]
    fn downsample_keeps_small_frames_and_shrinks_large_ones() {
        let mut frame = RgbdFrame::new(160, 120);
        for (i, d) in frame.depth.iter_mut().enumerate() {
            *d = 1.0 + i as f32;
        }
        assert_eq!(downsample(&frame, 160), frame);
        let half = downsample(&frame, 96);
        assert_eq!((half.width, half.height), (80, 60));
        assert_eq!(half.depth[0], frame.depth_at(1, 1));
        let tiny = downsample(&frame, 1);
        assert_eq!((tiny.width, tiny.height), (1, 1));
        assert_eq!(downsample(&RgbdFrame::new(0, 0), 96), RgbdFrame::new(0, 0));
    }

    #[test]
    fn merge_json_patches_nested_objects() {
        let mut target = serde_json::json!({"a": {"b": 1, "c": 2}, "d": 3});
        merge_json(&mut target, serde_json::json!({"a": {"b": 5}, "e": [1]}));
        assert_eq!(target, serde_json::json!({"a": {"b": 5, "c": 2}, "d": 3, "e": [1]}));
    }
}
