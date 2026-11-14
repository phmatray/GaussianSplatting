//! One capture, from the phone's `hello` to its `finish`.
//!
//! [`CaptureSession`] owns everything a capture accumulates (the voxel
//! fusion, the live layer, region and floor, counters) and speaks two
//! languages: the JSON messages of `/ingest` toward the phone, and the
//! ordinary wire protocol toward viewers (`State`, layer 0 `Append` /
//! `Replace`, layer 1 `Replace`, `Stats`, `Preview`), so that a viewer needs
//! nothing new to show a capture. It is driven by whoever owns the
//! connection:
//!
//! ```text
//! hello  -> Hello::parse(text, token)   -> CaptureSession::new, send welcome, broadcast snapshot()
//! text   -> session.handle_text(text)   -> Reply { to_phone, to_viewers }
//! binary -> session.handle_frame(bytes) -> Reply (ack first)
//! ~1 Hz  -> session.progress()          -> Reply (progress message, refreshed layer 0)
//! viewer joins -> session.snapshot()
//! ```
//!
//! # Coordinates
//!
//! ARKit's world has its origin where the phone was when the AR session
//! started. The fusion works in that world, untouched, so that a floor
//! height that arrives late (or is corrected) costs nothing but a new
//! broadcast: everything sent to viewers is shifted vertically on the way
//! out so that the floor is `y = 0`. The floor is, in this order: what the
//! phone reported (`floor` message), what the data shows (the lowest large
//! horizontal surface facing up), or a guess (the phone 1.3 m above it).

use glam::{Quat, Vec3};
use web_time::Instant;

use super::file::GscapHeader;
use super::frame::{decode_frame, CaptureFrame, FormatError, ErrorCode, Tracking};
use super::message::{CaptureRegion, ClientMessage, Hello, IngestErrorCode, ServerMessage};
pub use super::message::SessionError;
use super::prepare::{focus_point, prepare, IngestParams, Prepared, SkipReason};
use crate::codec::{self, Body, LayerMode, Message};
use crate::pipeline::{Outgoing, LAYER_DYNAMIC, LAYER_STATIC, SLOT_DYNAMIC, SLOT_PREVIEW_BASE, SLOT_RELIABLE, SLOT_STATS};
use crate::protocol::{
    CameraInfo, CaptureInfo, CaptureMode, CaptureStats, PipelineConfig, RegionInfo, ScanHead, ScanInfo, ScanState,
    ScanStats, SessionState, Stats, ViewerStart, PROTOCOL_VERSION,
};
use crate::reconstruct::{frame_to_splats, VoxelFusion, MASK_FOREGROUND, MASK_SKIP};
use crate::scene::SceneKind;
use crate::types::{Aabb, Camera, RgbdFrame, Splat};

/// Height of the phone above the floor assumed until the floor is known.
const ASSUMED_PHONE_HEIGHT: f32 = 1.3;
/// The floor moves (and layer 0 is broadcast again) only when it changes by
/// more than this.
const FLOOR_STEP: f32 = 0.01;
/// A horizontal surface must be at least this large to be taken for the
/// floor or the ceiling (square meters).
const MIN_FLOOR_AREA: f32 = 0.25;
/// Most points of the phone's path kept for [`ScanInfo`].
const MAX_PATH_POINTS: usize = 1024;
/// The State is sent again when the scene outgrows the announced bounds by
/// this much (meters).
const BOUNDS_SLACK: f32 = 0.5;

fn ms_since(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1e3
}

fn saturating_u32(value: usize) -> u32 {
    value.min(u32::MAX as usize) as u32
}

/// What happened to a frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FrameReport {
    /// The frame's index (what the `ack` names).
    pub index: u32,
    /// The frame went into the reconstruction.
    pub used: bool,
    /// Why not.
    pub skipped: Option<SkipReason>,
    /// Splats the frame added to layer 0 (fusing modes) or put into layer 1
    /// (`live`).
    pub splats: u32,
    /// Time spent on the frame, milliseconds.
    pub ms: f32,
}

/// What a call into the session asks its host to send.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Reply {
    /// Text frames for the phone, in order.
    pub to_phone: Vec<ServerMessage>,
    /// Messages to broadcast to every viewer, in order; `reliable` and
    /// `slot` follow the delivery contract of the wire format.
    pub to_viewers: Vec<Outgoing>,
    /// Set by [`CaptureSession::handle_frame`].
    pub frame: Option<FrameReport>,
}

impl Reply {
    /// The text frames for the phone as JSON.
    pub fn phone_json(&self) -> Vec<String> {
        self.to_phone.iter().map(ServerMessage::to_json).collect()
    }
}

/// Where the floor is, and how we know.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct Floor {
    /// From the phone (`floor` message, `.gscap` header).
    told: Option<f32>,
    /// From the data.
    estimated: Option<f32>,
    /// From the height of the first camera.
    guessed: Option<f32>,
}

impl Floor {
    fn y(&self) -> f32 {
        self.told.or(self.estimated).or(self.guessed).unwrap_or(0.0)
    }

    fn known(&self) -> bool {
        self.told.is_some() || self.estimated.is_some()
    }
}

/// The state of one capture. See the module documentation.
pub struct CaptureSession {
    hello: Hello,
    params: IngestParams,
    imported: bool,
    fusion: VoxelFusion,
    /// Layer 1 of the `live` mode, in capture coordinates.
    live: Vec<Splat>,
    /// Region of interest the phone sent.
    region: Option<CaptureRegion>,
    /// Region assumed until then: around what the first frame looks at.
    auto_region: Option<CaptureRegion>,
    floor: Floor,
    ceiling: Option<f32>,
    /// A person mask has been seen: frames without one show nobody.
    mask_seen: bool,

    frames_received: u32,
    frames_used: u32,
    paused: bool,
    finished: bool,
    tracking: Tracking,
    first_timestamp: Option<f64>,
    last_timestamp: f64,
    /// Smoothed interval between frames, seconds.
    interval: f64,

    /// The phone's camera at the last frame, in capture coordinates.
    camera: Option<Camera>,
    /// View a visitor starts from: eye and target, capture coordinates.
    start_view: Option<(Vec3, Vec3)>,
    path: Vec<Vec3>,
    path_spacing: f32,
    /// Everything fused or shown so far, capture coordinates.
    bounds: Aabb,
    /// The last preview, already upright (for late joiners).
    preview: Option<RgbdFrame>,

