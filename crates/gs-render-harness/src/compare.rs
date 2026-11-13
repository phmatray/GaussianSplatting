//! `?test=compare`: the GPU renderer against the CPU reference rasterizer.
//!
//! Every case uploads a deterministic cloud to the viewer, renders it with
//! the WebGL2 pipeline, renders the same splats with
//! `gs_core::raster::rasterize` in WASM and publishes PSNR and mean absolute
//! error in `window.__results`. Both images and their amplified difference
//! are appended to the page for screenshots.

use glam::{Affine3A, Mat4, Quat, Vec3};
use gs_core::capture::{render_rgbd, CaptureOptions};
use gs_core::codec::{self, LayerMode};
use gs_core::raster::{mean_abs_error, psnr, rasterize, Image, RasterOptions};
use gs_core::rig::RingRig;
use gs_core::scene::{Scene, StageScene};
use gs_core::{Camera, Splat};
use gs_render::rig::Rig;
use gs_render::{RenderParams, SceneHints, Viewer, ViewerOptions, LAYER_DYNAMIC, LAYER_STATIC};
use wasm_bindgen::prelude::*;
use web_sys::{HtmlCanvasElement, HtmlElement};

use crate::clouds::{self, UnprojectOptions};
use crate::dom;

/// Background of every case; not black, so that a wrong clear color shows.
const BACKGROUND: [u8; 3] = [18, 20, 28];
/// The difference image is amplified by this factor.
const DIFF_GAIN: u8 = 16;
const WIDTH: u32 = 480;
const HEIGHT: u32 = 360;

/// How the splats of a case reach the viewer.
#[derive(Clone, Copy, PartialEq)]
enum Upload {
    /// One `set_layer` on the static layer.
    Static,
    /// Half in the static layer (a `set_layer` followed by an `append_layer`),
    /// half in the dynamic layer through a wire message.
    BothLayers,
}

struct Case {
    name: &'static str,
    /// Index into the list of clouds.
    cloud: usize,
    upload: Upload,
    cam: Camera,
    params: RenderParams,
}

fn base_params() -> RenderParams {
    RenderParams {
        background: BACKGROUND.map(|c| c as f32 / 255.0),
        // The reference has no fill-rate clamp.
        max_radius_px: 1.0e6,
        show_gizmos: false,
        ..RenderParams::default()
    }
}

fn look_at(eye: [f32; 3], target: [f32; 3], width: u32, height: u32) -> Camera {
    Camera::look_at(Vec3::from(eye), Vec3::from(target), Vec3::Y, 50f32.to_radians(), width, height)
}

/// The stage scene as naive surfels from a ring of simulated RGB-D cameras.
fn captured_stage() -> Vec<Splat> {
    let snapshot = StageScene::new().snapshot(2.4);
    let rig = RingRig { count: 6, width: 160, height: 120, ..RingRig::default() };
    let mut splats = Vec::new();
    for cam in rig.cameras() {
        let frame = render_rgbd(&snapshot, &cam, &CaptureOptions::default());
        clouds::unproject_frame(&frame, &cam, &UnprojectOptions::default(), |_, _, _| true, &mut splats);
    }
    splats
}

/// The view of a VR user looking at the 1:15 miniature of the stage: the
/// camera pose is `world_from_rig · rig_from_head`, so its view matrix
/// carries the uniform world scale exactly like an XR eye in dollhouse mode.
fn miniature_camera() -> Camera {
    let hints = SceneHints { floor_center: Vec3::ZERO, ..SceneHints::default() };
    let rig_from_head =
        Mat4::from_rotation_translation(Quat::from_rotation_y(0.4) * Quat::from_rotation_x(-0.75), Vec3::new(0.2, 1.6, 0.1));
    let rig = Rig::miniature(&hints, &rig_from_head, 15.0, 0.3, 0.0);
    let world_from_cam = Affine3A::from_mat4(rig.world_from_rig() * rig_from_head);
    Camera::from_pose(world_from_cam, 70f32.to_radians(), WIDTH, HEIGHT)
}

