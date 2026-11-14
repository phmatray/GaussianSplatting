//! Novel-view quality of the whole pipeline.
//!
//! The pipeline's own messages are decoded like a viewer would, the
//! resulting cloud is rendered with the reference rasterizer from cameras
//! that are not part of the rig (or not on the scan path) and compared with
//! a ray cast of the true scene at the same instant.
//!
//! The thresholds are regression bounds a little below what the default
//! settings achieve (measured values in the comments; the pipeline is
//! deterministic, so they do not fluctuate). Set `GS_RECON_OUT=<dir>` to get
//! `reference | reconstruction | 4x difference` sheets.

mod common;

use std::sync::OnceLock;

use common::{
    clearance, evaluate, house_look_around_views, house_views, mean, observed_mask, output_dir, run_full_scan, save_png,
    scan_path_distance, side_by_side, stack, stage_view_above, stage_views, Client, Exposures, Score, BACKGROUND,
    HALL_VIEW_CLEARANCE, HOUSE_VIEW_CLEARANCE, HOUSE_VIEW_PATH_DISTANCE,
};
use gs_core::capture::{render_reference, NoiseModel};
use gs_core::glam::{Mat4, Quat, Vec3};
use gs_core::pipeline::{scan_frame, scan_frame_count, scan_sensor, Pipeline};
use gs_core::protocol::{CameraInfo, ControlMsg, PipelineConfig, ScanHead, ScanState};
use gs_core::raster::{psnr, rasterize, Image, RasterOptions};
use gs_core::rig::ScanPath;
use gs_core::scene::{house, make_scene, SceneKind, SceneSnapshot};
use gs_core::{Camera, Splat};

fn min(values: &[f32]) -> f32 {
    values.iter().copied().fold(f32::INFINITY, f32::min)
}

fn save_sheet(name: &str, scores: &[Score]) {
    if let Some(dir) = output_dir() {
        let sheets: Vec<Image> = scores.iter().map(side_by_side).collect();
        save_png(&dir.join(name), &stack(&sheets));
    }
}

/// Runs the stage through `dts` and scores every held-out view after each
/// tick. Returns the PSNR values and the client.
fn stage_scores(config: PipelineConfig, dts: &[f32], sheet: &str) -> (Vec<f32>, Client) {
    let shadows = config.shadows;
    let mut pipeline = Pipeline::new(config);
    let scene = make_scene(SceneKind::Stage);
    let mut client = Client::default();
    client.apply(&pipeline.snapshot());
    let views = stage_views();
    let (mut psnrs, mut scores) = (Vec::new(), Vec::new());
    for &dt in dts {
        client.apply(&pipeline.tick(dt));
        let snap = scene.snapshot(pipeline.scene_time());
        let cloud = client.cloud();
        for view in &views {
            let score = evaluate(&snap, &cloud, &view.cam, shadows, None);
            psnrs.push(score.psnr);
            scores.push(score);
        }
    }
    save_sheet(sheet, &scores);
    (psnrs, client)
}

#[test]
fn held_out_views_are_really_held_out() {
    // Stage: no viewpoint within 20 degrees of azimuth of a rig camera at a
    // similar height, and none at a rig camera's distance.
    let rig = PipelineConfig::default().rig.cameras();
    for view in stage_views() {
        let eye = view.cam.position();
        for cam in &rig {
            let p = cam.position();
            assert!((eye - p).length() > 0.9, "{} sits {} m from a rig camera", view.name, (eye - p).length());
        }
    }
    assert_eq!(stage_views().len(), 6);

    // House: every viewpoint in free space, inside the house, off the path.
    let snap = make_scene(SceneKind::House).snapshot(0.0);
    let views = house_views();
    assert_eq!(views.len(), 24);
    for view in &views {
        let eye = view.cam.position();
        assert!(common::inside_house(eye), "{} is outside", view.name);
        let hall = view.name.starts_with("hall");
        let free = clearance(&snap, eye);
        assert!(free >= if hall { HALL_VIEW_CLEARANCE } else { HOUSE_VIEW_CLEARANCE }, "{}: {free} m of clearance", view.name);
        let off_path = scan_path_distance(eye);
        let height_offset = (eye.y - house::EYE_HEIGHT).abs();
        if hall {
            assert!(off_path > 0.2 && height_offset > 0.2, "{}: {off_path} m, {height_offset} m", view.name);
        } else {
            assert!(off_path >= HOUSE_VIEW_PATH_DISTANCE && height_offset > 0.2, "{}: {off_path} m off the path", view.name);
        }
    }
}

