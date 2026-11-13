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
//! | [`SLOT_PREVIEW_BASE`]` + id` (16…) | Preview of camera `id` (the handheld camera is 0) |
//!
//! A client that connects late needs [`Pipeline::snapshot`], taken between
//! two ticks, followed by everything later ticks emit.

use std::sync::atomic::{AtomicU32, Ordering};

use glam::{Quat, Vec3};
#[cfg(feature = "par")]
use rayon::prelude::*;
use web_time::Instant;

use crate::capture::{render_rgbd, CaptureOptions};
use crate::codec::{self, Body, LayerMode, Message};
use crate::protocol::{
    CameraInfo, ControlMsg, PipelineConfig, ScanConfig, ScanInfo, ScanState, ScanStats, SessionState, Stats,
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
    /// Index of the next frame to capture; frame `i` is taken at walking
    /// time `i / frames_per_second` (the last one at the end of the path).
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
fn capture_ring(snap: &SceneSnapshot, cameras: &[Camera], config: &PipelineConfig, exposure: u32) -> Vec<RgbdFrame> {
    let shoot = |(i, cam): (usize, &Camera)| {
        // Distinct noise per camera and per exposure.
        let seed = exposure.wrapping_mul(0x0001_0001).wrapping_add(i as u32).wrapping_mul(0x9E37_79B1);
        render_rgbd(snap, cam, &capture_options(config, seed))
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

/// Peak yaw of the handheld camera away from the walking direction, radians
/// (126 degrees: the operator also looks back over each shoulder).
pub const SCAN_PAN_AMPLITUDE: f32 = 2.2;
/// Period of the yaw sweep in seconds of walking time.
pub const SCAN_PAN_PERIOD: f32 = 7.3;
/// Peak pitch of the handheld camera above and below the horizon, radians.
pub const SCAN_TILT_AMPLITUDE: f32 = 1.0;
/// Period of the pitch sweep in seconds of walking time.
pub const SCAN_TILT_PERIOD: f32 = 2.3;
/// Phase of the pitch sweep at the start of the walk, radians.
const SCAN_TILT_PHASE: f32 = 0.6;

/// Pose of the handheld scan camera at walking time `time` (seconds, clamped
/// to the path).
///
/// The camera rides along `path` at eye height and sweeps like someone
/// painting the rooms with a depth camera: a slow yaw sweep of
/// [`SCAN_PAN_AMPLITUDE`] around the walking direction combined with a
/// faster nod of [`SCAN_TILT_AMPLITUDE`] from floor to ceiling. The two
/// periods are incommensurate, so over a few meters of walking the view
/// directions cover nearly the whole sphere.
///
/// This replaces the gentler sweep of [`ScanPath::camera_at`], which looks
/// mostly ahead: with it 2.3 % of the surfaces a visitor of the finished
/// scan can see were never in view (ceilings behind beams, walls beside the
/// doors one walks through), against 0.5 % with this pattern (measured with
/// rays from random viewpoints in the house; held-out PSNR +1.2 dB).
pub fn scan_camera(path: &ScanPath, time: f32, scan: &ScanConfig) -> Camera {
    use std::f32::consts::TAU;
    let t = if time.is_finite() { time.clamp(0.0, path.duration()) } else { 0.0 };
    let heading = path.heading_at(t);
    let pan = SCAN_PAN_AMPLITUDE * (TAU * t / SCAN_PAN_PERIOD).sin();
    let tilt = SCAN_TILT_AMPLITUDE * (TAU * t / SCAN_TILT_PERIOD + SCAN_TILT_PHASE).sin();
    let yaw = heading.x.atan2(heading.z) + pan;
    // Forward for yaw 0 / tilt 0 is +Z; positive tilt looks up. The tilt
    // stays well short of the vertical, so the horizon is always level.
    let forward = Quat::from_rotation_y(yaw) * Quat::from_rotation_x(-tilt) * Vec3::Z;
    let eye = path.position_at(t);
    Camera::look_at(eye, eye + forward, Vec3::Y, scan.fov_y_deg.to_radians(), scan.width, scan.height)
}

impl Stage {
    /// Captures the empty stage, builds the plates and the static layer, and
    /// reconstructs the live layer at `time`.
    fn new(scene: &dyn Scene, config: &PipelineConfig, time: f64) -> Self {
        let mut stage = Self {
            cameras: config.rig.cameras(),
            plates: Vec::new(),
            static_splats: Vec::new(),
            dynamic_splats: Vec::new(),
            frames: Vec::new(),
            time,
            exposures: 0,
            preview_cursor: 0,
        };
        if config.background_subtraction {
            let empty = scene.snapshot_static();
            // A noisy sensor gets a plate averaged over several exposures.
            let exposures = if config.noise.is_none() { 1 } else { PLATE_EXPOSURES };
            let mut per_camera: Vec<Vec<RgbdFrame>> = stage.cameras.iter().map(|_| Vec::new()).collect();
            for _ in 0..exposures {
                let frames = capture_ring(&empty, &stage.cameras, config, stage.exposures);
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
        self.frames = capture_ring(&snap, &self.cameras, config, self.exposures);
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
        (self.duration() * config.scan.frames_per_second as f64).ceil() as u64
    }

    /// Walking time of frame `index`.
    fn frame_time(&self, index: u64, config: &PipelineConfig) -> f64 {
        (index as f64 / config.scan.frames_per_second as f64).min(self.duration())
    }

    /// Number of frames that are due at the current walking time.
    fn frames_due(&self, config: &PipelineConfig) -> u64 {
        if self.time >= self.duration() {
            self.last_frame(config) + 1
        } else {
            (self.time * config.scan.frames_per_second as f64).floor() as u64 + 1
        }
    }

    /// Walking time up to which the path has been fused.
    fn cursor(&self, config: &PipelineConfig) -> f64 {
        if self.next_frame >= self.frames_due(config) {
            self.time
        } else {
            self.frame_time(self.next_frame.saturating_sub(1), config)
        }
    }

    fn camera(&self, time: f64, config: &PipelineConfig) -> Camera {
        scan_camera(&self.path, time as f32, &config.scan)
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
        ScanStats { state: self.state, progress, camera: CameraInfo::from_camera(0, &self.camera(cursor, config)) }
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
    /// follow in round-robin order. House: the handheld camera walks on, the
    /// frames that became due are fused (as many as fit into
    /// [`SCAN_TICK_BUDGET_MS`], at least one) and their new splats are
    /// appended to layer 0. A Stats message is emitted on every tick, paused
    /// or not, always last.
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
        let mut last_frame = None;
        while h.next_frame < due {
            let frame_start = Instant::now();
            let time = h.frame_time(h.next_frame, &config);
            let cam = h.camera(time, &config);
            let frame = render_rgbd(&h.snapshot, &cam, &capture_options(&config, h.next_frame as u32));
            let captured = ms_since(frame_start);
            work.capture_ms += captured;
            work.pixels += frame.width as u64 * frame.height as u64;
            h.fusion.integrate(&frame, &cam, &config.recon);
            let spent = ms_since(frame_start);
            work.reconstruct_ms += spent - captured;
            h.frame_ms = if h.frame_ms > 0.0 { 0.7 * h.frame_ms + 0.3 * spent } else { spent };
            h.next_frame += 1;
            last_frame = Some(frame);
            if ms_since(start) + h.frame_ms > SCAN_TICK_BUDGET_MS {
                break;
            }
        }

        // The walk ends with a tick of its own that does no capture work:
        // it publishes the splats that were rewritten from better views
        // since they were appended, and announces `Done`.
        let walked = h.next_frame > h.last_frame(&config);
        let finished = walked && last_frame.is_none();
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
        if let (true, Some(frame)) = (config.preview.enabled, &last_frame) {
            out.push(self.encode_preview(0, frame));
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
    /// layers. In the house, changing the voxel size, the walking speed or
    /// the frame rate restarts the scan; other changes apply to the frames
    /// still to come. A [`ControlMsg::PatchConfig`] that does not describe a
    /// configuration is ignored.
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
                    || new.scan.frames_per_second != old.scan.frames_per_second;
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

    /// A small, fast house: about twenty coarse frames for the whole walk.
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
        for _ in 0..4 {
            let out = pipeline.tick(0.5);
            assert_eq!(kinds(&out), [Kind::Layer, Kind::Preview, Kind::Stats]);
            assert!(out[0].reliable && !out[1].reliable);
            assert_eq!(out[1].slot, SLOT_PREVIEW_BASE);
            let chunk = layer_of(&out[0]);
            assert_eq!((chunk.layer_id, chunk.mode), (LAYER_STATIC, LayerMode::Append));
            layer.extend_from_slice(&chunk.splats);
            let stats = stats_of(&out);
            assert_eq!(stats.splats_static as usize, layer.len());
            assert_eq!(stats.splats_dynamic, 0);
            // One or two frames became due.
            assert!(stats.pixels == 64 * 48 || stats.pixels == 2 * 64 * 48, "{}", stats.pixels);
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
    fn scan_camera_sweeps_floor_ceiling_and_looks_back() {
        let path = ScanPath::house_default();
        let scan = ScanConfig::default();
        let (mut up, mut down, mut back) = (0.0f32, 0.0f32, 1.0f32);
        let steps = 2000;
        for i in 0..=steps {
            let t = path.duration() * i as f32 / steps as f32;
            let cam = scan_camera(&path, t, &scan);
            assert!((cam.position() - path.position_at(t)).length() < 1e-4, "the camera rides on the path");
            assert!(Vec3::from(cam.world_from_cam.matrix3.x_axis).y.abs() < 1e-4, "level horizon at t = {t}");
            assert_eq!((cam.width, cam.height), (scan.width, scan.height));
            assert!((cam.fov_y().to_degrees() - scan.fov_y_deg).abs() < 1e-2);
            let forward = cam.forward();
            up = up.max(forward.y);
            down = down.min(forward.y);
            let heading = path.heading_at(t);
            back = back.min(forward.x * heading.x + forward.z * heading.z);
        }
        // Up to the ceiling above, down to the floor at its feet, and back
        // over the shoulder: what leaves no wall or ceiling unseen.
        assert!(up > 0.8 && down < -0.8, "pitch range {down}..{up}");
        assert!(back < -0.3, "never looks back ({back})");
        // Any time is safe.
        for t in [f32::NAN, f32::INFINITY, -5.0, 1.0e30] {
            let cam = scan_camera(&path, t, &scan);
            assert!(cam.position().is_finite() && cam.forward().is_finite(), "{t}");
        }
        let (odd, start) = (scan_camera(&path, f32::NAN, &scan), scan_camera(&path, 0.0, &scan));
        assert!((odd.forward() - start.forward()).length() < 1e-5 && odd.position() == start.position());
        // The pipeline reports exactly this camera.
        let mut pipeline = Pipeline::new(small_house());
        pipeline.handle_control(ControlMsg::ScanStart);
        let stats = stats_of(&pipeline.tick(0.4)).scan.unwrap();
        let config = *pipeline.config();
        let path = ScanPath::house_default().with_speed(config.scan.speed);
        let expected = CameraInfo::from_camera(0, &scan_camera(&path, pipeline.scene_time(), &config.scan));
        assert_eq!(stats.camera, expected);
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
