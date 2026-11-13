//! Plain-data options, hints and statistics of the viewer.
//!
//! Nothing here touches the browser, so these types exist on every target.

use glam::{Mat4, Vec3};
use gs_core::codec::{LayerMode, PreviewMsg};
use gs_core::Aabb;

use crate::nav::NavMode;

/// Layer id of the static layer (background plate or accumulated scan).
pub const LAYER_STATIC: u8 = 0;
/// Layer id of the dynamic layer (replaced every tick in live mode).
pub const LAYER_DYNAMIC: u8 = 1;
/// Number of splat layers the viewer keeps.
pub const LAYER_COUNT: usize = 2;

/// How splats are drawn. Cheap to copy; change it with `Viewer::set_params`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RenderParams {
    /// Multiplies every standard deviation (1 = as reconstructed).
    pub splat_scale: f32,
    /// Draw every splat as a small dot of fixed size on screen.
    pub point_mode: bool,
    /// Standard deviation of the dots of `point_mode`, in drawing-buffer pixels.
    pub point_sigma_px: f32,
    /// Clear color, sRGB in `0..=1`.
    pub background: [f32; 3],
    /// Hide splats whose center is above this world height (meters). Used to
    /// remove the ceiling when looking into a room from above. `None` = off.
    ///
    /// In VR miniature mode the viewer applies a clip of its own when a
    /// ceiling height hint is known; the lower of the two wins.
    pub clip_y_max: Option<f32>,
    /// Upper bound of the half-axes of a splat's 3σ ellipse, in pixels. Huge
    /// splats are shrunk to this size, which bounds the fill-rate.
    pub max_radius_px: f32,
    /// Visibility of layer 0 (static).
    pub show_static: bool,
    /// Visibility of layer 1 (dynamic).
    pub show_dynamic: bool,
    /// Visibility of the gizmos (grid, camera frusta, scan path, axes, VR controllers).
    pub show_gizmos: bool,
    /// Resolution of the drawing buffer relative to the display
    /// (`CSS size × min(devicePixelRatio, max_dpr) × render_scale`). Desktop
    /// only; the VR resolution is `XrOptions::framebuffer_scale`.
    pub render_scale: f32,
}

impl Default for RenderParams {
    fn default() -> Self {
        Self {
            splat_scale: 1.0,
            point_mode: false,
            point_sigma_px: 0.8,
            background: [0.03, 0.035, 0.05],
            clip_y_max: None,
            max_radius_px: 1024.0,
            show_static: true,
            show_dynamic: true,
            show_gizmos: true,
            render_scale: 1.0,
        }
    }
}

impl RenderParams {
    /// Copy with every field forced into its valid range (NaN and nonsense
    /// become the default), so that the renderer never has to re-check.
    pub fn sanitized(&self) -> Self {
        let d = Self::default();
        let pick = |v: f32, lo: f32, hi: f32, default: f32| if v.is_finite() { v.clamp(lo, hi) } else { default };
        Self {
            splat_scale: pick(self.splat_scale, 0.0, 100.0, d.splat_scale),
            point_mode: self.point_mode,
            point_sigma_px: pick(self.point_sigma_px, 0.1, 64.0, d.point_sigma_px),
            background: self.background.map(|c| pick(c, 0.0, 1.0, 0.0)),
            clip_y_max: self.clip_y_max.filter(|y| y.is_finite()),
            max_radius_px: pick(self.max_radius_px, 1.0, 1.0e6, d.max_radius_px),
            show_static: self.show_static,
            show_dynamic: self.show_dynamic,
            show_gizmos: self.show_gizmos,
            render_scale: pick(self.render_scale, 0.1, 4.0, d.render_scale),
        }
    }
}

/// WebXR session options.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct XrOptions {
    /// `framebufferScaleFactor` of the `XRWebGLLayer` (1 = the runtime's
    /// recommended resolution). Applies to the next session.
    pub framebuffer_scale: f32,
    /// World meters per real meter in miniature ("dollhouse") mode: `15`
    /// shows the scene at 1:15.
    pub miniature_scale: f32,
    /// Smooth locomotion speed at 1:1, real meters per second. The trigger
    /// multiplies it by up to 3.
    pub move_speed: f32,
    /// Snap-turn angle of the right stick, radians.
    pub snap_angle: f32,
    /// Fixed foveation level in `0..=1` requested on the layer where the
    /// runtime supports it (Quest); `None` leaves the runtime default.
    pub foveation: Option<f32>,
}

impl Default for XrOptions {
    fn default() -> Self {
        Self {
            framebuffer_scale: 1.0,
            miniature_scale: 15.0,
            move_speed: 2.0,
            snap_angle: 30f32.to_radians(),
            foveation: Some(1.0),
        }
    }
}

