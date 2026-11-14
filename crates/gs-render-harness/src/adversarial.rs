//! `?test=adversarial`: the GPU renderer against the CPU references on the
//! situations a renderer gets wrong first.
//!
//! The cases of [`crate::compare`] are the product's clouds from sensible
//! viewpoints. These are hand-built surfel clouds and viewpoints chosen to
//! stress one decision of the normalised mode each: splats that cross the
//! near plane or lie behind the eye, a surface seen at a grazing angle, two
//! surfaces a centimeter apart on either side of the depth tolerance, walls
//! seen from behind with and without back-face culling, views from 30 m and
//! 300 m, extreme layer weights, empty layers, faint and oversized splats.
//! Every case runs in the renderings that apply to it and is published like
//! a case of the compare page (`window.__results`, `window.__done`).
//!
//! `?only=<substring>` runs the cases whose name contains the substring.

use glam::Vec3;
use gs_core::{Camera, Splat};
use gs_render::{RenderParams, Viewer, ViewerOptions, LAYER_DYNAMIC, LAYER_STATIC};
use wasm_bindgen::prelude::*;
use web_sys::HtmlCanvasElement;

use crate::clouds;
use crate::compare::{base_params, look_at, renderings, run_case, HEIGHT, WIDTH};
use crate::dom;
use crate::scenes;

const ALL: &[&str] = &["sorted", "hard", "soft"];
const NORMALIZED: &[&str] = &["hard", "soft"];

/// A rectangular sheet of surfels in the plane through `origin` spanned by
/// `u` and `v` (unit vectors), facing `u × v`: `nu × nv` surfels `step`
/// apart that just touch, in a checkerboard of the two colours with 10 cm
/// tiles.
fn sheet(origin: Vec3, u: Vec3, v: Vec3, (nu, nv): (u32, u32), step: f32, colors: [[u8; 3]; 2]) -> Vec<Splat> {
    let normal = u.cross(v).normalize();
    let mut splats = Vec::with_capacity((nu * nv) as usize);
    for j in 0..nv {
        for i in 0..nu {
            let (a, b) = (i as f32 * step, j as f32 * step);
            let tile = (a / 0.1) as u32 + (b / 0.1) as u32;
            let [r, g, bl] = colors[(tile % 2) as usize];
            splats.push(Splat::surfel(origin + u * a + v * b, normal, 0.6 * step, 0.06 * step, [r, g, bl, 255]));
        }
    }
    splats
}

/// Two parallel sheets one centimeter apart, both facing +Z. The front one
/// (red and white) covers the left, the back one (blue and yellow) the
/// right, and they overlap in the middle.
fn two_sheets() -> Vec<Splat> {
    let mut splats = sheet(Vec3::new(-0.6, 0.2, 0.0), Vec3::X, Vec3::Y, (36, 51), 0.02, [[220, 40, 40], [240, 240, 235]]);
    splats.extend(sheet(Vec3::new(-0.3, 0.2, -0.01), Vec3::X, Vec3::Y, (46, 51), 0.02, [[40, 70, 220], [235, 215, 60]]));
    splats
}

/// A floor, and around a camera at `(0, 0.5, 0)` looking down −Z: a curtain
/// of large tilted surfels 12 cm in front of it whose discs reach through
/// the near plane and behind the eye, a row of splats between the eye and
/// the near plane, and a wall behind the eye that faces it.
fn near_plane_cloud() -> Vec<Splat> {
    let mut splats = clouds::floor(1.6, 0.03);
    let tilted = Vec3::new(0.0, 0.8, 0.6);
    for j in 0..5 {
        for i in 0..4 {
            let pos = Vec3::new(-0.3 + 0.08 * i as f32, 0.34 + 0.08 * j as f32, -0.12);
            let color = if (i + j) % 2 == 0 { [250, 150, 30, 255] } else { [60, 200, 120, 255] };
            splats.push(Splat::surfel(pos, tilted, 0.06, 0.004, color));
        }
    }
    for i in 0..8 {
        splats.push(Splat::surfel(Vec3::new(-0.07 + 0.02 * i as f32, 0.5, -0.03), Vec3::Z, 0.02, 0.002, [255, 0, 255, 255]));
    }
    splats.extend(sheet(Vec3::new(0.5, 0.0, 0.4), Vec3::NEG_X, Vec3::Y, (50, 50), 0.02, [[0, 255, 255], [255, 255, 0]]));
    splats
}

/// The floor and the sphere of the compare page as surfels, for the grazing
/// views.
fn floor_and_sphere() -> Vec<Splat> {
    let mut splats = clouds::floor(1.6, 0.02);
    splats.extend(clouds::sphere(Vec3::new(0.0, 0.5, 0.0), 0.5, 12_000));
    splats
}

