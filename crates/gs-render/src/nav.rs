//! Desktop camera navigation: orbit and fly, with smooth damping.
//!
//! Pure math, no browser types: the DOM listeners in `input` translate
//! pointer, wheel and key events into calls on [`Navigator`], and the frame
//! loop calls [`Navigator::update`] once per frame.
//!
//! The camera is described by a yaw (rotation around +Y; 0 looks along −Z,
//! positive turns left), a pitch (positive looks up), an anchor point and a
//! distance. In orbit mode the anchor is the look-at target and the eye sits
//! `dist` behind it; in fly mode the anchor is the eye itself. Every input
//! changes a *goal* pose; the pose actually rendered follows it exponentially.

use glam::{Mat4, Vec3};

/// Desktop navigation mode.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NavMode {
    /// Drag rotates around a target, wheel zooms, right/shift-drag pans.
    #[default]
    Orbit,
    /// WASD / arrow keys move, Q/E go down/up, drag looks around.
    Fly,
}

/// Movement keys of fly mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavKey {
    Forward,
    Back,
    Left,
    Right,
    Up,
    Down,
    /// Shift: move faster.
    Fast,
}

impl NavKey {
    /// Maps a `KeyboardEvent.code` (physical key, layout independent, so
    /// "WASD" is also right on AZERTY keyboards).
    pub fn from_code(code: &str) -> Option<Self> {
        Some(match code {
            "KeyW" | "ArrowUp" => Self::Forward,
            "KeyS" | "ArrowDown" => Self::Back,
            "KeyA" | "ArrowLeft" => Self::Left,
            "KeyD" | "ArrowRight" => Self::Right,
            "KeyE" | "Space" => Self::Up,
            "KeyQ" => Self::Down,
            "ShiftLeft" | "ShiftRight" => Self::Fast,
            _ => return None,
        })
    }
}

/// Radians of rotation per pixel of drag.
const ROTATE_SPEED: f32 = 0.005;
/// The pitch stops just short of straight up/down.
const PITCH_LIMIT: f32 = 1.55;
const MIN_DIST: f32 = 0.02;
const MAX_DIST: f32 = 500.0;
/// Time constant of the pose damping, seconds.
const DAMPING_TAU: f32 = 0.07;
/// Time constant of the fly velocity, seconds.
const VELOCITY_TAU: f32 = 0.12;
/// Zoom factor per wheel pixel (natural log).
const WHEEL_ZOOM: f32 = 0.0012;
const MIN_FLY_SPEED: f32 = 0.05;
const MAX_FLY_SPEED: f32 = 50.0;
/// Shift multiplies the fly speed by this.
const FAST_FACTOR: f32 = 3.0;
/// Auto-rotation starts after this many seconds without input.
const IDLE_DELAY: f32 = 4.0;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Pose {
    anchor: Vec3,
    yaw: f32,
    pitch: f32,
    dist: f32,
}

/// Unit view direction for a yaw and pitch.
fn forward(yaw: f32, pitch: f32) -> Vec3 {
    let (sy, cy) = yaw.sin_cos();
    let (sp, cp) = pitch.sin_cos();
    Vec3::new(-sy * cp, sp, -cy * cp)
}

/// Unit vector to the right of the view direction (always horizontal).
fn right(yaw: f32) -> Vec3 {
    let (sy, cy) = yaw.sin_cos();
    Vec3::new(cy, 0.0, -sy)
}

/// OpenGL-style perspective projection (clip z in `[-1, 1]`, right-handed,
/// camera looking along −Z) for a vertical field of view in radians. The same
/// matrix as `gs_core::Camera::projection_matrix` for a centered camera.
pub fn perspective(fov_y: f32, aspect: f32, near: f32, far: f32) -> Mat4 {
    let f = 1.0 / (0.5 * fov_y).tan();
    Mat4::from_cols_array(&[
        f / aspect,
        0.0,
        0.0,
        0.0,
        0.0,
        f,
        0.0,
        0.0,
        0.0,
        0.0,
        -(far + near) / (far - near),
        -1.0,
        0.0,
        0.0,
        -2.0 * far * near / (far - near),
        0.0,
    ])
}

