//! The live capture and reconstruction pipeline.
//!
//! [`Pipeline`] owns a scene, the simulated sensors and the reconstruction
//! state, and turns time into wire messages: call [`Pipeline::tick`] at the
//! configured rate and forward every [`Outgoing`] to the viewers. It never
//! sleeps, spawns nothing and does no I/O, so the same code runs on a native
//! thread behind a WebSocket server and inside a Web Worker.
//!
//! # Layers
//!
//! * Layer 0 is static. Live stage: the empty stage, reconstructed once from
//!   the background plates and sent as `Replace`. Home scan: the fused map,
//!   grown by `Append` messages that carry only the new splats.
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
//! | [`SLOT_PREVIEW_BASE`]` + id` (16…) | Preview of camera `id` |
//!
//! A client that connects late needs [`Pipeline::snapshot`], taken between
//! two ticks, followed by everything later ticks emit.

use glam::Vec3;
#[cfg(feature = "par")]
use rayon::prelude::*;
use web_time::Instant;

use crate::capture::{render_rgbd, CaptureOptions};
use crate::codec::{self, Body, LayerMode, Message};
use crate::protocol::{
    CameraInfo, ControlMsg, PipelineConfig, ScanInfo, ScanState, ScanStats, SessionState, Stats, ViewerStart,
    PROTOCOL_VERSION,
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

/// Wall-clock time one tick may spend fusing scan frames before it returns
/// (it always fuses at least one frame that is due).
const SCAN_TICK_BUDGET_MS: f64 = 25.0;
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
}

// The stage is the common case and lives as long as the pipeline; boxing it
// would only add an indirection.
#[allow(clippy::large_enum_variant)]
enum Mode {
    Stage(Stage),
    House(House),
}

