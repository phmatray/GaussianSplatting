//! VR locomotion: where the tracking space sits in the world.
//!
//! WebXR reports every pose (head, eyes, controllers) in a reference space
//! fixed to the real room, here called *rig space*. Moving through the scene
//! means moving that space inside the world, and the viewer does it with a
//! matrix (no `getOffsetReferenceSpace`, which would allocate JS objects every
//! frame):
//!
//! ```text
//! world_from_rig = T(pos) · R_y(yaw) · S(scale)
//! view(eye)      = inverse(rig_from_eye) · rig_from_world
//! ```
//!
//! `scale` is the number of world meters per real meter. At `1` the scene is
//! life-size; at `15` the user is a giant looking at a 1:15 miniature. Because
//! the scale is uniform and lives in the view matrix, the splat shader needs
//! nothing special (see step 5 of the recipe in `gs_core::splat`).
//!
//! Everything in this module is pure math so that it can be unit-tested
//! without a headset.

use glam::{Mat4, Vec3};

use crate::params::{SceneHints, XrOptions};

/// Thumbstick values below this magnitude count as zero.
pub const STICK_DEAD_ZONE: f32 = 0.15;
/// A snap turn fires when the stick passes this deflection ...
const SNAP_FIRE: f32 = 0.6;
/// ... and can fire again once it has come back below this one.
const SNAP_REARM: f32 = 0.3;
/// The trigger multiplies the speed by up to this factor.
const TRIGGER_BOOST: f32 = 3.0;
/// Speed at which the miniature slides past the user, real meters per second.
const MINIATURE_SPEED: f32 = 0.4;
/// Vertical speed relative to the horizontal one.
const VERTICAL_SPEED_RATIO: f32 = 0.5;
/// The miniature's floor sits this far below the user's eyes ...
const TABLE_BELOW_EYES: f32 = 0.65;
/// ... but at least this far above the real floor.
const TABLE_MIN_HEIGHT: f32 = 0.2;
/// Gap between the user and the near edge of the miniature, meters.
const TABLE_GAP: f32 = 0.45;
/// Head height assumed when the runtime only offers the eye-level `local`
/// reference space, meters.
pub const NOMINAL_EYE_HEIGHT: f32 = 1.6;

/// Placement of the tracking space in the world.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rig {
    /// World position of the tracking-space origin.
    pub pos: Vec3,
    /// Rotation of the tracking space around +Y, radians.
    pub yaw: f32,
    /// World meters per real meter.
    pub scale: f32,
}

impl Rig {
    pub fn world_from_rig(&self) -> Mat4 {
        Mat4::from_translation(self.pos) * Mat4::from_rotation_y(self.yaw) * Mat4::from_scale(Vec3::splat(self.scale))
    }

    pub fn rig_from_world(&self) -> Mat4 {
        Mat4::from_scale(Vec3::splat(1.0 / self.scale)) * Mat4::from_rotation_y(-self.yaw) * Mat4::from_translation(-self.pos)
    }

    /// Life-size rig for a user standing on the floor (`floor_y`) below `eye`
    /// and facing along `yaw` (the convention of [`crate::nav`]: 0 looks
    /// along −Z). `floor_rig_y` is the height of the real floor in rig space:
    /// 0 for `local-floor`, `−NOMINAL_EYE_HEIGHT` for `local`.
    pub fn standing(eye: Vec3, yaw: f32, floor_y: f32, floor_rig_y: f32) -> Self {
        Self { pos: Vec3::new(eye.x, floor_y - floor_rig_y, eye.z), yaw, scale: 1.0 }
    }

    /// Moves the rig so that the user travels `forward`/`strafe` real meters
    /// along/across the direction the head faces (projected on the floor) and
    /// `up` real meters vertically. `rig_from_head` is the head pose.
    pub fn travel(&mut self, rig_from_head: &Mat4, forward: f32, strafe: f32, up: f32) {
        let mut ahead = rig_from_head.transform_vector3(Vec3::NEG_Z);
        ahead.y = 0.0;
        // Looking straight up or down: fall back to the top of the head.
        if ahead.length_squared() < 1e-6 {
            ahead = rig_from_head.transform_vector3(Vec3::Y);
            ahead.y = 0.0;
        }
        let ahead = Mat4::from_rotation_y(self.yaw).transform_vector3(ahead.normalize_or(Vec3::NEG_Z));
        let side = ahead.cross(Vec3::Y);
        self.pos += (ahead * forward + side * strafe + Vec3::Y * up) * self.scale;
    }