/// A surfel far larger than the view next to a few of ordinary size.
fn giant_cloud() -> Vec<Splat> {
    let mut splats = sheet(Vec3::new(-0.4, 0.3, 0.3), Vec3::X, Vec3::Y, (40, 30), 0.02, [[230, 120, 30], [40, 160, 90]]);
    splats.push(Splat::surfel(Vec3::new(0.0, 0.7, 0.0), Vec3::Z, 5.0, 0.01, [90, 110, 200, 255]));
    splats
}

struct Case {
    name: &'static str,
    cam: Camera,
    params: RenderParams,
    only: &'static [&'static str],
}

/// The cases of one cloud.
struct Group {
    key: &'static str,
    /// Static layer, dynamic layer.
    layers: [Vec<Splat>; 2],
    cases: Vec<Case>,
}

fn hand_built() -> Vec<Group> {
    let params = base_params();
    let both_faces = RenderParams { cull_backfaces: false, ..params };
    let wide = |eye: [f32; 3], target: [f32; 3]| look_at(eye, target, WIDTH, HEIGHT);
    let faint: Vec<Splat> = two_sheets().into_iter().map(|s| Splat { rgba: [s.rgba[0], s.rgba[1], s.rgba[2], 3], ..s }).collect();
    vec![
        Group {
            key: "near",
            layers: [near_plane_cloud(), Vec::new()],
            cases: vec![
                Case { name: "near_plane_crossing", cam: wide([0.0, 0.5, 0.0], [0.0, 0.35, -1.0]), params, only: ALL },
                Case { name: "near_plane_crossing_both_faces", cam: wide([0.0, 0.5, 0.0], [0.0, 0.35, -1.0]), params: both_faces, only: NORMALIZED },
            ],
        },
        Group {
            key: "grazing",
            layers: [floor_and_sphere(), Vec::new()],
            cases: vec![
                // The floor at half a degree, then from exactly inside its
                // plane (the ray never meets the plane of a surfel there).
                Case { name: "grazing_floor", cam: wide([0.0, 0.015, 1.5], [0.0, 0.015, 0.0]), params, only: ALL },
                Case { name: "grazing_floor_in_plane", cam: wide([0.0, 0.0, 1.5], [0.0, 0.0, 0.0]), params, only: ALL },
                // From below the floor: culled, or the sphere through it.
                Case { name: "floor_from_below", cam: wide([0.4, -0.6, 1.2], [0.0, 0.3, 0.0]), params, only: NORMALIZED },
                Case { name: "floor_from_below_both_faces", cam: wide([0.4, -0.6, 1.2], [0.0, 0.3, 0.0]), params: both_faces, only: NORMALIZED },
            ],
        },
        Group {
            key: "sheets",
            layers: [two_sheets(), Vec::new()],
            cases: vec![
                // 1 cm at 0.5 m is 2 %: two surfaces. At 3 m it is 0.33 %: one.
                Case { name: "sheets_1cm_from_0.5m", cam: wide([0.0, 0.7, 0.5], [0.0, 0.7, 0.0]), params, only: ALL },
                Case { name: "sheets_1cm_from_3m", cam: wide([0.3, 0.9, 3.0], [0.0, 0.7, 0.0]), params, only: ALL },
                Case { name: "sheets_1cm_oblique", cam: wide([1.2, 0.8, 0.9], [0.0, 0.7, 0.0]), params, only: ALL },
                Case { name: "sheets_from_behind", cam: wide([0.2, 0.7, -1.5], [0.0, 0.7, 0.0]), params, only: NORMALIZED },
                Case { name: "sheets_from_behind_both_faces", cam: wide([0.2, 0.7, -1.5], [0.0, 0.7, 0.0]), params: both_faces, only: NORMALIZED },
            ],
        },
        Group {
            key: "faint",
            layers: [faint, Vec::new()],
            cases: vec![Case { name: "faint_sheets", cam: wide([0.0, 0.7, 0.9], [0.0, 0.7, 0.0]), params, only: ALL }],
        },
        Group {
            key: "giant",
            layers: [giant_cloud(), Vec::new()],
            cases: vec![Case { name: "giant_surfel", cam: wide([0.3, 0.8, 2.0], [0.0, 0.7, 0.0]), params, only: ALL }],
        },
        Group {
            key: "one_layer",
            // Nothing in the static layer: the dynamic one alone.
            layers: [Vec::new(), two_sheets()],
            cases: vec![Case { name: "dynamic_layer_alone", cam: wide([0.5, 0.9, 1.4], [0.0, 0.7, 0.0]), params, only: ALL }],
        },
        Group {
            key: "nothing",
            layers: [Vec::new(), Vec::new()],
            cases: vec![Case { name: "no_splats", cam: wide([0.5, 0.9, 1.4], [0.0, 0.7, 0.0]), params, only: ALL }],
        },
    ]
}