/// Fraction of the remaining distance covered in `dt` for a time constant `tau`.
fn blend(dt: f32, tau: f32) -> f32 {
    1.0 - (-dt.max(0.0) / tau).exp()
}

/// Camera state and input handling for the desktop view.
#[derive(Clone, Debug)]
pub struct Navigator {
    mode: NavMode,
    goal: Pose,
    cur: Pose,
    fly_speed: f32,
    velocity: Vec3,
    /// Pressed state indexed by `NavKey as usize`.
    keys: [bool; 7],
    idle_s: f32,
    auto_rotate: Option<f32>,
}

impl Navigator {
    pub fn new(mode: NavMode, eye: Vec3, target: Vec3) -> Self {
        let pose = Pose { anchor: Vec3::ZERO, yaw: 0.0, pitch: 0.0, dist: 1.0 };
        let mut nav =
            Self { mode, goal: pose, cur: pose, fly_speed: 1.5, velocity: Vec3::ZERO, keys: [false; 7], idle_s: 0.0, auto_rotate: None };
        nav.set_view(eye, target);
        nav
    }

    pub fn mode(&self) -> NavMode {
        self.mode
    }

    /// Switches mode without moving the camera: the eye and the view
    /// direction are kept, and the orbit target becomes the point `dist`
    /// ahead of the eye.
    pub fn set_mode(&mut self, mode: NavMode) {
        if mode == self.mode {
            return;
        }
        let convert = |p: &mut Pose| {
            let offset = forward(p.yaw, p.pitch) * p.dist;
            match mode {
                NavMode::Fly => p.anchor -= offset,
                NavMode::Orbit => p.anchor += offset,
            }
        };
        convert(&mut self.goal);
        convert(&mut self.cur);
        self.mode = mode;
        self.velocity = Vec3::ZERO;
        self.keys = [false; 7];
        self.idle_s = 0.0;
    }

    /// Places the camera at `eye` looking at `target`, immediately (no
    /// damping). Non-finite input is ignored.
    pub fn set_view(&mut self, eye: Vec3, target: Vec3) {
        if !(eye.is_finite() && target.is_finite()) {
            return;
        }
        let dir = target - eye;
        let dist = dir.length().clamp(MIN_DIST, MAX_DIST);
        let unit = dir.normalize_or(Vec3::NEG_Z);
        let pitch = unit.y.clamp(-1.0, 1.0).asin().clamp(-PITCH_LIMIT, PITCH_LIMIT);
        // Straight up/down has no heading: keep the current one.
        let yaw = if unit.x.abs() + unit.z.abs() > 1e-6 { (-unit.x).atan2(-unit.z) } else { self.goal.yaw };
        let anchor = match self.mode {
            NavMode::Orbit => eye + forward(yaw, pitch) * dist,
            NavMode::Fly => eye,
        };
        self.goal = Pose { anchor, yaw, pitch, dist };
        self.cur = self.goal;
        self.velocity = Vec3::ZERO;
        self.idle_s = 0.0;
    }

    /// Orbit auto-rotation speed in rad/s (`None` = off).
    pub fn set_auto_rotate(&mut self, speed: Option<f32>) {
        self.auto_rotate = speed.filter(|s| s.is_finite());
        self.idle_s = 0.0;
    }

    /// Fly speed in meters per second.
    pub fn fly_speed(&self) -> f32 {
        self.fly_speed
    }

    pub fn set_fly_speed(&mut self, speed: f32) {
        if speed.is_finite() {
            self.fly_speed = speed.clamp(MIN_FLY_SPEED, MAX_FLY_SPEED);
        }
    }

    /// Current (damped) eye position.
    pub fn eye(&self) -> Vec3 {
        match self.mode {
            NavMode::Orbit => self.cur.anchor - self.forward() * self.cur.dist,
            NavMode::Fly => self.cur.anchor,
        }
    }

    /// Current look-at point: the orbit target, or the point `dist` ahead in
    /// fly mode.
    pub fn target(&self) -> Vec3 {
        match self.mode {
            NavMode::Orbit => self.cur.anchor,
            NavMode::Fly => self.cur.anchor + self.forward() * self.cur.dist,
        }
    }