#[test]
fn stage_default_settings() {
    // Measured: mean 25.14 dB, worst view 22.86 dB (the one from above, which
    // sees floor that no camera sees).
    let (psnrs, client) = stage_scores(PipelineConfig::for_scene(SceneKind::Stage), &[1.3, 2.8, 3.6], "stage_default.png");
    let (mean_psnr, worst) = (mean(&psnrs), min(&psnrs));
    let (fixed, live) = (client.layers[0].len(), client.layers[1].len());
    println!("stage default: mean {mean_psnr:.2} dB, worst {worst:.2} dB, {fixed} + {live} splats");
    assert!(mean_psnr >= 24.6, "mean PSNR {mean_psnr:.2} dB");
    assert!(worst >= 22.3, "worst PSNR {worst:.2} dB");

    // Static plate about 69k splats, live layer about 9k: 16 bytes each.
    assert!((50_000..90_000).contains(&client.layers[0].len()), "{}", client.layers[0].len());
    assert!((5_000..14_000).contains(&client.layers[1].len()), "{}", client.layers[1].len());
    let stats = client.stats();
    assert!(stats.bytes < 300_000, "{} bytes per tick", stats.bytes);
    assert_eq!(stats.pixels, 8 * 160 * 120);
}

#[test]
fn stage_with_sensor_noise() {
    // Measured: mean 24.56 dB, worst 22.63 dB.
    let config = PipelineConfig { noise: NoiseModel::realistic(), ..PipelineConfig::for_scene(SceneKind::Stage) };
    let (psnrs, client) = stage_scores(config, &[1.3, 3.2], "stage_noise.png");
    let (mean_psnr, worst) = (mean(&psnrs), min(&psnrs));
    println!("stage with noise: mean {mean_psnr:.2} dB, worst {worst:.2} dB");
    assert!(mean_psnr >= 24.0, "mean PSNR {mean_psnr:.2} dB");
    assert!(worst >= 22.0, "worst PSNR {worst:.2} dB");
    // Noise must not leak background into the live layer.
    assert!(client.layers[1].len() < 14_000, "{} live splats", client.layers[1].len());
}

#[test]
fn stage_light_preset_and_without_background_subtraction() {
    // Measured: light preset mean 25.02 dB (worst 22.26); everything live
    // mean 25.47 dB (worst 23.03).
    let (psnrs, _) = stage_scores(PipelineConfig::light(SceneKind::Stage), &[2.1], "stage_light.png");
    println!("stage light: mean {:.2} dB, worst {:.2} dB", mean(&psnrs), min(&psnrs));
    assert!(mean(&psnrs) >= 24.3 && min(&psnrs) >= 21.7, "{psnrs:?}");

    let config = PipelineConfig { background_subtraction: false, ..PipelineConfig::for_scene(SceneKind::Stage) };
    let (psnrs, client) = stage_scores(config, &[2.1], "stage_all_live.png");
    println!("stage all live: mean {:.2} dB, worst {:.2} dB", mean(&psnrs), min(&psnrs));
    assert!(mean(&psnrs) >= 24.6 && min(&psnrs) >= 22.3, "{psnrs:?}");
    assert!(client.layers[0].is_empty());
}

/// Luminance-like mean of a pixel.
fn level(image: &Image, i: usize) -> f32 {
    (image.rgb[i * 3] as f32 + image.rgb[i * 3 + 1] as f32 + image.rgb[i * 3 + 2] as f32) / 3.0
}

fn render(splats: &[Splat], view: &common::View) -> Image {
    rasterize(splats, &view.cam, &RasterOptions { background: BACKGROUND, ..Default::default() })
}

