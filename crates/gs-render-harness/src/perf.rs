//! `?test=perf&n=200000`: CPU-side cost of the viewer for `n` splats.
//!
//! Fills the static layer with `n` surfels, then orbits the camera so that
//! every frame needs a new depth order, and reports sort and frame times in
//! `window.__perf`. Also measures what the data paths cost: appending a chunk
//! to the (already large) static layer and replacing the dynamic layer.
//!
//! GPU time is not measured: WebGL does not expose it, and in a headless
//! browser the rasterizer is software anyway.

use glam::Vec3;
use gs_core::codec::{self, LayerMode};
use gs_render::{Viewer, ViewerOptions, LAYER_DYNAMIC, LAYER_STATIC};
use wasm_bindgen::prelude::*;
use web_sys::HtmlCanvasElement;

use crate::clouds::random_surfels;
use crate::dom;

/// Frames of the orbit measurement.
const FRAMES: u32 = 90;
/// Splats in the dynamic layer and in one appended chunk.
const DYNAMIC_SPLATS: usize = 30_000;
const REPEATS: u32 = 10;

fn mean(values: &[f64]) -> f64 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    }
}

async fn run(viewer: &Viewer, n: usize) -> Result<js_sys::Object, String> {
    let half_extent = Vec3::new(4.0, 1.25, 4.0);
    let report = js_sys::Object::new();
    dom::set(&report, "n", n as u32);

    dom::set_status(&format!("perf: building {n} splats"));
    dom::sleep(0).await;
    let splats = random_surfels(n, half_extent, 5);
    let start = dom::now();
    viewer.set_layer(LAYER_STATIC, &splats)?;
    dom::set(&report, "uploadStaticMs", dom::now() - start);

    // Data paths, with the big static layer in place.
    let chunk = random_surfels(DYNAMIC_SPLATS, half_extent, 6);
    let mut message = Vec::new();
    let (mut append_ms, mut replace_ms) = (Vec::new(), Vec::new());
    for i in 0..REPEATS {
        codec::encode_layer(i, 0, LAYER_DYNAMIC, LayerMode::Replace, &chunk, &mut message);
        let start = dom::now();
        viewer.apply_message(&message)?;
        replace_ms.push(dom::now() - start);
    }
    for i in 0..REPEATS {
        codec::encode_layer(i, 0, LAYER_STATIC, LayerMode::Append, &chunk[..DYNAMIC_SPLATS / 3], &mut message);
        let start = dom::now();
        viewer.apply_message(&message)?;
        append_ms.push(dom::now() - start);
    }
    // Back to exactly `n` static splats for the frame measurement.
    viewer.set_layer(LAYER_STATIC, &splats)?;
    dom::set(&report, "dynamicSplats", DYNAMIC_SPLATS as u32);
    dom::set(&report, "replaceDynamicMs", mean(&replace_ms));
    dom::set(&report, "appendChunkSplats", (DYNAMIC_SPLATS / 3) as u32);
    dom::set(&report, "appendChunkMs", mean(&append_ms));

    dom::set_status("perf: orbiting");
    let target = Vec3::new(0.0, 1.0, 0.0);
    let (mut sort_ms, mut intervals) = (Vec::new(), Vec::new());
    let first = viewer.stats();
    let mut last = dom::now();
    for frame in 0..FRAMES {
        // Two degrees per frame: far beyond the "same direction" tolerance.
        let angle = (frame as f32 * 2.0).to_radians();
        viewer.set_view(target + Vec3::new(9.0 * angle.sin(), 2.5, 9.0 * angle.cos()), target);
        let sorts = viewer.stats().sorts;
        dom::next_frame().await;
        let stats = viewer.stats();
        if stats.sorts > sorts {
            sort_ms.push(stats.sort_ms as f64);
        }
        let now = dom::now();
        intervals.push(now - last);
        last = now;
    }
    let stats = viewer.stats();
    dom::set(&report, "frames", FRAMES);
    dom::set(&report, "framesRendered", (stats.frames_rendered - first.frames_rendered) as f64);
    dom::set(&report, "sorts", (stats.sorts - first.sorts) as f64);
    dom::set(&report, "sortMsMean", mean(&sort_ms));
    dom::set(&report, "sortMsMax", sort_ms.iter().copied().fold(0.0, f64::max));
    // Smoothed CPU time per animation frame inside the viewer (sort, uploads, draw calls).
    dom::set(&report, "viewerCpuMs", stats.frame_ms);
    dom::set(&report, "frameIntervalMs", mean(&intervals));
    dom::set(&report, "splatsTotal", stats.splats_total);
    dom::set(&report, "splatsDrawn", stats.splats_drawn);
    dom::set(&report, "canvasWidth", stats.canvas_width);
    dom::set(&report, "canvasHeight", stats.canvas_height);
    Ok(report)
}

pub fn start() -> Result<(), JsValue> {
    let canvas: HtmlCanvasElement = dom::by_id("gl");
    let n = dom::query_number("n", 200_000usize).clamp(1, 4_000_000);
    let viewer = Viewer::new(canvas, ViewerOptions::default())?;
    wasm_bindgen_futures::spawn_local(async move {
        match run(&viewer, n).await {
            Ok(report) => {
                dom::set_status("perf: done");
                dom::set_global("__perf", &report);
            }
            Err(error) => {
                dom::set_status(&format!("perf: FAILED: {error}"));
                dom::set_global("__error", &error.into());
            }
        }
        std::mem::forget(viewer);
    });
    Ok(())
}
