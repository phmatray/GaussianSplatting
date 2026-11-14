//! How a session is shown: scene hints, gizmos, default navigation and the
//! camera placements the interface chooses (dollhouse view, framing of an
//! imported file, the view of one rig camera).

use glam::{Mat4, Vec3};
use gs_core::protocol::{CameraInfo, ScanHead, ScanState, ScanStats, SessionState};
use gs_core::scene::SceneKind;
use gs_core::{Aabb, Splat};
use gs_render::{FrustumGizmo, GizmoScene, GridGizmo, NavMode, SceneHints};

/// The dollhouse view hides everything above the ceiling height minus this
/// margin (meters): the ceiling surfels and their fringe go, the walls stay.
pub const DOLLHOUSE_CLIP_MARGIN: f32 = 0.15;

const RIG_COLOR: [f32; 3] = [0.36, 0.72, 1.0];
const HANDHELD_COLOR: [f32; 3] = [1.0, 0.5, 0.3];
const GRID_COLOR: [f32; 3] = [0.3, 0.36, 0.46];

/// Navigation a scene starts with: orbit around the stage, walk through
/// the house.
pub fn default_nav(scene: SceneKind) -> NavMode {
    match scene {
        SceneKind::Stage => NavMode::Orbit,
        SceneKind::House => NavMode::Fly,
    }
}

/// Splat size a scene starts with (the "Taille des splats" slider). The
/// fused house is one round splat per voxel, sized for the best score
/// against ground truth; seen from one meter in a walk-through, slightly
/// larger discs close the gaps between neighbours on the walls.
pub fn default_splat_scale(scene: SceneKind) -> f32 {
    match scene {
        SceneKind::Stage => 1.0,
        SceneKind::House => 1.2,
    }
}

/// Center of the scene on the floor.
pub fn floor_center(bounds: &Aabb) -> Vec3 {
    let center = bounds.center();
    Vec3::new(center.x, 0.0, center.z)
}

/// What the viewer should know about the session (VR placement, miniature).
pub fn scene_hints(state: &SessionState) -> SceneHints {
    let bounded = !state.bounds.is_empty() && state.bounds.min.is_finite() && state.bounds.max.is_finite();
    SceneHints {
        ceiling_height: state.ceiling_height.filter(|h| h.is_finite()),
        floor_center: if bounded { floor_center(&state.bounds) } else { Vec3::ZERO },
        bounds: bounded.then_some(state.bounds),
    }
}

/// Height above which the dollhouse view clips, if the scene has a ceiling.
pub fn dollhouse_clip(state: &SessionState) -> Option<f32> {
    state.ceiling_height.filter(|h| h.is_finite()).map(|h| h - DOLLHOUSE_CLIP_MARGIN)
}

/// Ids of the cameras that have a thumbnail: the ring cameras of the
/// stage, the sensors of the scan head in the house (their previews carry
/// the sensor id).
pub fn camera_ids(state: &SessionState) -> Vec<u32> {
    match state.config.scene {
        SceneKind::Stage => state.cameras.iter().map(|c| c.id).collect(),
        SceneKind::House => (0..state.config.scan.head.sensor_count()).collect(),
    }
}

/// Name of one sensor of the scan head, from the way it is mounted. A
/// head with a single sensor is simply the mobile camera.
pub fn sensor_label(head: ScanHead, id: u32) -> &'static str {
    let mounts = head.mounts();
    let Some(mount) = mounts.get(id as usize) else { return "Capteur" };
    if mounts.len() == 1 {
        "Caméra mobile"
    } else if mount.pitch_deg > 1.0 {
        "Haut"
    } else if mount.pitch_deg < -1.0 {
        "Bas"
    } else if mount.yaw_deg > 1.0 {
        "Arrière gauche"
    } else if mount.yaw_deg < -1.0 {
        "Arrière droite"
    } else {
        "Avant"
    }
}

