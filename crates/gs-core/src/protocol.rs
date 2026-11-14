//! serde types shared by pipeline, server, worker and UI.
//!
//! Everything here travels as JSON: [`SessionState`] and [`Stats`] inside
//! binary [`crate::codec`] messages from the pipeline to the viewers,
//! [`ControlMsg`] as text frames the other way. Field names are snake_case.
//! Structs that a client may send partially are `#[serde(default)]`: missing
//! fields take their default, unknown fields are ignored.
//!
//! Anything received from a client is untrusted. [`PipelineConfig::sanitized`]
//! forces every field into a range the pipeline can serve with bounded memory
//! and time; the pipeline applies it to every configuration it is given.

use glam::Vec3;
use serde::{Deserialize, Serialize};

use crate::capture::NoiseModel;
use crate::multiview::MultiViewParams;
use crate::reconstruct::{ReconstructParams, VoxelFusion};
use crate::rig::{RingRig, ScanPath, MAX_FOV_DEG};
use crate::scene::SceneKind;
use crate::types::{Aabb, Camera};

/// Version of the JSON protocol, reported in [`SessionState::version`].
/// Bumped on incompatible changes.
pub const PROTOCOL_VERSION: u32 = 1;

/// Splat scale a viewer applies to the pipeline's layers in the *sorted*
/// blending mode of `docs/ARCHITECTURE.md`.
///
/// The clouds are sized for the normalised mode, where the splats of a
/// surface only have to touch: ring splats of 0.45 pixel footprints that the
/// multi-view fusion no longer stacks eight deep, scan splats of 0.45 cell.
/// Composited as translucent discs they leave the surface open (the stage
/// loses 5 dB); 1.4 times larger they close it again.
pub const SORTED_SPLAT_SCALE: f32 = 1.4;

/// `value` when it is a finite number, `fallback` otherwise, then clamped.
#[inline]
fn finite_clamp(value: f32, fallback: f32, lo: f32, hi: f32) -> f32 {
    if value.is_finite() { value.clamp(lo, hi) } else { fallback }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// How one sensor is mounted on the scan head.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SensorMount {
    /// Rotation to the left of the head's forward axis, degrees.
    pub yaw_deg: f32,
    /// Tilt above the head's horizon, degrees.
    pub pitch_deg: f32,
}

/// The handheld sensor head of the home scan: one or more identical RGB-D
/// sensors rigidly mounted on a bracket that is carried along the scan path,
/// looking where the operator walks.
///
/// Sensor 0 always looks straight ahead; it is the "main" camera of
/// [`ScanStats::camera`] and of preview slot 0.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanHead {
    /// One sensor looking ahead, taking every frame. Walls beside the path,
    /// ceilings and floors stay patchy: about 5 % of what a visitor of the
    /// house can see is never scanned, against 0.4 % with the surround head.
    Single,
    /// Five sensors: ahead, back over the left and the right shoulder (120
    /// degrees to either side), tilted up and tilted down (60 degrees).
    /// Together they see nearly everything around the operator, so the head
    /// can be carried calmly instead of being swept about.
    #[default]
    Surround,
}

impl ScanHead {
    /// Most sensors any head has.
    pub const MAX_SENSORS: usize = 5;

    /// The sensors of the head; the index is the sensor id.
    pub fn mounts(self) -> &'static [SensorMount] {
        const fn mount(yaw_deg: f32, pitch_deg: f32) -> SensorMount {
            SensorMount { yaw_deg, pitch_deg }
        }
        // Three sensors 120 degrees apart around the vertical axis, so the
        // side ones also look back over the operator's shoulders: at 90
        // degrees they leave a fifth more of the visible surface unscanned.
        // Up and down are tilted 60 degrees, not 90: with a vertical field
        // of view of 80 degrees they overlap the forward sensor and still
        // reach past the zenith and the operator's feet.
        const SURROUND: [SensorMount; 5] =
            [mount(0.0, 0.0), mount(120.0, 0.0), mount(-120.0, 0.0), mount(0.0, 60.0), mount(0.0, -60.0)];
        match self {
            Self::Single => &SURROUND[..1],
            Self::Surround => &SURROUND,
        }
    }

    /// Number of sensors.
    pub fn sensor_count(self) -> u32 {
        self.mounts().len() as u32
    }

    /// The sensors fire one at a time, in this repeating order (depth
    /// sensors that fire together disturb each other, and one frame at a
    /// time keeps the work per tick small). Frame `i` of the scan is taken
    /// by sensor `firing_order()[i % len]`.
    ///
    /// The forward sensor takes every other frame: what lies ahead comes
    /// closer with every step and is worth refining, and its picture is the
    /// one a viewer watches.
    pub fn firing_order(self) -> &'static [u8] {
        match self {
            Self::Single => &[0],
            Self::Surround => &[0, 1, 0, 2, 0, 3, 0, 4],
        }
    }
}

/// The handheld sensor head and the fusion of the home scan.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScanConfig {
    /// Edge length of a fusion cell in meters (one splat per cell). The
    /// cells, not the sensor, limit the fidelity of the scan, and the splat
    /// count grows with the inverse square of their size: about 280 000 for
    /// the whole house at 4 cm, 424 000 at 3.2 cm, 756 000 at 2.4 cm (the
    /// default, 12 MB on the wire) and 1.7 million at 1.6 cm. Held-out views
    /// at 1280x960 under normalised blending gain 0.9 dB from 3.2 to 2.4 cm
    /// and 0.7 dB more at 1.6 cm.
    pub voxel_size: f32,
    /// Cruise walking speed along the scan path in meters per second.
    pub speed: f32,
    /// Vertical field of view of every sensor of the head in degrees. The
    /// house needs at least 70 degrees to see floors and ceilings from eye
    /// height. With [`ScanHead::Surround`] the default of 80 (96
    /// horizontally) leaves about 0.4 % of what a visitor can see unscanned.
    /// Wider sensors see hardly more and spread their pixels thinner: 90
    /// degrees costs 4 % more splats, 100 degrees 9 % more and 0.2 dB.
    pub fov_y_deg: f32,
    /// Image width of every sensor in pixels.
    pub width: u32,
    /// Image height of every sensor in pixels.
    pub height: u32,
    /// Depth returns beyond this distance are not fused (meters).
    pub max_range: f32,
    /// Sensor frames captured per second of walking time, all sensors of the
    /// head together: they fire one at a time, in
    /// [`ScanHead::firing_order`]. The frames are tied to the path, not to
    /// the tick rate: frame `i` is always taken at walking time
    /// `i / frames_per_second`.
    pub frames_per_second: f32,
    /// Start scanning as soon as the house scene is loaded.
    pub autostart: bool,
    /// The sensor head that is carried through the house.
    pub head: ScanHead,
    /// Tangential standard deviation of a scan splat in cells. The default
    /// of 0.45 is sized for the normalised blending mode of
    /// `docs/ARCHITECTURE.md`: its visibility discs (1.6 standard
    /// deviations) then just close the cell grid, and anything larger blurs
    /// (0.6 costs 1.2 dB there). Sorted "over" compositing needs 0.6 for a
    /// closed surface, which a viewer in that mode applies as a splat scale
    /// of 1.35.
    pub kernel: f32,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            voxel_size: 0.024,
            speed: ScanPath::DEFAULT_SPEED,
            fov_y_deg: 80.0,
            width: 192,
            height: 144,
            max_range: 6.0,
            frames_per_second: 6.0,
            autostart: true,
            head: ScanHead::default(),
            kernel: 0.45,
        }
    }
}