/// Pixels of the view from above that show floor in a shadow cast by the
/// dancer or the balls, and a band of lit floor around them.
fn shadow_masks(scene_now: &SceneSnapshot, empty: &SceneSnapshot, view: &common::View) -> (Vec<bool>, Vec<bool>) {
    let cam = &view.cam;
    let lit = render_reference(scene_now, cam, false, 2, BACKGROUND);
    let shaded = render_reference(scene_now, cam, true, 2, BACKGROUND);
    let floor = render_reference(empty, cam, true, 1, BACKGROUND);
    let (w, h) = (cam.width as usize, cam.height as usize);
    let (lit, shaded) = (Image::from_frame(&lit), Image::from_frame(&shaded));
    let shaded_frame = render_reference(scene_now, cam, true, 1, BACKGROUND);
    // Static geometry: the same depth with and without the dancer.
    let is_floor = |i: usize| floor.depth[i] > 0.0 && (floor.depth[i] - shaded_frame.depth[i]).abs() < 1e-3;
    let shadow: Vec<bool> = (0..w * h).map(|i| is_floor(i) && level(&lit, i) - level(&shaded, i) > 25.0).collect();
    let near = |i: usize, radius: i64, mask: &[bool]| {
        let (x, y) = ((i % w) as i64, (i / w) as i64);
        (-radius..=radius).any(|dy| {
            (-radius..=radius).any(|dx| {
                let (xx, yy) = (x + dx, y + dy);
                xx >= 0 && yy >= 0 && xx < w as i64 && yy < h as i64 && mask[yy as usize * w + xx as usize]
            })
        })
    };
    // Lit floor 3 to 8 pixels away from any shadow, and from the dancer.
    let not_floor: Vec<bool> = (0..w * h).map(|i| !is_floor(i)).collect();
    let band: Vec<bool> = (0..w * h)
        .map(|i| is_floor(i) && !near(i, 2, &shadow) && near(i, 8, &shadow) && !near(i, 3, &not_floor))
        .collect();
    (shadow, band)
}

#[test]
fn the_dancers_shadow_is_reconstructed_without_a_halo() {
    let scene = make_scene(SceneKind::Stage);
    let empty = scene.snapshot_static();
    let view = stage_view_above();
    for (label, noise) in [("clean", NoiseModel::NONE), ("noisy", NoiseModel::realistic())] {
        let mut pipeline = Pipeline::new(PipelineConfig { noise, ..PipelineConfig::for_scene(SceneKind::Stage) });
        let mut client = Client::default();
        client.apply(&pipeline.snapshot());
        client.apply(&pipeline.tick(4.1));
        let snap = scene.snapshot(pipeline.scene_time());
        let (shadow, band) = shadow_masks(&snap, &empty, &view);
        let shadow_pixels = shadow.iter().filter(|&&m| m).count();
        assert!(shadow_pixels > 400, "the view shows only {shadow_pixels} shadow pixels");

        let with_shadow = Image::from_frame(&render_reference(&snap, &view.cam, true, 2, BACKGROUND));
        let without = Image::from_frame(&render_reference(&snap, &view.cam, false, 2, BACKGROUND));
        let reconstruction = render(&client.cloud(), &view);
        let static_only = render(&client.layers[0], &view);
        let error = |image: &Image, reference: &Image, mask: &[bool]| {
            let picked: Vec<f32> =
                (0..mask.len()).filter(|&i| mask[i]).map(|i| (level(image, i) - level(reference, i)).abs()).collect();
            mean(&picked)
        };
        // Measured (clean / noisy): in the shadow the reconstruction is
        // 12.3 / 12.5 levels from the shadowed truth and 55 from the lit
        // one; the plate alone is 41 levels off. In the band around the
        // shadow: 8.4 / 8.5 levels, against 8.1 / 8.3 for the plate alone.
        let (to_shadow, to_lit) = (error(&reconstruction, &with_shadow, &shadow), error(&reconstruction, &without, &shadow));
        let plate_to_shadow = error(&static_only, &with_shadow, &shadow);
        // Around the shadow the floor is as good as without live layer.
        let (halo, plate_halo) = (error(&reconstruction, &with_shadow, &band), error(&static_only, &with_shadow, &band));
        println!(
            "shadow ({label}): {shadow_pixels} px, error to shadowed truth {to_shadow:.1}, to lit truth {to_lit:.1}, \
             plate alone {plate_to_shadow:.1}; band around it {halo:.1} (plate alone {plate_halo:.1})"
        );
        assert!(to_shadow < 16.0, "{label}: the shadow is off by {to_shadow:.1} levels");
        assert!(to_lit > 3.0 * to_shadow, "{label}: the shadow is not visible ({to_lit:.1} vs {to_shadow:.1})");
        assert!(plate_to_shadow > 3.0 * to_shadow, "{label}: the live layer does not improve on the plate");
        assert!(halo < plate_halo + 2.0, "{label}: halo around the shadow, {halo:.1} vs {plate_halo:.1} levels");
        if let Some(dir) = output_dir() {
            save_png(&dir.join(format!("shadow_{label}.png")), &stack(&[with_shadow, reconstruction, static_only]));
        }
    }
}