/// Current pose of sensor `id` of the scan head.
pub fn sensor_camera(scan: &ScanStats, id: u32) -> Option<CameraInfo> {
    if id == 0 {
        Some(scan.camera)
    } else {
        scan.extra_cameras.as_slice().iter().find(|c| c.id == id).copied()
    }
}

fn frustum(info: &CameraInfo, color: [f32; 3], size: f32) -> FrustumGizmo {
    let aspect = if info.height > 0 { info.width as f32 / info.height as f32 } else { 4.0 / 3.0 };
    FrustumGizmo { world_from_cam: Mat4::from_cols_array(&info.world_from_cam), fov_y: info.fov_y, aspect, color, size }
}

/// The gizmos of a session: the rig cameras and a floor grid on the stage;
/// the scan path and the sensor head in the house while it is being
/// scanned (the sensor that looks ahead stands out, the others are drawn
/// smaller). Once the scan is done the house is shown bare, for walking
/// through it.
pub fn gizmos(state: &SessionState, scan: Option<&ScanStats>) -> GizmoScene {
    match state.config.scene {
        SceneKind::Stage => GizmoScene {
            grid: Some(GridGizmo { center: Vec3::ZERO, half_extent: 4.0, step: 0.5, color: GRID_COLOR }),
            frusta: state.cameras.iter().map(|c| frustum(c, RIG_COLOR, 0.28)).collect(),
            ..GizmoScene::default()
        },
        SceneKind::House => {
            let scan_state = scan.map_or(ScanState::Idle, |s| s.state);
            if scan_state == ScanState::Done {
                return GizmoScene::default();
            }
            let path = state.scan.as_ref().map(|s| s.path.iter().map(|p| Vec3::from_array(*p)).collect()).unwrap_or_default();
            let head = scan.filter(|s| matches!(s.state, ScanState::Scanning | ScanState::Paused));
            let handheld = head.map(|s| frustum(&s.camera, HANDHELD_COLOR, 0.35));
            let frusta = head.map(|s| s.extra_cameras.as_slice().iter().map(|c| frustum(c, HANDHELD_COLOR, 0.18)).collect()).unwrap_or_default();
            GizmoScene { path, handheld, frusta, ..GizmoScene::default() }
        }
    }
}

/// Distance at which a sphere of `radius` fills the smaller field of view.
fn fit_distance(radius: f32, fov_y: f32, aspect: f32) -> f32 {
    let half_y = (fov_y * 0.5).clamp(0.05, 1.4);
    let half_x = (half_y.tan() * aspect.clamp(0.2, 5.0)).atan();
    radius / half_y.min(half_x).sin()
}

/// Camera of the dollhouse view: above the house, tilted so that the rooms
/// read as a plan with some depth, the long side across the screen.
///
/// `wall_top` is the height the walls are cut at. `covered_right` is the
/// fraction of the view's width hidden by the side panel (`0` when it is
/// closed): the house is fitted into, and centered in, the part that
/// remains visible. Returns `(eye, target)`.
pub fn dollhouse_view(bounds: &Aabb, wall_top: f32, fov_y: f32, aspect: f32, covered_right: f32) -> (Vec3, Vec3) {
    /// Elevation of the camera above the horizon.
    const ELEVATION: f32 = 1.1;
    /// Free border around the house, in normalized device coordinates
    /// (the top bar and the chips at the bottom live there).
    const MARGIN: f32 = 0.12;
    let covered = if covered_right.is_finite() { covered_right.clamp(0.0, 0.6) } else { 0.0 };
    let wall_top = if wall_top.is_finite() { wall_top.max(0.0) } else { 0.0 };
    let center = floor_center(bounds);
    let half = bounds.size() * 0.5;
    let tan_y = (fov_y * 0.5).clamp(0.05, 1.4).tan();
    let tan_x = tan_y * aspect.clamp(0.2, 5.0);
    // The camera looks along -Z and down: +X is to the right of the screen.
    let direction = Vec3::new(0.0, ELEVATION.sin(), ELEVATION.cos());
    let (right, up, forward) = (Vec3::X, Vec3::new(0.0, ELEVATION.cos(), -ELEVATION.sin()), -direction);
    let place = |distance: f32| {
        // Aiming to the right of the house moves the house to the left,
        // into the middle of the part the panel leaves free.
        let target = center + right * (distance * tan_x * covered);
        (target + direction * distance, target)
    };
    let fits = |distance: f32| {
        let (eye, _) = place(distance);
        [-1.0f32, 1.0].into_iter().all(|sx| {
            [-1.0f32, 1.0].into_iter().all(|sz| {
                [0.0, wall_top].into_iter().all(|y| {
                    let v = center + Vec3::new(sx * half.x, y, sz * half.z) - eye;
                    let depth = v.dot(forward);
                    let (x, y) = (v.dot(right) / (depth * tan_x), v.dot(up) / (depth * tan_y));
                    depth > 0.0 && x >= -1.0 + MARGIN && x <= 1.0 - 2.0 * covered - MARGIN && y.abs() <= 1.0 - MARGIN
                })
            })
        })
    };
    // From too close, step back until the cut house fits.
    let mut distance = Vec3::new(half.x, 0.0, half.z).length().max(0.5);
    for _ in 0..200 {
        if fits(distance) {
            break;
        }
        distance *= 1.03;
    }
    place(distance)
}

