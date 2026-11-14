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

/// Blending weight of the live layer ([`LAYER_DYNAMIC`]) in the normalised
/// mode: `gs_core::experimental::wblend::LIVE_LAYER_WEIGHT`. The shadows the
/// dancer casts are live splats lying on the static floor; at equal weights
/// they come out at half strength and patchy.
pub const LIVE_LAYER_WEIGHT: f32 = gs_core::experimental::wblend::LIVE_LAYER_WEIGHT;

/// How overlapping splats are combined: the two blending modes of
/// `docs/ARCHITECTURE.md`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BlendMode {
    /// By content: [`BlendMode::Normalized`] while every visible layer holds
    /// sensor surfels ([`LayerContent::Surfels`]), [`BlendMode::Sorted`] as
    /// soon as one of them holds an imported file.
    #[default]
    Auto,
    /// Back-to-front "over" on premultiplied alpha, one CPU depth sort per
    /// view direction. Right for imported 3DGS files, which are translucent
    /// by nature, and the fallback where the GPU cannot render to half-float
    /// targets.
    Sorted,
    /// Order-independent weighted mean of the splats of the visible surface
    /// (visibility pass, accumulation, resolve): no CPU sort, no popping.
    /// Falls back to [`BlendMode::Sorted`] where the GPU cannot do it; see
    /// [`BlendStatus`].
    Normalized,
}

/// Quality level of [`BlendMode::Normalized`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BlendQuality {
    /// Hard silhouettes: the visible surface is opaque inside visibility
    /// discs of 1.6 standard deviations and ends there. One visibility pass
    /// and one half-float target; about half the fill-rate of
    /// [`BlendQuality::Soft`].
    Hard,
    /// Soft silhouettes over the next surface: a second visibility pass
    /// finds what lies behind the first surface, and the rim of the first
    /// fades into it. Two visibility passes and three accumulation targets.
    #[default]
    Soft,
}

/// What a layer holds, which decides how [`BlendMode::Auto`] draws it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LayerContent {
    /// Opaque surface samples from the capture pipeline: the opacity byte is
    /// a blending weight and the third axis of a splat is the surface
    /// normal. What `Viewer::apply_message` delivers.
    #[default]
    Surfels,
    /// A trained 3DGS scene (`.ply`, `.splat`): translucent Gaussians meant
    /// for sorted blending with the plain screen-space dilation they were
    /// optimized with. What `Viewer::set_layer` and `Viewer::append_layer`
    /// assume.
    Imported,
}

/// The blending a frame is actually drawn with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ActiveBlend {
    #[default]
    Sorted,
    NormalizedHard,
    NormalizedSoft,
}

impl ActiveBlend {
    /// Stable lower-case name (`"sorted"`, `"normalized-hard"`,
    /// `"normalized-soft"`) for logs, HUDs and tests.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sorted => "sorted",
            Self::NormalizedHard => "normalized-hard",
            Self::NormalizedSoft => "normalized-soft",
        }
    }

    pub fn is_normalized(self) -> bool {
        self != Self::Sorted
    }
}

/// Why [`BlendStatus::active`] is what it is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BlendReason {
    /// [`RenderParams::blend`] asked for exactly this mode.
    #[default]
    Requested,
    /// [`BlendMode::Auto`]: every visible layer holds surfels (or nothing).
    SurfelContent,
    /// [`BlendMode::Auto`]: a visible layer holds an imported file.
    ImportedContent,
    /// The normalised mode was wanted, but the GPU offers neither
    /// `EXT_color_buffer_half_float` nor `EXT_color_buffer_float`.
    NoFloatTargets,
    /// The normalised mode was wanted and the extension exists, but its
    /// shaders did not compile or its render targets could not be created
    /// (out of memory, an incomplete framebuffer); the browser console has
    /// the details.
    TargetsFailed,
}

impl BlendReason {
    /// Stable lower-case name for logs and tests.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::SurfelContent => "surfel-content",
            Self::ImportedContent => "imported-content",
            Self::NoFloatTargets => "no-float-targets",
            Self::TargetsFailed => "targets-failed",
        }
    }

    /// The viewer wanted the normalised mode and had to fall back.
    pub fn is_fallback(self) -> bool {
        matches!(self, Self::NoFloatTargets | Self::TargetsFailed)
    }
}

/// Blending mode in use and the reason for it: `Viewer::blend_status`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BlendStatus {
    pub active: ActiveBlend,
    pub reason: BlendReason,
}