impl ScanConfig {
    /// Largest handheld image.
    pub const MAX_WIDTH: u32 = 640;
    pub const MAX_HEIGHT: u32 = 480;

    /// The same configuration with every field forced into its safe range.
    pub fn sanitized(self) -> Self {
        let d = Self::default();
        Self {
            voxel_size: finite_clamp(self.voxel_size, d.voxel_size, 0.015, VoxelFusion::MAX_VOXEL_SIZE),
            speed: finite_clamp(self.speed, d.speed, 0.1, 5.0),
            fov_y_deg: finite_clamp(self.fov_y_deg, d.fov_y_deg, 20.0, 120.0),
            width: self.width.clamp(PipelineConfig::MIN_IMAGE_SIZE, Self::MAX_WIDTH),
            height: self.height.clamp(PipelineConfig::MIN_IMAGE_SIZE, Self::MAX_HEIGHT),
            max_range: finite_clamp(self.max_range, d.max_range, 0.5, 20.0),
            frames_per_second: finite_clamp(self.frames_per_second, d.frames_per_second, 0.5, 60.0),
            autostart: self.autostart,
            head: self.head,
            kernel: finite_clamp(self.kernel, d.kernel, 0.2, 1.5),
        }
    }
}

/// Camera feed thumbnails ([`crate::codec::Kind::Preview`] messages).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PreviewConfig {
    /// Send previews at all.
    pub enabled: bool,
    /// Frames wider than this are downsampled by an integer factor.
    pub max_width: u32,
    /// How many ring cameras get a fresh preview each tick (round-robin).
    pub cameras_per_tick: u32,
}

impl Default for PreviewConfig {
    fn default() -> Self {
        Self { enabled: true, max_width: 96, cameras_per_tick: 1 }
    }
}

impl PreviewConfig {
    /// The same configuration with every field forced into its safe range.
    pub fn sanitized(self) -> Self {
        Self {
            enabled: self.enabled,
            max_width: self.max_width.clamp(16, 640),
            cameras_per_tick: self.cameras_per_tick.clamp(1, RingRig::MAX_COUNT),
        }
    }
}

/// Everything that configures a [`crate::pipeline::Pipeline`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PipelineConfig {
    /// Which scene is captured; it also selects the mode (live stage or
    /// home scan).
    pub scene: SceneKind,
    /// The ring of static cameras of the live stage.
    pub rig: RingRig,
    /// Tick rate the host should drive the pipeline at, in Hz. The pipeline
    /// itself never sleeps; this is advice for whoever calls `tick`.
    pub target_hz: f32,
    /// Scene seconds per real second (`0` freezes the motion while the
    /// pipeline keeps capturing).
    pub time_scale: f32,
    /// Stop time: nothing is captured or scanned while paused.
    pub paused: bool,
    /// Let the simulated lights cast shadows.
    pub shadows: bool,
    /// Simulated sensor noise.
    pub noise: NoiseModel,
    /// Live stage: send the empty stage once as the static layer and only
    /// foreground pixels per tick. When off, every tick sends everything in
    /// the live layer.
    pub background_subtraction: bool,
    /// Depth-to-Gaussian parameters.
    pub recon: ReconstructParams,
    /// Live stage: fusion of the ring cameras' splats, for the static layer
    /// and for the live one.
    pub fusion: MultiViewParams,
    /// Home scan parameters.
    pub scan: ScanConfig,
    /// Camera feed thumbnails.
    pub preview: PreviewConfig,
}

impl Default for PipelineConfig {
    /// [`PipelineConfig::for_scene`] of the stage.
    fn default() -> Self {
        Self::for_scene(SceneKind::Stage)
    }
}

impl PipelineConfig {
    /// Smallest image width or height the pipeline works with.
    pub const MIN_IMAGE_SIZE: u32 = 16;
    /// Largest ring camera image.
    pub const MAX_RIG_WIDTH: u32 = 640;
    pub const MAX_RIG_HEIGHT: u32 = 480;
    /// Upper bound on the pixels all ring cameras capture per tick together
    /// (about sixteen 320x240 cameras, or the largest ring of 24 at the
    /// default 240x180). Larger rigs are scaled down.
    pub const MAX_RIG_PIXELS: u64 = 1_250_000;

    /// The image sizes of the ring cameras a user interface should offer:
    /// the light preset's, the default and the finest that still ticks at
    /// 30 Hz on four desktop cores (its live layer is 10 MB/s).
    pub const RIG_RESOLUTIONS: [(u32, u32); 3] = [(160, 120), (240, 180), (320, 240)];

    /// The default configuration for a scene: eight 240x180 cameras for the
    /// stage, fused across views (about 85 000 static splats and 14 000 live
    /// ones, 6.8 MB/s at 30 Hz); a handheld head of five 192x144 sensors with
    /// an 80 degree vertical field of view and 2.4 cm cells for the house
    /// (about 756 000 splats for the whole scan, a walk of 67 s).
    pub fn for_scene(scene: SceneKind) -> Self {
        Self {
            scene,
            rig: RingRig::default(),
            target_hz: 30.0,
            time_scale: 1.0,
            paused: false,
            shadows: true,
            noise: NoiseModel::NONE,
            background_subtraction: true,
            recon: ReconstructParams::default(),
            fusion: MultiViewParams::default(),
            scan: ScanConfig::default(),
            preview: PreviewConfig::default(),
        }
    }