/// Camera that frames a box from a three-quarter view. Returns `(eye, target)`.
pub fn frame_view(bounds: &Aabb, fov_y: f32, aspect: f32) -> (Vec3, Vec3) {
    let target = bounds.center();
    let radius = (bounds.size() * 0.5).length().max(0.05);
    let distance = fit_distance(radius, fov_y, aspect) * 1.05;
    let direction = Vec3::new(0.55, 0.42, 1.0).normalize();
    (target + direction * distance, target)
}

/// The view of a rig camera: its position, looking along its axis.
pub fn camera_view(info: &CameraInfo, look_distance: f32) -> (Vec3, Vec3) {
    let world_from_cam = Mat4::from_cols_array(&info.world_from_cam);
    let eye = world_from_cam.transform_point3(Vec3::ZERO);
    let forward = world_from_cam.transform_vector3(Vec3::NEG_Z).normalize_or(Vec3::NEG_Z);
    (eye, eye + forward * look_distance)
}

/// A box around the bulk of a cloud: per axis, the central 96 % of the
/// splat centers. Trained 3DGS files contain far "floaters" that would
/// otherwise push the camera miles away.
pub fn robust_bounds(splats: &[Splat]) -> Option<Aabb> {
    /// More than enough samples for stable percentiles.
    const MAX_SAMPLES: usize = 50_000;
    if splats.is_empty() {
        return None;
    }
    let step = splats.len().div_ceil(MAX_SAMPLES).max(1);
    let (mut min, mut max) = (Vec3::ZERO, Vec3::ZERO);
    for axis in 0..3 {
        let mut values: Vec<f32> = splats.iter().step_by(step).map(|s| s.pos[axis]).filter(|v| v.is_finite()).collect();
        if values.is_empty() {
            return None;
        }
        values.sort_by(f32::total_cmp);
        let last = values.len() - 1;
        min[axis] = values[last * 2 / 100];
        max[axis] = values[last - last * 2 / 100];
    }
    Some(Aabb::new(min, max))
}

#[cfg(test)]
mod tests {
    use gs_core::pipeline::Pipeline;
    use gs_core::protocol::PipelineConfig;
    use gs_render::nav::perspective;

    use super::*;

    fn house_state() -> SessionState {
        let mut config = PipelineConfig::light(SceneKind::House);
        config.scan.autostart = false;
        Pipeline::new(config).state()
    }

    fn stage_state() -> SessionState {
        let mut config = PipelineConfig::light(SceneKind::Stage);
        config.rig.count = 5;
        config.rig.width = 32;
        config.rig.height = 24;
        Pipeline::new(config).state()
    }