/// Why the normalised mode cannot be used on this context, if it cannot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NormalizedSupport {
    Available,
    Unavailable(BlendReason),
}

impl BlendStatus {
    /// The mode for the given request. `content[i]` is what layer `i` holds,
    /// `None` when it is hidden or empty.
    pub(crate) fn resolve(
        blend: BlendMode,
        quality: BlendQuality,
        content: [Option<LayerContent>; LAYER_COUNT],
        support: NormalizedSupport,
    ) -> Self {
        let (wanted, reason) = match blend {
            BlendMode::Sorted => (false, BlendReason::Requested),
            BlendMode::Normalized => (true, BlendReason::Requested),
            BlendMode::Auto if content.contains(&Some(LayerContent::Imported)) => (false, BlendReason::ImportedContent),
            BlendMode::Auto => (true, BlendReason::SurfelContent),
        };
        match (wanted, support) {
            (false, _) => Self { active: ActiveBlend::Sorted, reason },
            (true, NormalizedSupport::Unavailable(why)) => Self { active: ActiveBlend::Sorted, reason: why },
            (true, NormalizedSupport::Available) => {
                let active = match quality {
                    BlendQuality::Hard => ActiveBlend::NormalizedHard,
                    BlendQuality::Soft => ActiveBlend::NormalizedSoft,
                };
                Self { active, reason }
            }
        }
    }
}