/// Capture → reconstruct → encode, one tick at a time.
pub struct Pipeline {
    config: PipelineConfig,
    scene: Box<dyn Scene>,
    mode: Mode,
    seq: u32,
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
        let mut house = Self {
            snapshot: scene.snapshot_static(),
            path: ScanPath::house_default().with_speed(config.scan.speed),
            fusion: VoxelFusion::new(config.scan.voxel_size),
            state: ScanState::Idle,
            finishing: false,
            time: 0.0,
            next_frame: 0,
        };
        house.fusion.max_range = config.scan.max_range;
        if config.scan.autostart {
            house.state = ScanState::Scanning;
        }
        house
    }

    /// Discards the map and rewinds the walk.
    fn reset(&mut self, state: ScanState) {
        self.fusion.clear();
        self.state = state;
        self.finishing = false;
        self.time = 0.0;
        self.next_frame = 0;
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
        let scan = &config.scan;
        self.path.camera_at(time as f32, scan.fov_y_deg, scan.width, scan.height)
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
        Self { config, scene, mode, seq: 0, tick: 0, last_tick: None, tick_hz: 0.0, last_stats: Stats::default() }
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

    fn next_seq(&mut self) -> u32 {
        self.seq = self.seq.wrapping_add(1);
        self.seq
    }

    /// Scene time as the wire header carries it (wrapping milliseconds).
    fn time_ms(&self) -> u32 {
        (self.scene_time() as f64 * 1e3) as u64 as u32
    }

    fn encode_state(&mut self) -> Outgoing {
        let json = serde_json::to_string(&self.state()).unwrap_or_else(|_| "{}".to_string());
        let msg = Message { seq: self.next_seq(), time_ms: self.time_ms(), body: Body::State(json) };
        Outgoing { bytes: codec::encode(&msg), reliable: true, slot: SLOT_RELIABLE }
    }

    fn encode_stats(&mut self, stats: &Stats) -> Outgoing {
        let json = serde_json::to_string(stats).unwrap_or_else(|_| "{}".to_string());
        let msg = Message { seq: self.next_seq(), time_ms: self.time_ms(), body: Body::Stats(json) };
        Outgoing { bytes: codec::encode(&msg), reliable: false, slot: SLOT_STATS }
    }

    /// Encodes the current content of a layer as `Replace`.
    fn encode_layer(&mut self, layer: u8) -> Outgoing {
        let (seq, time_ms) = (self.next_seq(), self.time_ms());
        let splats = if layer == LAYER_STATIC { self.static_splats() } else { self.dynamic_splats() };
        let mut bytes = Vec::new();
        codec::encode_layer(seq, time_ms, layer, LayerMode::Replace, splats, &mut bytes);
        let reliable = layer == LAYER_STATIC;
        Outgoing { bytes, reliable, slot: if reliable { SLOT_RELIABLE } else { SLOT_DYNAMIC } }
    }

    fn encode_preview(&mut self, camera_id: u8, frame: &RgbdFrame) -> Outgoing {
        let (seq, time_ms) = (self.next_seq(), self.time_ms());
        let small = downsample(frame, self.config.preview.max_width);
        let mut bytes = Vec::new();
        codec::encode_preview(seq, time_ms, camera_id, &small, &mut bytes);
        Outgoing { bytes, reliable: false, slot: SLOT_PREVIEW_BASE.saturating_add(camera_id) }
    }

    /// State plus both layers as `Replace`: a full resynchronization.
    fn encode_everything(&mut self) -> Vec<Outgoing> {
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
            splats_static: self.static_splats().len().min(u32::MAX as usize) as u32,
            splats_dynamic: self.dynamic_splats().len().min(u32::MAX as usize) as u32,
            bytes: bytes.min(u32::MAX as usize) as u32,
            pixels: work.pixels.min(u32::MAX as u64) as u32,
            scan: match &self.mode {
                Mode::Stage(_) => None,
                Mode::House(house) => Some(house.stats(&self.config)),
            },
        }
    }

    /// The last Stats with the scan section refreshed, so that the effect of
    /// a control message shows without waiting for the next tick.
    fn encode_fresh_stats(&mut self) -> Outgoing {
        let mut stats = self.last_stats;
        stats.scene_time = self.scene_time();
        stats.splats_static = self.static_splats().len().min(u32::MAX as usize) as u32;
        stats.splats_dynamic = self.dynamic_splats().len().min(u32::MAX as usize) as u32;
        if let Mode::House(house) = &self.mode {
            stats.scan = Some(house.stats(&self.config));
        }
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
    /// frames that became due are fused (under a time budget of about 25 ms
    /// per tick once more than one is due) and their new splats are appended
    /// to layer 0. A Stats message is emitted on every tick, paused or not.
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
            let is_stage = matches!(self.mode, Mode::Stage(_));
            if is_stage {
                self.tick_stage(advance, &mut work, &mut out);
            } else {
                self.tick_house(advance, start, &mut work, &mut out);
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

        let encode = Instant::now();
        out.push(self.encode_layer(LAYER_DYNAMIC));
        if self.config.preview.enabled {
            let Mode::Stage(stage) = &mut self.mode else { return };
            let count = stage.frames.len();
            let per_tick = (self.config.preview.cameras_per_tick as usize).min(count);
            let first = stage.preview_cursor % count.max(1);
            stage.preview_cursor = (first + per_tick) % count.max(1);
            for k in 0..per_tick {
                let id = (first + k) % count;
                // Temporarily take the frame so that `self` can be borrowed.
                let Mode::Stage(stage) = &mut self.mode else { return };
                let frame = std::mem::take(&mut stage.frames[id]);
                out.push(self.encode_preview(id as u8, &frame));
                let Mode::Stage(stage) = &mut self.mode else { return };
                stage.frames[id] = frame;
            }
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
            let time = h.frame_time(h.next_frame, &config);
            let cam = h.camera(time, &config);
            let capture = Instant::now();
            let frame = render_rgbd(&h.snapshot, &cam, &capture_options(&config, h.next_frame as u32));
            work.capture_ms += ms_since(capture);
            work.pixels += frame.width as u64 * frame.height as u64;
            let fuse = Instant::now();
            h.fusion.integrate(&frame, &cam, &config.recon);
            work.reconstruct_ms += ms_since(fuse);
            h.next_frame += 1;
            last_frame = Some(frame);
            if ms_since(start) > SCAN_TICK_BUDGET_MS {
                break;
            }
        }
        if h.next_frame > h.last_frame(&config) {
            h.state = ScanState::Done;
            h.finishing = false;
        }

        let encode = Instant::now();
        if h.fusion.len() > first_new {
            let mut bytes = Vec::new();
            let seq = self.seq.wrapping_add(1);
            self.seq = seq;
            let Mode::House(h) = &self.mode else { return };
            let time_ms = (h.time * 1e3) as u64 as u32;
            codec::encode_layer(seq, time_ms, LAYER_STATIC, LayerMode::Append, &h.fusion.splats()[first_new..], &mut bytes);
            out.push(Outgoing { bytes, reliable: true, slot: SLOT_RELIABLE });
        }
        if let (true, Some(frame)) = (config.preview.enabled, last_frame) {
            out.push(self.encode_preview(0, &frame));
        }
        work.encode_ms += ms_since(encode);
    }

    /// Everything a client that connects now needs: the State, the whole
    /// static layer as `Replace` (background plate splats or the scan so
    /// far), the latest live layer and the latest Stats.
    ///
    /// Take it between two ticks and deliver it before anything a later tick
    /// emits; `Append` messages only make sense on top of it.
    pub fn snapshot(&mut self) -> Vec<Outgoing> {
        let mut out = self.encode_everything();
        out.push(self.encode_fresh_stats());
        out
    }

    // -- control -----------------------------------------------------------

    /// Applies a control message and returns the messages to broadcast right
    /// away: a new State whenever the configuration changed, `Replace`
    /// messages for the layers it invalidated, and a Stats message after
    /// scan commands.
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
                match &mut self.mode {
                    Mode::Stage(stage) => {
                        stage.time = 0.0;
                        stage.refresh(self.scene.as_ref(), &config, &mut TickWork::default());
                        Ok(vec![self.encode_layer(LAYER_DYNAMIC), self.encode_fresh_stats()])
                    }
                    Mode::House(house) => {
                        house.reset(if config.scan.autostart { ScanState::Scanning } else { ScanState::Idle });
                        Ok(vec![self.encode_layer(LAYER_STATIC), self.encode_fresh_stats()])
                    }
                }
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
                if house.state == ScanState::Scanning {
                    house.state = ScanState::Paused;
                    house.finishing = false;
                }
                false
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
    /// returns whether it emptied the map.
    fn scan_command(&mut self, command: impl FnOnce(&mut House) -> bool) -> Vec<Outgoing> {
        let Mode::House(house) = &mut self.mode else { return Vec::new() };
        let cleared = command(house);
        let mut out = Vec::new();
        if cleared {
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
                self.encode_everything()
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