/// Splats a hostile or broken source could send: none of them may crash,
/// poison the frame or disturb the well-formed splats around them.
fn degenerate_splats(center: Vec3) -> Vec<Splat> {
    let ok = Splat::surfel(center, Vec3::Z, 0.05, 0.005, [255, 60, 200, 255]);
    let at = |x: f32, y: f32| (center + Vec3::new(x, y, 0.0)).to_array();
    vec![
        Splat { pos: [f32::NAN, 0.5, 0.0], ..ok },
        Splat { pos: [0.0, f32::INFINITY, 0.0], ..ok },
        Splat { pos: [0.0, 0.5, f32::NEG_INFINITY], ..ok },
        Splat { pos: at(0.2, 0.0), scale: [f32::NAN, 0.05, 0.05], ..ok },
        Splat { pos: at(0.4, 0.0), scale: [f32::INFINITY, 0.05, 0.05], ..ok },
        Splat { pos: at(-0.2, 0.0), scale: [1.0e15, 1.0e15, 1.0e15], ..ok },
        Splat { pos: at(-0.4, 0.0), scale: [-0.05, 0.05, 0.05], ..ok },
        // A point: only the low-pass filter gives it a footprint.
        Splat { pos: at(0.0, 0.3), scale: [0.0; 3], ..ok },
        // Zero and non-finite quaternions mean "no rotation".
        Splat { pos: at(0.2, 0.3), scale: [0.08, 0.01, 0.01], rot: [0.0; 4], ..ok },
        Splat { pos: at(-0.2, 0.3), scale: [0.08, 0.01, 0.01], rot: [f32::NAN, 0.0, 0.0, 1.0], ..ok },
        // Not normalized.
        Splat { pos: at(0.4, 0.3), scale: [0.08, 0.01, 0.01], rot: [0.0, 0.0, 30.0, 40.0], ..ok },
        // Invisible.
        Splat { pos: at(0.0, 0.6), rgba: [255, 255, 255, 0], ..ok },
        ok,
    ]
}

