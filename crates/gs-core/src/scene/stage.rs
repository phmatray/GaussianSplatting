//! The live-capture stage: a round platform, an articulated dancer and three
//! bouncing balls.
//!
//! The dancer is a joint hierarchy posed by forward kinematics (spine, head,
//! arms) and analytic two-bone inverse kinematics (legs), so the feet stay
//! where the choreography plants them while the pelvis sways and bounces.
//! Every motion channel is a smooth function of the beat that is periodic
//! over [`LOOP_DURATION`], which makes the dance loop seamlessly.

use core::f32::consts::{PI, TAU};

use glam::{Affine3A, Mat3, Quat, Vec3};

use super::material::{srgb, Axis, Material};
use super::primitive::{Primitive, Shape};
use super::{Light, Lighting, Scene, SceneSnapshot};
use crate::types::Aabb;

/// Length of the dance loop in seconds (16 beats at 100 BPM).
pub const LOOP_DURATION: f32 = 9.6;
/// Radius of the stage disc; its top is the floor plane `y = 0`.
pub const STAGE_RADIUS: f32 = 2.2;
/// Radius of the cylinder (around the Y axis) that everything moving stays in.
pub const CAPTURE_RADIUS: f32 = 1.5;
/// Height of that cylinder above the stage.
pub const CAPTURE_HEIGHT: f32 = 2.3;

const BEATS: f32 = 16.0;
const STUDIO_FLOOR_RADIUS: f32 = 3.8;
const STAGE_THICKNESS: f32 = 0.12;

// Skeleton dimensions (meters). Standing height is about 1.75 m.
const ANKLE_HEIGHT: f32 = 0.08;
const SHIN: f32 = 0.42;
const THIGH: f32 = 0.44;
const HIP_OFFSET: Vec3 = Vec3::new(0.09, -0.03, 0.0);
const ABDOMEN_JOINT: f32 = 0.06;
const CHEST_JOINT: f32 = 0.15;
const NECK_JOINT: f32 = 0.27;
const HEAD_CENTER: f32 = 0.175;
const HEAD_RADII: Vec3 = Vec3::new(0.085, 0.115, 0.095);
const SHOULDER_OFFSET: Vec3 = Vec3::new(0.185, 0.215, 0.0);
const UPPER_ARM: f32 = 0.27;
const FOREARM: f32 = 0.24;
/// Where the feet stand in the body frame (left is +X).
const STANCE: f32 = 0.13;

/// World-space joint positions of the dancer at one instant.
///
/// "Left" and "right" are the dancer's own; index 0 is left, 1 is right.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DancerJoints {
    pub pelvis: Vec3,
    pub chest: Vec3,
    pub neck: Vec3,
    pub head: Vec3,
    pub shoulder: [Vec3; 2],
    pub elbow: [Vec3; 2],
    pub wrist: [Vec3; 2],
    pub hip: [Vec3; 2],
    pub knee: [Vec3; 2],
    pub ankle: [Vec3; 2],
    /// Sole contact point under each ankle as planted by the choreography
    /// (`y` is the lift above the floor).
    pub foot: [Vec3; 2],
    /// Unit vector the pelvis faces.
    pub facing: Vec3,
}

/// The volumetric-capture stage scene.
#[derive(Clone, Debug)]
pub struct StageScene {
    static_prims: Vec<Primitive>,
    lighting: Lighting,
}

impl Default for StageScene {
    fn default() -> Self {
        Self::new()
    }
}

impl StageScene {
    /// Builds the stage.
    pub fn new() -> Self {
        Self { static_prims: static_prims(), lighting: lighting() }
    }

    /// Joint positions of the dancer at time `t` (seconds).
    pub fn dancer_joints(&self, t: f32) -> DancerJoints {
        pose(t).joints
    }
}

impl Scene for StageScene {
    fn name(&self) -> &'static str {
        "stage"
    }

    fn snapshot(&self, t: f32) -> SceneSnapshot {
        let mut prims = Vec::with_capacity(self.static_prims.len() + 48);
        prims.extend_from_slice(&self.static_prims);
        dancer_prims(&pose(t), &mut prims);
        ball_prims(t, &mut prims);
        SceneSnapshot::new(prims, self.lighting.clone())
    }

    fn snapshot_static(&self) -> SceneSnapshot {
        SceneSnapshot::new(self.static_prims.clone(), self.lighting.clone())
    }

    fn bounds(&self) -> Aabb {
        Aabb::new(
            Vec3::new(-STUDIO_FLOOR_RADIUS, -STAGE_THICKNESS - 0.1, -STUDIO_FLOOR_RADIUS),
            Vec3::new(STUDIO_FLOOR_RADIUS, CAPTURE_HEIGHT, STUDIO_FLOOR_RADIUS),
        )
    }

    fn loop_duration(&self) -> Option<f32> {
        Some(LOOP_DURATION)
    }

    fn viewer_start(&self) -> (Vec3, Vec3) {
        (Vec3::new(0.6, 1.6, 4.0), Vec3::new(0.0, 0.95, 0.0))
    }
}