    /// Rotates the world around the user's head by `angle` radians (positive
    /// turns the user to the left). The head keeps its world position, so the
    /// user does not swing sideways when standing off-center in the room.
    pub fn turn_about_head(&mut self, rig_from_head: &Mat4, angle: f32) {
        let head_rig = rig_from_head.w_axis.truncate();
        let head_world = self.world_from_rig().transform_point3(head_rig);
        self.yaw += angle;
        let offset = Mat4::from_rotation_y(self.yaw).transform_vector3(head_rig * self.scale);
        self.pos = head_world - offset;
    }

    /// Rig that shows the world as a miniature on a virtual table in front of
    /// the user: `hints.floor_center` lands a little below eye height, far
    /// enough ahead that the whole model (`hints.bounds`) is within reach.
    /// The world keeps the compass orientation given by `yaw`.
    pub fn miniature(hints: &SceneHints, rig_from_head: &Mat4, scale: f32, yaw: f32, floor_rig_y: f32) -> Self {
        let scale = if scale.is_finite() { scale.clamp(1.0, 1000.0) } else { 15.0 };
        let head = rig_from_head.w_axis.truncate();
        let mut ahead = rig_from_head.transform_vector3(Vec3::NEG_Z);
        ahead.y = 0.0;
        let ahead = ahead.normalize_or(Vec3::NEG_Z);
        let radius = hints
            .bounds
            .filter(|b| !b.is_empty())
            .map(|b| 0.5 * b.size().x.max(b.size().z) / scale)
            .filter(|r| r.is_finite())
            .unwrap_or(0.3)
            .clamp(0.05, 1.5);
        let table_y = (head.y - TABLE_BELOW_EYES).max(floor_rig_y + TABLE_MIN_HEIGHT);
        let place = Vec3::new(head.x, table_y, head.z) + ahead * (TABLE_GAP + radius);
        // rig_from_world(floor_center) = place  ⇒  pos = floor_center − R_y(yaw)·place·scale.
        let pos = hints.floor_center - Mat4::from_rotation_y(yaw).transform_vector3(place * scale);
        Self { pos, yaw, scale }
    }
}

/// One controller, read from its `xr-standard` gamepad.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PadState {
    /// Thumbstick `[x, y]`; `y` is −1 when pushed forward.
    pub stick: [f32; 2],
    /// Analog trigger in `0..=1`.
    pub trigger: f32,
    /// A (right) or X (left).
    pub primary: bool,
    /// B (right) or Y (left).
    pub secondary: bool,
}

/// What the controller buttons asked for in one frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LocomotionEvents {
    /// Left X was pressed: toggle the gizmos.
    pub toggle_gizmos: bool,
    /// Right A was pressed: the scale mode changed.
    pub miniature_changed: bool,
    /// Right B was pressed: the rig was reset.
    pub reset: bool,
}

/// Per-frame context of [`Locomotion::update`].
#[derive(Clone, Copy, Debug)]
pub struct LocomotionContext {
    pub options: XrOptions,
    pub hints: SceneHints,
    /// Height of the real floor in rig space (see [`Rig::standing`]).
    pub floor_rig_y: f32,
}

/// Rescales a stick axis so that it starts at 0 right outside the dead zone.
fn dead_zone(v: f32) -> f32 {
    let magnitude = v.abs();
    if magnitude.is_finite() && magnitude > STICK_DEAD_ZONE {
        v.signum() * ((magnitude - STICK_DEAD_ZONE) / (1.0 - STICK_DEAD_ZONE)).min(1.0)
    } else {
        0.0
    }
}