/// How splats are drawn. Cheap to copy; change it with `Viewer::set_params`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RenderParams {
    /// Blending mode. The default, [`BlendMode::Auto`], is right for the
    /// live stage, the house scan and imported files alike.
    pub blend: BlendMode,
    /// Quality level of the normalised mode (ignored by the sorted mode).
    pub quality: BlendQuality,
    /// Normalised mode: skip surfels whose normal points away from the eye.
    /// A surface is then never painted with the samples of its other side
    /// (measured in the lab: +0.2 dB and 20 % fewer fragments in the house).
    pub cull_backfaces: bool,
    /// Normalised mode: blending weight of each layer. Where splats of both
    /// layers lie on the same surface, the colour is their mean weighted by
    /// these (times the opacity byte of each splat). The default lets the
    /// live layer win 16 to 1 ([`LIVE_LAYER_WEIGHT`]).
    pub layer_weights: [f32; LAYER_COUNT],
    /// Sorted mode: extra factor on the standard deviations of surfel
    /// layers. The capture pipeline sizes its surfels for the normalised
    /// mode, where they only have to touch; stacked as translucent discs
    /// they must overlap more to close the surface. Not applied to imported
    /// layers.
    pub sorted_surfel_scale: f32,
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
            blend: BlendMode::Auto,
            quality: BlendQuality::Soft,
            cull_backfaces: true,
            layer_weights: [1.0, LIVE_LAYER_WEIGHT],
            sorted_surfel_scale: Self::SORTED_SURFEL_SCALE,
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
    /// Default of [`RenderParams::sorted_surfel_scale`].
    pub const SORTED_SURFEL_SCALE: f32 = 1.0;

    /// Copy with every field forced into its valid range (NaN and nonsense
    /// become the default), so that the renderer never has to re-check.
    pub fn sanitized(&self) -> Self {
        let d = Self::default();
        let pick = |v: f32, lo: f32, hi: f32, default: f32| if v.is_finite() { v.clamp(lo, hi) } else { default };
        let weight = |layer: usize| pick(self.layer_weights[layer], 0.0, 1.0e4, d.layer_weights[layer]);
        Self {
            blend: self.blend,
            quality: self.quality,
            cull_backfaces: self.cull_backfaces,
            layer_weights: [weight(0), weight(1)],
            sorted_surfel_scale: pick(self.sorted_surfel_scale, 0.0, 100.0, d.sorted_surfel_scale),
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
    /// Splats are fill-rate bound, so rendering the periphery coarser is the
    /// cheapest speed-up there is. Applies to the next session.
    pub foveation: Option<f32>,
}

impl Default for XrOptions {
    fn default() -> Self {
        Self { framebuffer_scale: 1.0, miniature_scale: 15.0, move_speed: 2.0, snap_angle: 30f32.to_radians(), foveation: Some(0.5) }
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
    /// The normalised blending mode never sorts: the value then stays what
    /// the last sorted frame left (0 if there was none).
    pub sort_ms: f32,
    /// Splats held in both layers.
    pub splats_total: u32,
    /// Splats drawn per view in the last rendered frame: those of the
    /// visible layers that are not above the height clip.
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
    /// Blending the next frame is drawn with, and why (the same as
    /// `Viewer::blend_status`).
    pub blend: BlendStatus,
    /// Size of the off-screen targets of the normalised mode, pixels: the
    /// size of the view last drawn with it, `0` while none are allocated
    /// (sorted mode, or nothing drawn yet).
    pub offscreen_width: u32,
    pub offscreen_height: u32,
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

#[cfg(test)]
mod tests {
    use super::*;

    const SURFELS: Option<LayerContent> = Some(LayerContent::Surfels);
    const IMPORTED: Option<LayerContent> = Some(LayerContent::Imported);

    #[test]
    fn auto_follows_the_content() {
        let resolve = |content| BlendStatus::resolve(BlendMode::Auto, BlendQuality::Soft, content, NormalizedSupport::Available);
        for content in [[SURFELS, SURFELS], [SURFELS, None], [None, SURFELS], [None, None]] {
            assert_eq!(resolve(content), BlendStatus { active: ActiveBlend::NormalizedSoft, reason: BlendReason::SurfelContent });
        }
        for content in [[IMPORTED, None], [IMPORTED, SURFELS], [SURFELS, IMPORTED], [IMPORTED, IMPORTED]] {
            assert_eq!(resolve(content), BlendStatus { active: ActiveBlend::Sorted, reason: BlendReason::ImportedContent });
        }
    }

    #[test]
    fn forced_modes_ignore_the_content_and_quality_selects_the_level() {
        let all = [[SURFELS, SURFELS], [IMPORTED, None], [None, None]];
        for content in all {
            let sorted = BlendStatus::resolve(BlendMode::Sorted, BlendQuality::Soft, content, NormalizedSupport::Available);
            assert_eq!(sorted, BlendStatus { active: ActiveBlend::Sorted, reason: BlendReason::Requested });
            let hard = BlendStatus::resolve(BlendMode::Normalized, BlendQuality::Hard, content, NormalizedSupport::Available);
            assert_eq!(hard, BlendStatus { active: ActiveBlend::NormalizedHard, reason: BlendReason::Requested });
            let soft = BlendStatus::resolve(BlendMode::Normalized, BlendQuality::Soft, content, NormalizedSupport::Available);
            assert_eq!(soft.active, ActiveBlend::NormalizedSoft);
            assert!(soft.active.is_normalized() && !sorted.active.is_normalized());
        }
    }

    #[test]
    fn missing_support_falls_back_to_sorted_with_its_reason() {
        for why in [BlendReason::NoFloatTargets, BlendReason::TargetsFailed] {
            let support = NormalizedSupport::Unavailable(why);
            for blend in [BlendMode::Auto, BlendMode::Normalized] {
                let status = BlendStatus::resolve(blend, BlendQuality::Soft, [SURFELS, SURFELS], support);
                assert_eq!(status, BlendStatus { active: ActiveBlend::Sorted, reason: why });
                assert!(status.reason.is_fallback());
            }
            // Sorted was wanted anyway: not a fallback.
            let imported = BlendStatus::resolve(BlendMode::Auto, BlendQuality::Soft, [IMPORTED, None], support);
            assert_eq!(imported.reason, BlendReason::ImportedContent);
            let forced = BlendStatus::resolve(BlendMode::Sorted, BlendQuality::Hard, [SURFELS, None], support);
            assert_eq!(forced.reason, BlendReason::Requested);
        }
    }

    #[test]
    fn defaults_are_the_documented_ones() {
        let d = RenderParams::default();
        assert_eq!((d.blend, d.quality, d.cull_backfaces), (BlendMode::Auto, BlendQuality::Soft, true));
        assert_eq!(d.layer_weights, [1.0, LIVE_LAYER_WEIGHT]);
        assert_eq!(d.sanitized(), d);
        let odd = RenderParams { layer_weights: [f32::NAN, -3.0], sorted_surfel_scale: f32::INFINITY, ..d }.sanitized();
        assert_eq!(odd.layer_weights, [1.0, 0.0]);
        assert_eq!(odd.sorted_surfel_scale, d.sorted_surfel_scale);
        assert_eq!(ActiveBlend::NormalizedSoft.as_str(), "normalized-soft");
        assert_eq!(BlendReason::NoFloatTargets.as_str(), "no-float-targets");
    }
}