fn lighting() -> Lighting {
    Lighting {
        ambient_sky: Vec3::new(0.40, 0.41, 0.44),
        ambient_ground: Vec3::new(0.24, 0.23, 0.22),
        lights: vec![
            // Key light: warm, from the front left, casts the floor shadows.
            Light::directional(Vec3::new(-0.45, 1.0, 0.6), Vec3::new(0.60, 0.57, 0.52)),
            // Cool fill from behind, no shadows.
            Light::directional(Vec3::new(0.5, 0.6, -0.7), Vec3::new(0.10, 0.12, 0.16)).with_shadow(false),
        ],
        sky_zenith: Vec3::splat(0.012),
        sky_horizon: Vec3::splat(0.035),
    }
}

fn static_prims() -> Vec<Primitive> {
    let mut prims = Vec::new();
    // Studio floor around and below the stage.
    prims.push(Primitive::cylinder_y(
        Vec3::new(0.0, -STAGE_THICKNESS - 0.05, 0.0),
        STUDIO_FLOOR_RADIUS,
        0.05,
        Material::tiles(srgb(0x4a4d55), srgb(0x3b3e46), 0.6),
    ));
    // Stage disc with a polar checker so the floor reconstruction is readable.
    prims.push(Primitive::cylinder_y(
        Vec3::new(0.0, -0.5 * STAGE_THICKNESS, 0.0),
        STAGE_RADIUS,
        0.5 * STAGE_THICKNESS,
        Material::polar(srgb(0xe6e1d6), srgb(0x9fb4c8), Axis::Y, 0.44, 12),
    ));
    // Rim band slightly proud of the disc edge.
    prims.push(Primitive::cylinder_y(
        Vec3::new(0.0, -0.5 * STAGE_THICKNESS - 0.015, 0.0),
        STAGE_RADIUS + 0.05,
        0.5 * STAGE_THICKNESS - 0.015,
        Material::polar(srgb(0xd9772b), srgb(0x2b2d33), Axis::Y, 10.0, 48),
    ));
    // Four floor monitors near the edge of the stage, outside the capture volume.
    for i in 0..4 {
        let angle = PI * 0.25 + i as f32 * PI * 0.5;
        let pos = Vec3::new(angle.sin() * 1.92, 0.11, angle.cos() * 1.92);
        // Tilted so the grille faces the center of the stage.
        let rot = Quat::from_rotation_y(angle) * Quat::from_rotation_x(0.45);
        prims.push(Primitive::oriented_box(pos, rot, Vec3::new(0.2, 0.12, 0.14), Material::solid(srgb(0x24262b))));
        let grille = pos + rot * Vec3::new(0.0, 0.0, -0.141);
        prims.push(Primitive::oriented_box(
            grille,
            rot,
            Vec3::new(0.17, 0.09, 0.004),
            Material::checker(srgb(0x6a6f7a), srgb(0x3a3d44), 0.03),
        ));
    }
    prims
}

// ---------------------------------------------------------------------------
// Choreography
// ---------------------------------------------------------------------------

/// Hermite smoothstep of `x` between `a` and `b`.
#[inline]
fn ramp(x: f32, a: f32, b: f32) -> f32 {
    let s = ((x - a) / (b - a)).clamp(0.0, 1.0);
    s * s * (3.0 - 2.0 * s)
}

/// Quintic smoothstep (zero velocity and acceleration at both ends).
#[inline]
fn ramp5(x: f32, a: f32, b: f32) -> f32 {
    let s = ((x - a) / (b - a)).clamp(0.0, 1.0);
    s * s * s * (s * (s * 6.0 - 15.0) + 10.0)
}

/// Smooth 0 -> 1 -> 0 bump over `[a, b]`.
#[inline]
fn bump(x: f32, a: f32, b: f32) -> f32 {
    let s = ((x - a) / (b - a)).clamp(0.0, 1.0);
    let v = (PI * s).sin();
    v * v
}

/// Joint angles of one arm (radians).
#[derive(Clone, Copy, Default)]
struct Arm {
    /// Raise sideways: 0 hangs down, pi/2 is horizontal, pi points up.
    abduct: f32,
    /// Raise forward.
    flex: f32,
    /// Roll of the upper arm, turning the elbow's bending plane inward.
    twist: f32,
    /// Elbow bend.
    elbow: f32,
}

impl Arm {
    fn mix(self, o: Arm, w: f32) -> Arm {
        Arm {
            abduct: self.abduct + (o.abduct - self.abduct) * w,
            flex: self.flex + (o.flex - self.flex) * w,
            twist: self.twist + (o.twist - self.twist) * w,
            elbow: self.elbow + (o.elbow - self.elbow) * w,
        }
    }
}

/// All animation channels at one instant, in the body frame (+Z forward,
/// +X to the dancer's left).
#[derive(Clone, Copy, Default)]
struct Channels {
    yaw: f32,
    pelvis: Vec3,
    /// Pelvis twist (about Y), roll (about Z) and pitch (about X).
    pelvis_rot: Vec3,
    /// Abdomen twist / side bend / forward bend.
    abdomen: Vec3,
    chest: Vec3,
    /// Head yaw / roll / pitch.
    head: Vec3,
    arms: [Arm; 2],
    /// Sole positions (y = lift) and toe-out angles.
    feet: [Vec3; 2],
    foot_yaw: [f32; 2],
    /// How far each knee is turned outward.
    knee_out: [f32; 2],
}