    /// Current unit view direction.
    pub fn forward(&self) -> Vec3 {
        forward(self.cur.yaw, self.cur.pitch)
    }

    /// Current heading, radians around +Y (0 looks along −Z).
    pub fn yaw(&self) -> f32 {
        self.cur.yaw
    }

    /// World-to-camera matrix of the current pose.
    pub fn view_matrix(&self) -> Mat4 {
        let f = self.forward();
        let r = right(self.cur.yaw);
        let u = r.cross(f);
        let eye = self.eye();
        // Inverse of the rigid transform [r u −f | eye]: transposed rotation
        // and the rotated, negated translation.
        Mat4::from_cols(
            Vec3::new(r.x, u.x, -f.x).extend(0.0),
            Vec3::new(r.y, u.y, -f.y).extend(0.0),
            Vec3::new(r.z, u.z, -f.z).extend(0.0),
            Vec3::new(-r.dot(eye), -u.dot(eye), f.dot(eye)).extend(1.0),
        )
    }

    /// Pointer drag by `(dx, dy)` pixels (y down).
    ///
    /// Orbit: the scene follows the pointer. Fly: the view turns toward the
    /// pointer motion like a first-person camera, or, with `grab` (touch),
    /// the scene is dragged along with the finger.
    pub fn rotate(&mut self, dx: f32, dy: f32, grab: bool) {
        let sign = if self.mode == NavMode::Fly && grab { 1.0 } else { -1.0 };
        self.goal.yaw += sign * dx * ROTATE_SPEED;
        self.goal.pitch = (self.goal.pitch + sign * dy * ROTATE_SPEED).clamp(-PITCH_LIMIT, PITCH_LIMIT);
        self.idle_s = 0.0;
    }

    /// Drags the scene by `(dx, dy)` pixels in the view plane. `fov_y` and
    /// `viewport_height` (same pixel unit as the deltas) convert pixels to
    /// meters at the anchor distance, so the point under the cursor follows it.
    pub fn pan(&mut self, dx: f32, dy: f32, fov_y: f32, viewport_height: f32) {
        let meters_per_px = 2.0 * self.goal.dist * (0.5 * fov_y).tan() / viewport_height.max(1.0);
        let r = right(self.goal.yaw);
        let u = r.cross(forward(self.goal.yaw, self.goal.pitch));
        self.goal.anchor += (u * dy - r * dx) * meters_per_px;
        self.idle_s = 0.0;
    }

    /// Multiplies the viewing distance by `factor` (`< 1` moves closer). In
    /// fly mode the eye moves along the view direction by the same amount.
    pub fn zoom(&mut self, factor: f32) {
        if !(factor.is_finite() && factor > 0.0) {
            return;
        }
        match self.mode {
            NavMode::Orbit => self.goal.dist = (self.goal.dist * factor).clamp(MIN_DIST, MAX_DIST),
            NavMode::Fly => {
                let f = forward(self.goal.yaw, self.goal.pitch);
                self.goal.anchor += f * self.goal.dist * (1.0 - factor);
            }
        }
        self.idle_s = 0.0;
    }

    /// Mouse wheel by `delta` pixels (positive = scroll down). Orbit: zoom
    /// out/in. Fly: decrease/increase the fly speed.
    pub fn wheel(&mut self, delta: f32) {
        if !delta.is_finite() {
            return;
        }
        let delta = delta.clamp(-2000.0, 2000.0);
        match self.mode {
            NavMode::Orbit => self.zoom((delta * WHEEL_ZOOM).exp()),
            NavMode::Fly => {
                self.fly_speed = (self.fly_speed * (-delta * WHEEL_ZOOM).exp()).clamp(MIN_FLY_SPEED, MAX_FLY_SPEED);
                self.idle_s = 0.0;
            }
        }
    }

    /// Records a key press or release.
    pub fn set_key(&mut self, key: NavKey, down: bool) {
        self.keys[key as usize] = down;
        self.idle_s = 0.0;
    }

    /// Releases every key (focus lost).
    pub fn clear_keys(&mut self) {
        self.keys = [false; 7];
    }

    fn key(&self, key: NavKey) -> f32 {
        self.keys[key as usize] as u8 as f32
    }