fn cases() -> (Vec<Vec<Splat>>, Vec<Case>) {
    let sphere_center = Vec3::new(0.0, 0.5, 0.0);
    let needle_center = Vec3::new(-1.0, 0.7, -0.6);
    let blob_center = Vec3::new(0.0, 1.0, 0.0);
    let mut sphere_floor = clouds::floor(1.6, 0.02);
    sphere_floor.extend(clouds::sphere(sphere_center, 0.5, 12_000));
    let mut hostile = sphere_floor.clone();
    hostile.extend(degenerate_splats(Vec3::new(0.0, 1.2, 0.0)));
    // One stray splat a thousand kilometers away, behind the camera (think
    // of a floater in an imported file): it must not cost the visible
    // splats their depth order.
    let mut outlier = sphere_floor.clone();
    outlier.push(Splat::surfel(Vec3::new(4.0e5, 3.0e5, 8.0e5), Vec3::Y, 10.0, 1.0, [255, 255, 255, 255]));
    let clouds = vec![
        sphere_floor,
        clouds::needles(needle_center, 0.5),
        clouds::translucent(blob_center, 1500, 11),
        captured_stage(),
        hostile,
        outlier,
    ];
    let (scene, needles, blobs, stage, hostile, outlier) = (0, 1, 2, 3, 4, 5);

    let params = base_params();
    let cam_a = look_at([1.7, 1.4, 2.4], [-0.2, 0.4, 0.0], WIDTH, HEIGHT);
    let cam_stage = look_at([2.3, 1.9, 3.1], [0.0, 0.9, 0.0], WIDTH, HEIGHT);
    // What a WebXR eye looks like: principal point far from the center and
    // different focal lengths per axis.
    let off_axis = Camera { fx: 430.0, fy: 395.0, cx: 0.31 * WIDTH as f32, cy: 0.62 * HEIGHT as f32, ..cam_a };

    let cases = vec![
        Case { name: "sphere_floor", cloud: scene, upload: Upload::Static, cam: cam_a, params },
        Case {
            name: "sphere_close",
            cloud: scene,
            upload: Upload::Static,
            cam: look_at([0.55, 0.8, 0.95], [0.0, 0.5, 0.0], WIDTH, HEIGHT),
            params,
        },
        Case { name: "off_axis_projection", cloud: scene, upload: Upload::Static, cam: off_axis, params },
        Case {
            name: "splat_scale_half",
            cloud: scene,
            upload: Upload::Static,
            cam: cam_a,
            params: RenderParams { splat_scale: 0.5, ..params },
        },
        Case {
            name: "needles",
            cloud: needles,
            upload: Upload::Static,
            cam: look_at((needle_center + Vec3::new(0.3, 0.1, 1.5)).to_array(), needle_center.to_array(), 400, 400),
            params,
        },
        Case {
            name: "translucent",
            cloud: blobs,
            upload: Upload::Static,
            cam: look_at([1.2, 1.5, 2.2], blob_center.to_array(), WIDTH, HEIGHT),
            params,
        },
        Case {
            name: "translucent_two_layers",
            cloud: blobs,
            upload: Upload::BothLayers,
            cam: look_at([-2.0, 0.8, 1.2], blob_center.to_array(), WIDTH, HEIGHT),
            params,
        },
        Case { name: "captured_stage", cloud: stage, upload: Upload::BothLayers, cam: cam_stage, params },
        Case {
            name: "captured_stage_close",
            cloud: stage,
            upload: Upload::BothLayers,
            cam: look_at([0.5, 1.5, 1.6], [0.0, 1.1, 0.0], WIDTH, HEIGHT),
            params,
        },
        Case {
            name: "captured_stage_points",
            cloud: stage,
            upload: Upload::BothLayers,
            cam: cam_stage,
            params: RenderParams { point_mode: true, ..params },
        },
        Case { name: "captured_stage_miniature", cloud: stage, upload: Upload::BothLayers, cam: miniature_camera(), params },
        Case {
            name: "captured_stage_clip_y",
            cloud: stage,
            upload: Upload::BothLayers,
            cam: cam_stage,
            params: RenderParams { clip_y_max: Some(1.0), ..params },
        },
        Case { name: "degenerate_splats", cloud: hostile, upload: Upload::Static, cam: cam_a, params },
        Case { name: "far_outlier", cloud: outlier, upload: Upload::Static, cam: cam_a, params },
    ];
    (clouds, cases)
}

/// Puts `splats` into the viewer the way `upload` says.
fn upload(viewer: &Viewer, splats: &[Splat], how: Upload) -> Result<(), String> {
    viewer.clear_layers();
    match how {
        Upload::Static => viewer.set_layer(LAYER_STATIC, splats),
        Upload::BothLayers => {
            let (first, rest) = splats.split_at(splats.len() / 4);
            let (second, dynamic) = rest.split_at(rest.len() / 3);
            viewer.set_layer(LAYER_STATIC, first)?;
            viewer.append_layer(LAYER_STATIC, second)?;
            let mut message = Vec::new();
            codec::encode_layer(1, 0, LAYER_DYNAMIC, LayerMode::Replace, dynamic, &mut message);
            viewer.apply_message(&message).map(|_| ())
        }
    }
}

/// The reference image for what the viewer currently holds.
fn reference(viewer: &Viewer, cam: &Camera, params: &RenderParams) -> Image {
    // The viewer's own copy: after a wire round trip it is the quantized data.
    let mut splats = viewer.splats();
    if let Some(clip) = params.clip_y_max {
        splats.retain(|s| s.pos[1] <= clip);
    }
    let options = RasterOptions {
        background: BACKGROUND,
        splat_scale: params.splat_scale,
        point_mode: params.point_mode,
        point_sigma_px: params.point_sigma_px,
        ..RasterOptions::default()
    };
    rasterize(&splats, cam, &options)
}