fn channels(t: f32) -> Channels {
    let b = (t / LOOP_DURATION).rem_euclid(1.0) * BEATS;

    // Phase weights: groove (beats 0-8), wide-stance squats (8-12), spin (12-16).
    let wide = ramp(b, 8.0, 8.9) - ramp(b, 11.6, 12.5);
    let spin = ramp(b, 12.1, 12.9) - ramp(b, 15.0, 15.8);
    let groove = (1.0 - wide - spin).max(0.0);

    // Oscillators. Every period divides the 16-beat loop.
    let sway = (PI * b).sin(); // weight left (+) / right (-), 2 beats
    let bounce = 0.5 - 0.5 * (TAU * b).cos(); // 0 on the beat, 1 between
    let slow = (PI * 0.5 * b).sin(); // 4 beats
    let slow_lag = (PI * 0.5 * b - 1.1).sin();

    let mut c = Channels::default();

    // One full turn on the left foot.
    c.yaw = TAU * ramp5(b, 12.7, 15.2) - 0.25 * bump(b, 12.0, 12.9);

    // Pelvis: sways over the supporting foot, bounces on the beat, sinks in
    // the wide stance and rises over the pivot foot for the spin.
    let stance_center = -0.075 * wide;
    c.pelvis = Vec3::new(
        (1.0 - spin) * (stance_center + sway * (0.07 + 0.03 * wide)) + spin * 0.105,
        0.925 - 0.035 * bounce * (1.0 - spin) - wide * (0.10 + 0.09 * bounce) + spin * 0.03,
        0.015 * (PI * b + 0.6).sin() * groove,
    );
    c.pelvis_rot = Vec3::new(
        0.22 * (PI * b + 0.9).sin() * (1.0 - spin),
        (0.09 * groove + 0.05 * wide) * sway,
        0.06 * wide,
    );

    // Torso counter-rotates against the hips and leans with the sway.
    c.abdomen = Vec3::new(
        -0.20 * (PI * b + 0.9).sin() * (1.0 - spin),
        (-0.07 * groove + 0.10 * wide) * sway,
        0.05 + 0.10 * wide,
    );
    c.chest = Vec3::new(
        (-0.18 * groove - 0.3 * wide) * (PI * b + 0.9).sin(),
        (-0.05 * groove + 0.12 * wide) * sway,
        0.04 * bounce * groove - 0.06 * wide,
    );
    c.head = Vec3::new(
        0.30 * slow * groove + 0.2 * sway * wide,
        0.08 * sway * (1.0 - spin),
        0.07 * (TAU * b).cos() * groove - 0.12 * wide,
    );

    // Arms.
    let up_l = 0.5 + 0.5 * slow;
    let up_r = 0.5 - 0.5 * slow;
    let lag_l = 0.5 + 0.5 * slow_lag;
    let lag_r = 0.5 - 0.5 * slow_lag;
    let groove_arm = |up: f32, lag: f32| Arm {
        abduct: 0.30 + 1.25 * up,
        flex: 0.20 + 0.25 * bounce * (1.0 - up),
        twist: 0.3 + 0.5 * lag,
        elbow: 0.35 + 0.95 * lag * (0.4 + 0.6 * up),
    };
    let wide_arm = |side: f32| Arm {
        abduct: 2.35 + side * 0.40 * sway,
        flex: 0.10,
        twist: 0.9,
        elbow: 0.55 + 0.35 * bounce,
    };
    // Arms gather in front of the chest for the spin, then open.
    let open = bump(b, 14.6, 15.9);
    let spin_arm = Arm { abduct: 0.55 + 0.8 * open, flex: 0.75 - 0.3 * open, twist: 1.25, elbow: 1.75 - 1.0 * open };
    let groove_or_wide = |ga: Arm, wa: Arm| {
        let w = if groove + wide > 1e-6 { wide / (groove + wide) } else { 0.0 };
        ga.mix(wa, w).mix(spin_arm, spin)
    };
    c.arms = [groove_or_wide(groove_arm(up_l, lag_l), wide_arm(1.0)), groove_or_wide(groove_arm(up_r, lag_r), wide_arm(-1.0))];

    // Feet. The left foot is the pivot and never travels; it only taps.
    let tap_l = (-sway).max(0.0).powi(2) * groove;
    let tap_r = sway.max(0.0).powi(2) * groove;
    c.feet[0] = Vec3::new(STANCE, 0.055 * tap_l, 0.0);
    // The right foot steps out for the wide stance, tucks up to the left
    // knee during the spin (passe) and lands back in place.
    let step_out = ramp(b, 8.05, 8.85) - ramp(b, 12.05, 12.85);
    let tuck = ramp(b, 12.05, 12.95) - ramp(b, 14.9, 15.75);
    let rest = Vec3::new(-STANCE - 0.17 * step_out, 0.0, 0.0);
    let tucked = Vec3::new(0.0, 0.34, 0.13);
    let step_lift = 0.07 * (bump(b, 8.05, 8.85) + bump(b, 12.05, 12.85) * (1.0 - tuck));
    c.feet[1] = rest.lerp(tucked, tuck) + Vec3::Y * (0.055 * tap_r + step_lift);
    c.foot_yaw = [0.28 + 0.1 * wide, -0.28 - 0.25 * wide + 0.2 * tuck];
    c.knee_out = [0.25 + 0.5 * wide, 0.25 + 0.5 * wide + 1.1 * tuck];
    c
}