#[test]
fn a_frozen_scene_does_not_flicker_under_sensor_noise() {
    // Time stands still, the sensor noise does not: the picture from above
    // (floor, shadow, dancer) must stay put from tick to tick.
    let config = PipelineConfig { noise: NoiseModel::realistic(), ..PipelineConfig::for_scene(SceneKind::Stage) };
    let mut pipeline = Pipeline::new(config);
    let mut client = Client::default();
    client.apply(&pipeline.snapshot());
    client.apply(&pipeline.tick(4.1));
    client.apply(&pipeline.handle_control(ControlMsg::SetTimeScale { time_scale: 0.0 }));
    let scene = make_scene(SceneKind::Stage);
    let snap = scene.snapshot(pipeline.scene_time());
    let empty = scene.snapshot_static();
    let view = stage_view_above();
    let (shadow, _) = shadow_masks(&snap, &empty, &view);

    let mut frames = Vec::new();
    for _ in 0..4 {
        client.apply(&pipeline.tick(1.0 / 30.0));
        frames.push(render(&client.cloud(), &view));
    }
    let mut worst_psnr = f32::INFINITY;
    let mut worst_shadow = 0.0f32;
    for pair in frames.windows(2) {
        worst_psnr = worst_psnr.min(psnr(&pair[0], &pair[1]));
        let changes: Vec<f32> =
            (0..shadow.len()).filter(|&i| shadow[i]).map(|i| (level(&pair[0], i) - level(&pair[1], i)).abs()).collect();
        worst_shadow = worst_shadow.max(mean(&changes));
    }
    // Measured: 37.8 dB between consecutive ticks, 1.7 levels in the shadow.
    println!("flicker: {worst_psnr:.1} dB between consecutive ticks, {worst_shadow:.2} levels in the shadow");
    assert!(worst_psnr > 35.0, "consecutive ticks differ: {worst_psnr:.1} dB");
    assert!(worst_shadow < 5.0, "the shadow flickers by {worst_shadow:.2} levels");
}

/// Angle of the rotation that takes one orientation to the other, in
/// degrees (accurate for small angles too).
fn rotation_between(a: Quat, b: Quat) -> f32 {
    let delta = b * a.conjugate();
    (2.0 * delta.xyz().length().atan2(delta.w.abs())).to_degrees()
}

fn orientation(cam: &Camera) -> Quat {
    Quat::from_mat3a(&cam.world_from_cam.matrix3)
}

/// Position and orientation of a camera as the Stats message reports it.
fn pose_of(info: &CameraInfo) -> (Vec3, Quat) {
    let (_, rotation, position) = Mat4::from_cols_array(&info.world_from_cam).to_scale_rotation_translation();
    (position, rotation)
}