fn difference(a: &Image, b: &Image) -> Image {
    let rgb = a.rgb.iter().zip(&b.rgb).map(|(x, y)| x.abs_diff(*y).saturating_mul(DIFF_GAIN)).collect();
    Image { width: a.width, height: a.height, rgb }
}

struct Outcome {
    name: &'static str,
    psnr: f32,
    mae: f32,
    splats: usize,
    gpu_ms: f64,
    cpu_ms: f64,
}

/// Shows one result row and records it in `results`.
fn publish(results: &js_sys::Array, outcome: &Outcome, gpu: &Image, cpu: &Image) {
    let document = dom::document();
    let row: HtmlElement = document.create_element("div").expect("createElement").unchecked_into();
    row.set_class_name("case");
    row.set_id(&format!("case-{}", outcome.name));
    let caption: HtmlElement = document.create_element("p").expect("createElement").unchecked_into();
    caption.set_text_content(Some(&format!(
        "{}: PSNR {:.2} dB, mean abs error {:.3}, {} splats (GPU {:.1} ms, CPU {:.0} ms). Left: WebGL2, middle: CPU reference, right: |difference| x{}",
        outcome.name, outcome.psnr, outcome.mae, outcome.splats, outcome.gpu_ms, outcome.cpu_ms, DIFF_GAIN
    )));
    let _ = row.append_child(&caption);
    dom::append_image(&row, gpu, "WebGL2");
    dom::append_image(&row, cpu, "CPU reference");
    dom::append_image(&row, &difference(gpu, cpu), "difference");
    let _ = dom::by_id::<HtmlElement>("results").append_child(&row);

    let entry = js_sys::Object::new();
    dom::set(&entry, "name", outcome.name);
    // JSON has no Infinity: identical images report 99 dB.
    dom::set(&entry, "psnr", outcome.psnr.min(99.0));
    dom::set(&entry, "mae", outcome.mae);
    dom::set(&entry, "splats", outcome.splats as u32);
    dom::set(&entry, "gpuMs", outcome.gpu_ms);
    dom::set(&entry, "cpuMs", outcome.cpu_ms);
    results.push(&entry);
}

/// The on-screen path: the desktop camera drawn by the frame loop into the
/// canvas itself, read back in the frame it was drawn.
async fn canvas_case(viewer: &Viewer, canvas: &HtmlCanvasElement, results: &js_sys::Array) -> Result<(), String> {
    let (eye, target) = (Vec3::new(2.3, 1.9, 3.1), Vec3::new(0.0, 0.9, 0.0));
    let params = base_params();
    viewer.set_params(params);
    viewer.set_fov_y(50f32.to_radians());
    for _ in 0..120 {
        let rendered = viewer.stats().frames_rendered;
        // Also marks the view dirty, so the next frame draws.
        viewer.set_view(eye, target);
        dom::next_frame().await;
        if viewer.stats().frames_rendered == rendered {
            continue;
        }
        let start = dom::now();
        let gpu = dom::read_canvas(canvas).ok_or("could not read the canvas back")?;
        let gpu_ms = dom::now() - start;
        let cam = Camera::look_at(eye, target, Vec3::Y, viewer.fov_y(), gpu.width, gpu.height);
        let start = dom::now();
        let cpu = reference(viewer, &cam, &params);
        let cpu_ms = dom::now() - start;
        let outcome = Outcome {
            name: "canvas_desktop_camera",
            psnr: psnr(&gpu, &cpu),
            mae: mean_abs_error(&gpu, &cpu),
            splats: viewer.splats().len(),
            gpu_ms,
            cpu_ms,
        };
        publish(results, &outcome, &gpu, &cpu);
        return Ok(());
    }
    Err("the viewer never drew a frame".into())
}