    /// Floor height layer 0 was last broadcast with.
    sent_floor: f32,
    /// Bounds announced in the last State, viewer coordinates.
    sent_bounds: Aabb,
    /// [`VoxelFusion::refined`] when layer 0 was last broadcast as a whole.
    sent_refined: u64,
    /// Layer 0 must be broadcast again as a whole (region, floor, clean-up).
    replace_due: bool,
    state_due: bool,
    /// [`CaptureSession::progress`] calls since layer 0 was last replaced.
    quiet_ticks: u32,
    last_stats: Stats,
    seq: u32,
}

impl std::fmt::Debug for CaptureSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CaptureSession")
            .field("mode", &self.hello.mode)
            .field("name", &self.hello.name)
            .field("frames_received", &self.frames_received)
            .field("frames_used", &self.frames_used)
            .field("splats", &self.fusion.len())
            .field("finished", &self.finished)
            .finish_non_exhaustive()
    }
}

impl CaptureSession {
    /// Starts a capture for an accepted `hello` ([`Hello::parse`] has
    /// checked protocol and token; the token is not kept).
    pub fn new(hello: &Hello, params: IngestParams) -> Self {
        let params = params.sanitized();
        let mode = params.mode(hello.mode);
        let mut fusion = VoxelFusion::new(mode.voxel_size);
        fusion.max_range = mode.far;
        fusion.radius_factor = params.kernel;
        fusion.max_voxels = params.max_voxels;
        fusion.blend_frames = params.blend_frames;
        fusion.exposure_compensation = params.exposure_compensation;
        Self {
            hello: Hello { token: String::new(), ..hello.clone() },
            params,
            imported: false,
            fusion,
            live: Vec::new(),
            region: None,
            auto_region: None,
            floor: Floor::default(),
            ceiling: None,
            mask_seen: false,
            frames_received: 0,
            frames_used: 0,
            paused: false,
            finished: false,
            tracking: Tracking::NotAvailable,
            first_timestamp: None,
            last_timestamp: 0.0,
            interval: 0.0,
            camera: None,
            start_view: None,
            path: Vec::new(),
            path_spacing: 0.05,
            bounds: Aabb::EMPTY,
            preview: None,
            sent_floor: 0.0,
            sent_bounds: Aabb::EMPTY,
            sent_refined: 0,
            replace_due: false,
            state_due: false,
            quiet_ticks: 0,
            last_stats: Stats::default(),
            seq: 0,
        }
    }

    /// Starts a capture from the header of a `.gscap`: mode, name, device,
    /// and the region and floor the phone knew when it closed the file.
    /// Fails with `bad_header` when the header names no known mode or
    /// another protocol version.
    pub fn from_header(header: &GscapHeader, params: IngestParams) -> Result<Self, FormatError> {
        let bad = |detail: String| FormatError { code: ErrorCode::BadHeader, detail };
        if let Some(protocol) = header.protocol.filter(|&p| p != super::message::INGEST_PROTOCOL) {
            return Err(bad(format!("protocol {protocol}")));
        }
        let mode = header.capture_mode().ok_or_else(|| bad(format!("unknown mode {:?}", header.mode)))?;
        let mut session = Self::new(&Hello::offline(mode, &header.name, &header.device, &header.app), params);
        session.imported = true;
        session.region = header.region.map(CaptureRegion::sanitized).filter(|_| mode.uses_region());
        session.floor.told = header.floor.filter(|y| y.abs() <= 1.0e4);
        Ok(session)
    }

    /// Starts a `live` capture over what this one has fused: its layer 0
    /// (as viewers have it, cropped to the region of interest) stays as
    /// the backdrop and its floor is kept, while every frame of the new
    /// capture replaces layer 1.
    ///
    /// This only makes sense while the phone's AR session goes on, so that
    /// both captures share one world: the app keeps its session between
    /// captures unless it was restarted or reset. The host decides; a
    /// plain [`CaptureSession::new`] starts with an empty layer 0.
    pub fn into_live(mut self, hello: &Hello) -> Self {
        if let Some(region) = self.region() {
            self.fusion.retain(|splat, _| region.contains(splat.position()));
        }
        let mut live = Self::new(&Hello { mode: CaptureMode::Live, ..hello.clone() }, self.params);
        live.fusion = self.fusion;
        live.floor = self.floor;
        live.sent_floor = live.floor.y();
        live.bounds = Aabb::from_points(live.fusion.splats().iter().map(Splat::position));
        live.seq = self.seq;
        live
    }

    // -- inspection ----------------------------------------------------------

    pub fn mode(&self) -> CaptureMode {
        self.hello.mode
    }

    pub fn name(&self) -> &str {
        &self.hello.name
    }

    /// The hello this capture was started with (without its token).
    pub fn hello(&self) -> &Hello {
        &self.hello
    }

    pub fn params(&self) -> &IngestParams {
        &self.params
    }

    /// A phone that reconnects with this hello continues this capture
    /// (same name and mode) instead of starting a new one.
    pub fn matches(&self, hello: &Hello) -> bool {
        hello.mode == self.hello.mode && hello.name == self.hello.name
    }

    pub fn frames_received(&self) -> u32 {
        self.frames_received
    }

    pub fn frames_used(&self) -> u32 {
        self.frames_used
    }