#[test]
fn the_scan_head_is_carried_calmly() {
    let config = PipelineConfig::for_scene(SceneKind::House);
    let scan = &config.scan;
    let path = ScanPath::house_default().with_speed(scan.speed);
    let sensors = scan.head.sensor_count();
    assert_eq!((scan.head, sensors), (ScanHead::Surround, 5));
    // Measured: 67.4 s of walking at the default speed.
    assert!((45.0..=75.0).contains(&path.duration()), "the scan takes {} s", path.duration());

    // The motion itself, sampled at the tick rate (this is what the camera
    // gizmo of a viewer shows): the angular speed of every sensor, and how
    // much it changes from one tick to the next.
    let dt = 1.0 / 30.0;
    let steps = (path.duration() / dt).ceil() as usize;
    let (mut peak, mut sum, mut peak_change, mut fastest) = (0.0f32, 0.0f64, 0.0f32, 0.0f32);
    for sensor in 0..sensors {
        let mut prev = scan_sensor(&path, 0.0, scan, sensor);
        let mut prev_rate: Option<f32> = None;
        for i in 1..=steps {
            let cam = scan_sensor(&path, i as f32 * dt, scan, sensor);
            let rate = rotation_between(orientation(&prev), orientation(&cam)) / dt;
            peak = peak.max(rate);
            sum += rate as f64;
            if let Some(before) = prev_rate {
                peak_change = peak_change.max((rate - before).abs());
            }
            fastest = fastest.max((cam.position() - prev.position()).length() / dt);
            (prev, prev_rate) = (cam, Some(rate));
        }
    }
    let mean_rate = sum / (steps as f64 * sensors as f64);
    println!(
        "scan head: {:.1} s, turns at {mean_rate:.1} deg/s on average, {peak:.1} deg/s at most, \
         changing by at most {peak_change:.2} deg/s per tick; walks at up to {fastest:.2} m/s",
        path.duration()
    );
    // Measured: mean 20.0 deg/s, peak 36.7 deg/s (the look-around sweep of
    // one camera that this head replaces averaged 133 deg/s and peaked at
    // 215). From one tick to the next the angular speed changes by at most
    // 1.29 deg/s, which is 39 deg/s in a second. Before the head averaged
    // the heading of the path over a second it jumped by 7.6 deg/s in one
    // tick, a good second before the end of the walk, where that heading
    // freezes.
    assert!(peak <= 40.0, "a sensor turns at {peak} deg/s");
    assert!(mean_rate <= 22.0, "the sensors turn at {mean_rate} deg/s on average");
    assert!(peak_change <= 2.0, "the angular speed jumps by {peak_change} deg/s between two ticks");
    assert!(fastest <= scan.speed * 1.02, "the head moves at {fastest} m/s");

    // The frames each sensor takes (this is what its preview shows): the
    // rotation and the way between two consecutive frames of one sensor.
    let mut last: Vec<Option<Camera>> = vec![None; sensors as usize];
    let mut turn = vec![0.0f32; sensors as usize];
    let mut way = vec![0.0f32; sensors as usize];
    for i in 0..scan_frame_count(&path, scan) {
        let frame = scan_frame(&path, scan, i);
        let cam = scan_sensor(&path, frame.time, scan, frame.sensor);
        let k = frame.sensor as usize;
        if let Some(before) = &last[k] {
            turn[k] = turn[k].max(rotation_between(orientation(before), orientation(&cam)));
            way[k] = way[k].max((cam.position() - before.position()).length());
        }
        last[k] = Some(cam);
    }
    println!("scan head: at most {turn:.1?} degrees and {way:.2?} m between consecutive frames of each sensor");
    // Measured: 12.2 degrees and 0.27 m for the forward sensor, which takes
    // every other frame; 46.6 to 47.7 degrees and 1.07 m for the others, in
    // the 1.33 s between their frames, always turning the way the walk
    // turns. The sweep threw its one camera up to 29 degrees between any
    // two frames, back and forth.
    assert!(turn[0] <= 13.5 && way[0] <= 0.3, "forward sensor: {} degrees, {} m", turn[0], way[0]);
    assert!(turn[1..].iter().all(|&a| a <= 50.0) && way[1..].iter().all(|&d| d <= 1.2), "{turn:?} {way:?}");
}