// ---------------------------------------------------------------------------
// Kinematics
// ---------------------------------------------------------------------------

/// Posed skeleton: rigid frames in world space for every body part.
struct Pose {
    pelvis: Affine3A,
    abdomen: Affine3A,
    chest: Affine3A,
    head: Affine3A,
    /// Frames centered on the bones, local +Y pointing toward the parent joint.
    upper_arm: [Affine3A; 2],
    forearm: [Affine3A; 2],
    thigh: [Affine3A; 2],
    shin: [Affine3A; 2],
    /// Frames at the ankle with +Z toward the toes.
    foot: [Affine3A; 2],
    joints: DancerJoints,
}

#[inline]
fn rot(yaw: f32, roll: f32, pitch: f32) -> Affine3A {
    Affine3A::from_quat(Quat::from_rotation_y(yaw) * Quat::from_rotation_z(roll) * Quat::from_rotation_x(pitch))
}

#[inline]
fn offset(v: Vec3) -> Affine3A {
    Affine3A::from_translation(v)
}

/// Rigid frame centered between `top` and `bottom`, +Y toward `top`, +Z as
/// close to `forward` as possible.
fn bone_frame(top: Vec3, bottom: Vec3, forward: Vec3) -> Affine3A {
    let y = (top - bottom).normalize_or(Vec3::Y);
    let z = (forward - y * forward.dot(y)).normalize_or(y.any_orthonormal_vector());
    let x = y.cross(z);
    Affine3A::from_mat3_translation(Mat3::from_cols(x, y, z), 0.5 * (top + bottom))
}

/// Analytic two-bone IK: knee position for a hip, an ankle target and a pole
/// direction the knee should point to. Returns `(knee, reachable ankle)`.
fn solve_leg(hip: Vec3, ankle: Vec3, pole: Vec3) -> (Vec3, Vec3) {
    let to_ankle = ankle - hip;
    let dist = to_ankle.length().clamp((THIGH - SHIN).abs() + 1e-3, THIGH + SHIN - 1e-3);
    let dir = to_ankle.normalize_or(Vec3::NEG_Y);
    let ankle = hip + dir * dist;
    // Distance from the hip to the foot of the knee's perpendicular.
    let a = (THIGH * THIGH - SHIN * SHIN + dist * dist) / (2.0 * dist);
    let h = (THIGH * THIGH - a * a).max(0.0).sqrt();
    let side = (pole - dir * pole.dot(dir)).normalize_or(dir.any_orthonormal_vector());
    (hip + dir * a + side * h, ankle)
}

fn pose(t: f32) -> Pose {
    let c = channels(t);

    // Body frame: yawed about the left (pivot) foot so that foot never slides.
    let pivot = Vec3::new(STANCE, 0.0, 0.0);
    let yaw = Quat::from_rotation_y(c.yaw);
    let body = Affine3A::from_rotation_translation(yaw, pivot - yaw * pivot);

    let pelvis = body * offset(c.pelvis) * rot(c.pelvis_rot.x, c.pelvis_rot.y, c.pelvis_rot.z);
    let abdomen = pelvis * offset(Vec3::Y * ABDOMEN_JOINT) * rot(c.abdomen.x, c.abdomen.y, c.abdomen.z);
    let chest = abdomen * offset(Vec3::Y * CHEST_JOINT) * rot(c.chest.x, c.chest.y, c.chest.z);
    let neck = chest * offset(Vec3::Y * NECK_JOINT) * rot(c.head.x, c.head.y, c.head.z);
    let head = neck * offset(Vec3::Y * HEAD_CENTER);

    let mut upper_arm = [Affine3A::IDENTITY; 2];
    let mut forearm = [Affine3A::IDENTITY; 2];
    let mut thigh = [Affine3A::IDENTITY; 2];
    let mut shin = [Affine3A::IDENTITY; 2];
    let mut foot = [Affine3A::IDENTITY; 2];
    let mut joints = DancerJoints {
        pelvis: pelvis.translation.into(),
        chest: chest.translation.into(),
        neck: neck.translation.into(),
        head: head.translation.into(),
        shoulder: [Vec3::ZERO; 2],
        elbow: [Vec3::ZERO; 2],
        wrist: [Vec3::ZERO; 2],
        hip: [Vec3::ZERO; 2],
        knee: [Vec3::ZERO; 2],
        ankle: [Vec3::ZERO; 2],
        foot: [Vec3::ZERO; 2],
        facing: pelvis.transform_vector3(Vec3::Z),
    };

    for i in 0..2 {
        // +1 for the left side, -1 for the right: mirrors offsets and angles.
        let side = if i == 0 { 1.0 } else { -1.0 };
        let mirror = Vec3::new(side, 1.0, 1.0);

        // Arm: forward kinematics from the chest.
        let arm = c.arms[i];
        let shoulder_rot = Quat::from_rotation_z(side * arm.abduct)
            * Quat::from_rotation_x(-arm.flex)
            * Quat::from_rotation_y(-side * arm.twist);
        let shoulder = chest * offset(SHOULDER_OFFSET * mirror) * Affine3A::from_quat(shoulder_rot);
        let elbow = shoulder * offset(Vec3::NEG_Y * UPPER_ARM) * Affine3A::from_rotation_x(-arm.elbow);
        upper_arm[i] = shoulder * offset(Vec3::NEG_Y * (0.5 * UPPER_ARM));
        forearm[i] = elbow * offset(Vec3::NEG_Y * (0.5 * FOREARM));
        joints.shoulder[i] = shoulder.translation.into();
        joints.elbow[i] = elbow.translation.into();
        joints.wrist[i] = elbow.transform_point3(Vec3::NEG_Y * FOREARM);

        // Leg: inverse kinematics from the hip to the planted foot.
        let hip = pelvis.transform_point3(HIP_OFFSET * mirror);
        let lift = c.feet[i].y;
        let foot_yaw = Quat::from_rotation_y(c.foot_yaw[i]);
        // A lifted foot points its toes down; the pitch is small enough that
        // the toes stay above the floor (see `feet_stay_on_the_floor`).
        let foot_rot = foot_yaw * Quat::from_rotation_x((2.6 * lift).min(1.0));
        let sole = body.transform_point3(c.feet[i]);
        let ankle_target = sole + Vec3::Y * ANKLE_HEIGHT;
        let forward = foot_yaw * Vec3::Z;
        let outward = Vec3::X * side;
        let pole = body.transform_vector3(forward + outward * c.knee_out[i]);
        let (knee, ankle) = solve_leg(hip, ankle_target, pole);
        thigh[i] = bone_frame(hip, knee, pole);
        shin[i] = bone_frame(knee, ankle, pole);
        foot[i] = Affine3A::from_rotation_translation(yaw * foot_rot, ankle);
        joints.hip[i] = hip;
        joints.knee[i] = knee;
        joints.ankle[i] = ankle;
        joints.foot[i] = sole;
    }

    Pose { pelvis, abdomen, chest, head, upper_arm, forearm, thigh, shin, foot, joints }
}