    /// Advances the animation by `dt` seconds. Returns `true` when the
    /// rendered pose changed.
    pub fn update(&mut self, dt: f32) -> bool {
        let dt = if dt.is_finite() { dt.clamp(0.0, 0.1) } else { 0.0 };
        let before = self.cur;

        match self.mode {
            NavMode::Fly => {
                let f = forward(self.goal.yaw, self.goal.pitch);
                let r = right(self.goal.yaw);
                let wish = f * (self.key(NavKey::Forward) - self.key(NavKey::Back))
                    + r * (self.key(NavKey::Right) - self.key(NavKey::Left))
                    + Vec3::Y * (self.key(NavKey::Up) - self.key(NavKey::Down));
                let boost = if self.keys[NavKey::Fast as usize] { FAST_FACTOR } else { 1.0 };
                let wish = wish.normalize_or_zero() * self.fly_speed * boost;
                self.velocity += (wish - self.velocity) * blend(dt, VELOCITY_TAU);
                if wish == Vec3::ZERO && self.velocity.length_squared() < 1e-8 {
                    self.velocity = Vec3::ZERO;
                }
                // The velocity is already smooth: move both poses together.
                let step = self.velocity * dt;
                self.goal.anchor += step;
                self.cur.anchor += step;
            }
            NavMode::Orbit => {
                self.idle_s += dt;
                if let Some(speed) = self.auto_rotate {
                    if self.idle_s > IDLE_DELAY {
                        self.goal.yaw += speed * dt;
                    }
                }
            }
        }

        let a = blend(dt, DAMPING_TAU);
        let (goal, cur) = (self.goal, &mut self.cur);
        cur.anchor += (goal.anchor - cur.anchor) * a;
        cur.yaw += (goal.yaw - cur.yaw) * a;
        cur.pitch += (goal.pitch - cur.pitch) * a;
        cur.dist += (goal.dist - cur.dist) * a;
        // Finish the tail instead of creeping forever (and redrawing forever).
        let settled = (goal.anchor - cur.anchor).length_squared() < (1e-4 * goal.dist).powi(2)
            && (goal.yaw - cur.yaw).abs() < 1e-5
            && (goal.pitch - cur.pitch).abs() < 1e-5
            && (goal.dist - cur.dist).abs() < 1e-4 * goal.dist;
        if settled {
            *cur = goal;
        }
        *cur != before
    }
}

#[cfg(test)]
mod tests {
    use gs_core::Camera;

    use super::*;

    fn settle(nav: &mut Navigator) {
        for _ in 0..200 {
            nav.update(1.0 / 60.0);
        }
    }

    fn assert_vec(a: Vec3, b: Vec3, tol: f32) {
        assert!((a - b).length() < tol, "{a} vs {b}");
    }

    #[test]
    fn set_view_roundtrips_in_both_modes() {
        let (eye, target) = (Vec3::new(3.0, 2.0, 4.5), Vec3::new(0.2, 1.0, -0.3));
        for mode in [NavMode::Orbit, NavMode::Fly] {
            let nav = Navigator::new(mode, eye, target);
            assert_vec(nav.eye(), eye, 1e-4);
            assert_vec(nav.target(), target, 1e-4);
            assert_vec(nav.forward(), (target - eye).normalize(), 1e-5);
        }
    }

    #[test]
    fn view_matrix_matches_core_camera() {
        let (eye, target) = (Vec3::new(-2.0, 1.4, 3.0), Vec3::new(0.5, 0.2, -1.0));
        let nav = Navigator::new(NavMode::Orbit, eye, target);
        let reference = Camera::look_at(eye, target, Vec3::Y, 1.0, 64, 64).view_matrix();
        let got = nav.view_matrix();
        for (a, b) in got.to_cols_array().iter().zip(reference.to_cols_array()) {
            assert!((a - b).abs() < 1e-5, "{got:?} vs {reference:?}");
        }
    }