    /// A lighter configuration for single-threaded WebAssembly on a laptop or
    /// a standalone headset: eight 160x120 ring cameras at 20 Hz (about
    /// 38 000 static and 7 000 live splats), and a scan head of 144x108
    /// sensors taking 5 frames per second into 3.2 cm cells (about 430 000
    /// splats). On one native core a stage tick takes 10 ms and a scan tick
    /// 11 ms on average, 16 ms at the 95th percentile.
    pub fn light(scene: SceneKind) -> Self {
        Self {
            rig: RingRig { width: 160, height: 120, ..RingRig::default() },
            target_hz: 20.0,
            scan: ScanConfig { voxel_size: 0.032, width: 144, height: 108, frames_per_second: 5.0, ..ScanConfig::default() },
            preview: PreviewConfig { max_width: 64, ..PreviewConfig::default() },
            ..Self::for_scene(scene)
        }
    }

    /// The same configuration with every field forced into a range the
    /// pipeline can serve with bounded memory and time. Numbers that are not
    /// finite are replaced by their default. Sanitizing twice changes
    /// nothing more.
    pub fn sanitized(self) -> Self {
        let d = Self::for_scene(self.scene);
        let mut rig = self.rig.sanitized();
        let dr = RingRig::default();
        rig.radius = rig.radius.clamp(0.5, 20.0);
        rig.elevation = rig.elevation.clamp(0.05, 10.0);
        rig.upper_elevation = rig.upper_elevation.clamp(0.05, 10.0);
        rig.target = if rig.target.abs().max_element() <= 10.0 { rig.target } else { dr.target };
        rig.fov_y_deg = rig.fov_y_deg.clamp(10.0, 120.0f32.min(MAX_FOV_DEG));
        rig.width = rig.width.clamp(Self::MIN_IMAGE_SIZE, Self::MAX_RIG_WIDTH);
        rig.height = rig.height.clamp(Self::MIN_IMAGE_SIZE, Self::MAX_RIG_HEIGHT);
        let pixels = rig.count as u64 * rig.width as u64 * rig.height as u64;
        if pixels > Self::MAX_RIG_PIXELS {
            let shrink = (Self::MAX_RIG_PIXELS as f64 / pixels as f64).sqrt();
            rig.width = ((rig.width as f64 * shrink).floor() as u32).max(Self::MIN_IMAGE_SIZE);
            rig.height = ((rig.height as f64 * shrink).floor() as u32).max(Self::MIN_IMAGE_SIZE);
        }
        let noise = NoiseModel {
            depth_sigma_at_1m: finite_clamp(self.noise.depth_sigma_at_1m, 0.0, 0.0, 0.05),
            color_sigma: finite_clamp(self.noise.color_sigma, 0.0, 0.0, 64.0),
            dropout: finite_clamp(self.noise.dropout, 0.0, 0.0, 0.9),
        };
        Self {
            scene: self.scene,
            rig,
            target_hz: finite_clamp(self.target_hz, d.target_hz, 1.0, 120.0),
            time_scale: finite_clamp(self.time_scale, d.time_scale, 0.0, 8.0),
            paused: self.paused,
            shadows: self.shadows,
            noise,
            background_subtraction: self.background_subtraction,
            recon: self.recon.sanitized(),
            fusion: self.fusion.sanitized(),
            scan: self.scan.sanitized(),
            preview: self.preview.sanitized(),
        }
    }
}

// ---------------------------------------------------------------------------
// Pipeline -> viewer
// ---------------------------------------------------------------------------

/// Pose and optics of one camera, for gizmos and feed thumbnails.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CameraInfo {
    /// Index of the camera; previews carry the same id.
    pub id: u32,
    /// Camera-to-world matrix, column-major (`glam::Mat4::from_cols_array`).
    /// Camera space is +X right, +Y up, -Z forward.
    pub world_from_cam: [f32; 16],
    /// Vertical field of view in radians.
    pub fov_y: f32,
    /// Image size in pixels.
    pub width: u32,
    pub height: u32,
}

impl Default for CameraInfo {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl CameraInfo {
    /// The default: camera 0 at the origin, without an image.
    const IDENTITY: Self =
        Self { id: 0, world_from_cam: glam::Mat4::IDENTITY.to_cols_array(), fov_y: 1.0, width: 0, height: 0 };

    pub fn from_camera(id: u32, cam: &Camera) -> Self {
        Self {
            id,
            world_from_cam: glam::Mat4::from(cam.world_from_cam).to_cols_array(),
            fov_y: cam.fov_y(),
            width: cam.width,
            height: cam.height,
        }
    }
}

/// Where a free viewer should start.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ViewerStart {
    pub eye: [f32; 3],
    pub target: [f32; 3],
}

impl Default for ViewerStart {
    fn default() -> Self {
        Self { eye: [0.0, 1.6, 4.0], target: [0.0, 1.0, 0.0] }
    }
}

/// The scan path of the house, for drawing it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScanInfo {
    /// Points evenly spaced along the walk, start to end.
    pub path: Vec<[f32; 3]>,
    /// Walking time of the whole path in seconds at the configured speed.
    pub duration: f32,
}

/// The "State" message: everything a viewer needs to interpret the layers.
/// Sent first to every client and again whenever any of it changes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SessionState {
    /// [`PROTOCOL_VERSION`] of the sender.
    pub version: u32,
    /// The configuration in effect (already sanitized).
    pub config: PipelineConfig,
    /// Scene identifier: `"stage"` or `"house"`.
    pub scene: String,
    /// The ring cameras (stage). Empty for the house, whose moving sensor
    /// head is reported in [`ScanStats::camera`] and
    /// [`ScanStats::extra_cameras`].
    pub cameras: Vec<CameraInfo>,
    /// Box containing the scene at every instant.
    pub bounds: Aabb,
    pub viewer_start: ViewerStart,
    /// Period of the animation in seconds, if the scene loops.
    pub loop_duration: Option<f32>,
    /// Height of the ceiling above the floor (house). Lets a viewer clip the
    /// ceiling away for a dollhouse view.
    pub ceiling_height: Option<f32>,
    /// The scan path (house).
    pub scan: Option<ScanInfo>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            version: PROTOCOL_VERSION,
            config: PipelineConfig::default(),
            scene: String::new(),
            cameras: Vec::new(),
            bounds: Aabb::new(Vec3::splat(-1.0), Vec3::splat(1.0)),
            viewer_start: ViewerStart::default(),
            loop_duration: None,
            ceiling_height: None,
            scan: None,
        }
    }
}