// ---------------------------------------------------------------------------
// Geometry
// ---------------------------------------------------------------------------

fn dancer_prims(p: &Pose, out: &mut Vec<Primitive>) {
    let skin = Material::solid(srgb(0xd9a07a));
    let shirt = Material::stripes(srgb(0xd8343a), srgb(0xf3ede2), Axis::Y, 0.045);
    let shorts = Material::dots(srgb(0x2c4f9e), srgb(0xf2d34b), 0.075, 0.021);
    let leggings = Material::checker(srgb(0x1f7a78), srgb(0x175e5d), 0.06);
    let warmers = Material::stripes(srgb(0xe85fa0), srgb(0xf7e9ef), Axis::Y, 0.05);
    let shoe = Material::solid(srgb(0xf08a24));
    let sole = Material::solid(srgb(0xf4f1ea));
    let hair = Material::solid(srgb(0x3a2418));
    let dark = Material::solid(srgb(0x1c1512));

    let mut push = |shape: Shape, frame: Affine3A, scale: Vec3, material: Material| {
        out.push(Primitive::in_frame(shape, frame, scale, material).with_dynamic(true));
    };
    let ball = Shape::Sphere;

    // Trunk.
    push(ball, p.pelvis * offset(Vec3::new(0.0, -0.01, 0.0)), Vec3::new(0.158, 0.115, 0.118), shorts);
    push(ball, p.abdomen * offset(Vec3::new(0.0, 0.075, 0.0)), Vec3::new(0.138, 0.135, 0.102), shirt);
    push(ball, p.chest * offset(Vec3::new(0.0, 0.125, 0.005)), Vec3::new(0.172, 0.165, 0.118), shirt);
    // Neck, from the top of the chest into the head.
    let neck_frame = bone_frame(p.head.translation.into(), p.joints.neck - Vec3::from(p.chest.matrix3.y_axis) * 0.03, p.joints.facing);
    push(Shape::Capsule { half_len: 0.5 * (HEAD_CENTER + 0.03) - 0.02, radius: 0.047 }, neck_frame, Vec3::ONE, skin);

    // Head with enough of a face to read its orientation.
    push(ball, p.head, HEAD_RADII, skin);
    push(ball, p.head * offset(Vec3::new(0.0, 0.035, -0.02)), Vec3::new(0.091, 0.102, 0.1), hair);
    push(ball, p.head * offset(Vec3::new(0.0, 0.085, -0.105)), Vec3::splat(0.05), hair);
    for side in [1.0f32, -1.0] {
        push(ball, p.head * offset(Vec3::new(side * 0.033, 0.018, 0.079)), Vec3::new(0.02, 0.017, 0.012), Material::solid(srgb(0xf7f4ee)));
        push(ball, p.head * offset(Vec3::new(side * 0.033, 0.018, 0.087)), Vec3::splat(0.0095), dark);
        // Ears.
        push(ball, p.head * offset(Vec3::new(side * 0.084, 0.0, 0.0)), Vec3::new(0.012, 0.026, 0.018), skin);
    }
    push(ball, p.head * offset(Vec3::new(0.0, -0.014, 0.094)), Vec3::new(0.013, 0.022, 0.016), Material::solid(srgb(0xc98c68)));
    push(ball, p.head * offset(Vec3::new(0.0, -0.057, 0.079)), Vec3::new(0.027, 0.008, 0.012), Material::solid(srgb(0x9c2f35)));

    for i in 0..2 {
        // Arms: short sleeve on the upper half of the upper arm.
        let sleeve = Material::stripes(srgb(0xd8343a), srgb(0xd9a07a), Axis::Y, 0.2);
        push(Shape::Capsule { half_len: 0.5 * UPPER_ARM, radius: 0.05 }, p.upper_arm[i], Vec3::ONE, sleeve);
        push(Shape::Capsule { half_len: 0.5 * FOREARM, radius: 0.041 }, p.forearm[i], Vec3::ONE, skin);
        // Wristbands in different colors tell left from right.
        let band = Material::solid(if i == 0 { srgb(0x28c7d9) } else { srgb(0xf2d34b) });
        push(Shape::Cylinder, p.forearm[i] * offset(Vec3::NEG_Y * (0.5 * FOREARM - 0.035)), Vec3::new(0.046, 0.022, 0.046), band);
        push(ball, p.forearm[i] * offset(Vec3::NEG_Y * (0.5 * FOREARM + 0.075)), Vec3::new(0.026, 0.085, 0.045), skin);

        // Legs.
        push(Shape::Capsule { half_len: 0.5 * THIGH, radius: 0.078 }, p.thigh[i], Vec3::ONE, leggings);
        push(Shape::Capsule { half_len: 0.5 * SHIN, radius: 0.056 }, p.shin[i], Vec3::ONE, warmers);
        // Shoe: rounded upper on a flat sole; the sole's underside is 8 cm
        // below the ankle.
        push(ball, p.foot[i] * offset(Vec3::new(0.0, -0.036, 0.055)), Vec3::new(0.05, 0.042, 0.128), shoe);
        push(Shape::Box, p.foot[i] * offset(Vec3::new(0.0, -0.066, 0.05)), Vec3::new(0.046, 0.014, 0.118), sole);
    }
}