    #[test]
    fn perspective_matches_core_camera() {
        let fov = 50f32.to_radians();
        let cam = Camera::look_at(Vec3::ZERO, Vec3::NEG_Z, Vec3::Y, fov, 640, 480);
        let reference = cam.projection_matrix(0.05, 200.0);
        assert!(perspective(fov, 640.0 / 480.0, 0.05, 200.0).abs_diff_eq(reference, 1e-5));
    }

    #[test]
    fn orbit_drag_keeps_target_and_distance() {
        let target = Vec3::new(0.0, 1.0, 0.0);
        let mut nav = Navigator::new(NavMode::Orbit, Vec3::new(0.0, 1.0, 4.0), target);
        nav.rotate(120.0, -40.0, false);
        settle(&mut nav);
        assert_vec(nav.target(), target, 1e-4);
        assert!(((nav.eye() - target).length() - 4.0).abs() < 1e-3);
        // Dragging right moves the camera to the left of where it was (the
        // scene follows the pointer); dragging up lowers the camera.
        assert!(nav.eye().x < -0.5, "{}", nav.eye());
        assert!(nav.eye().y < 1.0, "{}", nav.eye());
    }

    #[test]
    fn pitch_is_clamped() {
        let mut nav = Navigator::new(NavMode::Orbit, Vec3::new(0.0, 1.0, 4.0), Vec3::new(0.0, 1.0, 0.0));
        // Dragging far down: the camera ends up almost, not exactly, overhead.
        nav.rotate(0.0, 1.0e5, false);
        settle(&mut nav);
        assert!(nav.forward().y < -0.99 && nav.forward().y > -1.0);
        assert!(nav.view_matrix().is_finite());
        nav.rotate(0.0, -1.0e5, false);
        settle(&mut nav);
        assert!(nav.forward().y > 0.99 && nav.forward().y < 1.0);
    }

    #[test]
    fn damping_is_gradual_and_converges() {
        let mut nav = Navigator::new(NavMode::Orbit, Vec3::new(0.0, 1.0, 4.0), Vec3::new(0.0, 1.0, 0.0));
        nav.zoom(0.5);
        assert!(nav.update(1.0 / 60.0));
        let d = (nav.eye() - nav.target()).length();
        assert!(d < 4.0 && d > 2.2, "one frame must not jump: {d}");
        settle(&mut nav);
        assert!(((nav.eye() - nav.target()).length() - 2.0).abs() < 1e-3);
        assert!(!nav.update(1.0 / 60.0), "settled pose must report no change");
    }

    #[test]
    fn wheel_zooms_in_orbit_and_changes_speed_in_fly() {
        let mut nav = Navigator::new(NavMode::Orbit, Vec3::new(0.0, 1.0, 4.0), Vec3::new(0.0, 1.0, 0.0));
        nav.wheel(-300.0);
        settle(&mut nav);
        assert!((nav.eye() - nav.target()).length() < 3.5);
        nav.set_mode(NavMode::Fly);
        let (eye, speed) = (nav.eye(), nav.fly_speed());
        nav.wheel(-300.0);
        settle(&mut nav);
        assert!(nav.fly_speed() > speed);
        assert_vec(nav.eye(), eye, 1e-4);
    }

    #[test]
    fn mode_switch_keeps_the_camera() {
        let mut nav = Navigator::new(NavMode::Orbit, Vec3::new(3.0, 2.0, 4.5), Vec3::new(0.0, 1.0, 0.0));
        let (eye, fwd, view) = (nav.eye(), nav.forward(), nav.view_matrix());
        nav.set_mode(NavMode::Fly);
        assert_vec(nav.eye(), eye, 1e-4);
        assert_vec(nav.forward(), fwd, 1e-6);
        nav.set_mode(NavMode::Orbit);
        assert_vec(nav.eye(), eye, 1e-4);
        assert!(nav.view_matrix().abs_diff_eq(view, 1e-4));
    }