    /// Normalized device coordinates of `point` for a camera at `eye`
    /// looking at `target`; `None` behind the camera.
    fn ndc(eye: Vec3, target: Vec3, fov_y: f32, aspect: f32, point: Vec3) -> Option<Vec3> {
        let clip = perspective(fov_y, aspect, 0.05, 1000.0) * gs_core::Camera::look_at(eye, target, Vec3::Y, fov_y, 16, 16).view_matrix() * point.extend(1.0);
        (clip.w > 0.0).then(|| clip.truncate() / clip.w)
    }

    fn corners(bounds: &Aabb) -> Vec<Vec3> {
        (0..8)
            .map(|i| {
                Vec3::new(
                    if i & 1 == 0 { bounds.min.x } else { bounds.max.x },
                    if i & 2 == 0 { bounds.min.y } else { bounds.max.y },
                    if i & 4 == 0 { bounds.min.z } else { bounds.max.z },
                )
            })
            .collect()
    }

    #[test]
    fn stage_shows_the_rig_and_a_grid() {
        let state = stage_state();
        let scene = gizmos(&state, None);
        assert_eq!(scene.frusta.len(), 5);
        assert!(scene.grid.is_some() && scene.path.is_empty() && scene.handheld.is_none());
        // The gizmo sits where the camera is.
        let eye = scene.frusta[0].world_from_cam.transform_point3(Vec3::ZERO);
        assert!((eye - Vec3::from_slice(&state.cameras[0].world_from_cam[12..15])).length() < 1e-6);
        assert!((scene.frusta[0].aspect - 32.0 / 24.0).abs() < 1e-6);
        assert_eq!(default_nav(SceneKind::Stage), NavMode::Orbit);
        let hints = scene_hints(&state);
        assert_eq!(hints.ceiling_height, None);
        assert!(hints.bounds.is_some());
        assert_eq!(dollhouse_clip(&state), None);
    }

    #[test]
    fn house_shows_the_path_until_the_scan_is_done() {
        let state = house_state();
        let idle = gizmos(&state, None);
        assert!(idle.path.len() > 10 && idle.handheld.is_none() && idle.grid.is_none() && idle.frusta.is_empty());

        let mut scan = ScanStats { state: ScanState::Scanning, progress: 0.3, ..ScanStats::default() };
        scan.camera.world_from_cam[12..15].copy_from_slice(&[1.0, 1.4, -2.0]);
        scan.camera.width = 144;
        scan.camera.height = 108;
        let scanning = gizmos(&state, Some(&scan));
        let handheld = scanning.handheld.expect("handheld camera while scanning");
        assert_eq!(handheld.world_from_cam.transform_point3(Vec3::ZERO), Vec3::new(1.0, 1.4, -2.0));
        assert_eq!(scanning.path.len(), idle.path.len());

        scan.state = ScanState::Done;
        assert_eq!(gizmos(&state, Some(&scan)), GizmoScene::default());
        assert_eq!(default_nav(SceneKind::House), NavMode::Fly);
        assert!(default_splat_scale(SceneKind::House) > default_splat_scale(SceneKind::Stage));
        assert_eq!(default_splat_scale(SceneKind::Stage), 1.0);
    }

    /// Statistics of a house whose scan has just started.
    fn scanning_stats(head: ScanHead) -> (SessionState, ScanStats) {
        let mut config = PipelineConfig::light(SceneKind::House);
        config.scan.head = head;
        config.scan.width = 32;
        config.scan.height = 24;
        let mut pipeline = Pipeline::new(config);
        let state = pipeline.state();
        for _ in 0..20 {
            let stats = pipeline.tick(0.05).into_iter().rev().find_map(|m| match gs_core::codec::decode(&m.bytes).unwrap().body {
                gs_core::codec::Body::Stats(json) => serde_json::from_str::<gs_core::protocol::Stats>(&json).ok(),
                _ => None,
            });
            if let Some(scan) = stats.and_then(|s| s.scan).filter(|s| s.state == ScanState::Scanning && s.progress > 0.0) {
                return (state, scan);
            }
        }
        panic!("the scan did not start");
    }

