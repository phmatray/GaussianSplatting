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
use crate::reconstruct::{ReconstructParams, VoxelFusion};
use crate::rig::{RingRig, ScanPath, MAX_FOV_DEG};
use crate::scene::SceneKind;
use crate::types::{Aabb, Camera};

/// Version of the JSON protocol, reported in [`SessionState::version`].
/// Bumped on incompatible changes.
pub const PROTOCOL_VERSION: u32 = 1;

/// `value` when it is a finite number, `fallback` otherwise, then clamped.
#[inline]
fn finite_clamp(value: f32, fallback: f32, lo: f32, hi: f32) -> f32 {
    if value.is_finite() { value.clamp(lo, hi) } else { fallback }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// The handheld camera and the fusion of the home scan.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScanConfig {
    /// Edge length of a fusion cell in meters (one splat per cell). The
    /// splat count grows with its inverse square: about 275 000 for the
    /// whole house at 4 cm, 415 000 at 3.2 cm, 475 000 at 3 cm, 680 000 at
    /// 2.5 cm, for a gain of roughly 0.1 dB of held-out PSNR per 50 000
    /// splats.
    pub voxel_size: f32,
    /// Cruise walking speed along the scan path in meters per second.
    pub speed: f32,
    /// Vertical field of view of the handheld camera in degrees. The house
    /// needs at least 70 degrees to see floors and ceilings from eye height.
    /// With the sweep of [`crate::pipeline::scan_camera`] the default of 90
    /// (106 horizontally) leaves about 0.5 % of what a visitor can see
    /// unscanned; 80 leaves twice as much, 100 adds splats but no quality.
    pub fov_y_deg: f32,
    /// Image width of the handheld camera in pixels.
    pub width: u32,
    /// Image height of the handheld camera in pixels.
    pub height: u32,
    /// Depth returns beyond this distance are not fused (meters).
    pub max_range: f32,
    /// Frames captured per second of walking time. The frames are tied to
    /// the path, not to the tick rate: frame `i` is always taken at walking
    /// time `i / frames_per_second`.
    pub frames_per_second: f32,
    /// Start scanning as soon as the house scene is loaded.
    pub autostart: bool,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            voxel_size: 0.032,
            speed: ScanPath::DEFAULT_SPEED,
            fov_y_deg: 90.0,
            width: 192,
            height: 144,
            max_range: 6.0,
            frames_per_second: 6.0,
            autostart: true,
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
    /// (about sixteen 320x240 cameras). Larger rigs are scaled down.
    pub const MAX_RIG_PIXELS: u64 = 1_250_000;

    /// The default configuration for a scene: eight 160x120 cameras for the
    /// stage; a 192x144 handheld camera with a 90 degree vertical field of
    /// view and 3.2 cm cells for the house (about 410 000 splats for the
    /// whole scan).
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
            scan: ScanConfig::default(),
            preview: PreviewConfig::default(),
        }
    }

    /// A lighter configuration for single-threaded WebAssembly on a laptop or
    /// a standalone headset: six 120x90 ring cameras at 20 Hz, and a 144x108
    /// handheld camera with 4 cm cells.
    pub fn light(scene: SceneKind) -> Self {
        Self {
            rig: RingRig { count: 6, width: 120, height: 90, ..RingRig::default() },
            target_hz: 20.0,
            scan: ScanConfig { voxel_size: 0.04, width: 144, height: 108, frames_per_second: 5.0, ..ScanConfig::default() },
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
        Self { id: 0, world_from_cam: glam::Mat4::IDENTITY.to_cols_array(), fov_y: 1.0, width: 0, height: 0 }
    }
}

impl CameraInfo {
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
    /// The ring cameras (stage). Empty for the house, whose single moving
    /// camera is reported in [`ScanStats::camera`].
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

/// Scan section of [`Stats`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ScanStats {
    pub state: ScanState,
    /// Fraction of the path that has been fused, `0..=1`.
    pub progress: f32,
    /// Current pose of the handheld camera.
    pub camera: CameraInfo,
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
    /// Time spent turning frames into Gaussians.
    pub reconstruct_ms: f32,
    /// Time spent encoding wire messages.
    pub encode_ms: f32,
    /// Whole tick.
    pub total_ms: f32,
    /// Measured tick rate in Hz (smoothed), `0` until two ticks have run.
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
            scan: Some(ScanStats { state: ScanState::Scanning, progress: 0.25, camera: state.cameras[0] }),
            ..Stats::default()
        };
        let json = serde_json::to_string(&stats).unwrap();
        assert!(json.contains("\"state\":\"scanning\""));
        assert_eq!(serde_json::from_str::<Stats>(&json).unwrap(), stats);
        assert_eq!(serde_json::from_str::<Stats>("{}").unwrap(), Stats::default());
    }
}