/// The live stage from far away and with extreme layer weights.
fn stage_cases() -> Vec<Case> {
    let params = base_params();
    let target = [0.0, 0.9, 0.0];
    let orbit = look_at([2.3, 1.9, 3.1], target, WIDTH, HEIGHT);
    let from = |scale: f32, fov_degrees: f32| {
        Camera::look_at(Vec3::new(17.0, 11.0, 23.0) * scale, Vec3::from(target), Vec3::Y, fov_degrees.to_radians(), WIDTH, HEIGHT)
    };
    let weights = |weights: [f32; 2]| RenderParams { layer_weights: weights, ..params };
    vec![
        Case { name: "stage_30m", cam: from(1.0, 50.0), params, only: ALL },
        // The same distance through a long lens: the splats keep their size
        // on screen, the depths are those of 30 m.
        Case { name: "stage_30m_tele", cam: from(1.0, 6.0), params, only: ALL },
        Case { name: "stage_300m", cam: from(10.0, 50.0), params, only: ALL },
        Case { name: "stage_300m_tele", cam: from(10.0, 0.6), params, only: NORMALIZED },
        Case { name: "weights_live_10000", cam: orbit, params: weights([1.0, 1.0e4]), only: NORMALIZED },
        Case { name: "weights_static_10000", cam: orbit, params: weights([1.0e4, 1.0]), only: NORMALIZED },
        Case { name: "weights_both_10000", cam: orbit, params: weights([1.0e4, 1.0e4]), only: NORMALIZED },
        Case { name: "weights_live_zero", cam: orbit, params: weights([1.0, 0.0]), only: NORMALIZED },
        Case { name: "weights_static_zero", cam: orbit, params: weights([0.0, 16.0]), only: NORMALIZED },
        Case { name: "weights_both_tiny", cam: orbit, params: weights([1.0e-3, 1.0e-3]), only: NORMALIZED },
    ]
}

async fn run_group(viewer: &Viewer, results: &js_sys::Array, cases: &[Case], wanted: &dyn Fn(&str) -> bool) -> Result<(), String> {
    for case in cases {
        for (suffix, params) in renderings(case.params) {
            let name = format!("{}_{suffix}", case.name);
            if case.only.contains(&suffix) && wanted(&name) {
                run_case(viewer, results, &name, &case.cam, params).await?;
            }
        }
    }
    Ok(())
}

async fn run_cases(viewer: &Viewer, results: &js_sys::Array) -> Result<(), String> {
    let only = dom::query("only");
    let wanted = |name: &str| only.as_deref().is_none_or(|filter| name.contains(filter));
    let any_wanted = |cases: &[Case]| cases.iter().any(|case| case.only.iter().any(|suffix| wanted(&format!("{}_{suffix}", case.name))));

    for group in hand_built() {
        if !any_wanted(&group.cases) {
            continue;
        }
        dom::set_status(&format!("adversarial: {}", group.key));
        viewer.clear_layers();
        viewer.set_surfel_layer(LAYER_STATIC, &group.layers[0])?;
        viewer.set_surfel_layer(LAYER_DYNAMIC, &group.layers[1])?;
        run_group(viewer, results, &group.cases, &wanted).await?;
    }

    let stage = stage_cases();
    if any_wanted(&stage) {
        scenes::stage(2.4)?.upload(viewer)?;
        run_group(viewer, results, &stage, &wanted).await?;
        // The live layer emptied by a `Replace` without splats.
        let cam = look_at([2.3, 1.9, 3.1], [0.0, 0.9, 0.0], WIDTH, HEIGHT);
        viewer.set_surfel_layer(LAYER_DYNAMIC, &[])?;
        run_group(viewer, results, &[Case { name: "stage_live_layer_emptied", cam, params: base_params(), only: ALL }], &wanted).await?;
    }
    Ok(())
}

pub fn start() -> Result<(), JsValue> {
    let canvas: HtmlCanvasElement = dom::by_id("gl");
    let style = canvas.style();
    style.set_property("position", "static")?;
    style.set_property("width", &format!("{WIDTH}px"))?;
    style.set_property("height", &format!("{HEIGHT}px"))?;
    let viewer = Viewer::new(canvas, ViewerOptions { params: base_params(), max_dpr: 1.0, ..ViewerOptions::default() })?;
    let results = js_sys::Array::new();
    dom::set_global("__results", &results);
    wasm_bindgen_futures::spawn_local(async move {
        match run_cases(&viewer, &results).await {
            Ok(()) => dom::set_status("adversarial: done"),
            Err(error) => {
                dom::set_status(&format!("adversarial: FAILED: {error}"));
                dom::set_global("__error", &error.into());
            }
        }
        dom::set_global("__done", &true.into());
        std::mem::forget(viewer);
    });
    Ok(())
}