#[test]
fn the_reported_scan_camera_moves_steadily_tick_after_tick() {
    // A cheap scan walked in real time at 30 Hz: the poses in the Stats
    // messages are what a viewer animates its camera gizmos with.
    let mut config = PipelineConfig::for_scene(SceneKind::House);
    config.scan.width = 64;
    config.scan.height = 48;
    config.scan.voxel_size = 0.1;
    let mut pipeline = Pipeline::new(config);
    let mut client = Client::default();
    client.apply(&pipeline.snapshot());
    let sensors = config.scan.head.sensor_count() as usize;
    let mut prev: Option<Vec<(Vec3, Quat)>> = None;
    let (mut peak_turn, mut peak_step, mut progress, mut ticks) = (0.0f32, 0.0f32, 0.0f32, 0);
    while client.scan_state() != Some(ScanState::Done) {
        client.apply(&pipeline.tick(1.0 / 30.0));
        let scan = client.stats().scan.expect("the house reports its scan");
        assert!(scan.progress >= progress, "progress stepped back from {progress} to {}", scan.progress);
        progress = scan.progress;
        assert_eq!(scan.camera.id, 0);
        let ids: Vec<u32> = scan.extra_cameras.iter().map(|c| c.id).collect();
        assert_eq!(ids, (1..sensors as u32).collect::<Vec<_>>());
        let poses: Vec<(Vec3, Quat)> = std::iter::once(&scan.camera).chain(scan.extra_cameras.iter()).map(pose_of).collect();
        if let Some(prev) = &prev {
            for (before, now) in prev.iter().zip(&poses) {
                peak_turn = peak_turn.max(rotation_between(before.1, now.1));
                peak_step = peak_step.max((now.0 - before.0).length());
            }
        }
        prev = Some(poses);
        ticks += 1;
        assert!(ticks < 10_000, "the scan never finishes");
    }
    println!("reported scan cameras: at most {peak_turn:.2} degrees and {:.1} mm per tick, {ticks} ticks", 1e3 * peak_step);
    // Measured: 1.22 degrees and 26.7 mm per tick (36.7 deg/s, 0.8 m/s).
    assert!(peak_turn <= 40.0 / 30.0, "a reported camera turns {peak_turn} degrees in one tick");
    assert!(peak_step <= 0.03, "a reported camera jumps {peak_step} m in one tick");
    // Every sensor delivered previews under its own id.
    assert_eq!(client.previews.keys().copied().collect::<Vec<u8>>(), (0..sensors as u8).collect::<Vec<_>>());
}

/// The finished default home scan, shared by the house tests.
struct Scan {
    config: PipelineConfig,
    cloud: Vec<Splat>,
    ticks: usize,
    appended_bytes: usize,
}

fn full_scan() -> &'static Scan {
    static SCAN: OnceLock<Scan> = OnceLock::new();
    SCAN.get_or_init(|| {
        let mut config = PipelineConfig::for_scene(SceneKind::House);
        config.scan.autostart = false;
        config.preview.enabled = false;
        let mut pipeline = Pipeline::new(config);
        let mut client = Client::default();
        let ticks = run_full_scan(&mut pipeline, &mut client);
        assert_eq!(pipeline.scan_state(), Some(ScanState::Done));
        assert!(client.layers[1].is_empty());
        let state = client.state.as_ref().unwrap();
        assert_eq!(state.scene, "house");
        assert_eq!(state.ceiling_height, Some(house::CEILING_HEIGHT));
        Scan { config: *pipeline.config(), cloud: client.cloud(), ticks, appended_bytes: client.bytes }
    })
}