/// Progress of the home scan.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScanState {
    /// Nothing scanned yet; waiting for [`ControlMsg::ScanStart`].
    #[default]
    Idle,
    /// Walking the path and fusing frames.
    Scanning,
    /// Stopped midway by [`ControlMsg::ScanPause`]; the map is kept.
    Paused,
    /// The whole path has been fused. The tick that announces it also
    /// replaces layer 0 with the final, refined map.
    Done,
}

/// A short list of cameras stored inline, so that [`Stats`] stays `Copy`.
/// It reads like a slice and travels as a plain JSON array.
#[derive(Clone, Copy, Debug)]
pub struct CameraList {
    len: u8,
    items: [CameraInfo; Self::CAPACITY],
}

impl CameraList {
    /// Most cameras the list holds.
    pub const CAPACITY: usize = 5;

    /// The empty list.
    pub fn new() -> Self {
        Self { len: 0, items: [CameraInfo::IDENTITY; Self::CAPACITY] }
    }

    /// Appends a camera. Returns `false`, and drops the camera, when the
    /// list is full.
    pub fn push(&mut self, camera: CameraInfo) -> bool {
        let Some(slot) = self.items.get_mut(self.len as usize) else { return false };
        *slot = camera;
        self.len += 1;
        true
    }

    pub fn as_slice(&self) -> &[CameraInfo] {
        &self.items[..self.len as usize]
    }
}

impl Default for CameraList {
    fn default() -> Self {
        Self::new()
    }
}

impl std::ops::Deref for CameraList {
    type Target = [CameraInfo];

    fn deref(&self) -> &[CameraInfo] {
        self.as_slice()
    }
}

impl PartialEq for CameraList {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl FromIterator<CameraInfo> for CameraList {
    /// Keeps the first [`CameraList::CAPACITY`] cameras.
    fn from_iter<I: IntoIterator<Item = CameraInfo>>(cameras: I) -> Self {
        let mut list = Self::new();
        for camera in cameras.into_iter().take(Self::CAPACITY) {
            list.push(camera);
        }
        list
    }
}

impl Serialize for CameraList {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.as_slice())
    }
}

impl<'de> Deserialize<'de> for CameraList {
    /// Reads an array of any length and keeps its first
    /// [`CameraList::CAPACITY`] cameras.
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;

        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = CameraList;

            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("an array of cameras")
            }

            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<CameraList, A::Error> {
                let mut list = CameraList::new();
                while let Some(camera) = seq.next_element::<CameraInfo>()? {
                    // Past the capacity the cameras are parsed and dropped.
                    list.push(camera);
                }
                Ok(list)
            }
        }

        deserializer.deserialize_seq(Visitor)
    }
}

/// Scan section of [`Stats`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScanStats {
    pub state: ScanState,
    /// Fraction of the path that has been fused, `0..=1`.
    pub progress: f32,
    /// Current pose of the main sensor of the head, the one looking ahead
    /// (sensor 0; its previews carry camera id 0).
    pub camera: CameraInfo,
    /// Current poses of the other sensors of the head, with their sensor
    /// ids (1 and up; their previews carry the same ids). Empty for
    /// [`ScanHead::Single`].
    #[serde(skip_serializing_if = "<[CameraInfo]>::is_empty")]
    pub extra_cameras: CameraList,
}

/// The "Stats" message, sent every tick.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Stats {
    /// Tick counter since the pipeline was created.
    pub tick: u64,
    /// Scene time in seconds (stage: position in the dance loop; house:
    /// walking time along the scan path).
    pub scene_time: f32,
    /// Time spent ray casting the sensors this tick, in milliseconds.
    pub capture_ms: f32,
    /// Time spent turning frames into Gaussians, multi-view fusion included.
    pub reconstruct_ms: f32,
    /// The part of `reconstruct_ms` spent in the multi-view fusion of the
    /// ring cameras (stage).
    pub fuse_ms: f32,
    /// Time spent encoding wire messages.
    pub encode_ms: f32,
    /// Whole tick.
    pub total_ms: f32,
    /// Measured tick rate in Hz: the inverse of the smoothed interval
    /// between ticks, `0` until two ticks have run.
    pub tick_hz: f32,
    /// Splats in the static layer 0.
    pub splats_static: u32,
    /// Splats in the live layer 1.
    pub splats_dynamic: u32,
    /// Bytes of all other messages emitted this tick (layers and previews).
    pub bytes: u32,
    /// Depth pixels captured this tick.
    pub pixels: u32,
    /// Home scan progress (house only).
    pub scan: Option<ScanStats>,
}

// ---------------------------------------------------------------------------
// Viewer -> pipeline
// ---------------------------------------------------------------------------