    pub fn is_paused(&self) -> bool {
        self.paused
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// Height of the floor in capture coordinates (what is subtracted from
    /// every `y` on the way to viewers), and whether it is more than a guess.
    pub fn floor(&self) -> (f32, bool) {
        (self.floor.y(), self.floor.known())
    }

    /// The region of interest in effect, in capture coordinates.
    pub fn region(&self) -> Option<CaptureRegion> {
        self.region.or(self.auto_region).filter(|_| self.mode().uses_region())
    }

    /// Splats of layer 0 as viewers have it: inside the region, floor at
    /// `y = 0`.
    pub fn static_splats(&self) -> Vec<Splat> {
        self.visible(self.fusion.splats())
    }

    /// Splats of layer 1 as viewers have it.
    pub fn dynamic_splats(&self) -> Vec<Splat> {
        self.shifted(&self.live)
    }

    /// Number of splats viewers have, both layers.
    pub fn splat_count(&self) -> usize {
        self.visible_count() + self.live.len()
    }

    // -- coordinates ---------------------------------------------------------

    fn shift(&self) -> Vec3 {
        Vec3::new(0.0, -self.floor.y(), 0.0)
    }

    fn shifted(&self, splats: &[Splat]) -> Vec<Splat> {
        let dy = -self.floor.y();
        splats.iter().map(|s| Splat { pos: [s.pos[0], s.pos[1] + dy, s.pos[2]], ..*s }).collect()
    }

    /// The part of `splats` that viewers get: inside the region of interest
    /// (modes that have one), moved to viewer coordinates.
    fn visible(&self, splats: &[Splat]) -> Vec<Splat> {
        let dy = -self.floor.y();
        let region = self.region();
        splats
            .iter()
            .filter(|s| region.is_none_or(|r| r.contains(s.position())))
            .map(|s| Splat { pos: [s.pos[0], s.pos[1] + dy, s.pos[2]], ..*s })
            .collect()
    }

    /// Mean position of the fused splats (a sample of them: this is for
    /// pointing a camera, not for measuring).
    fn centroid(&self) -> Option<Vec3> {
        let splats = self.fusion.splats();
        let step = (splats.len() / 4096).max(1);
        let (mut sum, mut count) = (glam::DVec3::ZERO, 0u32);
        for splat in splats.iter().step_by(step) {
            sum += splat.position().as_dvec3();
            count += 1;
        }
        (count > 0).then(|| (sum / count as f64).as_vec3())
    }

    fn visible_count(&self) -> usize {
        match self.region() {
            Some(region) => self.fusion.splats().iter().filter(|s| region.contains(s.position())).count(),
            None => self.fusion.len(),
        }
    }

    fn shifted_camera(&self, cam: &Camera) -> Camera {
        let mut cam = *cam;
        cam.world_from_cam.translation += glam::Vec3A::from(self.shift());
        cam
    }

    // -- wire messages -------------------------------------------------------

    fn next_seq(&mut self) -> u32 {
        self.seq = self.seq.wrapping_add(1);
        self.seq
    }

    /// Seconds of capture: from the first frame to the last.
    fn elapsed(&self) -> f64 {
        self.first_timestamp.map_or(0.0, |first| (self.last_timestamp - first).max(0.0))
    }

    fn time_ms(&self) -> u32 {
        (self.elapsed() * 1e3) as u64 as u32
    }

    fn encode_splats(&mut self, layer: u8, mode: LayerMode, splats: &[Splat]) -> Outgoing {
        let mut bytes = Vec::new();
        let (seq, time_ms) = (self.next_seq(), self.time_ms());
        codec::encode_layer(seq, time_ms, layer, mode, splats, &mut bytes);
        let reliable = layer == LAYER_STATIC;
        Outgoing { bytes, reliable, slot: if reliable { SLOT_RELIABLE } else { SLOT_DYNAMIC } }
    }

    fn encode_state(&mut self) -> Outgoing {
        let seq = self.next_seq();
        self.encode_state_as(seq)
    }

    /// The State with a sequence number reserved earlier, for a State that
    /// must precede messages that were encoded before it became due.
    fn encode_state_as(&mut self, seq: u32) -> Outgoing {
        let state = self.state();
        self.sent_bounds = state.bounds;
        self.state_due = false;
        let json = serde_json::to_string(&state).unwrap_or_else(|_| "{}".to_string());
        let msg = Message { seq, time_ms: self.time_ms(), body: Body::State(json) };
        Outgoing { bytes: codec::encode(&msg), reliable: true, slot: SLOT_RELIABLE }
    }

    fn encode_stats(&mut self, stats: &Stats) -> Outgoing {
        let json = serde_json::to_string(stats).unwrap_or_else(|_| "{}".to_string());
        let msg = Message { seq: self.next_seq(), time_ms: self.time_ms(), body: Body::Stats(json) };
        Outgoing { bytes: codec::encode(&msg), reliable: false, slot: SLOT_STATS }
    }

    fn encode_preview(&mut self, frame: &RgbdFrame) -> Outgoing {
        let mut bytes = Vec::new();
        let (seq, time_ms) = (self.next_seq(), self.time_ms());
        codec::encode_preview(seq, time_ms, 0, frame, &mut bytes);
        Outgoing { bytes, reliable: false, slot: SLOT_PREVIEW_BASE }
    }

    /// Layer 0 as a whole, and everything that depends on it being current.
    fn encode_static_replace(&mut self) -> Outgoing {
        let splats = self.static_splats();
        self.sent_floor = self.floor.y();
        self.sent_refined = self.fusion.refined();
        self.replace_due = false;
        self.quiet_ticks = 0;
        self.encode_splats(LAYER_STATIC, LayerMode::Replace, &splats)
    }

    /// The session description viewers need to interpret the layers.
    pub fn state(&self) -> SessionState {
        let mode = self.params.mode(self.mode());
        let shift = self.shift();
        let mut config = PipelineConfig::for_scene(SceneKind::House);
        config.paused = self.paused;
        config.target_hz = if self.mode() == CaptureMode::Live { 15.0 } else { 10.0 };
        config.preview.max_width = self.params.preview_width;
        config.scan.head = ScanHead::Single;
        config.scan.autostart = false;
        config.scan.voxel_size = mode.voxel_size;
        config.scan.max_range = mode.far;
        config.scan.kernel = self.params.kernel;
        config.scan.frames_per_second = config.target_hz;
        if let Some(cam) = &self.camera {
            (config.scan.width, config.scan.height) = (cam.width, cam.height);
            config.scan.fov_y_deg = cam.fov_y().to_degrees();
        }

        let region = self.region();
        let mut bounds = match region {
            Some(r) => {
                let (c, r) = (Vec3::from_array(r.center), Vec3::splat(r.radius));
                Aabb::new(c - r, c + r)
            }
            None => self.bounds,
        };
        if bounds.is_empty() {
            // Nothing seen yet: a room-sized box around the origin.
            let floor = self.floor.y();
            bounds = Aabb::new(Vec3::new(-2.5, floor, -2.5), Vec3::new(2.5, floor + 2.5, 2.5));
        }
        let bounds = Aabb::new(bounds.min + shift, bounds.max + shift);
        let (eye, target) = match self.start_view {
            Some((eye, target)) => {
                // In a room, look into it: toward the bulk of what has been
                // scanned rather than at the wall the phone happened to
                // face first.
                let middle = self.centroid().map(|c| Vec3::new(c.x, eye.y - 0.3, c.z));
                let into_room = middle.filter(|m| self.mode() == CaptureMode::Room && m.distance(eye) > 0.75);
                (eye + shift, into_room.unwrap_or(target) + shift)
            }
            None => (Vec3::new(0.0, 1.5, 2.5), Vec3::new(0.0, 1.0, 0.0)),
        };
        SessionState {
            version: PROTOCOL_VERSION,
            config,
            scene: "capture".to_string(),
            cameras: Vec::new(),
            bounds,
            viewer_start: ViewerStart { eye: eye.to_array(), target: target.to_array() },
            loop_duration: None,
            ceiling_height: self.ceiling.map(|c| c - self.floor.y()).filter(|h| h.is_finite() && *h > 1.0),
            scan: Some(ScanInfo {
                path: self.path.iter().map(|p| (*p + shift).to_array()).collect(),
                duration: self.elapsed() as f32,
            }),
            capture: Some(CaptureInfo {
                mode: self.mode(),
                name: self.hello.name.clone(),
                device: self.hello.device.clone(),
                app: self.hello.app.clone(),
                region: region.map(|r| RegionInfo { center: (Vec3::from_array(r.center) + shift).to_array(), radius: r.radius }),
                floor_known: self.floor.known(),
                imported: self.imported,
            }),
        }
    }

    /// The counters and the phone's camera, as the Stats message carries
    /// them. Timings are those of the last frame.
    pub fn stats(&self) -> Stats {
        let state = if self.finished {
            ScanState::Done
        } else if self.paused {
            ScanState::Paused
        } else if self.frames_received == 0 {
            ScanState::Idle
        } else {
            ScanState::Scanning
        };
        let camera = self.camera.map(|cam| CameraInfo::from_camera(0, &self.shifted_camera(&cam))).unwrap_or_default();
        Stats {
            tick: self.frames_received as u64,
            scene_time: self.elapsed() as f32,
            tick_hz: if self.interval > 0.0 { (1.0 / self.interval) as f32 } else { 0.0 },
            splats_static: saturating_u32(self.visible_count()),
            splats_dynamic: saturating_u32(self.live.len()),
            scan: Some(ScanStats {
                state,
                progress: if self.finished { 1.0 } else { 0.0 },
                camera,
                ..ScanStats::default()
            }),
            capture: Some(CaptureStats {
                frames_received: self.frames_received,
                frames_used: self.frames_used,
                paused: self.paused,
                finished: self.finished,
                full: self.mode().fuses() && self.fusion.is_full(),
                tracking: self.tracking as u8,
            }),
            ..self.last_stats
        }
    }

    fn fresh_stats(&mut self) -> Outgoing {
        let stats = self.stats();
        self.last_stats = stats;
        self.encode_stats(&stats)
    }

    /// Everything a viewer that joins now needs: State, both layers as
    /// `Replace`, the last preview and Stats.
    pub fn snapshot(&mut self) -> Vec<Outgoing> {
        let mut out = vec![self.encode_state(), self.encode_static_replace()];
        let live = self.dynamic_splats();
        out.push(self.encode_splats(LAYER_DYNAMIC, LayerMode::Replace, &live));
        if let Some(preview) = self.preview.take() {
            out.push(self.encode_preview(&preview));
            self.preview = Some(preview);
        }
        out.push(self.fresh_stats());
        out
    }

    // -- text messages -------------------------------------------------------

    /// Handles a text message that follows the hello. An error means the
    /// connection must be closed after sending [`SessionError::to_message`].
    pub fn handle_text(&mut self, text: &str) -> Result<Reply, SessionError> {
        Ok(self.handle_message(&ClientMessage::parse(text)?))
    }

    /// Handles a parsed client message (a hello is ignored here).
    pub fn handle_message(&mut self, message: &ClientMessage) -> Reply {
        let mut reply = Reply::default();
        match message {
            ClientMessage::Hello(_) => return reply,
            ClientMessage::Region(region) => {
                if self.mode().uses_region() {
                    self.set_region(region.sanitized());
                    reply.to_viewers.push(self.encode_state());
                }
            }
            ClientMessage::Floor { y } => {
                if self.floor.told.is_none_or(|old| (old - y).abs() >= FLOOR_STEP) {
                    self.floor.told = Some(*y);
                    self.floor_moved();
                    reply.to_viewers.push(self.encode_state());
                }
            }
            ClientMessage::Pause => self.paused = true,
            ClientMessage::Resume => {
                self.paused = false;
                // A phone that goes on after its `finish` continues the scan.
                self.finished = false;
            }
            ClientMessage::Reset => return self.reset(),
            ClientMessage::Finish => return self.finish(),
        }
        reply.to_viewers.push(self.fresh_stats());
        reply
    }

    fn set_region(&mut self, region: CaptureRegion) {
        if self.region == Some(region) {
            return;
        }
        self.region = Some(region);
        // What lies far outside is never shown again: free it.
        let keep = CaptureRegion { center: region.center, radius: region.radius * self.params.region_margin };
        self.fusion.retain(|splat, _| keep.contains(splat.position()));
        self.replace_due = true;
        self.state_due = true;
        if let Some((eye, _)) = self.start_view {
            self.start_view = Some((eye, Vec3::from_array(region.center)));
        }
    }

    /// The floor height changed: viewers need the new State, and layer 0
    /// again if they hold any of it.
    fn floor_moved(&mut self) {
        if (self.floor.y() - self.sent_floor).abs() >= FLOOR_STEP {
            self.state_due = true;
            if self.fusion.is_empty() {
                self.sent_floor = self.floor.y();
            } else {
                self.replace_due = true;
            }
        }
    }

    /// Layer 0 as viewers hold it was built with another floor height.
    fn floor_is_stale(&self) -> bool {
        !self.fusion.is_empty() && (self.floor.y() - self.sent_floor).abs() >= FLOOR_STEP
    }

    /// The phone reconnected with a hello that [`CaptureSession::matches`]:
    /// the capture goes on where it was. Returns what a viewer needs to see
    /// it as running again.
    pub fn resume(&mut self) -> Reply {
        self.paused = false;
        self.finished = false;
        Reply { to_viewers: vec![self.fresh_stats()], ..Reply::default() }
    }

    /// Throws away everything captured (the phone also starts a new world
    /// origin): viewers get an empty session.
    pub fn reset(&mut self) -> Reply {
        let fresh = Self::new(&self.hello, self.params);
        let (seq, imported) = (self.seq, self.imported);
        *self = Self { seq, imported, ..fresh };
        let mut reply = Reply { to_viewers: self.snapshot(), ..Reply::default() };
        reply.to_phone.push(self.progress_message());
        reply
    }

    fn progress_message(&self) -> ServerMessage {
        ServerMessage::Progress { frames: self.frames_received as u64, splats: self.splat_count() as u64 }
    }

    // -- frames --------------------------------------------------------------

    /// Handles one binary message: decodes the frame, fuses it (or says why
    /// not) and returns the `ack` for the phone and what viewers get. A
    /// frame that does not decode is an error: send
    /// [`SessionError::to_message`] and close.
    pub fn handle_frame(&mut self, bytes: &[u8]) -> Result<Reply, SessionError> {
        let start = Instant::now();
        let frame = decode_frame(bytes).map_err(|e| {
            SessionError::new(IngestErrorCode::BadFrame, "Image de capture illisible : la capture est interrompue.", e.to_string())
        })?;
        Ok(self.ingest(&frame, start))
    }

    /// [`CaptureSession::handle_frame`] for a frame that is already decoded.
    pub fn handle_decoded(&mut self, frame: &CaptureFrame) -> Reply {
        self.ingest(frame, Instant::now())
    }

    fn ingest(&mut self, frame: &CaptureFrame, start: Instant) -> Reply {
        self.frames_received = self.frames_received.saturating_add(1);
        self.tracking = frame.tracking;
        let mut reply = Reply::default();
        let mut work = FrameWork::default();
        // A frame can change what the State says (where to look from, the
        // region assumed, the floor guessed); viewers must read that before
        // the layers of the same reply.
        let state_seq = self.next_seq();
        let outcome = self.reconstruct(frame, &mut work, &mut reply.to_viewers);
        let used = outcome.is_ok();
        if used {
            self.frames_used = self.frames_used.saturating_add(1);
        }
        if self.state_due {
            let state = self.encode_state_as(state_seq);
            reply.to_viewers.insert(0, state);
        }
        let bytes: usize = reply.to_viewers.iter().map(|m| m.bytes.len()).sum();
        let total_ms = ms_since(start);
        self.last_stats = Stats {
            capture_ms: work.prepare_ms as f32,
            reconstruct_ms: work.fuse_ms as f32,
            fuse_ms: 0.0,
            encode_ms: work.encode_ms as f32,
            total_ms: total_ms as f32,
            bytes: saturating_u32(bytes),
            pixels: work.pixels,
            ..self.last_stats
        };
        reply.to_viewers.push(self.fresh_stats());
        reply.to_phone.push(ServerMessage::Ack { frame: frame.index, used });
        reply.frame = Some(FrameReport {
            index: frame.index,
            used,
            skipped: outcome.err(),
            splats: work.splats,
            ms: total_ms as f32,
        });
        reply
    }

    /// Turns a frame into splats. On success the viewer messages are in
    /// `out`.
    fn reconstruct(&mut self, frame: &CaptureFrame, work: &mut FrameWork, out: &mut Vec<Outgoing>) -> Result<(), SkipReason> {
        let mode = self.mode();
        if self.finished {
            return Err(SkipReason::Finished);
        }
        if self.paused {
            return Err(SkipReason::Paused);
        }
        // A fused map cannot take back a frame placed by a guessed pose;
        // the live view has nothing to lose.
        if mode.fuses() && frame.tracking != Tracking::Normal {
            return Err(SkipReason::Tracking);
        }
        let range = *self.params.mode(mode);
        let start = Instant::now();
        let prepared = prepare(frame, range.near, range.far, &self.params)?;
        work.prepare_ms = ms_since(start);
        work.pixels = prepared.valid;
        if self.camera.is_none() && self.floor.guessed.is_none() {
            // Before anything is sent: the first splats already stand at a
            // plausible height.
            self.floor.guessed = Some(prepared.cam.position().y - ASSUMED_PHONE_HEIGHT);
            self.floor_moved();
        }

        let start = Instant::now();
        let mask = self.pixel_mask(frame, &prepared)?;
        if mode.fuses() {
            let fused = self.fusion.integrate_masked(&prepared.frame, &prepared.cam, &self.params.recon, mask.as_deref());
            work.fuse_ms = ms_since(start);
            let start = Instant::now();
            let fresh = self.visible(&self.fusion.splats()[fused.first_new..]);
            for splat in &self.fusion.splats()[fused.first_new..] {
                self.bounds.expand(splat.position());
            }
            work.splats = saturating_u32(fresh.len());
            if self.floor_is_stale() {
                // Appending in new coordinates to a layer in old ones would
                // show two floors: the whole layer goes out again.
                out.push(self.encode_static_replace());
            } else if !fresh.is_empty() {
                out.push(self.encode_splats(LAYER_STATIC, LayerMode::Append, &fresh));
            }
            work.encode_ms = ms_since(start);
        } else {
            self.live.clear();
            frame_to_splats(&prepared.frame, &prepared.cam, &self.params.recon, mask.as_deref(), &mut self.live);
            work.fuse_ms = ms_since(start);
            if self.floor.told.is_none() && (self.floor.estimated.is_none() || self.frames_used.is_multiple_of(30)) {
                let level = horizontal_level(&self.live, self.params.recon.footprint, Facing::Up);
                self.floor_estimated(level);
            }
            let start = Instant::now();
            for splat in &self.live {
                self.bounds.expand(splat.position());
            }
            work.splats = saturating_u32(self.live.len());
            let live = self.dynamic_splats();
            out.push(self.encode_splats(LAYER_DYNAMIC, LayerMode::Replace, &live));
            work.encode_ms = ms_since(start);
        }
        self.note_camera(frame, &prepared);
        let start = Instant::now();
        let preview = upright(&shrink(&prepared.frame, self.params.preview_width), frame.orientation.quarter_turns_cw());
        out.push(self.encode_preview(&preview));
        self.preview = Some(preview);
        work.encode_ms += ms_since(start);
        Ok(())
    }

    /// Which pixels of a frame may become splats: `None` for all of them.
    fn pixel_mask(&mut self, frame: &CaptureFrame, prepared: &Prepared) -> Result<Option<Vec<u8>>, SkipReason> {
        let mode = self.mode();
        let n = prepared.frame.depth.len();
        let person_mask = frame.mask.as_deref().filter(|m| m.len() == n);
        let person_only = match mode {
            CaptureMode::Person => true,
            CaptureMode::Live => self.params.live_person_only,
            _ => false,
        };
        let mut mask: Option<Vec<u8>> = None;
        if person_only {
            match person_mask {
                Some(person) => {
                    self.mask_seen = true;
                    if !person.iter().any(|&p| p >= 128) {
                        return Err(SkipReason::NoPerson);
                    }
                    mask = Some(person.iter().map(|&p| if p >= 128 { MASK_FOREGROUND } else { MASK_SKIP }).collect());
                }
                // The phone sends masks when it can segment people: a frame
                // without one, after frames with one, shows nobody.
                None if self.mask_seen => return Err(SkipReason::NoPerson),
                None => {}
            }
        }
        if mode.uses_region() {
            if self.region.is_none() && self.auto_region.is_none() {
                let center = focus_point(prepared).unwrap_or_else(|| prepared.cam.position() + prepared.cam.forward());
                self.auto_region = Some(CaptureRegion { center: center.to_array(), radius: self.params.mode(mode).default_radius });
                self.state_due = true;
            }
            if let Some(region) = self.region() {
                // Fuse a margin around the region, so that a region that is
                // nudged or enlarged later still finds its surroundings.
                let keep = CaptureRegion { center: region.center, radius: region.radius * self.params.region_margin };
                let (w, cam) = (prepared.frame.width as usize, &prepared.cam);
                let mask = mask.get_or_insert_with(|| vec![MASK_FOREGROUND; n]);
                for (i, (m, &z)) in mask.iter_mut().zip(&prepared.frame.depth).enumerate() {
                    if *m != MASK_SKIP && z > 0.0 {
                        let p = cam.unproject((i % w) as f32 + 0.5, (i / w) as f32 + 0.5, z);
                        if !keep.contains(p) {
                            *m = MASK_SKIP;
                        }
                    }
                }
            }
        }
        Ok(mask)
    }

    fn note_camera(&mut self, frame: &CaptureFrame, prepared: &Prepared) {
        let cam = prepared.cam;
        let position = cam.position();
        if frame.timestamp.is_finite() {
            match self.first_timestamp {
                None => self.first_timestamp = Some(frame.timestamp),
                Some(_) => {
                    let dt = frame.timestamp - self.last_timestamp;
                    if dt > 0.0 && dt < 10.0 {
                        self.interval = if self.interval > 0.0 { 0.8 * self.interval + 0.2 * dt } else { dt };
                    }
                }
            }
            self.last_timestamp = frame.timestamp;
        }
        if self.camera.is_none() {
            // A visitor starts where the phone started, looking at what it
            // looked at; a little behind it for the single view of `live`.
            let focus = focus_point(prepared).unwrap_or(position + cam.forward() * 2.0);
            let target = self.region().map_or(focus, |r| Vec3::from_array(r.center));
            let eye = if self.mode() == CaptureMode::Live { position - cam.forward() * 0.6 + Vec3::Y * 0.2 } else { position };
            self.start_view = Some((eye, target));
            self.state_due = true;
        }
        self.camera = Some(cam);
        self.bounds.expand(position);
        if self.path.last().is_none_or(|last| last.distance(position) >= self.path_spacing) {
            self.path.push(position);
            if self.path.len() > MAX_PATH_POINTS {
                // Keep every other point and space the next ones wider.
                let mut keep = false;
                self.path.retain(|_| {
                    keep = !keep;
                    keep
                });
                self.path_spacing *= 2.0;
            }
        }
    }

    // -- floor and ceiling ---------------------------------------------------

    /// Takes a floor height found in the data, unless the phone reported
    /// one (ARKit's plane detection knows better).
    fn floor_estimated(&mut self, level: Option<f32>) {
        let Some(y) = level.filter(|_| self.floor.told.is_none()) else { return };
        if self.floor.estimated.is_none_or(|old| (old - y).abs() >= FLOOR_STEP) {
            self.floor.estimated = Some(y);
            self.floor_moved();
        }
    }

    /// Looks for the floor (when the phone has not reported one) and, in a
    /// room, for the ceiling in the fused cloud.
    fn estimate_levels(&mut self) {
        if !self.mode().fuses() {
            return;
        }
        let kernel = self.params.kernel;
        if self.floor.told.is_none() {
            let level = horizontal_level(self.fusion.splats(), kernel, Facing::Up);
            self.floor_estimated(level);
        }
        if self.mode() == CaptureMode::Room {
            let ceiling = horizontal_level(self.fusion.splats(), kernel, Facing::Down);
            if ceiling.is_some() && ceiling != self.ceiling {
                self.ceiling = ceiling;
                self.state_due = true;
            }
        }
    }

    // -- periodic work and the end -------------------------------------------

    /// Call about once a second while the capture runs: returns the
    /// `progress` message for the phone and brings viewers up to date with
    /// what changed slowly (a floor that was found, a region that moved,
    /// splats that were refined in place since they were appended).
    pub fn progress(&mut self) -> Reply {
        let mut reply = Reply::default();
        if !self.finished {
            self.estimate_levels();
            self.quiet_ticks = self.quiet_ticks.saturating_add(1);
            // Refined splats are worth a whole new layer 0 once enough of
            // them changed, and the less often the larger the layer is
            // (16 bytes per splat).
            let count = self.fusion.len();
            let refined = self.fusion.refined().saturating_sub(self.sent_refined);
            let patience = 4 + (count / 100_000) as u32;
            let polish = refined as usize >= (count / 20).max(500) && self.quiet_ticks >= patience;
            let outgrown = !self.bounds.is_empty() && {
                let shift = self.shift();
                let (lo, hi) = (self.bounds.min + shift, self.bounds.max + shift);
                self.region().is_none()
                    && ((self.sent_bounds.min - lo).max_element() > BOUNDS_SLACK || (hi - self.sent_bounds.max).max_element() > BOUNDS_SLACK)
            };
            if self.state_due || outgrown {
                reply.to_viewers.push(self.encode_state());
            }
            if self.replace_due || polish {
                reply.to_viewers.push(self.encode_static_replace());
            }
        }
        reply.to_viewers.push(self.fresh_stats());
        reply.to_phone.push(self.progress_message());
        reply
    }

    /// Ends the capture: a final pass over the fused cloud (lone cells that
    /// only one frame ever built are dropped, floor and ceiling are settled), then
    /// the final State, layer 0 and Stats for viewers and a last `progress`
    /// for the phone. Frames that arrive afterwards are acknowledged as not
    /// used. Calling it twice changes nothing more.
    pub fn finish(&mut self) -> Reply {
        let mut reply = Reply::default();
        if !self.finished {
            self.finished = true;
            if self.mode().fuses() {
                self.drop_specks();
                self.estimate_levels();
                self.bounds = Aabb::from_points(self.fusion.splats().iter().map(Splat::position).chain(self.path.iter().copied()));
            }
            reply.to_viewers.push(self.encode_state());
            reply.to_viewers.push(self.encode_static_replace());
            if self.mode().fuses() {
                reply.to_viewers.push(self.encode_splats(LAYER_DYNAMIC, LayerMode::Replace, &[]));
            }
        }
        reply.to_viewers.push(self.fresh_stats());
        reply.to_phone.push(self.progress_message());
        reply
    }
}

impl CaptureSession {
    /// The final pass over the fused cloud: drops the cells that fewer than
    /// [`IngestParams::min_frames`] frames built and that stand alone, with
    /// fewer than three occupied cells among the 26 around them. That is
    /// what a flying pixel or a burst of depth noise leaves behind; a patch
    /// of real surface that happened to be filmed once has its neighbours
    /// and stays.
    fn drop_specks(&mut self) {
        const MIN_NEIGHBOURS: usize = 3;
        let min_frames = self.params.min_frames;
        if min_frames <= 1 || self.fusion.is_empty() {
            return;
        }
        let inv = 1.0 / self.fusion.voxel_size();
        let cell = |s: &Splat| {
            let c = (s.position() * inv).floor();
            [c.x as i32, c.y as i32, c.z as i32]
        };
        let occupied: std::collections::HashSet<[i32; 3]> = self.fusion.splats().iter().map(cell).collect();
        let removed = self.fusion.retain(|splat, frames| {
            if frames >= min_frames {
                return true;
            }
            let [x, y, z] = cell(splat);
            let mut around = 0;
            for dx in -1..=1 {
                for dy in -1..=1 {
                    for dz in -1..=1 {
                        let other = [x.saturating_add(dx), y.saturating_add(dy), z.saturating_add(dz)];
                        around += usize::from((dx, dy, dz) != (0, 0, 0) && occupied.contains(&other));
                    }
                }
            }
            around >= MIN_NEIGHBOURS
        });
        if removed > 0 {
            self.replace_due = true;
        }
    }
}

/// Timings and counts of one frame.
#[derive(Default)]
struct FrameWork {
    prepare_ms: f64,
    fuse_ms: f64,
    encode_ms: f64,
    pixels: u32,
    splats: u32,
}

#[derive(Clone, Copy, PartialEq)]
enum Facing {
    Up,
    Down,
}

/// The height of the floor (`Facing::Up`: the lowest large horizontal
/// surface facing up) or of the ceiling (`Facing::Down`: the highest one
/// facing down) in a cloud of surfels, if there is one of at least
/// [`MIN_FLOOR_AREA`].
///
/// Heights are binned (2 cm); a bin counts when its surface, summed with
/// its neighbours, reaches the minimum area and a fifth of the largest
/// level. Below a real floor there is only noise (reflections in a glossy
/// floor, a few flying pixels), which that threshold ignores; above it,
/// tables and shelves are higher and do not matter.
fn horizontal_level(splats: &[Splat], kernel: f32, facing: Facing) -> Option<f32> {
    const BIN: f32 = 0.02;
    const MAX_BINS: usize = 4096;
    let sign = if facing == Facing::Up { 1.0 } else { -1.0 };
    let level = |s: &&Splat| (Quat::from_array(s.rot) * Vec3::Z).y * sign > 0.85 && s.pos[1].is_finite();
    let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
    for s in splats.iter().filter(level) {
        lo = lo.min(s.pos[1]);
        hi = hi.max(s.pos[1]);
    }
    if lo > hi {
        return None;
    }
    let bins = (((hi - lo) / BIN) as usize + 1).min(MAX_BINS);
    let bin_of = |y: f32| (((y - lo) / BIN) as usize).min(bins - 1);
    // Surface a splat stands for: its standard deviations undo the kernel.
    let inv = 1.0 / (kernel * kernel).max(1e-6);
    let (mut area, mut moment) = (vec![0.0f32; bins], vec![0.0f32; bins]);
    for s in splats.iter().filter(level) {
        let a = s.scale[0] * s.scale[1] * inv;
        let b = bin_of(s.pos[1]);
        area[b] += a;
        moment[b] += a * s.pos[1];
    }
    let window = |b: usize| {
        let range = b.saturating_sub(1)..(b + 2).min(bins);
        (area[range.clone()].iter().sum::<f32>(), moment[range].iter().sum::<f32>())
    };
    let largest = (0..bins).map(|b| window(b).0).fold(0.0f32, f32::max);
    let threshold = MIN_FLOOR_AREA.max(0.2 * largest);
    let pick = |b: &usize| window(*b).0 >= threshold;
    let found = if facing == Facing::Up { (0..bins).find(pick) } else { (0..bins).rev().find(pick) }?;
    // The surface is the densest of the bins around the first one that
    // qualified (the window of that one may only graze it).
    let peak = (found.saturating_sub(1)..(found + 2).min(bins)).max_by(|a, b| window(*a).0.total_cmp(&window(*b).0))?;
    let (a, m) = window(peak);
    Some(m / a)
}

/// `frame` reduced by an integer factor so that it is at most `max_width`
/// wide: colour averaged over every block, depth taken at its center.
fn shrink(frame: &RgbdFrame, max_width: u32) -> RgbdFrame {
    let step = frame.width.div_ceil(max_width.max(1)).max(1);
    if step == 1 {
        return frame.clone();
    }
    let (w, h) = ((frame.width / step).max(1), (frame.height / step).max(1));
    let mut out = RgbdFrame::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let mut sum = [0u32; 3];
            let mut count = 0u32;
            for sy in y * step..((y + 1) * step).min(frame.height) {
                for sx in x * step..((x + 1) * step).min(frame.width) {
                    let i = frame.index(sx, sy) * 3;
                    for (total, &value) in sum.iter_mut().zip(&frame.rgb[i..i + 3]) {
                        *total += value as u32;
                    }
                    count += 1;
                }
            }
            let dst = out.index(x, y);
            let center = frame.index((x * step + step / 2).min(frame.width - 1), (y * step + step / 2).min(frame.height - 1));
            out.depth[dst] = frame.depth[center];
            for (value, total) in out.rgb[dst * 3..dst * 3 + 3].iter_mut().zip(sum) {
                *value = ((total + count / 2) / count.max(1)) as u8;
            }
        }
    }
    out
}