/// The rig plus everything that needs memory between frames: button edges,
/// the snap-turn latch, and where to go back to.
#[derive(Clone, Debug)]
pub struct Locomotion {
    rig: Rig,
    /// Life-size rig of the session start; right B returns here.
    home: Rig,
    /// Life-size rig to return to while the miniature is shown.
    life_size: Option<Rig>,
    snap_armed: bool,
    prev_gizmos: bool,
    prev_miniature: bool,
    prev_reset: bool,
}

impl Locomotion {
    pub fn new(home: Rig) -> Self {
        Self { rig: home, home, life_size: None, snap_armed: true, prev_gizmos: false, prev_miniature: false, prev_reset: false }
    }

    pub fn rig(&self) -> Rig {
        self.rig
    }

    pub fn is_miniature(&self) -> bool {
        self.life_size.is_some()
    }

    /// Switches between life-size and miniature.
    pub fn set_miniature(&mut self, on: bool, rig_from_head: &Mat4, ctx: &LocomotionContext) {
        match (on, self.life_size) {
            (true, None) => {
                self.life_size = Some(self.rig);
                self.rig = Rig::miniature(&ctx.hints, rig_from_head, ctx.options.miniature_scale, self.rig.yaw, ctx.floor_rig_y);
            }
            (false, Some(saved)) => {
                self.rig = saved;
                self.life_size = None;
            }
            _ => {}
        }
    }

    /// Life-size: back to where the session started. Miniature: the model is
    /// put back on the table in front of the user.
    pub fn reset(&mut self, rig_from_head: &Mat4, ctx: &LocomotionContext) {
        if self.is_miniature() {
            self.rig = Rig::miniature(&ctx.hints, rig_from_head, ctx.options.miniature_scale, self.home.yaw, ctx.floor_rig_y);
        } else {
            self.rig = self.home;
        }
    }