    #[test]
    fn every_sensor_of_the_scan_head_has_a_tile_a_name_and_a_gizmo() {
        let (state, scan) = scanning_stats(ScanHead::Surround);
        let ids = camera_ids(&state);
        assert_eq!(ids, [0, 1, 2, 3, 4]);
        let labels: Vec<&str> = ids.iter().map(|id| sensor_label(ScanHead::Surround, *id)).collect();
        assert_eq!(labels, ["Avant", "Arrière gauche", "Arrière droite", "Haut", "Bas"]);

        // The names say where the sensors really look, relative to the one
        // that looks ahead.
        let axes = |info: &CameraInfo| {
            let m = Mat4::from_cols_array(&info.world_from_cam);
            (m.transform_vector3(Vec3::NEG_Z), m.transform_vector3(Vec3::X))
        };
        let (ahead, right) = axes(&sensor_camera(&scan, 0).unwrap());
        for id in 1..5 {
            let camera = sensor_camera(&scan, id).expect("pose of every sensor");
            assert_eq!(camera.id, id);
            let (forward, _) = axes(&camera);
            match sensor_label(ScanHead::Surround, id) {
                "Arrière gauche" => assert!(forward.dot(ahead) < 0.0 && forward.dot(right) < -0.5, "{id}: {forward}"),
                "Arrière droite" => assert!(forward.dot(ahead) < 0.0 && forward.dot(right) > 0.5, "{id}: {forward}"),
                "Haut" => assert!(forward.y > ahead.y + 0.5, "{id}: {forward}"),
                "Bas" => assert!(forward.y < ahead.y - 0.5, "{id}: {forward}"),
                other => panic!("unexpected label {other}"),
            }
        }
        assert_eq!(sensor_camera(&scan, 9), None);

        let scene = gizmos(&state, Some(&scan));
        assert!(scene.handheld.is_some());
        assert_eq!(scene.frusta.len(), 4);
        assert!(scene.frusta.iter().all(|f| f.size < scene.handheld.unwrap().size));

        // A head with one sensor is the mobile camera, alone.
        let (state, scan) = scanning_stats(ScanHead::Single);
        assert_eq!(camera_ids(&state), [0]);
        assert_eq!(sensor_label(ScanHead::Single, 0), "Caméra mobile");
        assert_eq!(sensor_label(ScanHead::Single, 3), "Capteur");
        let scene = gizmos(&state, Some(&scan));
        assert!(scene.handheld.is_some() && scene.frusta.is_empty());
        // The stage lists its ring cameras.
        assert_eq!(camera_ids(&stage_state()), [0, 1, 2, 3, 4]);
    }

    #[test]
    fn house_hints_and_clip() {
        let state = house_state();
        let hints = scene_hints(&state);
        let ceiling = hints.ceiling_height.expect("the house has a ceiling");
        assert_eq!(dollhouse_clip(&state), Some(ceiling - DOLLHOUSE_CLIP_MARGIN));
        assert_eq!(hints.floor_center.y, 0.0);
        assert!(hints.floor_center.x.abs() < 0.5 && hints.floor_center.z.abs() < 0.5);
    }