/// `frame` turned clockwise by `quarter_turns` quarter turns, so that a
/// picture taken with the phone held upright is shown upright. Only the
/// preview is turned: the geometry stays in the sensor's orientation.
fn upright(frame: &RgbdFrame, quarter_turns: u8) -> RgbdFrame {
    let (w, h) = (frame.width, frame.height);
    let turns = quarter_turns % 4;
    if turns == 0 {
        return frame.clone();
    }
    let (ow, oh) = if turns == 2 { (w, h) } else { (h, w) };
    let mut out = RgbdFrame::new(ow, oh);
    for y in 0..oh {
        for x in 0..ow {
            // Source pixel that lands on (x, y).
            let (sx, sy) = match turns {
                1 => (y, h - 1 - x),
                2 => (w - 1 - x, h - 1 - y),
                _ => (w - 1 - y, x),
            };
            let (src, dst) = (frame.index(sx, sy), out.index(x, y));
            out.depth[dst] = frame.depth[src];
            out.rgb[dst * 3..dst * 3 + 3].copy_from_slice(&frame.rgb[src * 3..src * 3 + 3]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disc(y: f32, normal: Vec3, count: usize, sigma: f32) -> Vec<Splat> {
        // A square patch of `count` cells of 2 cm.
        let side = (count as f32).sqrt().ceil() as usize;
        (0..count)
            .map(|i| {
                let pos = Vec3::new((i % side) as f32 * 0.02, y, (i / side) as f32 * 0.02);
                Splat::surfel(pos, normal, sigma, 0.001, [128, 128, 128, 255])
            })
            .collect()
    }

    #[test]
    fn floor_is_the_lowest_large_level_surface_and_ceiling_the_highest() {
        let sigma = 0.45 * 0.02;
        let mut cloud = disc(-1.31, Vec3::Y, 4000, sigma); // floor, 1.6 m2
        cloud.extend(disc(-0.55, Vec3::Y, 2500, sigma)); // table, 1 m2
        cloud.extend(disc(1.12, Vec3::NEG_Y, 3000, sigma)); // ceiling
        cloud.extend(disc(-0.2, Vec3::NEG_Y, 2000, sigma)); // underside of the table
        cloud.extend(disc(-1.6, Vec3::Y, 40, sigma)); // reflections below the floor
        cloud.extend(disc(0.3, Vec3::X, 5000, sigma)); // a wall
        let floor = horizontal_level(&cloud, 0.45, Facing::Up).unwrap();
        let ceiling = horizontal_level(&cloud, 0.45, Facing::Down).unwrap();
        assert!((floor + 1.31).abs() < 0.005, "{floor}");
        assert!((ceiling - 1.12).abs() < 0.005, "{ceiling}");
        // A table alone is taken for the floor (an object on a table then
        // stands at y = 0); a patch too small is nothing.
        let table = disc(0.7, Vec3::Y, 2500, sigma);
        assert!((horizontal_level(&table, 0.45, Facing::Up).unwrap() - 0.7).abs() < 0.005);
        assert_eq!(horizontal_level(&disc(0.7, Vec3::Y, 300, sigma), 0.45, Facing::Up), None);
        assert_eq!(horizontal_level(&table, 0.45, Facing::Down), None);
        assert_eq!(horizontal_level(&[], 0.45, Facing::Up), None);
        // Noise spreads a floor over several bins; it is still one level.
        let noisy: Vec<Splat> = disc(0.0, Vec3::Y, 4000, sigma)
            .into_iter()
            .enumerate()
            .map(|(i, s)| Splat { pos: [s.pos[0], ((i * 37) % 21) as f32 * 0.001 - 0.01, s.pos[2]], ..s })
            .collect();
        assert!(horizontal_level(&noisy, 0.45, Facing::Up).unwrap().abs() < 0.01);
        // Absurd heights do not make an absurd histogram.
        let mut wild = disc(0.0, Vec3::Y, 2500, sigma);
        wild.push(Splat::surfel(Vec3::new(0.0, 1.0e9, 0.0), Vec3::Y, sigma, 0.001, [0; 4]));
        wild.push(Splat::surfel(Vec3::new(0.0, f32::NAN, 0.0), Vec3::Y, sigma, 0.001, [0; 4]));
        assert!(horizontal_level(&wild, 0.45, Facing::Up).is_some());
    }

    #[test]
    fn previews_are_shrunk_and_turned_upright() {
        // 4x2, each pixel its own colour and depth.
        let mut frame = RgbdFrame::new(4, 2);
        for i in 0..8 {
            frame.rgb[i * 3] = i as u8 * 10;
            frame.depth[i] = i as f32;
        }
        let reds = |f: &RgbdFrame| f.rgb.iter().step_by(3).copied().collect::<Vec<_>>();
        assert_eq!(upright(&frame, 0), frame);
        // Clockwise: the left column becomes the top row, read bottom-up.
        let cw = upright(&frame, 1);
        assert_eq!((cw.width, cw.height), (2, 4));
        assert_eq!(reds(&cw), [40, 0, 50, 10, 60, 20, 70, 30]);
        assert_eq!(cw.depth, [4.0, 0.0, 5.0, 1.0, 6.0, 2.0, 7.0, 3.0]);
        let half = upright(&frame, 2);
        assert_eq!(reds(&half), [70, 60, 50, 40, 30, 20, 10, 0]);
        let ccw = upright(&frame, 3);
        assert_eq!(reds(&ccw), [30, 70, 20, 60, 10, 50, 0, 40]);
        // Four quarter turns are none.
        assert_eq!(upright(&upright(&upright(&cw, 1), 1), 1), frame);

        let small = shrink(&frame, 2);
        assert_eq!((small.width, small.height), (2, 1));
        assert_eq!(reds(&small), [25, 45], "colour is averaged over the block");
        assert_eq!(shrink(&frame, 4), frame);
        assert_eq!(shrink(&frame, 0).width, 1);
    }
}