    /// Applies one frame of controller input. `rig_from_head` is the most
    /// recent head pose, `dt` the frame time in seconds.
    pub fn update(
        &mut self,
        left: &PadState,
        right: &PadState,
        rig_from_head: &Mat4,
        dt: f32,
        ctx: &LocomotionContext,
    ) -> LocomotionEvents {
        let mut events = LocomotionEvents::default();
        let pressed = |now: bool, prev: &mut bool| {
            let edge = now && !*prev;
            *prev = now;
            edge
        };
        events.toggle_gizmos = pressed(left.primary, &mut self.prev_gizmos);
        if pressed(right.primary, &mut self.prev_miniature) {
            self.set_miniature(!self.is_miniature(), rig_from_head, ctx);
            events.miniature_changed = true;
        }
        if pressed(right.secondary, &mut self.prev_reset) {
            self.reset(rig_from_head, ctx);
            events.reset = true;
        }

        // Snap turn with a latch: one turn per flick, however long it is held.
        let turn = right.stick[0];
        if self.snap_armed && turn.abs() > SNAP_FIRE {
            // Stick to the right turns the user to the right (negative yaw).
            self.rig.turn_about_head(rig_from_head, -turn.signum() * ctx.options.snap_angle);
            self.snap_armed = false;
        } else if turn.abs() < SNAP_REARM {
            self.snap_armed = true;
        }

        let dt = if dt.is_finite() { dt.clamp(0.0, 0.1) } else { 0.0 };
        let boost = 1.0 + (TRIGGER_BOOST - 1.0) * left.trigger.max(right.trigger).clamp(0.0, 1.0);
        let speed = if self.is_miniature() { MINIATURE_SPEED } else { ctx.options.move_speed };
        let meters = speed * boost * dt;
        let (strafe, forward) = (dead_zone(left.stick[0]), -dead_zone(left.stick[1]));
        // The right stick turns or lifts, whichever axis dominates, so that a
        // sloppy sideways flick does not also change the height.
        let lifting = right.stick[1].abs() > right.stick[0].abs();
        let up = if lifting { -dead_zone(right.stick[1]) * VERTICAL_SPEED_RATIO } else { 0.0 };
        if forward != 0.0 || strafe != 0.0 || up != 0.0 {
            self.rig.travel(rig_from_head, forward * meters, strafe * meters, up * meters);
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use glam::Quat;
    use gs_core::Aabb;

    use super::*;

    fn ctx() -> LocomotionContext {
        LocomotionContext { options: XrOptions::default(), hints: SceneHints::default(), floor_rig_y: 0.0 }
    }

    fn head(pos: Vec3, yaw: f32) -> Mat4 {
        Mat4::from_rotation_translation(Quat::from_rotation_y(yaw), pos)
    }

    fn assert_vec(a: Vec3, b: Vec3, tol: f32) {
        assert!((a - b).length() < tol, "{a} vs {b}");
    }

    #[test]
    fn matrices_are_inverse_and_scale_is_uniform() {
        let rig = Rig { pos: Vec3::new(1.0, 0.5, -2.0), yaw: 0.7, scale: 15.0 };
        let product = rig.world_from_rig() * rig.rig_from_world();
        assert!(product.abs_diff_eq(Mat4::IDENTITY, 1e-5));
        // One real meter is `scale` world meters, in every direction.
        for axis in [Vec3::X, Vec3::Y, Vec3::Z] {
            assert!((rig.world_from_rig().transform_vector3(axis).length() - 15.0).abs() < 1e-4);
        }
        assert_vec(rig.world_from_rig().transform_point3(Vec3::ZERO), rig.pos, 1e-6);
    }

    #[test]
    fn standing_puts_the_feet_on_the_floor() {
        let rig = Rig::standing(Vec3::new(2.0, 1.4, 3.0), 0.3, 0.0, 0.0);
        assert_eq!((rig.pos, rig.yaw, rig.scale), (Vec3::new(2.0, 0.0, 3.0), 0.3, 1.0));
        // Eye-level reference space: the origin is at head height instead.
        let rig = Rig::standing(Vec3::new(2.0, 1.4, 3.0), 0.3, 0.0, -NOMINAL_EYE_HEIGHT);
        assert_vec(rig.pos, Vec3::new(2.0, 1.6, 3.0), 1e-6);
    }

    #[test]
    fn left_stick_moves_along_the_head_direction() {
        let mut loco = Locomotion::new(Rig { pos: Vec3::ZERO, yaw: 0.0, scale: 1.0 });
        let left = PadState { stick: [0.0, -1.0], ..Default::default() };
        // Head turned 90° to the left looks along −X; pitch must not matter.
        let rig_from_head = head(Vec3::new(0.0, 1.6, 0.0), 90f32.to_radians()) * Mat4::from_rotation_x(-0.6);
        for _ in 0..90 {
            loco.update(&left, &PadState::default(), &rig_from_head, 1.0 / 90.0, &ctx());
        }
        assert_vec(loco.rig().pos, Vec3::new(-2.0, 0.0, 0.0), 1e-3);

        // The rig yaw rotates the direction; the trigger makes it faster.
        let mut loco = Locomotion::new(Rig { pos: Vec3::ZERO, yaw: 180f32.to_radians(), scale: 1.0 });
        let left = PadState { stick: [0.0, -1.0], trigger: 1.0, ..Default::default() };
        for _ in 0..90 {
            loco.update(&left, &PadState::default(), &head(Vec3::new(0.0, 1.6, 0.0), 0.0), 1.0 / 90.0, &ctx());
        }
        assert_vec(loco.rig().pos, Vec3::new(0.0, 0.0, 6.0), 1e-3);
    }

    #[test]
    fn strafe_and_vertical_axes() {
        let mut loco = Locomotion::new(Rig { pos: Vec3::ZERO, yaw: 0.0, scale: 1.0 });
        let left = PadState { stick: [1.0, 0.0], ..Default::default() };
        let right = PadState { stick: [0.0, -1.0], ..Default::default() };
        for _ in 0..90 {
            loco.update(&left, &right, &head(Vec3::new(0.0, 1.6, 0.0), 0.0), 1.0 / 90.0, &ctx());
        }
        assert_vec(loco.rig().pos, Vec3::new(2.0, 1.0, 0.0), 1e-3);

        // A diagonal flick of the right stick turns without changing the height.
        let mut loco = Locomotion::new(Rig { pos: Vec3::ZERO, yaw: 0.0, scale: 1.0 });
        let flick = PadState { stick: [0.9, -0.5], ..Default::default() };
        for _ in 0..30 {
            loco.update(&PadState::default(), &flick, &head(Vec3::new(0.0, 1.6, 0.0), 0.0), 1.0 / 90.0, &ctx());
        }
        assert_eq!(loco.rig().pos.y, 0.0);
        assert!((loco.rig().yaw + 30f32.to_radians()).abs() < 1e-6);
    }

    #[test]
    fn dead_zone_ignores_stick_drift() {
        let mut loco = Locomotion::new(Rig { pos: Vec3::ZERO, yaw: 0.0, scale: 1.0 });
        let drift = PadState { stick: [0.1, -0.12], ..Default::default() };
        let events = loco.update(&drift, &drift, &head(Vec3::new(0.0, 1.6, 0.0), 0.0), 0.1, &ctx());
        assert_eq!(loco.rig(), Rig { pos: Vec3::ZERO, yaw: 0.0, scale: 1.0 });
        assert_eq!(events, LocomotionEvents::default());
        assert_eq!(dead_zone(1.0), 1.0);
        assert_eq!(dead_zone(-1.0), -1.0);
        assert_eq!(dead_zone(f32::NAN), 0.0);
    }

    #[test]
    fn snap_turn_fires_once_per_flick_and_pivots_on_the_head() {
        let mut loco = Locomotion::new(Rig { pos: Vec3::new(1.0, 0.0, 2.0), yaw: 0.2, scale: 1.0 });
        let rig_from_head = head(Vec3::new(0.8, 1.6, -0.4), 0.3);
        let head_world = loco.rig().world_from_rig().transform_point3(Vec3::new(0.8, 1.6, -0.4));
        let right = PadState { stick: [1.0, 0.0], ..Default::default() };
        for _ in 0..30 {
            loco.update(&PadState::default(), &right, &rig_from_head, 1.0 / 90.0, &ctx());
        }
        assert!((loco.rig().yaw - (0.2 - 30f32.to_radians())).abs() < 1e-6, "held stick turns once");
        let head_after = loco.rig().world_from_rig().transform_point3(Vec3::new(0.8, 1.6, -0.4));
        assert_vec(head_after, head_world, 1e-5);

        // Half-way back is not enough to re-arm; the center is.
        let half = PadState { stick: [0.45, 0.0], ..Default::default() };
        loco.update(&PadState::default(), &half, &rig_from_head, 1.0 / 90.0, &ctx());
        loco.update(&PadState::default(), &right, &rig_from_head, 1.0 / 90.0, &ctx());
        assert!((loco.rig().yaw - (0.2 - 30f32.to_radians())).abs() < 1e-6);
        loco.update(&PadState::default(), &PadState::default(), &rig_from_head, 1.0 / 90.0, &ctx());
        let left_flick = PadState { stick: [-1.0, 0.0], ..Default::default() };
        loco.update(&PadState::default(), &left_flick, &rig_from_head, 1.0 / 90.0, &ctx());
        assert!((loco.rig().yaw - 0.2).abs() < 1e-6, "left flick turns back");
    }

    #[test]
    fn miniature_lands_on_a_table_in_front_of_the_user() {
        let hints = SceneHints {
            ceiling_height: Some(2.5),
            floor_center: Vec3::new(1.0, 0.0, -2.0),
            bounds: Some(Aabb::new(Vec3::new(-5.5, 0.0, -4.0), Vec3::new(5.5, 2.5, 4.0))),
        };
        // User stands off-center, facing −X.
        let rig_from_head = head(Vec3::new(0.5, 1.7, 0.2), 90f32.to_radians());
        let rig = Rig::miniature(&hints, &rig_from_head, 15.0, 0.4, 0.0);
        assert_eq!(rig.scale, 15.0);
        let center = rig.rig_from_world().transform_point3(hints.floor_center);
        // Below the eyes, straight ahead (−X), beyond the model's half-width (5.5 / 15).
        assert!((center.y - 1.05).abs() < 1e-4, "{center}");
        assert!((center.z - 0.2).abs() < 1e-4, "{center}");
        let ahead = 0.5 - center.x;
        assert!((ahead - (0.45 + 5.5 / 15.0)).abs() < 1e-4, "{center}");
        // A real meter of the model is 1/15 m on the table.
        let a = rig.rig_from_world().transform_point3(Vec3::ZERO);
        let b = rig.rig_from_world().transform_point3(Vec3::X);
        assert!(((a - b).length() - 1.0 / 15.0).abs() < 1e-6);

        // Seated / eye-level space: the table never sinks into the floor.
        let low = Rig::miniature(&hints, &head(Vec3::new(0.0, 0.0, 0.0), 0.0), 15.0, 0.0, -1.6);
        let center = low.rig_from_world().transform_point3(hints.floor_center);
        assert!((center.y - (-0.65)).abs() < 1e-4);
        let lying = Rig::miniature(&hints, &head(Vec3::new(0.0, 0.3, 0.0), 0.0), 15.0, 0.0, 0.0);
        assert!((lying.rig_from_world().transform_point3(hints.floor_center).y - 0.2).abs() < 1e-4);
        // Nonsense scale falls back instead of producing NaN.
        assert!(Rig::miniature(&hints, &rig_from_head, f32::NAN, 0.0, 0.0).pos.is_finite());
    }

    #[test]
    fn buttons_are_edge_triggered_and_miniature_toggles_back() {
        let home = Rig { pos: Vec3::new(3.0, 0.0, 4.0), yaw: 0.5, scale: 1.0 };
        let mut loco = Locomotion::new(home);
        let rig_from_head = head(Vec3::new(0.0, 1.6, 0.0), 0.0);
        let a = PadState { primary: true, ..Default::default() };
        let none = PadState::default();

        let events = loco.update(&none, &a, &rig_from_head, 0.011, &ctx());
        assert!(events.miniature_changed && loco.is_miniature());
        assert_eq!(loco.rig().scale, 15.0);
        // Still held: nothing more happens.
        let events = loco.update(&none, &a, &rig_from_head, 0.011, &ctx());
        assert!(!events.miniature_changed && loco.is_miniature());
        loco.update(&none, &none, &rig_from_head, 0.011, &ctx());
        // Moving the miniature, then toggling back, returns to the life-size spot.
        let push = PadState { stick: [0.0, -1.0], ..Default::default() };
        loco.update(&push, &none, &rig_from_head, 0.05, &ctx());
        let events = loco.update(&none, &a, &rig_from_head, 0.011, &ctx());
        assert!(events.miniature_changed && !loco.is_miniature());
        assert_eq!(loco.rig(), home);

        // Left X toggles the gizmos (once), right B resets to the start.
        let x = PadState { primary: true, ..Default::default() };
        assert!(loco.update(&x, &none, &rig_from_head, 0.011, &ctx()).toggle_gizmos);
        assert!(!loco.update(&x, &none, &rig_from_head, 0.011, &ctx()).toggle_gizmos);
        loco.update(&push, &none, &rig_from_head, 0.05, &ctx());
        assert_ne!(loco.rig(), home);
        let b = PadState { secondary: true, ..Default::default() };
        assert!(loco.update(&none, &b, &rig_from_head, 0.011, &ctx()).reset);
        assert_eq!(loco.rig(), home);
    }

    #[test]
    fn miniature_moves_slowly_in_real_meters() {
        let mut loco = Locomotion::new(Rig { pos: Vec3::ZERO, yaw: 0.0, scale: 1.0 });
        let rig_from_head = head(Vec3::new(0.0, 1.6, 0.0), 0.0);
        loco.set_miniature(true, &rig_from_head, &ctx());
        let start = loco.rig().pos;
        let push = PadState { stick: [0.0, -1.0], ..Default::default() };
        for _ in 0..100 {
            loco.update(&push, &PadState::default(), &rig_from_head, 0.01, &ctx());
        }
        // 0.4 real m/s for 1 s = 6 world meters at 1:15.
        assert_vec(loco.rig().pos - start, Vec3::new(0.0, 0.0, -6.0), 1e-2);
    }
}
