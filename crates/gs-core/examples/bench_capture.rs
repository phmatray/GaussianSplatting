//! Timings of the scene and sensor simulation.
//!
//! ```text
//! cargo run -p gs-core --release --example bench_capture                  # single-threaded
//! cargo run -p gs-core --release --features par --example bench_capture   # rayon
//! ```

use std::hint::black_box;
use std::time::Instant;

use gs_core::capture::{render_rgbd, CaptureOptions, NoiseModel};
use gs_core::rig::{RingRig, ScanPath};
use gs_core::scene::{make_scene, SceneKind};

/// Best-of-`runs` wall time of `f` in milliseconds.
fn time_ms(runs: usize, mut f: impl FnMut()) -> f64 {
    (0..runs)
        .map(|_| {
            let start = Instant::now();
            f();
            start.elapsed().as_secs_f64() * 1e3
        })
        .fold(f64::INFINITY, f64::min)
}

fn main() {
    let threads = if cfg!(feature = "par") {
        "rayon"
    } else {
        "single-threaded"
    };
    println!("capture benchmark ({threads})");

    // Stage: snapshot build, then the whole ring with shadows.
    let stage = make_scene(SceneKind::Stage);
    let mut t = 0.0f32;
    let build = time_ms(2000, || {
        t += 0.0137;
        black_box(stage.snapshot(black_box(t)));
    });
    println!(
        "stage snapshot build ({} primitives): {:.3} ms",
        stage.snapshot(0.0).len(),
        build
    );

    let cams = RingRig::default().cameras();
    for (label, noise) in [("clean", NoiseModel::NONE), ("noisy", NoiseModel::realistic())] {
        let mut worst = 0.0f64;
        let mut sum = 0.0;
        let instants = 24;
        for k in 0..instants {
            let snap = stage.snapshot(9.6 * k as f32 / instants as f32);
            let opts = CaptureOptions {
                noise,
                ..Default::default()
            };
            let ms = time_ms(9, || {
                for cam in &cams {
                    black_box(render_rgbd(&snap, cam, &opts));
                }
            });
            worst = worst.max(ms);
            sum += ms;
        }
        println!(
            "stage {} x {}x{} with shadows, {label}: mean {:.2} ms, worst {:.2} ms",
            cams.len(),
            cams[0].width,
            cams[0].height,
            sum / instants as f64,
            worst
        );
    }

    // House: build once, then one handheld frame along the tour.
    let build = time_ms(20, || {
        black_box(make_scene(SceneKind::House));
    });
    let house = make_scene(SceneKind::House);
    let snap = house.snapshot(0.0);
    println!("house build ({} primitives): {:.3} ms", snap.len(), build);
    let path = ScanPath::house_default();
    for shadows in [true, false] {
        let opts = CaptureOptions {
            shadows,
            ..Default::default()
        };
        let frames = 40;
        let (mut worst, mut sum) = (0.0f64, 0.0);
        for k in 0..frames {
            let cam = path.camera_at(path.duration() * k as f32 / frames as f32, 70.0, 160, 120);
            let ms = time_ms(9, || {
                black_box(render_rgbd(&snap, &cam, &opts));
            });
            worst = worst.max(ms);
            sum += ms;
        }
        println!(
            "house 160x120 frame, shadows {shadows}: mean {:.2} ms, worst {:.2} ms",
            sum / frames as f64,
            worst
        );
    }
}