    #[test]
    fn dollhouse_view_looks_down_on_the_whole_plan() {
        let state = house_state();
        for aspect in [16.0 / 9.0, 4.0 / 3.0, 0.6] {
            // Panel closed, then covering the right 30 % of the view.
            for covered in [0.0, 0.3] {
                let fov_y = 50f32.to_radians();
                let (eye, target) = dollhouse_view(&state.bounds, dollhouse_clip(&state).unwrap(), fov_y, aspect, covered);
                assert!(eye.y > state.ceiling_height.unwrap() + 2.0 && eye.y < 60.0, "eye {eye}");
                assert_eq!(target.y, 0.0);
                let down = (target - eye).normalize();
                assert!(down.y < -0.8, "looks mostly down: {down}");
                // Every corner of the floor plan is on screen, left of the
                // panel, and so is the top of the walls above it.
                let right_edge = 1.0 - 2.0 * covered;
                let wall_top = dollhouse_clip(&state).unwrap();
                let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
                for corner in corners(&state.bounds).into_iter().filter(|c| c.y < 0.0) {
                    for y in [0.0, wall_top] {
                        let p = ndc(eye, target, fov_y, aspect, Vec3::new(corner.x, y, corner.z)).expect("in front");
                        assert!(p.x >= -1.0 && p.x <= right_edge && p.y.abs() <= 1.0, "aspect {aspect}, covered {covered}: {corner} at {p}");
                    }
                    let p = ndc(eye, target, fov_y, aspect, Vec3::new(corner.x, 0.0, corner.z)).unwrap();
                    (lo, hi) = (lo.min(p.x), hi.max(p.x));
                }
                // And the plan sits in the middle of the free part.
                let middle = (lo + hi) * 0.5;
                assert!((middle - (right_edge - 1.0) * 0.5).abs() < 0.05, "aspect {aspect}, covered {covered}: middle {middle}");
            }
        }
        // Nonsense input is ignored.
        let sane = dollhouse_view(&state.bounds, 2.35, 0.9, 1.5, 0.0);
        assert_eq!(dollhouse_view(&state.bounds, 2.35, 0.9, 1.5, f32::NAN), sane);
        assert_eq!(dollhouse_view(&state.bounds, 2.35, 0.9, 1.5, -3.0), sane);
        let flat = dollhouse_view(&state.bounds, f32::NAN, 0.9, 1.5, 0.0);
        assert!(flat.0.is_finite() && flat.0.y > 2.0);
    }

    #[test]
    fn frame_view_contains_the_box() {
        let bounds = Aabb::new(Vec3::new(-3.0, 0.0, -1.0), Vec3::new(2.0, 4.0, 5.0));
        for aspect in [2.0, 1.0, 0.5] {
            let fov_y = 50f32.to_radians();
            let (eye, target) = frame_view(&bounds, fov_y, aspect);
            assert_eq!(target, bounds.center());
            for corner in corners(&bounds) {
                let p = ndc(eye, target, fov_y, aspect, corner).expect("in front");
                assert!(p.x.abs() <= 1.0 && p.y.abs() <= 1.0, "aspect {aspect}: {corner} at {p}");
            }
        }
        // A degenerate box still gives a usable camera.
        let (eye, target) = frame_view(&Aabb::new(Vec3::ONE, Vec3::ONE), 0.9, 1.5);
        assert!(eye.is_finite() && (eye - target).length() > 0.05);
    }

    #[test]
    fn camera_view_follows_the_camera_axis() {
        let state = stage_state();
        let (eye, target) = camera_view(&state.cameras[2], 3.0);
        assert!(((target - eye).length() - 3.0).abs() < 1e-4);
        // Rig cameras look at the dancer, near the axis of the stage.
        let rig_target = state.config.rig.target;
        let to_target = (rig_target - eye).normalize();
        assert!(to_target.dot((target - eye).normalize()) > 0.999);
    }

    #[test]
    fn robust_bounds_ignore_floaters() {
        let mut splats: Vec<Splat> = (0..1000)
            .map(|i| {
                let t = i as f32 / 1000.0;
                Splat { pos: [t * 2.0 - 1.0, t, 0.5 - t], scale: [0.01; 3], rot: [0.0, 0.0, 0.0, 1.0], rgba: [255; 4] }
            })
            .collect();
        splats.push(Splat { pos: [5000.0, -900.0, 77.0], ..splats[0] });
        let bounds = robust_bounds(&splats).unwrap();
        assert!(bounds.max.x <= 1.0 && bounds.min.x >= -1.0 && bounds.size().x > 1.8, "{bounds:?}");
        assert!(bounds.min.y >= 0.0 && bounds.max.z <= 0.5);
        assert_eq!(robust_bounds(&[]), None);
        let one = robust_bounds(&splats[..1]).unwrap();
        assert_eq!(one.min, one.max);
    }
}