#[test]
fn house_full_scan() {
    let scan = full_scan();
    let snap = make_scene(SceneKind::House).snapshot(0.0);
    let exposures = Exposures::of_scan(&snap, &scan.config);
    let (mut psnrs, mut coverages, mut psnrs_observed, mut coverages_observed, mut scores) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    for view in house_views() {
        let mask = observed_mask(&snap, &exposures, &view.cam);
        let score = evaluate(&snap, &scan.cloud, &view.cam, scan.config.shadows, Some(&mask));
        psnrs.push(score.psnr);
        coverages.push(score.coverage);
        psnrs_observed.push(score.psnr_observed);
        coverages_observed.push(score.coverage_observed);
        scores.push(score);
    }
    for (i, chunk) in scores.chunks(6).enumerate() {
        save_sheet(&format!("house_{i}.png"), chunk);
    }
    println!(
        "house: {} splats in {} ticks, {:.1} MB; PSNR mean {:.2} dB (worst {:.2}), coverage mean {:.2} % (worst {:.2}); \
         where the scan has looked: PSNR mean {:.2} dB (worst {:.2}), coverage mean {:.2} % (worst {:.2})",
        scan.cloud.len(),
        scan.ticks,
        scan.appended_bytes as f32 / 1e6,
        mean(&psnrs),
        min(&psnrs),
        100.0 * mean(&coverages),
        100.0 * min(&coverages),
        mean(&psnrs_observed),
        min(&psnrs_observed),
        100.0 * mean(&coverages_observed),
        100.0 * min(&coverages_observed),
    );
    // Measured: 423 672 splats; PSNR mean 21.79 dB (worst 18.95); coverage
    // mean 99.86 % (worst 97.95 %). On the pixels whose surface point some
    // scan frame has seen: PSNR mean 21.98 dB (worst 19.00), coverage mean
    // 99.98 % (worst 99.79 %). What keeps the PSNR below 22 dB is the cell
    // size: 3.2 cm cells are 2 to 3 pixels wide in these views, which blurs
    // every color edge of the patterned rugs, tiles and pictures. The 0.4 %
    // of the pixels that the scan never saw (floor behind furniture, the
    // niche beside the wardrobe) cost another 0.2 dB.
    //
    // The look-around sweep of a single camera that the five-sensor head
    // replaces scored 21.46 dB (worst 19.16) and 99.69 % (worst 97.68 %)
    // with 416 522 splats, while turning seven times as fast.
    assert!(scan.cloud.len() <= 440_000, "{} splats", scan.cloud.len());
    assert!(scan.cloud.len() >= 380_000, "{} splats", scan.cloud.len());
    assert!(mean(&psnrs) >= 21.4, "mean PSNR {:.2} dB", mean(&psnrs));
    // The worst view (the kitchen seen from the dining room, past a lamp
    // shade the scan sees edge-on) is touchy: turning the head by 2 degrees
    // here and there moves it by 0.4 dB while the mean stays put, so its
    // bound keeps that much distance.
    assert!(min(&psnrs) >= 18.6, "worst PSNR {:.2} dB", min(&psnrs));
    assert!(mean(&coverages) >= 0.996, "mean coverage {:.4}", mean(&coverages));
    assert!(min(&coverages) >= 0.973, "worst coverage {:.4}", min(&coverages));
    assert!(mean(&psnrs_observed) >= 21.6, "mean observed PSNR {:.2} dB", mean(&psnrs_observed));
    assert!(mean(&coverages_observed) >= 0.998, "mean observed coverage {:.4}", mean(&coverages_observed));
    assert!(min(&coverages_observed) >= 0.993, "worst observed coverage {:.4}", min(&coverages_observed));
}

#[test]
fn house_light_preset_scan() {
    // What the WebAssembly worker runs on a laptop or a headset: sensors of
    // 144x108 taking 5 frames per second into 4 cm cells, so each of the
    // four side, up and down sensors fires only every 1.6 s.
    let mut config = PipelineConfig::light(SceneKind::House);
    config.scan.autostart = false;
    config.preview.enabled = false;
    let mut pipeline = Pipeline::new(config);
    let mut client = Client::default();
    let ticks = run_full_scan(&mut pipeline, &mut client);
    let cloud = client.cloud();
    let snap = make_scene(SceneKind::House).snapshot(0.0);
    let (mut psnrs, mut coverages) = (Vec::new(), Vec::new());
    // Every other standard view (one direction per spot) and the views at
    // ceilings, corners and floors.
    for view in house_views().into_iter().step_by(2).chain(house_look_around_views()) {
        let score = evaluate(&snap, &cloud, &view.cam, config.shadows, None);
        psnrs.push(score.psnr);
        coverages.push(score.coverage);
    }
    println!(
        "house light: {} splats in {ticks} ticks; PSNR mean {:.2} dB (worst {:.2}), \
         coverage mean {:.2} % (worst {:.2})",
        cloud.len(),
        mean(&psnrs),
        min(&psnrs),
        100.0 * mean(&coverages),
        100.0 * min(&coverages),
    );
    // Measured: 280 304 splats; PSNR mean 22.71 dB (worst 19.60), coverage
    // mean 99.86 % (worst 97.90 %).
    assert!((250_000..=300_000).contains(&cloud.len()), "{} splats", cloud.len());
    assert!(mean(&psnrs) >= 22.3 && min(&psnrs) >= 19.1, "{psnrs:?}");
    assert!(mean(&coverages) >= 0.996 && min(&coverages) >= 0.973, "{coverages:?}");
}