/// Construction options of the viewer.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ViewerOptions {
    pub params: RenderParams,
    pub nav_mode: NavMode,
    /// Vertical field of view of the desktop camera, radians.
    pub fov_y: f32,
    /// Initial desktop camera position and look-at point.
    pub eye: Vec3,
    pub target: Vec3,
    /// `devicePixelRatio` is capped at this value when sizing the drawing buffer.
    pub max_dpr: f32,
    /// Orbit mode: turn slowly around the target when the user has been idle
    /// for a few seconds, at this speed in rad/s. `None` = off.
    pub auto_rotate: Option<f32>,
    pub xr: XrOptions,
}

impl Default for ViewerOptions {
    fn default() -> Self {
        Self {
            params: RenderParams::default(),
            nav_mode: NavMode::Orbit,
            fov_y: 50f32.to_radians(),
            eye: Vec3::new(3.0, 2.0, 4.5),
            target: Vec3::new(0.0, 1.0, 0.0),
            max_dpr: 2.0,
            auto_rotate: None,
            xr: XrOptions::default(),
        }
    }
}

/// What the viewer knows about the scene it shows. All optional; it only
/// improves the VR experience.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SceneHints {
    /// World height of the ceiling (meters). When set, VR miniature mode hides
    /// everything above `ceiling_height − CEILING_CLIP_MARGIN` so that one can
    /// look into the rooms from above.
    pub ceiling_height: Option<f32>,
    /// Point on the floor the scene is centered on. The miniature is placed
    /// with this point on the virtual table; its `y` is the floor height the
    /// user stands on at 1:1.
    pub floor_center: Vec3,
    /// Extent of the scene, used to keep the miniature at arm's length.
    pub bounds: Option<Aabb>,
}

impl SceneHints {
    /// The automatic ceiling clip of miniature mode sits this far below the
    /// ceiling (meters): it removes the ceiling surfels and their noise but
    /// keeps the walls and door lintels.
    pub const CEILING_CLIP_MARGIN: f32 = 0.1;
}

impl Default for SceneHints {
    fn default() -> Self {
        Self { ceiling_height: None, floor_center: Vec3::ZERO, bounds: None }
    }
}

/// Counters and timings, refreshed every animation frame.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ViewerStats {
    /// Animation-frame rate (display or headset refresh the page achieves), Hz.
    pub fps: f32,
    /// CPU time spent inside the viewer per animation frame, smoothed, ms.
    /// GPU time is not included (WebGL does not expose it).
    pub frame_ms: f32,
    /// Duration of the most recent depth sort including the index upload, ms.
    pub sort_ms: f32,
    /// Splats held in both layers.
    pub splats_total: u32,
    /// Splats submitted per view in the last rendered frame (visible layers,
    /// after the height clip).
    pub splats_drawn: u32,
    /// An immersive session is running.
    pub xr_active: bool,
    /// Size of the drawing buffer last rendered to (per eye in VR), pixels.
    pub canvas_width: u32,
    pub canvas_height: u32,
    /// Animation frames seen since the viewer was created (desktop + VR).
    pub frames: u64,
    /// Frames actually redrawn (the desktop loop skips frames in which
    /// nothing changed).
    pub frames_rendered: u64,
    /// Depth sorts performed.
    pub sorts: u64,
}

/// Result of `Viewer::apply_message`.
#[derive(Clone, Debug, PartialEq)]
pub enum Applied {
    /// A splat layer message; it has been uploaded to the GPU.
    Layer {
        layer: u8,
        mode: LayerMode,
        /// Splats carried by this message.
        count: u32,
        /// Splats in that layer afterwards.
        layer_total: u32,
        seq: u32,
        time_ms: u32,
    },
    /// Session state JSON (`gs_core::protocol::SessionState`), not interpreted.
    State(String),
    /// Statistics JSON (`gs_core::protocol::Stats`), not interpreted.
    Stats(String),
    /// A camera preview, not interpreted.
    Preview(PreviewMsg),
}

/// One eye of the running VR session, as rendered in the last frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct XrViewInfo {
    /// `[x, y, width, height]` inside the session framebuffer, pixels.
    pub viewport: [i32; 4],
    /// World-to-eye matrix (includes the rig and the world scale).
    pub view: Mat4,
    /// The runtime's projection matrix for this eye.
    pub projection: Mat4,
}

/// Snapshot of the running VR session for HUDs and tests.
#[derive(Clone, Debug, PartialEq)]
pub struct XrInfo {
    /// `"local-floor"` or `"local"`.
    pub reference_space: &'static str,
    /// XR animation frames delivered in this session.
    pub frames: u32,
    /// World position of the tracking-space origin.
    pub rig_position: Vec3,
    /// Rotation of the tracking space around +Y, radians.
    pub rig_yaw: f32,
    /// World meters per real meter (1 = life-size, 15 = miniature 1:15).
    pub world_scale: f32,
    pub miniature: bool,
    /// Head pose in world space (rig applied).
    pub world_from_head: Mat4,
    pub views: Vec<XrViewInfo>,
    /// Grip poses of the tracked controllers in world space: `(is_left, pose)`.
    pub controllers: Vec<(bool, Mat4)>,
}