/// Three balls circling the dancer on bouncing arcs.
fn ball_prims(t: f32, out: &mut Vec<Primitive>) {
    const ORBIT: f32 = 1.13;
    const GRAVITY: f32 = 9.81;
    let u = (t / LOOP_DURATION).rem_euclid(1.0);
    let balls = [
        (0.09, 12.0, Material::stripes(srgb(0xe23b3b), srgb(0xfaf6ee), Axis::Y, 0.03)),
        (0.075, 8.0, Material::checker(srgb(0xf2c230), srgb(0x2a57c9), 0.05)),
        (0.10, 16.0, Material::dots(srgb(0x3fae5a), srgb(0xfaf6ee), 0.07, 0.022)),
    ];
    for (i, (radius, bounces, material)) in balls.into_iter().enumerate() {
        let angle = TAU * (u + i as f32 / 3.0) + 0.5;
        // Ballistic arcs: with `bounces` per loop the apex follows from gravity.
        let period = LOOP_DURATION / bounces;
        let apex = GRAVITY * period * period / 8.0;
        let s = (u * bounces + i as f32 * 0.37).fract();
        let y = radius + apex * 4.0 * s * (1.0 - s);
        let center = Vec3::new(angle.sin() * ORBIT, y, angle.cos() * ORBIT);
        // Roll around the horizontal axis pointing to the stage center.
        let spin = Quat::from_rotation_y(angle) * Quat::from_rotation_z(TAU * 3.0 * u) * Quat::from_rotation_x(0.6);
        out.push(Primitive::ellipsoid(center, spin, Vec3::splat(radius), material).with_dynamic(true));
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_util::Lcg;
    use super::*;

    fn times(n: usize) -> impl Iterator<Item = f32> {
        (0..n).map(move |i| LOOP_DURATION * i as f32 / n as f32)
    }

    #[test]
    fn static_plate_is_the_non_dynamic_part() {
        let scene = StageScene::new();
        let plate = scene.snapshot_static();
        let full = scene.snapshot(1.0);
        assert!(plate.primitives().iter().all(|p| !p.dynamic));
        let statics = full.primitives().iter().filter(|p| !p.dynamic).count();
        assert_eq!(statics, plate.len());
        assert!(full.len() - statics >= 30, "dancer and balls present");
        assert_eq!(scene.loop_duration(), Some(LOOP_DURATION));
        assert!((8.0..=12.0).contains(&LOOP_DURATION));
    }

    #[test]
    fn stage_top_is_the_floor_plane() {
        let scene = StageScene::new();
        let plate = scene.snapshot_static();
        let hit = plate.raycast(Vec3::new(0.7, 3.0, -0.4), Vec3::NEG_Y, 10.0).unwrap();
        assert!(hit.pos.y.abs() < 1e-5);
        assert!((hit.normal - Vec3::Y).length() < 1e-5);
        // Textured: the polar checker shows both colors.
        let a = plate.raycast(Vec3::new(0.2, 3.0, 0.05), Vec3::NEG_Y, 10.0).unwrap().albedo;
        let b = plate.raycast(Vec3::new(0.6, 3.0, 0.05), Vec3::NEG_Y, 10.0).unwrap().albedo;
        assert_ne!(a, b);
        // Just inside the stage radius is still the floor plane, outside is lower.
        assert!(plate.raycast(Vec3::new(STAGE_RADIUS - 0.01, 3.0, 0.0), Vec3::NEG_Y, 10.0).unwrap().pos.y.abs() < 1e-5);
        assert!(plate.raycast(Vec3::new(STAGE_RADIUS + 0.2, 3.0, 0.0), Vec3::NEG_Y, 10.0).unwrap().pos.y < -0.05);
    }

    #[test]
    fn dance_loops_seamlessly() {
        let scene = StageScene::new();
        let a = scene.snapshot(0.0);
        let b = scene.snapshot(LOOP_DURATION);
        assert_eq!(a.len(), b.len());
        for (pa, pb) in a.primitives().iter().zip(b.primitives()) {
            let (ma, mb) = (pa.world_from_local(), pb.world_from_local());
            assert!(ma.abs_diff_eq(*mb, 2e-3), "{ma:?} vs {mb:?}");
        }
        // No pop across the seam either: approaching it from both sides
        // moves no primitive faster than anywhere else in the loop.
        let dt = 1.0 / 240.0;
        let step = |t0: f32, t1: f32| {
            let (s0, s1) = (scene.snapshot(t0), scene.snapshot(t1));
            s0.primitives()
                .iter()
                .zip(s1.primitives())
                .map(|(p, q)| (p.world_from_local().translation - q.world_from_local().translation).length())
                .fold(0.0f32, f32::max)
        };
        let seam = step(LOOP_DURATION - 0.5 * dt, 0.5 * dt + LOOP_DURATION);
        let typical = times(64).map(|t| step(t + 0.31, t + 0.31 + dt)).fold(0.0f32, f32::max);
        assert!(seam <= typical * 1.05 + 1e-4, "seam step {seam} vs max in-loop step {typical}");
        // And time wraps: t and t + loop are the same pose.
        let c = scene.snapshot(3.3);
        let d = scene.snapshot(3.3 + LOOP_DURATION);
        for (pc, pd) in c.primitives().iter().zip(d.primitives()) {
            assert!(pc.world_from_local().abs_diff_eq(*pd.world_from_local(), 2e-3));
        }
    }

    #[test]
    fn motion_is_smooth() {
        // No primitive teleports: bounded speed everywhere in the loop
        // (the bouncing balls reverse instantly but stay below ~6 m/s).
        let scene = StageScene::new();
        let dt = 1.0 / 120.0;
        let mut fastest = 0.0f32;
        for t in times(1152) {
            let (s0, s1) = (scene.snapshot(t), scene.snapshot(t + dt));
            for (p, q) in s0.primitives().iter().zip(s1.primitives()) {
                let v = (p.world_from_local().translation - q.world_from_local().translation).length() / dt;
                fastest = fastest.max(v);
            }
        }
        assert!(fastest < 7.0, "max speed {fastest} m/s");
        assert!(fastest > 1.0, "the scene should actually move ({fastest} m/s)");
    }

    #[test]
    fn everything_dynamic_stays_in_the_capture_volume_above_the_floor() {
        let scene = StageScene::new();
        for t in times(960) {
            let snap = scene.snapshot(t);
            let mut lowest = f32::INFINITY;
            for p in snap.primitives().iter().filter(|p| p.dynamic) {
                let bb = p.world_aabb();
                assert!(bb.min.y >= -2e-3, "t = {t}: below the floor by {}", bb.min.y);
                assert!(bb.max.y <= CAPTURE_HEIGHT, "t = {t}: too high {}", bb.max.y);
                // Farthest corner of the box from the stage axis.
                let r = bb.min.x.abs().max(bb.max.x.abs()).hypot(bb.min.z.abs().max(bb.max.z.abs()));
                assert!(r <= CAPTURE_RADIUS, "t = {t}: radius {r}");
                lowest = lowest.min(bb.min.y);
            }
            assert!(lowest < 0.02, "t = {t}: nothing touches the floor (lowest {lowest})");
        }
    }

    #[test]
    fn dancer_proportions() {
        let scene = StageScene::new();
        let mut top = 0.0f32;
        for t in times(480) {
            let j = scene.dancer_joints(t);
            for i in 0..2 {
                // Bones keep their length: limbs never stretch or detach.
                assert!(((j.hip[i] - j.knee[i]).length() - THIGH).abs() < 1e-4, "thigh at {t}");
                assert!(((j.knee[i] - j.ankle[i]).length() - SHIN).abs() < 1e-4, "shin at {t}");
                assert!(((j.shoulder[i] - j.elbow[i]).length() - UPPER_ARM).abs() < 1e-4);
                assert!(((j.elbow[i] - j.wrist[i]).length() - FOREARM).abs() < 1e-4);
                // The IK reaches the planted foot: the ankle is right above it.
                assert!((j.ankle[i] - (j.foot[i] + Vec3::Y * ANKLE_HEIGHT)).length() < 1e-4, "foot {i} out of reach at {t}");
                // Knees bend forward-ish, never backwards.
                let mid = 0.5 * (j.hip[i] + j.ankle[i]);
                assert!((j.knee[i] - mid).dot(j.facing) > -0.02, "knee {i} bends backwards at {t}");
                // Hands stay above the floor and elbows away from the head.
                assert!(j.wrist[i].y > 0.5, "wrist {i} at {t}");
            }
            assert!(j.head.y > j.chest.y && j.chest.y > j.pelvis.y);
            assert!(j.pelvis.y > 0.65 && j.pelvis.y < 1.0, "pelvis height {} at {t}", j.pelvis.y);
            top = top.max(j.head.y + HEAD_RADII.y);
        }
        assert!((1.68..=1.80).contains(&top), "standing height {top}");
    }

    #[test]
    fn feet_stay_on_the_floor() {
        let scene = StageScene::new();
        let mut left_positions = Aabb::EMPTY;
        for t in times(960) {
            let j = scene.dancer_joints(t);
            // The pivot foot never travels, it only taps.
            left_positions.expand(Vec3::new(j.foot[0].x, 0.0, j.foot[0].z));
            assert!(j.foot[0].y <= 0.06 && j.foot[1].y <= 0.36);
            // At least one foot is planted at any time.
            assert!(j.foot[0].y.min(j.foot[1].y) < 0.012, "both feet in the air at {t}");
            // The shoes (prims) never dip below the floor.
            let snap = scene.snapshot(t);
            for p in snap.primitives().iter().filter(|p| p.dynamic) {
                assert!(p.world_aabb().min.y > -2e-3);
            }
        }
        assert!(left_positions.size().max_element() < 1e-3, "pivot foot slides: {left_positions:?}");
        // A planted foot does not slide: while the right foot is down in the
        // groove section it keeps its position.
        let a = scene.dancer_joints(0.0).foot[1];
        let b = scene.dancer_joints(0.9 * 0.6 * 3.0).foot[1];
        assert!((Vec3::new(a.x, 0.0, a.z) - Vec3::new(b.x, 0.0, b.z)).length() < 1e-4);
    }

    #[test]
    fn dancer_spins_once_per_loop() {
        let scene = StageScene::new();
        let mut total = 0.0;
        let mut prev = scene.dancer_joints(0.0).facing;
        for t in times(960).skip(1).chain([LOOP_DURATION]) {
            let f = scene.dancer_joints(t).facing;
            total += prev.cross(f).y.atan2(prev.dot(f));
            prev = f;
        }
        assert!((total.abs() - TAU).abs() < 0.05, "net rotation {total}");
        // Faces the default viewer (+Z) at the start of the loop.
        assert!(scene.dancer_joints(0.0).facing.z > 0.9);
    }

    #[test]
    fn bvh_matches_brute_force() {
        let scene = StageScene::new();
        let mut rng = Lcg(11);
        let mut hits = 0;
        for k in 0..8 {
            let snap = scene.snapshot(k as f32 * 1.234);
            for _ in 0..1500 {
                let origin = rng.in_box(Vec3::new(-4.0, 0.05, -4.0), Vec3::new(4.0, 3.0, 4.0));
                let target = rng.in_box(Vec3::new(-1.3, 0.0, -1.3), Vec3::new(1.3, 2.0, 1.3));
                let dir = (target - origin).normalize();
                let t_max = if rng.next_f32() < 0.3 { rng.range(0.5, 6.0) } else { f32::INFINITY };
                let fast = snap.raycast(origin, dir, t_max);
                let slow = snap.raycast_brute_force(origin, dir, t_max);
                match (fast, slow) {
                    (None, None) => {}
                    (Some(a), Some(b)) => {
                        hits += 1;
                        assert!((a.t - b.t).abs() < 1e-4, "t {} vs {}", a.t, b.t);
                        // Coincident surfaces may resolve to either primitive.
                        if a.prim == b.prim {
                            assert_eq!(a, b);
                        }
                    }
                    (a, b) => panic!("bvh {a:?} vs brute force {b:?}"),
                }
                assert_eq!(snap.occluded(origin, dir, t_max), slow.is_some());
            }
        }
        assert!(hits > 6000, "{hits} hits");
    }

    #[test]
    fn hit_normals_are_unit_and_face_the_ray() {
        let scene = StageScene::new();
        let snap = scene.snapshot(4.2);
        let mut rng = Lcg(3);
        let mut dynamic_hits = 0;
        for _ in 0..4000 {
            let origin = rng.in_box(Vec3::new(-3.5, 0.2, -3.5), Vec3::new(3.5, 2.8, 3.5));
            let target = rng.in_box(Vec3::new(-0.6, 0.0, -0.6), Vec3::new(0.6, 1.8, 0.6));
            let dir = (target - origin).normalize();
            if let Some(h) = snap.raycast(origin, dir, 50.0) {
                assert!((h.normal.length() - 1.0).abs() < 1e-4);
                assert!(h.normal.dot(dir) <= 1e-4);
                assert!((h.pos - (origin + dir * h.t)).length() < 1e-4);
                assert!(h.albedo.min_element() >= 0.0 && h.albedo.max_element() <= 1.0);
                dynamic_hits += h.dynamic as u32;
            }
        }
        assert!(dynamic_hits > 500);
    }

    #[test]
    fn bounds_contain_every_snapshot() {
        let scene = StageScene::new();
        let b = scene.bounds();
        for t in times(64) {
            let s = scene.snapshot(t).bounds();
            assert!(s.min.cmpge(b.min - 1e-4).all() && s.max.cmple(b.max + 1e-4).all(), "{s:?} not in {b:?}");
        }
        let (eye, target) = scene.viewer_start();
        assert!(eye.distance(target) > 2.0);
        assert!(scene.snapshot(0.0).raycast(eye, (target - eye).normalize(), 20.0).is_some());
    }
}