#[test]
fn house_ceilings_corners_and_floors_are_scanned() {
    // Looking up from inside the rooms: no black slits beside the ceiling
    // beams, no missing patch of ceiling over the shower.
    let scan = full_scan();
    let snap = make_scene(SceneKind::House).snapshot(0.0);
    let (mut psnrs, mut scores) = (Vec::new(), Vec::new());
    for view in house_look_around_views() {
        assert!(clearance(&snap, view.cam.position()) > 0.3, "{} is not in free space", view.name);
        let score = evaluate(&snap, &scan.cloud, &view.cam, scan.config.shadows, None);
        println!("look around, {:16} {:.2} dB, coverage {:.2} %", view.name, score.psnr, 100.0 * score.coverage);
        assert!(score.coverage >= 0.995, "{}: coverage {:.4}", view.name, score.coverage);
        psnrs.push(score.psnr);
        scores.push(score);
    }
    save_sheet("house_look_around.png", &scores);
    println!("look around: mean {:.2} dB, worst {:.2} dB", mean(&psnrs), min(&psnrs));
    // Measured: every view fully covered; mean 25.73 dB, worst 20.94 dB (the
    // kitchen ceiling with its beams and lamp cords). A single sensor that
    // only looks ahead leaves holes over 5 to 8 % of three of these views,
    // which then score 14.6 to 16 dB.
    assert!(mean(&psnrs) >= 25.2 && min(&psnrs) >= 20.4, "{psnrs:?}");
}

#[test]
fn house_surfaces_are_single_layered_and_walls_do_not_bleed() {
    let scan = full_scan();
    let voxel = scan.config.scan.voxel_size;

    // Floor and ceiling: one layer of splats each, facing into the room.
    let facing = |s: &Splat| s.rotation() * Vec3::Z;
    let floor: Vec<&Splat> = scan.cloud.iter().filter(|s| s.pos[1].abs() < 0.02 && facing(s).y > 0.9).collect();
    let ceiling: Vec<&Splat> =
        scan.cloud.iter().filter(|s| (s.pos[1] - house::CEILING_HEIGHT).abs() < 0.02 && facing(s).y < -0.9).collect();
    let cell = voxel * voxel;
    let area = (2.0 * house::HALF_X) * (2.0 * house::HALF_Z);
    // Furniture and rugs hide part of the floor; nothing hides the ceiling
    // but its beams and lamps. More than one splat per cell would mean
    // doubled surfaces.
    let (floor_cells, ceiling_cells) = (floor.len() as f32 * cell / area, ceiling.len() as f32 * cell / area);
    println!("floor covers {:.2} of the plan, ceiling {:.2}", floor_cells, ceiling_cells);
    assert!((0.55..=1.02).contains(&floor_cells), "floor: {floor_cells}");
    assert!((0.75..=1.02).contains(&ceiling_cells), "ceiling: {ceiling_cells}");
    // Nothing hovers in the open: every splat lies within the house shell.
    for s in &scan.cloud {
        assert!(
            s.pos[0].abs() < house::HALF_X + 0.3 && s.pos[2].abs() < house::HALF_Z + 0.3 && (-0.3..=2.8).contains(&s.pos[1]),
            "a splat at {:?}",
            s.pos
        );
    }

    // The wall between the hall (white) and the bathroom (tiled, mint): its
    // two faces are 12 cm apart. Each face must show its own room's colors:
    // seen from inside a room, no splat of the other face is in front.
    let snap = make_scene(SceneKind::House).snapshot(0.0);
    for (name, eye, target) in [
        ("hall side", Vec3::new(-0.3, 1.5, 2.2), Vec3::new(0.4, 1.3, 2.4)),
        ("bathroom side", Vec3::new(1.6, 1.5, 2.2), Vec3::new(0.4, 1.3, 2.4)),
    ] {
        let cam = gs_core::Camera::look_at(eye, target, Vec3::Y, 50f32.to_radians(), 160, 120);
        let score = evaluate(&snap, &scan.cloud, &cam, scan.config.shadows, None);
        println!("wall, {name}: {:.2} dB, coverage {:.3}", score.psnr, score.coverage);
        assert!(score.coverage > 0.99, "{name}: holes in the wall ({:.3})", score.coverage);
        // Measured: 27.2 dB from the hall, 21.8 dB from the bathroom (tiles).
        assert!(score.psnr > 20.8, "{name}: {:.2} dB, the far face bleeds through", score.psnr);
        save_sheet(&format!("wall_{}.png", name.replace(' ', "_")), &[score]);
    }
}