    #[test]
    fn fly_keys_move_along_the_view() {
        let mut nav = Navigator::new(NavMode::Fly, Vec3::new(0.0, 1.5, 0.0), Vec3::new(0.0, 1.5, -1.0));
        nav.set_key(NavKey::Forward, true);
        for _ in 0..60 {
            nav.update(1.0 / 60.0);
        }
        let z_slow = nav.eye().z;
        assert!(z_slow < -1.0 && z_slow > -1.6, "about fly_speed × 1 s: {z_slow}");
        assert!(nav.eye().x.abs() < 1e-4 && (nav.eye().y - 1.5).abs() < 1e-4);
        nav.set_key(NavKey::Fast, true);
        for _ in 0..60 {
            nav.update(1.0 / 60.0);
        }
        assert!(nav.eye().z - z_slow < -3.0, "shift is faster");
        nav.set_key(NavKey::Forward, false);
        nav.set_key(NavKey::Fast, false);
        nav.set_key(NavKey::Up, true);
        nav.set_key(NavKey::Right, true);
        let before = nav.eye();
        for _ in 0..120 {
            nav.update(1.0 / 60.0);
        }
        assert!(nav.eye().y > before.y + 0.5 && nav.eye().x > before.x + 0.5);
        nav.clear_keys();
        settle(&mut nav);
        assert!(!nav.update(1.0 / 60.0), "the camera coasts to a stop");
    }

    #[test]
    fn fly_look_direction_depends_on_pointer_kind() {
        let mut mouse = Navigator::new(NavMode::Fly, Vec3::ZERO, Vec3::NEG_Z);
        mouse.rotate(100.0, 0.0, false);
        settle(&mut mouse);
        assert!(mouse.forward().x > 0.3, "mouse right turns right");
        let mut touch = Navigator::new(NavMode::Fly, Vec3::ZERO, Vec3::NEG_Z);
        touch.rotate(100.0, 0.0, true);
        settle(&mut touch);
        assert!(touch.forward().x < -0.3, "a finger drags the scene");
        assert_vec(touch.eye(), Vec3::ZERO, 1e-6);
    }

    #[test]
    fn pan_moves_perpendicular_to_the_view() {
        let mut nav = Navigator::new(NavMode::Orbit, Vec3::new(0.0, 1.0, 4.0), Vec3::new(0.0, 1.0, 0.0));
        let fov = 50f32.to_radians();
        // Dragging by the full viewport height moves by the visible height at the target.
        nav.pan(0.0, 600.0, fov, 600.0);
        settle(&mut nav);
        let visible = 2.0 * 4.0 * (0.5 * fov).tan();
        assert_vec(nav.target(), Vec3::new(0.0, 1.0 + visible, 0.0), 1e-3);
        nav.pan(100.0, 0.0, fov, 600.0);
        settle(&mut nav);
        assert!(nav.target().x < -0.1, "dragging right moves the target left");
        assert!(((nav.eye() - nav.target()).length() - 4.0).abs() < 1e-3);
    }

    #[test]
    fn auto_rotate_waits_for_idle() {
        let mut nav = Navigator::new(NavMode::Orbit, Vec3::new(0.0, 1.0, 4.0), Vec3::new(0.0, 1.0, 0.0));
        nav.set_auto_rotate(Some(0.5));
        for _ in 0..60 {
            nav.update(1.0 / 60.0);
        }
        assert_vec(nav.eye(), Vec3::new(0.0, 1.0, 4.0), 1e-5);
        for _ in 0..600 {
            nav.update(1.0 / 60.0);
        }
        assert!(nav.eye().x.abs() > 0.5, "rotating after the idle delay");
        assert_vec(nav.target(), Vec3::new(0.0, 1.0, 0.0), 1e-4);
    }

    #[test]
    fn nonsense_input_is_ignored() {
        let mut nav = Navigator::new(NavMode::Orbit, Vec3::new(0.0, 1.0, 4.0), Vec3::new(0.0, 1.0, 0.0));
        let view = nav.view_matrix();
        nav.set_view(Vec3::NAN, Vec3::ZERO);
        nav.zoom(f32::NAN);
        nav.zoom(-1.0);
        nav.wheel(f32::INFINITY);
        nav.update(f32::NAN);
        assert_eq!(nav.view_matrix(), view);
        // Looking straight down keeps a finite, orthonormal view.
        nav.set_view(Vec3::new(0.0, 5.0, 0.0), Vec3::ZERO);
        assert!(nav.view_matrix().is_finite());
        assert!((nav.view_matrix().determinant() - 1.0).abs() < 1e-4);
    }
}