async fn run_cases(viewer: &Viewer, canvas: &HtmlCanvasElement, results: &js_sys::Array) -> Result<(), String> {
    let start = dom::now();
    let (clouds, cases) = cases();
    web_sys::console::log_1(&format!("[compare] clouds built in {:.0} ms", dom::now() - start).into());
    let mut current = None;
    for case in &cases {
        dom::set_status(&format!("compare: {}", case.name));
        // Let the page paint the previous row.
        dom::sleep(0).await;
        if current != Some((case.cloud, case.upload == Upload::BothLayers)) {
            upload(viewer, &clouds[case.cloud], case.upload)?;
            current = Some((case.cloud, case.upload == Upload::BothLayers));
        }
        viewer.set_params(case.params);
        let start = dom::now();
        let gpu = viewer.snapshot(&case.cam).map_err(|e| format!("snapshot failed: {e:?}"))?;
        let gpu_ms = dom::now() - start;
        let start = dom::now();
        let cpu = reference(viewer, &case.cam, &case.params);
        let cpu_ms = dom::now() - start;
        let outcome = Outcome {
            name: case.name,
            psnr: psnr(&gpu, &cpu),
            mae: mean_abs_error(&gpu, &cpu),
            splats: viewer.splats().len(),
            gpu_ms,
            cpu_ms,
        };
        web_sys::console::log_1(&format!("[compare] {}: PSNR {:.2} dB, MAE {:.3}", outcome.name, outcome.psnr, outcome.mae).into());
        publish(results, &outcome, &gpu, &cpu);
    }
    dom::set_global("__snapshotSizes", &snapshot_sizes(viewer).into());
    dom::set_status("compare: canvas_desktop_camera");
    canvas_case(viewer, canvas, results).await
}

/// Degenerate snapshot sizes: no pixels, one pixel, far too many pixels.
/// Returns a description of what went wrong, or `"ok"`.
fn snapshot_sizes(viewer: &Viewer) -> String {
    let cam = look_at([1.7, 1.4, 2.4], [0.0, 0.4, 0.0], 1, 1);
    let mut problems = Vec::new();
    match viewer.snapshot(&cam) {
        Ok(image) if (image.width, image.height, image.rgb.len()) == (1, 1, 3) => {}
        other => problems.push(format!("1x1: {:?}", other.map(|i| i.rgb.len()))),
    }
    for (width, height) in [(0, 0), (0, 64), (64, 0)] {
        match viewer.snapshot(&Camera { width, height, ..cam }) {
            Ok(image) if image.rgb.is_empty() => {}
            other => problems.push(format!("{width}x{height}: {:?}", other.map(|i| i.rgb.len()))),
        }
    }
    // 4·10^10 bytes: must be refused, not attempted.
    for (width, height) in [(100_000, 100_000), (u32::MAX, 2), (3, 9000)] {
        if let Ok(image) = viewer.snapshot(&Camera { width, height, ..cam }) {
            problems.push(format!("{width}x{height} gave {} bytes", image.rgb.len()));
        }
    }
    if problems.is_empty() {
        "ok".to_string()
    } else {
        problems.join("; ")
    }
}

pub fn start() -> Result<(), JsValue> {
    let canvas: HtmlCanvasElement = dom::by_id("gl");
    // Fixed size: the on-screen case compares against a camera of this size.
    let style = canvas.style();
    style.set_property("position", "static")?;
    style.set_property("width", &format!("{WIDTH}px"))?;
    style.set_property("height", &format!("{HEIGHT}px"))?;
    let viewer = Viewer::new(canvas.clone(), ViewerOptions { params: base_params(), max_dpr: 1.0, ..ViewerOptions::default() })?;
    let results = js_sys::Array::new();
    dom::set_global("__results", &results);
    wasm_bindgen_futures::spawn_local(async move {
        match run_cases(&viewer, &canvas, &results).await {
            Ok(()) => dom::set_status("compare: done"),
            Err(error) => {
                dom::set_status(&format!("compare: FAILED: {error}"));
                dom::set_global("__error", &error.into());
            }
        }
        dom::set_global("__done", &true.into());
        // Keep the viewer (and the canvas picture) alive for the screenshots.
        std::mem::forget(viewer);
    });
    Ok(())
}