/// Commands a client sends to the pipeline, as JSON objects tagged by
/// `"type"`, for example `{"type":"set_paused","paused":true}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlMsg {
    /// Replace the whole configuration. Missing fields take their defaults,
    /// so send back the [`SessionState::config`] you received with your
    /// changes applied, or use [`ControlMsg::PatchConfig`].
    SetConfig { config: PipelineConfig },
    /// Change some configuration fields and keep the rest:
    /// `patch` is a partial [`PipelineConfig`] object that is merged into
    /// the current one (nested objects merge recursively), for example
    /// `{"type":"patch_config","patch":{"rig":{"count":12},"shadows":false}}`.
    PatchConfig { patch: serde_json::Value },
    /// Switch to another scene, keeping every other setting.
    SetScene { scene: SceneKind },
    /// Pause or resume time.
    SetPaused { paused: bool },
    /// Change the speed of time (scene seconds per second).
    SetTimeScale { time_scale: f32 },
    /// Start over: rewind the scene to time zero; in the house also discard
    /// the scan (it starts again if `scan.autostart` is set).
    Restart,
    /// House: start or resume walking the scan path. After a finished scan
    /// this starts a new one from scratch.
    ScanStart,
    /// House: stop walking; the map is kept and [`ControlMsg::ScanStart`]
    /// resumes. Layer 0 is sent again as a whole if splats were refined
    /// since they were appended.
    ScanPause,
    /// House: discard the map and go back to idle.
    ScanReset,
    /// House: fuse the rest of the path as fast as possible (a bounded
    /// amount of work per tick) instead of in real time. Starts the scan if
    /// it is idle or paused.
    ScanFinish,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The configurations a build of the last commit before the quality
    /// round (d1a8e3f) wrote, verbatim: the default stage and the light,
    /// noisy house.
    const D1A8E3F_STAGE: &str = r#"{"background_subtraction":true,"noise":{"color_sigma":0.0,"depth_sigma_at_1m":0.0,"dropout":0.0},"paused":false,"preview":{"cameras_per_tick":1,"enabled":false,"max_width":96},"recon":{"background":{"color_threshold":20.0,"depth_margin":0.019999999552965164,"depth_sigma_at_1m":0.001500000013038516,"min_neighbors":2,"noise_sigmas":4.0,"shadow_lift":0.6000000238418579},"edge_shrink":0.5,"edge_threshold":0.029999999329447746,"footprint":0.44999998807907104,"max_anisotropy":4.0,"min_cos":0.20000000298023224,"min_support":1,"opacity":0.8500000238418579,"smooth_passes":2,"thickness":0.10000000149011612},"rig":{"count":8,"elevation":1.2999999523162842,"fov_y_deg":50.0,"height":120,"radius":3.200000047683716,"target":[0.0,1.0,0.0],"tiers":2,"upper_elevation":2.5,"width":160},"scan":{"autostart":false,"fov_y_deg":80.0,"frames_per_second":6.0,"head":"surround","height":144,"max_range":6.0,"speed":0.800000011920929,"voxel_size":0.03200000151991844,"width":192},"scene":"stage","shadows":true,"target_hz":30.0,"time_scale":1.0}"#;
    const D1A8E3F_HOUSE_LIGHT: &str = r#"{"background_subtraction":true,"noise":{"color_sigma":2.0,"depth_sigma_at_1m":0.001500000013038516,"dropout":0.009999999776482582},"paused":false,"preview":{"cameras_per_tick":1,"enabled":false,"max_width":64},"recon":{"background":{"color_threshold":20.0,"depth_margin":0.019999999552965164,"depth_sigma_at_1m":0.001500000013038516,"min_neighbors":2,"noise_sigmas":4.0,"shadow_lift":0.6000000238418579},"edge_shrink":0.5,"edge_threshold":0.029999999329447746,"footprint":0.44999998807907104,"max_anisotropy":4.0,"min_cos":0.20000000298023224,"min_support":1,"opacity":0.8500000238418579,"smooth_passes":2,"thickness":0.10000000149011612},"rig":{"count":6,"elevation":1.2999999523162842,"fov_y_deg":50.0,"height":90,"radius":3.200000047683716,"target":[0.0,1.0,0.0],"tiers":2,"upper_elevation":2.5,"width":120},"scan":{"autostart":false,"fov_y_deg":80.0,"frames_per_second":5.0,"head":"surround","height":108,"max_range":6.0,"speed":0.800000011920929,"voxel_size":0.03999999910593033,"width":144},"scene":"house","shadows":true,"target_hz":20.0,"time_scale":1.0}"#;

    #[test]
    fn configurations_of_the_previous_release_parse_verbatim() {
        let stage: PipelineConfig = serde_json::from_str(D1A8E3F_STAGE).unwrap();
        // Everything that release wrote is kept, the sizes it ran at included
        // (they are not this release's defaults any more).
        assert_eq!((stage.rig.count, stage.rig.width, stage.rig.height), (8, 160, 120));
        assert_eq!((stage.scan.voxel_size, stage.recon.edge_shrink, stage.recon.footprint), (0.032, 0.5, 0.45));
        assert!(!stage.preview.enabled && !stage.scan.autostart && stage.noise.is_none());
        // What it did not know takes this release's defaults.
        assert_eq!(stage.fusion, MultiViewParams::default());
        assert_eq!((stage.scan.kernel, stage.recon.background.edge_shrink), (0.45, 0.3));
        // Nothing in it needs sanitising, and it differs from today's default
        // in exactly the settings whose defaults moved.
        assert_eq!(stage.sanitized(), stage);
        let mut today = PipelineConfig::for_scene(SceneKind::Stage);
        (today.rig.width, today.rig.height, today.scan.voxel_size) = (160, 120, 0.032);
        (today.preview.enabled, today.scan.autostart) = (false, false);
        assert_eq!(stage, today);

        let house: PipelineConfig = serde_json::from_str(D1A8E3F_HOUSE_LIGHT).unwrap();
        assert_eq!(house.scene, SceneKind::House);
        assert_eq!((house.rig.count, house.rig.width, house.scan.width, house.scan.voxel_size), (6, 120, 144, 0.04));
        assert_eq!((house.target_hz, house.noise, house.scan.head), (20.0, NoiseModel::realistic(), ScanHead::Surround));
        assert_eq!(house.sanitized(), house);

        // The messages that release sent and received.
        let control = format!(r#"{{"type":"set_config","config":{D1A8E3F_STAGE}}}"#);
        assert_eq!(serde_json::from_str::<ControlMsg>(&control).unwrap(), ControlMsg::SetConfig { config: stage });
        let stats = r#"{"tick":7,"scene_time":0.25,"capture_ms":6.1,"reconstruct_ms":1.4,"encode_ms":0.3,"total_ms":7.9,
            "tick_hz":29.5,"splats_static":68856,"splats_dynamic":9079,"bytes":170000,"pixels":153600,"scan":null}"#;
        let stats: Stats = serde_json::from_str(stats).unwrap();
        assert_eq!((stats.tick, stats.splats_static, stats.pixels, stats.fuse_ms), (7, 68856, 153_600, 0.0));
        assert!(stats.scan.is_none());
        // And back: that release's reader ignores unknown fields, so what it
        // reads of today's messages is today's values of the fields it knew.
        let json = serde_json::to_value(PipelineConfig::default()).unwrap();
        let old: serde_json::Value = serde_json::from_str(D1A8E3F_STAGE).unwrap();
        fn keys_are_kept(old: &serde_json::Value, new: &serde_json::Value, path: &str) {
            if let (Some(old), Some(new)) = (old.as_object(), new.as_object()) {
                for (key, value) in old {
                    let found = new.get(key).unwrap_or_else(|| panic!("{path}{key} is gone from the configuration"));
                    assert_eq!(value.is_object(), found.is_object(), "{path}{key} changed its shape");
                    assert!(value.is_object() || std::mem::discriminant(value) == std::mem::discriminant(found), "{path}{key} changed its type");
                    keys_are_kept(value, found, &format!("{path}{key}."));
                }
            }
        }
        keys_are_kept(&old, &json, "");
    }

    #[test]
    fn default_config_is_already_sane() {
        for kind in [SceneKind::Stage, SceneKind::House] {
            for config in [PipelineConfig::for_scene(kind), PipelineConfig::light(kind)] {
                assert_eq!(config.sanitized(), config, "{kind:?}");
                assert_eq!(config.scene, kind);
            }
        }
        assert_eq!(PipelineConfig::default(), PipelineConfig::for_scene(SceneKind::Stage));
        // The house needs a wide handheld camera.
        assert!(ScanConfig::default().fov_y_deg >= 70.0);
    }

    #[test]
    fn scan_head_travels_as_a_snake_case_name_and_defaults_to_surround() {
        assert_eq!(ScanConfig::default().head, ScanHead::Surround);
        assert_eq!(PipelineConfig::light(SceneKind::House).scan.head, ScanHead::Surround);
        let json = serde_json::to_string(&ScanConfig::default()).unwrap();
        assert!(json.contains(r#""head":"surround""#), "{json}");
        // A client written before the head existed does not send it.
        let old: ScanConfig = serde_json::from_str(r#"{"voxel_size":0.05,"frames_per_second":4}"#).unwrap();
        assert_eq!((old.head, old.voxel_size), (ScanHead::Surround, 0.05));
        let single: ScanConfig = serde_json::from_str(r#"{"head":"single"}"#).unwrap();
        assert_eq!(single.head, ScanHead::Single);
        assert_eq!(single.sanitized().head, ScanHead::Single);
        assert!(serde_json::from_str::<ScanConfig>(r#"{"head":"fisheye"}"#).is_err());
        assert!(serde_json::from_str::<ScanConfig>(r#"{"head":7}"#).is_err());

        assert_eq!((ScanHead::Single.sensor_count(), ScanHead::Surround.sensor_count()), (1, 5));
        // Stats hold the main camera plus the extra ones inline.
        const _: () = assert!(ScanHead::MAX_SENSORS - 1 <= CameraList::CAPACITY);
    }

    #[test]
    fn camera_list_is_a_bounded_inline_array() {
        let cam = |id: u32| CameraInfo { id, width: 64, height: 48, ..CameraInfo::default() };
        let mut list = CameraList::new();
        assert!(list.is_empty() && list == CameraList::default());
        assert_eq!(serde_json::to_string(&list).unwrap(), "[]");
        for id in 0..CameraList::CAPACITY as u32 {
            assert!(list.push(cam(id + 1)));
        }
        assert!(!list.push(cam(99)), "a full list drops further cameras");
        assert_eq!(list.len(), CameraList::CAPACITY);
        assert_eq!(list.iter().map(|c| c.id).collect::<Vec<_>>(), [1, 2, 3, 4, 5]);
        assert_eq!(list[1], cam(2));

        // Plain JSON array, both ways; equality ignores unused slots.
        let json = serde_json::to_string(&list).unwrap();
        assert!(json.starts_with(r#"[{"id":1,"#), "{json}");
        assert_eq!(serde_json::from_str::<CameraList>(&json).unwrap(), list);
        let two: CameraList = [cam(1), cam(2)].into_iter().collect();
        let mut other = list;
        other.len = 2;
        assert_eq!(other, two);
        assert_ne!(two, list);
        // Too many cameras are cut, not rejected; anything else is an error.
        let many = serde_json::to_string(&(1..=9).map(cam).collect::<Vec<_>>()).unwrap();
        assert_eq!(serde_json::from_str::<CameraList>(&many).unwrap(), list);
        assert_eq!((1..=9).map(cam).collect::<CameraList>(), list);
        assert!(serde_json::from_str::<CameraList>(r#"{"id":1}"#).is_err());
        assert!(serde_json::from_str::<CameraList>(r#"[{"id":"x"}]"#).is_err());
    }

    #[test]
    fn messages_written_before_the_scan_head_still_parse() {
        // Verbatim what protocol version 1 sent before the head existed: a
        // configuration without `scan.head`, Stats without `extra_cameras`.
        let config = r#"{"scene":"house","target_hz":30.0,"time_scale":1.0,"paused":false,"shadows":true,
            "background_subtraction":true,
            "scan":{"voxel_size":0.032,"speed":0.8,"fov_y_deg":90.0,"width":192,"height":144,"max_range":6.0,
                    "frames_per_second":6.0,"autostart":true},
            "preview":{"enabled":true,"max_width":96,"cameras_per_tick":1}}"#;
        let config: PipelineConfig = serde_json::from_str(config).unwrap();
        // The head is the new default; everything that was sent is kept, and
        // what came later (the splat kernel of the scan, the multi-view
        // fusion of the ring) takes its default.
        assert_eq!(config.scan, ScanConfig { fov_y_deg: 90.0, voxel_size: 0.032, ..ScanConfig::default() });
        assert_eq!((config.scan.kernel, config.fusion), (0.45, MultiViewParams::default()));
        assert!(config.fusion.enabled);
        assert_eq!(config.sanitized(), config);
        // Hostile values of the new fields are tamed like the old ones.
        let wild = r#"{"scan":{"kernel":1e30},"fusion":{"margin":-5,"tolerance":1e9,"min_normal_dot":7}}"#;
        let wild: PipelineConfig = serde_json::from_str(wild).unwrap();
        let tame = wild.sanitized();
        assert_eq!((tame.scan.kernel, tame.fusion.margin, tame.fusion.tolerance, tame.fusion.min_normal_dot), (1.5, 0.0, 0.5, 1.0));
        let mut nan = PipelineConfig::default();
        (nan.scan.kernel, nan.fusion.margin) = (f32::NAN, f32::NAN);
        assert_eq!(nan.sanitized(), PipelineConfig::default());
        let control = r#"{"type":"set_config","config":{"scene":"house","scan":{"voxel_size":0.04}}}"#;
        let ControlMsg::SetConfig { config } = serde_json::from_str(control).unwrap() else { panic!() };
        assert_eq!((config.scan.head, config.scan.voxel_size), (ScanHead::Surround, 0.04));

        let stats = r#"{"tick":12,"scene_time":1.5,"capture_ms":9.5,"reconstruct_ms":5.0,"encode_ms":0.2,
            "total_ms":15.0,"tick_hz":30.0,"splats_static":1000,"splats_dynamic":0,"bytes":16000,"pixels":27648,
            "scan":{"state":"scanning","progress":0.02,
                    "camera":{"id":0,"world_from_cam":[1,0,0,0,0,1,0,0,0,0,1,0,-0.3,1.4,3.4,1],"fov_y":1.5707964,
                              "width":192,"height":144}}}"#;
        let stats: Stats = serde_json::from_str(stats).unwrap();
        let scan = stats.scan.unwrap();
        assert_eq!((stats.tick, scan.state, scan.camera.width), (12, ScanState::Scanning, 192));
        assert_eq!(stats.fuse_ms, 0.0);
        assert!(scan.extra_cameras.is_empty());
        // And the other way: a reader that ignores unknown fields, as every
        // struct here does, loses nothing but the extra sensors.
        let state = r#"{"version":1,"scene":"house","config":{"scene":"house","scan":{"head":"single"}},"cameras":[]}"#;
        assert_eq!(serde_json::from_str::<SessionState>(state).unwrap().config.scan.head, ScanHead::Single);
    }

    #[test]
    fn hostile_camera_lists_stay_bounded() {
        let camera = serde_json::to_string(&CameraInfo::default()).unwrap();
        let thousand = format!("[{}]", vec![camera.as_str(); 1000].join(","));
        let list: CameraList = serde_json::from_str(&thousand).unwrap();
        assert_eq!(list.len(), CameraList::CAPACITY);
        assert!(std::mem::size_of::<Stats>() < 1024, "Stats is copied around every tick");
        let json = format!(r#"{{"scan":{{"extra_cameras":{thousand}}}}}"#);
        let stats: Stats = serde_json::from_str(&json).unwrap();
        assert_eq!(stats.scan.unwrap().extra_cameras.len(), CameraList::CAPACITY);
        assert_eq!(serde_json::from_str::<CameraList>("[]").unwrap(), CameraList::new());
        // Cameras with missing or absurd numbers are data like any other;
        // only something that is not a list of cameras is refused.
        let odd: CameraList = serde_json::from_str(r#"[{}, {"id":4000000000,"fov_y":-1e30,"width":0}]"#).unwrap();
        assert_eq!((odd.len(), odd[0], odd[1].id), (2, CameraInfo::default(), 4_000_000_000));
        for bad in ["null", "7", r#""all""#, "[7]", "[null]", r#"[{"world_from_cam":[1,2,3]}]"#, r#"[{"id":-1}]"#] {
            assert!(serde_json::from_str::<CameraList>(bad).is_err(), "{bad} was accepted");
        }
        // The head is a closed set of names: nothing numeric to clamp.
        for bad in ["null", "0", "1000", r#""""#, r#""Surround""#, "[]", r#"{"sensors":0}"#] {
            let json = format!(r#"{{"head":{bad}}}"#);
            assert!(serde_json::from_str::<ScanConfig>(&json).is_err(), "head {bad} was accepted");
        }
        for head in [ScanHead::Single, ScanHead::Surround] {
            assert!((1..=ScanHead::MAX_SENSORS).contains(&head.mounts().len()));
            assert!(!head.firing_order().is_empty());
            assert!(head.mounts().iter().all(|m| m.yaw_deg.abs() <= 180.0 && m.pitch_deg.abs() <= 90.0));
        }
    }

    #[test]
    fn light_preset_is_lighter() {
        let (full, light) = (PipelineConfig::for_scene(SceneKind::Stage), PipelineConfig::light(SceneKind::Stage));
        let pixels = |c: &PipelineConfig| c.rig.count * c.rig.width * c.rig.height;
        assert!(pixels(&light) * 2 < pixels(&full));
        assert!(light.scan.width * light.scan.height < full.scan.width * full.scan.height);
        assert!(light.scan.voxel_size > full.scan.voxel_size);
    }

    #[test]
    fn config_json_roundtrip_and_partial_input() {
        let mut config = PipelineConfig::light(SceneKind::House);
        config.noise = NoiseModel::realistic();
        config.recon.footprint = 0.8;
        let json = serde_json::to_string(&config).unwrap();
        assert_eq!(serde_json::from_str::<PipelineConfig>(&json).unwrap(), config);
        assert!(json.contains("\"scene\":\"house\"") && json.contains("\"background_subtraction\""));

        let partial: PipelineConfig =
            serde_json::from_str(r#"{"scene":"house","rig":{"count":5},"scan":{"voxel_size":0.05},"bogus":1}"#).unwrap();
        assert_eq!(partial.scene, SceneKind::House);
        assert_eq!(partial.rig.count, 5);
        assert_eq!(partial.rig.width, RingRig::default().width);
        assert_eq!(partial.scan.voxel_size, 0.05);
        assert_eq!(partial.scan.fov_y_deg, ScanConfig::default().fov_y_deg);
        assert_eq!(serde_json::from_str::<PipelineConfig>("{}").unwrap(), PipelineConfig::default());
    }

    #[test]
    fn hostile_config_is_clamped() {
        let json = r#"{
            "rig": {"count": 4000000000, "tiers": 99, "radius": 1e30, "elevation": -1e30, "target": [1e20, 0, 0],
                    "fov_y_deg": 1e9, "width": 4000000000, "height": 4000000000},
            "target_hz": 1e38, "time_scale": -5,
            "noise": {"depth_sigma_at_1m": 1e30, "color_sigma": -3, "dropout": 7},
            "recon": {"footprint": 1e30, "thickness": 0, "opacity": -1, "min_cos": 9, "edge_threshold": 0,
                      "max_anisotropy": 1e30, "min_support": 99, "smooth_passes": 4000000000,
                      "background": {"depth_margin": 1e30, "noise_sigmas": 1e30, "shadow_lift": 1e30}},
            "scan": {"voxel_size": 1e-30, "speed": 0, "fov_y_deg": 0, "width": 4000000000, "height": 0,
                     "max_range": 1e30, "frames_per_second": 1e30},
            "preview": {"max_width": 0, "cameras_per_tick": 4000000000}
        }"#;
        let c = serde_json::from_str::<PipelineConfig>(json).unwrap().sanitized();
        let rig_pixels = c.rig.count as u64 * c.rig.width as u64 * c.rig.height as u64;
        assert!(rig_pixels <= PipelineConfig::MAX_RIG_PIXELS, "{rig_pixels}");
        assert!(c.rig.count <= RingRig::MAX_COUNT && c.rig.width >= 16 && c.rig.height >= 16);
        assert!(c.rig.radius <= 20.0 && c.rig.elevation >= 0.05 && c.rig.fov_y_deg <= 120.0);
        assert_eq!(c.rig.target, RingRig::default().target);
        assert!(c.target_hz <= 120.0 && c.time_scale == 0.0);
        assert!(c.noise.depth_sigma_at_1m <= 0.05 && c.noise.color_sigma == 0.0 && c.noise.dropout <= 0.9);
        assert!(c.recon.footprint <= 4.0 && c.recon.thickness > 0.0 && c.recon.opacity > 0.0);
        assert!(c.recon.min_cos < 1.0 && c.recon.edge_threshold > 0.0 && c.recon.max_anisotropy <= 20.0);
        assert!(c.recon.min_support <= 4 && c.recon.smooth_passes <= ReconstructParams::MAX_SMOOTH_PASSES);
        assert!(c.scan.voxel_size >= 0.015 && c.scan.speed >= 0.1 && c.scan.fov_y_deg >= 20.0);
        assert!(c.scan.width <= ScanConfig::MAX_WIDTH && c.scan.height >= 16 && c.scan.max_range <= 20.0);
        assert!(c.scan.frames_per_second <= 60.0);
        assert!(c.preview.max_width >= 16 && c.preview.cameras_per_tick <= RingRig::MAX_COUNT);
        // Idempotent.
        assert_eq!(c.sanitized(), c);
    }

    #[test]
    fn non_finite_numbers_fall_back_to_defaults() {
        let mut c = PipelineConfig::for_scene(SceneKind::Stage);
        c.target_hz = f32::NAN;
        c.time_scale = f32::INFINITY;
        c.rig.radius = f32::NAN;
        c.recon.footprint = f32::NAN;
        c.recon.background.color_threshold = f32::NEG_INFINITY;
        c.scan.voxel_size = f32::NAN;
        c.noise.dropout = f32::NAN;
        let s = c.sanitized();
        let d = PipelineConfig::default();
        assert_eq!(s.target_hz, d.target_hz);
        assert_eq!(s.time_scale, d.time_scale);
        assert_eq!(s.rig.radius, d.rig.radius);
        assert_eq!(s.recon.footprint, d.recon.footprint);
        assert_eq!(s.recon.background.color_threshold, d.recon.background.color_threshold);
        assert_eq!(s.scan.voxel_size, d.scan.voxel_size);
        assert_eq!(s.noise.dropout, 0.0);
    }

    #[test]
    fn control_messages_use_snake_case_tags() {
        let cases = [
            (r#"{"type":"set_paused","paused":true}"#, ControlMsg::SetPaused { paused: true }),
            (r#"{"type":"set_scene","scene":"house"}"#, ControlMsg::SetScene { scene: SceneKind::House }),
            (r#"{"type":"set_time_scale","time_scale":0.5}"#, ControlMsg::SetTimeScale { time_scale: 0.5 }),
            (r#"{"type":"restart"}"#, ControlMsg::Restart),
            (r#"{"type":"scan_start"}"#, ControlMsg::ScanStart),
            (r#"{"type":"scan_pause"}"#, ControlMsg::ScanPause),
            (r#"{"type":"scan_reset"}"#, ControlMsg::ScanReset),
            (r#"{"type":"scan_finish"}"#, ControlMsg::ScanFinish),
            (
                r#"{"type":"set_config","config":{"scene":"house"}}"#,
                ControlMsg::SetConfig { config: PipelineConfig::for_scene(SceneKind::House) },
            ),
        ];
        for (json, expected) in cases {
            let parsed: ControlMsg = serde_json::from_str(json).unwrap();
            assert_eq!(parsed, expected, "{json}");
            let again: ControlMsg = serde_json::from_str(&serde_json::to_string(&parsed).unwrap()).unwrap();
            assert_eq!(again, expected);
        }
        let patch: ControlMsg = serde_json::from_str(r#"{"type":"patch_config","patch":{"shadows":false}}"#).unwrap();
        assert!(matches!(patch, ControlMsg::PatchConfig { .. }));
        assert!(serde_json::from_str::<ControlMsg>(r#"{"type":"format_disk"}"#).is_err());
        assert!(serde_json::from_str::<ControlMsg>(r#"{"paused":true}"#).is_err());
    }

    #[test]
    fn state_and_stats_roundtrip() {
        let cam = Camera::look_at(Vec3::new(1.0, 2.0, 3.0), Vec3::ZERO, Vec3::Y, 1.0, 64, 48);
        let state = SessionState {
            scene: "house".into(),
            cameras: vec![CameraInfo::from_camera(3, &cam)],
            ceiling_height: Some(2.5),
            scan: Some(ScanInfo { path: vec![[0.0, 1.4, 0.0], [1.0, 1.4, 0.0]], duration: 12.0 }),
            ..SessionState::default()
        };
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(serde_json::from_str::<SessionState>(&json).unwrap(), state);
        // Column-major: the translation is the last column.
        assert_eq!(&state.cameras[0].world_from_cam[12..15], &[1.0, 2.0, 3.0]);
        assert!((state.cameras[0].fov_y - 1.0).abs() < 1e-5);
        assert!(json.contains("\"bounds\":{\"min\":[") && json.contains("\"viewer_start\":{\"eye\":["));

        let stats = Stats {
            tick: 7,
            scan: Some(ScanStats {
                state: ScanState::Scanning,
                progress: 0.25,
                camera: state.cameras[0],
                ..ScanStats::default()
            }),
            ..Stats::default()
        };
        let json = serde_json::to_string(&stats).unwrap();
        assert!(json.contains("\"state\":\"scanning\""));
        assert_eq!(serde_json::from_str::<Stats>(&json).unwrap(), stats);
        assert_eq!(serde_json::from_str::<Stats>("{}").unwrap(), Stats::default());
        // Without extra sensors the message is what it was before they
        // existed, and such a message still parses.
        assert!(!json.contains("extra_cameras"), "{json}");

        let mut with_extras = stats;
        let scan = with_extras.scan.as_mut().unwrap();
        scan.extra_cameras = (1..=4).map(|id| CameraInfo { id, ..state.cameras[0] }).collect();
        let json = serde_json::to_string(&with_extras).unwrap();
        assert!(json.contains("\"extra_cameras\":[{\"id\":1,"), "{json}");
        let back: Stats = serde_json::from_str(&json).unwrap();
        assert_eq!(back, with_extras);
        assert_eq!(back.scan.unwrap().extra_cameras.iter().map(|c| c.id).collect::<Vec<_>>(), [1, 2, 3, 4]);
        // Stats stay `Copy`: hosts keep the latest one by value.
        let copy = with_extras;
        assert_eq!(copy, with_extras);
    }
}
