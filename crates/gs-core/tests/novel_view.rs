//! Novel-view quality of the whole pipeline.
//!
//! The pipeline's own messages are decoded like a viewer would, the
//! resulting cloud is drawn from cameras that are not part of the rig (or
//! not on the scan path) and compared with a ray cast of the true scene at
//! the same instant. Every cloud is judged under both blending modes of a
//! viewer ([`Renderer`]): the normalised one it is produced for, and the
//! sorted fallback with its splat scale. The views are 640x480: at 320x240
//! they are coarser than the sensors and tell a fine reconstruction from a
//! coarse one no longer.
//!
//! The thresholds are regression bounds a little below what the default
//! settings achieve (measured values in the comments; the pipeline is
//! deterministic, so they do not fluctuate). Set `GS_RECON_OUT=<dir>` to get
//! `reference | reconstruction | 4x difference` sheets.

mod common;

use std::sync::OnceLock;

use common::{
    clearance, evaluate, house_look_around_views, house_views, mean, observed_mask, output_dir, run_full_scan, save_png,
    scan_path_distance, side_by_side, stack, stage_view_above, stage_views, Client, Exposures, Renderer, Score, BACKGROUND,
    HALL_VIEW_CLEARANCE, HOUSE_VIEW_CLEARANCE, HOUSE_VIEW_PATH_DISTANCE,
};
use gs_core::capture::{render_reference, render_rgbd, CaptureOptions, NoiseModel};
use gs_core::experimental::metrics::dilate;
use gs_core::experimental::wblend::{self, Blend, Coverage, Layer, Normalized, Silhouette, WblendOptions};
use gs_core::glam::{Mat4, Quat, Vec3};
use gs_core::pipeline::{scan_frame, scan_frame_count, scan_sensor, Pipeline};
use gs_core::protocol::{CameraInfo, ControlMsg, PipelineConfig, ScanHead, ScanState};
use gs_core::raster::{psnr, Image};
use gs_core::rig::ScanPath;
use gs_core::scene::stage::StageScene;
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

/// The scores of a set of views under one renderer.
#[derive(Default)]
struct Tally {
    psnr: Vec<f32>,
    ssim: Vec<f32>,
    interior: Vec<f32>,
    texture: Vec<f32>,
    silhouette: Vec<f32>,
    coverage: Vec<f32>,
    psnr_observed: Vec<f32>,
    coverage_observed: Vec<f32>,
}

impl Tally {
    fn push(&mut self, score: &Score) {
        self.psnr.push(score.psnr);
        self.ssim.push(score.ssim);
        self.interior.push(score.interior);
        self.texture.push(score.texture);
        self.silhouette.push(score.silhouette);
        self.coverage.push(score.coverage);
        self.psnr_observed.push(score.psnr_observed);
        self.coverage_observed.push(score.coverage_observed);
    }

    /// Mean and worst PSNR, mean SSIM.
    fn headline(&self) -> (f32, f32, f32) {
        (mean(&self.psnr), min(&self.psnr), mean(&self.ssim))
    }

    fn report(&self, label: &str) {
        let finite = |values: &[f32]| mean(&values.iter().copied().filter(|v| v.is_finite()).collect::<Vec<_>>());
        println!(
            "{label}: PSNR mean {:.2} dB (worst {:.2}), SSIM {:.3}; interior {:.1}, texture {:.1}, silhouette {:.1} dB; \
             coverage mean {:.2} % (worst {:.2})",
            mean(&self.psnr),
            min(&self.psnr),
            mean(&self.ssim),
            finite(&self.interior),
            finite(&self.texture),
            finite(&self.silhouette),
            100.0 * mean(&self.coverage),
            100.0 * min(&self.coverage),
        );
    }
}

/// Runs the stage through `dts` and scores every held-out view after each
/// tick under both renderers ([`Renderer::BOTH`] order). Returns the scores
/// and the client.
fn stage_scores(config: PipelineConfig, dts: &[f32], label: &str) -> ([Tally; 2], Client) {
    let shadows = config.shadows;
    let mut pipeline = Pipeline::new(config);
    let scene = make_scene(SceneKind::Stage);
    let mut client = Client::default();
    client.apply(&pipeline.snapshot());
    let views = stage_views();
    let mut tallies = [Tally::default(), Tally::default()];
    for &dt in dts {
        client.apply(&pipeline.tick(dt));
        let snap = scene.snapshot(pipeline.scene_time());
        for (renderer, tally) in Renderer::BOTH.into_iter().zip(&mut tallies) {
            let scores: Vec<Score> =
                views.iter().map(|view| evaluate(&snap, &client.layers, &view.cam, shadows, None, renderer)).collect();
            scores.iter().for_each(|score| tally.push(score));
            save_sheet(&format!("{}_{}_{:.1}.png", label.replace(' ', "_"), renderer.name(), pipeline.scene_time()), &scores);
        }
    }
    for (renderer, tally) in Renderer::BOTH.into_iter().zip(&tallies) {
        tally.report(&format!("{label}, {}", renderer.name()));
    }
    (tallies, client)
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
    let (tallies, client) = stage_scores(PipelineConfig::for_scene(SceneKind::Stage), &[1.3, 2.8, 3.6], "stage default");
    let [normalized, sorted] = &tallies;
    let (fixed, live) = (client.layers[0].len(), client.layers[1].len());
    println!("stage default: {fixed} + {live} splats");
    // Measured, normalised: mean 28.54 dB, worst view 24.42 dB (the one
    // from above, which sees floor that no camera sees), SSIM 0.940; around
    // color edges 25.3 dB, around silhouettes 20.9 dB.
    let (mean_psnr, worst, ssim) = normalized.headline();
    assert!(mean_psnr >= 28.2 && worst >= 24.0 && ssim >= 0.93, "normalised: {mean_psnr:.2} dB, worst {worst:.2}, SSIM {ssim:.3}");
    assert!(mean(&normalized.texture) >= 24.9, "texture {:.2} dB", mean(&normalized.texture));
    assert!(mean(&normalized.silhouette) >= 20.5, "silhouettes {:.2} dB", mean(&normalized.silhouette));
    // Measured, sorted fallback: mean 26.40 dB, worst 23.68 dB, SSIM 0.902.
    let (mean_psnr, worst, ssim) = sorted.headline();
    assert!(mean_psnr >= 26.2 && worst >= 23.3 && ssim >= 0.894, "sorted: {mean_psnr:.2} dB, worst {worst:.2}, SSIM {ssim:.3}");

    // Static plate about 85k splats, live layer about 14k: 16 bytes each.
    assert!((70_000..95_000).contains(&fixed), "{fixed}");
    assert!((8_000..17_000).contains(&live), "{live}");
    let stats = client.stats();
    assert!(stats.bytes < 300_000, "{} bytes per tick", stats.bytes);
    assert_eq!(stats.pixels, 8 * 240 * 180);
}

#[test]
fn stage_with_sensor_noise() {
    let config = PipelineConfig { noise: NoiseModel::realistic(), ..PipelineConfig::for_scene(SceneKind::Stage) };
    let (tallies, client) = stage_scores(config, &[1.3, 3.2], "stage with noise");
    let [normalized, sorted] = &tallies;
    // Measured, normalised: mean 27.05 dB, worst 23.93 dB, SSIM 0.928.
    let (mean_psnr, worst, ssim) = normalized.headline();
    assert!(mean_psnr >= 26.8 && worst >= 23.5 && ssim >= 0.919, "normalised: {mean_psnr:.2} dB, worst {worst:.2}, SSIM {ssim:.3}");
    // Measured, sorted fallback: mean 25.55 dB, worst 23.40 dB, SSIM 0.892.
    let (mean_psnr, worst, ssim) = sorted.headline();
    assert!(mean_psnr >= 25.4 && worst >= 23.0 && ssim >= 0.884, "sorted: {mean_psnr:.2} dB, worst {worst:.2}, SSIM {ssim:.3}");
    // Noise must not leak background into the live layer.
    assert!(client.layers[1].len() < 17_000, "{} live splats", client.layers[1].len());
}

#[test]
fn stage_light_preset_and_without_background_subtraction() {
    // Measured, light preset (eight 160x120 cameras): normalised mean
    // 27.37 dB (worst 24.06), SSIM 0.921; sorted 25.63 dB (worst 23.56).
    let (tallies, client) = stage_scores(PipelineConfig::light(SceneKind::Stage), &[2.1], "stage light");
    let (mean_psnr, worst, ssim) = tallies[0].headline();
    assert!(mean_psnr >= 27.0 && worst >= 23.6 && ssim >= 0.911, "light, normalised: {mean_psnr:.2} dB, worst {worst:.2}, SSIM {ssim:.3}");
    let (mean_psnr, worst, _) = tallies[1].headline();
    assert!(mean_psnr >= 25.5 && worst >= 23.2, "light, sorted: {mean_psnr:.2} dB, worst {worst:.2}");
    assert_eq!(client.stats().pixels, 8 * 160 * 120);

    // Measured, everything live: normalised mean 29.17 dB (worst 24.58),
    // SSIM 0.948; sorted 26.89 dB (worst 23.89).
    let config = PipelineConfig { background_subtraction: false, ..PipelineConfig::for_scene(SceneKind::Stage) };
    let (tallies, client) = stage_scores(config, &[2.1], "stage all live");
    let (mean_psnr, worst, ssim) = tallies[0].headline();
    assert!(mean_psnr >= 28.8 && worst >= 24.1 && ssim >= 0.94, "all live, normalised: {mean_psnr:.2} dB, worst {worst:.2}, SSIM {ssim:.3}");
    let (mean_psnr, worst, _) = tallies[1].headline();
    assert!(mean_psnr >= 26.6 && worst >= 23.5, "all live, sorted: {mean_psnr:.2} dB, worst {worst:.2}");
    assert!(client.layers[0].is_empty());
}

/// Luminance-like mean of a pixel.
fn level(image: &Image, i: usize) -> f32 {
    (image.rgb[i * 3] as f32 + image.rgb[i * 3 + 1] as f32 + image.rgb[i * 3 + 2] as f32) / 3.0
}

/// The luma change between two pictures that the change of the ground
/// truth between the same instants does not explain, as an RMS over the
/// image in 8-bit levels: what remains is flicker.
fn flicker(pictures: &[Image], truth: Option<&[Image]>) -> f32 {
    let luma = |image: &Image, i: usize| {
        0.299 * image.rgb[i * 3] as f32 + 0.587 * image.rgb[i * 3 + 1] as f32 + 0.114 * image.rgb[i * 3 + 2] as f32
    };
    let (mut sum, mut count) = (0.0f64, 0u64);
    for k in 1..pictures.len() {
        for i in 0..(pictures[k].width * pictures[k].height) as usize {
            let expected = truth.map_or(0.0, |truth| luma(&truth[k], i) - luma(&truth[k - 1], i));
            let excess = luma(&pictures[k], i) - luma(&pictures[k - 1], i) - expected;
            sum += (excess * excess) as f64;
            count += 1;
        }
    }
    (sum / count.max(1) as f64).sqrt() as f32
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
    // Lit floor 3 to 8 pixels of a 320-pixel-wide view away from any
    // shadow, and from the dancer: the same band on the floor whatever the
    // resolution of the view.
    let unit = (w / 320).max(1);
    let not_floor: Vec<bool> = (0..w * h).map(|i| !is_floor(i)).collect();
    let (beside, around, occluded) =
        (dilate(&shadow, w, h, 2 * unit), dilate(&shadow, w, h, 8 * unit), dilate(&not_floor, w, h, 3 * unit));
    let band: Vec<bool> = (0..w * h).map(|i| is_floor(i) && !beside[i] && around[i] && !occluded[i]).collect();
    (shadow, band)
}

#[test]
fn the_dancers_shadow_is_reconstructed_without_a_halo() {
    let scene = make_scene(SceneKind::Stage);
    let empty = scene.snapshot_static();
    let view = stage_view_above();
    let cases = [("clean", NoiseModel::NONE), ("noisy", NoiseModel::realistic())];
    for ((label, noise), renderer) in cases.into_iter().flat_map(|case| Renderer::BOTH.map(|renderer| (case, renderer))) {
        let label = format!("{label}, {}", renderer.name());
        let mut pipeline = Pipeline::new(PipelineConfig { noise, ..PipelineConfig::for_scene(SceneKind::Stage) });
        let mut client = Client::default();
        client.apply(&pipeline.snapshot());
        client.apply(&pipeline.tick(4.1));
        let snap = scene.snapshot(pipeline.scene_time());
        let (shadow, band) = shadow_masks(&snap, &empty, &view);
        let shadow_pixels = shadow.iter().filter(|&&m| m).count();
        assert!(shadow_pixels > 1600, "the view shows only {shadow_pixels} shadow pixels");

        let with_shadow = Image::from_frame(&render_reference(&snap, &view.cam, true, 2, BACKGROUND));
        let without = Image::from_frame(&render_reference(&snap, &view.cam, false, 2, BACKGROUND));
        let (reconstruction, _) = renderer.draw(&client.layers, &view.cam);
        let (static_only, _) = renderer.draw(&[client.layers[0].clone(), Vec::new()], &view.cam);
        let error = |image: &Image, reference: &Image, mask: &[bool]| {
            let picked: Vec<f32> =
                (0..mask.len()).filter(|&i| mask[i]).map(|i| (level(image, i) - level(reference, i)).abs()).collect();
            mean(&picked)
        };
        // Measured (normalised, clean / noisy): in the shadow the
        // reconstruction is 5.9 / 6.3 levels from the shadowed truth and 48
        // from the lit one; the plate alone is 38 levels off. In the band
        // around the shadow: 5.1 / 5.5 levels, against 5.0 / 5.4 for the
        // plate alone. Sorted: 7.5 / 7.8 and 53 levels in the shadow, 10.6 /
        // 10.5 in the band against 10.4 / 10.3.
        let (to_shadow, to_lit) = (error(&reconstruction, &with_shadow, &shadow), error(&reconstruction, &without, &shadow));
        let plate_to_shadow = error(&static_only, &with_shadow, &shadow);
        // Around the shadow the floor is as good as without live layer.
        let (halo, plate_halo) = (error(&reconstruction, &with_shadow, &band), error(&static_only, &with_shadow, &band));
        println!(
            "shadow ({label}): {shadow_pixels} px, error to shadowed truth {to_shadow:.1}, to lit truth {to_lit:.1}, \
             plate alone {plate_to_shadow:.1}; band around it {halo:.1} (plate alone {plate_halo:.1})"
        );
        assert!(to_shadow < 9.0, "{label}: the shadow is off by {to_shadow:.1} levels");
        assert!(to_lit > 3.0 * to_shadow, "{label}: the shadow is not visible ({to_lit:.1} vs {to_shadow:.1})");
        assert!(plate_to_shadow > 3.0 * to_shadow, "{label}: the live layer does not improve on the plate");
        assert!(halo < plate_halo + 1.0, "{label}: halo around the shadow, {halo:.1} vs {plate_halo:.1} levels");
        if let Some(dir) = output_dir() {
            let name = format!("shadow_{}.png", label.replace(", ", "_"));
            save_png(&dir.join(name), &stack(&[with_shadow, reconstruction, static_only]));
        }
    }
}

/// Consecutive ticks of the stage, `1/30` s apart from scene time 4.1 s on,
/// drawn from `view`; with them the ground truth of the same instants.
fn stage_sequence(config: PipelineConfig, time_scale: f32, ticks: usize, view: &common::View, renderer: Renderer) -> (Vec<Image>, Vec<Image>) {
    let mut pipeline = Pipeline::new(config);
    let mut client = Client::default();
    client.apply(&pipeline.snapshot());
    client.apply(&pipeline.tick(4.1));
    client.apply(&pipeline.handle_control(ControlMsg::SetTimeScale { time_scale }));
    let scene = make_scene(SceneKind::Stage);
    let (mut pictures, mut truth) = (Vec::new(), Vec::new());
    for _ in 0..ticks {
        client.apply(&pipeline.tick(1.0 / 30.0));
        pictures.push(renderer.draw(&client.layers, &view.cam).0);
        let snap = scene.snapshot(pipeline.scene_time());
        truth.push(Image::from_frame(&render_reference(&snap, &view.cam, config.shadows, 1, BACKGROUND)));
    }
    (pictures, truth)
}

#[test]
fn a_frozen_scene_does_not_flicker_under_sensor_noise() {
    // Time stands still, the sensor noise does not: the picture from above
    // (floor, shadow, dancer) must stay put from tick to tick.
    let config = PipelineConfig { noise: NoiseModel::realistic(), ..PipelineConfig::for_scene(SceneKind::Stage) };
    let scene = make_scene(SceneKind::Stage);
    let view = stage_view_above();
    let (shadow, _) = shadow_masks(&scene.snapshot(4.1), &scene.snapshot_static(), &view);
    for renderer in Renderer::BOTH {
        let (frames, _) = stage_sequence(config, 0.0, 4, &view, renderer);
        let mut worst_psnr = f32::INFINITY;
        let mut worst_shadow = 0.0f32;
        for pair in frames.windows(2) {
            worst_psnr = worst_psnr.min(psnr(&pair[0], &pair[1]));
            let changes: Vec<f32> =
                (0..shadow.len()).filter(|&i| shadow[i]).map(|i| (level(&pair[0], i) - level(&pair[1], i)).abs()).collect();
            worst_shadow = worst_shadow.max(mean(&changes));
        }
        // The same sensors without the multi-view fusion: choosing one
        // camera per surface point must not add flicker of its own.
        let mut stacked = config;
        stacked.fusion.enabled = false;
        let (stacked_frames, _) = stage_sequence(stacked, 0.0, 4, &view, renderer);
        let (fused_rms, stacked_rms) = (flicker(&frames, None), flicker(&stacked_frames, None));
        println!(
            "flicker, {}: {worst_psnr:.1} dB between consecutive ticks, {worst_shadow:.2} levels in the shadow; \
             {fused_rms:.2} levels rms, {stacked_rms:.2} without fusion",
            renderer.name()
        );
        // Measured, normalised: 36.4 dB between consecutive ticks, 1.1
        // levels in the shadow, 3.34 levels rms against 2.99 without fusion.
        // Sorted: 36.5 dB, 1.8 levels, 3.19 against 3.00. The fused cloud
        // averages fewer observations per pixel, which is where its 6 to
        // 12 % come from (a hysteresis on the choice of camera changes
        // nothing; see `gs_core::multiview`).
        assert!(worst_psnr > 35.0, "{}: consecutive ticks differ: {worst_psnr:.1} dB", renderer.name());
        assert!(worst_shadow < 4.0, "{}: the shadow flickers by {worst_shadow:.2} levels", renderer.name());
        assert!(fused_rms < 1.16 * stacked_rms, "{}: {fused_rms:.2} levels against {stacked_rms:.2} stacked", renderer.name());
    }
}

#[test]
fn a_moving_scene_changes_like_the_ground_truth() {
    // The dance goes on: from one tick to the next the picture may change
    // where the truth changes, and by as much. What changes beyond that is
    // flicker (splats of another camera taking over, silhouettes jumping by
    // a sensor pixel). Judged from a held-out view at eye height, with and
    // without sensor noise, against the same sensors without fusion.
    let view = &stage_views()[0];
    for (label, noise) in [("clean", NoiseModel::NONE), ("noisy", NoiseModel::realistic())] {
        let config = PipelineConfig { noise, ..PipelineConfig::for_scene(SceneKind::Stage) };
        let (frames, truth) = stage_sequence(config, 1.0, 5, view, Renderer::Normalized);
        let mut stacked = config;
        stacked.fusion.enabled = false;
        let (stacked_frames, _) = stage_sequence(stacked, 1.0, 5, view, Renderer::Normalized);
        let truth_rms = flicker(&truth, None);
        let (fused_rms, stacked_rms) = (flicker(&frames, Some(&truth)), flicker(&stacked_frames, Some(&truth)));
        println!("moving scene, {label}: the truth changes by {truth_rms:.2} levels rms; unexplained change {fused_rms:.2}, {stacked_rms:.2} without fusion");
        // Measured: the truth changes by 10.7 levels rms; unexplained
        // change 7.20 levels (clean) and 8.07 (noisy), against 7.50 and 8.31
        // without fusion.
        assert!(truth_rms > 5.0, "the dance stands still: {truth_rms:.2}");
        assert!(fused_rms < 9.0, "{label}: {fused_rms:.2} levels of flicker");
        assert!(fused_rms < 1.02 * stacked_rms, "{label}: {fused_rms:.2} levels against {stacked_rms:.2} stacked");
    }
}

/// The live layer of the stage at scene time `t`.
fn live_layer(config: PipelineConfig, t: f32) -> Vec<Splat> {
    let mut pipeline = Pipeline::new(config);
    let mut client = Client::default();
    client.apply(&pipeline.snapshot());
    client.apply(&pipeline.tick(t));
    std::mem::take(&mut client.layers[1])
}

/// Share of the pixels well inside the dancer, seen from close-ups all
/// around the chest at three instants, that the stacked live layer closes
/// and the fused one leaves open, and the share the fused one leaves open
/// at all. "Closed" means inside an opaque visibility disc of the normalised
/// mode's hard level, the layer drawn alone with back faces culled, so that
/// neither the far side of a limb nor the plate hides a hole.
fn holes_of_the_fusion(config: PipelineConfig) -> (f32, f32) {
    let scene = make_scene(SceneKind::Stage);
    let empty = scene.snapshot_static();
    let hard = WblendOptions::normalized_hard();
    let Blend::Normalized(params) = hard.blend else { panic!("the hard level is normalised") };
    let discs = WblendOptions {
        cull_backfaces: true,
        blend: Blend::Normalized(Normalized { vis_cutoff: 1.6, coverage: Coverage::None, silhouette: Silhouette::Hard, ..params }),
        ..hard
    };
    let mut stacked_config = config;
    stacked_config.fusion.enabled = false;
    let clean = CaptureOptions { shadows: config.shadows, ..CaptureOptions::default() };
    let (mut pixels, mut lost, mut open) = (0u32, 0u32, 0u32);
    // Instants at which a hand passes in front of the body.
    for t in [3.1, 7.9, 9.1] {
        let (fused, stacked) = (live_layer(config, t), live_layer(stacked_config, t));
        let snap = scene.snapshot(t);
        let chest = StageScene::new().dancer_joints(t).chest;
        for k in 0..6 {
            let angle = (20.0 + 60.0 * k as f32).to_radians();
            let eye = chest + Vec3::new(0.94 * angle.sin(), 0.34, 0.94 * angle.cos()) * 0.8;
            let cam = Camera::look_at(eye, chest, Vec3::Y, 50f32.to_radians(), 320, 240);
            let (full, plate) = (render_rgbd(&snap, &cam, &clean), render_rgbd(&empty, &cam, &clean));
            let subject: Vec<bool> =
                full.depth.iter().zip(&plate.depth).map(|(&z, &zp)| z > 0.0 && (zp == 0.0 || z < zp - 1.0e-3)).collect();
            let outside: Vec<bool> = subject.iter().map(|inside| !inside).collect();
            let rim = dilate(&outside, 320, 240, 2);
            let coverage = |live: &[Splat]| wblend::render(&[Layer { splats: live, weight: 1.0 }], &cam, &discs).alpha;
            let (a, b) = (coverage(&stacked), coverage(&fused));
            for i in (0..subject.len()).filter(|&i| !rim[i]) {
                pixels += 1;
                open += u32::from(b[i] < 0.5);
                lost += u32::from(b[i] < 0.5 && a[i] >= 0.5);
            }
        }
    }
    assert!(pixels > 100_000, "the close-ups show only {pixels} pixels of the dancer");
    (lost as f32 / pixels as f32, open as f32 / pixels as f32)
}

#[test]
fn the_fusion_opens_no_holes_in_the_dancer() {
    for (label, noise) in [("clean", NoiseModel::NONE), ("noisy", NoiseModel::realistic())] {
        let config = PipelineConfig { noise, ..PipelineConfig::for_scene(SceneKind::Stage) };
        let (lost, open) = holes_of_the_fusion(config);
        // The rule without the tangent-plane test and the demoting band:
        // a hand could replace the chest behind it.
        let mut old = config;
        (old.fusion.plane_tolerance, old.fusion.demote_band) = (0.5, 0);
        let (old_lost, old_open) = holes_of_the_fusion(old);
        println!(
            "holes, {label}: {:.3} % of the dancer closed by the stacked cloud is open in the fused one ({:.3} % open in all); \
             {:.3} % ({:.3} %) with the depth test alone",
            100.0 * lost,
            100.0 * open,
            100.0 * old_lost,
            100.0 * old_open
        );
        // Measured: 0.188 % clean and 0.180 % noisy, against 0.272 % and
        // 0.420 % with the depth test alone (0.48 % and 0.50 % of the dancer
        // are open in all: what no camera sees, and pinholes between discs
        // that the stacked cloud has as well).
        assert!(lost < 0.0023, "{label}: {:.3} % of the dancer is open", 100.0 * lost);
        assert!(lost < 0.75 * old_lost, "{label}: {lost} against {old_lost}");
        assert!(open < 0.006, "{label}: {:.3} % of the dancer is open in all", 100.0 * open);
    }
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
    /// The map in layer 0; layer 1 stays empty in the house.
    layers: [Vec<Splat>; 2],
    ticks: usize,
    appended_bytes: usize,
}

impl Scan {
    fn cloud(&self) -> &[Splat] {
        &self.layers[0]
    }
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
        let bytes = client.bytes;
        Scan { config: *pipeline.config(), layers: client.layers, ticks, appended_bytes: bytes }
    })
}

#[test]
fn house_full_scan() {
    let scan = full_scan();
    let snap = make_scene(SceneKind::House).snapshot(0.0);
    let exposures = Exposures::of_scan(&snap, &scan.config);
    let (mut normalized, mut sorted) = (Tally::default(), Tally::default());
    let mut scores = Vec::new();
    for (i, view) in house_views().iter().enumerate() {
        let mask = observed_mask(&snap, &exposures, &view.cam);
        let score = evaluate(&snap, &scan.layers, &view.cam, scan.config.shadows, Some(&mask), Renderer::Normalized);
        normalized.push(&score);
        scores.push(score);
        // The fallback on one direction per spot.
        if i % 3 == 0 {
            sorted.push(&evaluate(&snap, &scan.layers, &view.cam, scan.config.shadows, Some(&mask), Renderer::Sorted));
        }
    }
    for (i, chunk) in scores.chunks(6).enumerate() {
        save_sheet(&format!("house_{i}.png"), chunk);
    }
    println!("house: {} splats in {} ticks, {:.1} MB", scan.cloud().len(), scan.ticks, scan.appended_bytes as f32 / 1e6);
    normalized.report("house, normalised");
    sorted.report("house, sorted (every third view)");
    println!(
        "house, where the scan has looked: PSNR mean {:.2} dB (worst {:.2}), coverage mean {:.2} % (worst {:.2})",
        mean(&normalized.psnr_observed),
        min(&normalized.psnr_observed),
        100.0 * mean(&normalized.coverage_observed),
        100.0 * min(&normalized.coverage_observed),
    );
    // Measured: 756 234 splats of 2.4 cm cells. Normalised: PSNR mean
    // 23.65 dB (worst 20.50), SSIM 0.851; coverage mean 99.80 % (worst
    // 97.63 %). On the pixels whose surface point some scan frame has seen:
    // PSNR mean 23.97 dB, coverage mean 99.96 % (worst 99.57 %). Sorted, on
    // every third view: 22.76 dB (worst 21.29), SSIM 0.827. What limits the
    // PSNR is still the cell size: a cell is 3 to 4 pixels wide in these
    // views. The 0.4 % of the pixels that the scan never saw (floor behind
    // furniture, the niche beside the wardrobe) cost 0.3 dB.
    assert!((700_000..=800_000).contains(&scan.cloud().len()), "{} splats", scan.cloud().len());
    let (mean_psnr, worst, ssim) = normalized.headline();
    // The worst view (the kitchen seen from the dining room, past a lamp
    // shade the scan sees edge-on) is touchy: turning the head by 2 degrees
    // here and there moves it by 0.4 dB while the mean stays put, so its
    // bound keeps that much distance.
    assert!(mean_psnr >= 23.25 && worst >= 20.0 && ssim >= 0.84, "normalised: {mean_psnr:.2} dB, worst {worst:.2}, SSIM {ssim:.3}");
    assert!(mean(&normalized.coverage) >= 0.996, "mean coverage {:.4}", mean(&normalized.coverage));
    assert!(min(&normalized.coverage) >= 0.973, "worst coverage {:.4}", min(&normalized.coverage));
    assert!(mean(&normalized.psnr_observed) >= 23.55, "mean observed PSNR {:.2} dB", mean(&normalized.psnr_observed));
    assert!(mean(&normalized.coverage_observed) >= 0.998, "mean observed coverage {:.4}", mean(&normalized.coverage_observed));
    assert!(min(&normalized.coverage_observed) >= 0.993, "worst observed coverage {:.4}", min(&normalized.coverage_observed));
    let (mean_psnr, worst, ssim) = sorted.headline();
    assert!(mean_psnr >= 22.3 && worst >= 20.8 && ssim >= 0.815, "sorted: {mean_psnr:.2} dB, worst {worst:.2}, SSIM {ssim:.3}");
    assert!(mean(&sorted.coverage) >= 0.995, "sorted: mean coverage {:.4}", mean(&sorted.coverage));
}

#[test]
fn house_light_preset_scan() {
    // What the WebAssembly worker runs on a laptop or a headset: sensors of
    // 144x108 taking 5 frames per second into 3.2 cm cells, so each of the
    // four side, up and down sensors fires only every 1.6 s.
    let mut config = PipelineConfig::light(SceneKind::House);
    config.scan.autostart = false;
    config.preview.enabled = false;
    let mut pipeline = Pipeline::new(config);
    let mut client = Client::default();
    let ticks = run_full_scan(&mut pipeline, &mut client);
    let snap = make_scene(SceneKind::House).snapshot(0.0);
    let mut tally = Tally::default();
    // One direction per spot of the standard views and the views at
    // ceilings, corners and floors.
    for view in house_views().into_iter().step_by(3).chain(house_look_around_views()) {
        tally.push(&evaluate(&snap, &client.layers, &view.cam, config.shadows, None, Renderer::Normalized));
    }
    println!("house light: {} splats in {ticks} ticks", client.layers[0].len());
    tally.report("house light, normalised");
    // Measured: 432 408 splats; PSNR mean 25.20 dB (worst 21.24), SSIM
    // 0.863; coverage mean 99.84 % (worst 97.76 %).
    assert!((400_000..=460_000).contains(&client.layers[0].len()), "{} splats", client.layers[0].len());
    let (mean_psnr, worst, ssim) = tally.headline();
    assert!(mean_psnr >= 24.8 && worst >= 20.7 && ssim >= 0.85, "{mean_psnr:.2} dB, worst {worst:.2}, SSIM {ssim:.3}");
    assert!(mean(&tally.coverage) >= 0.996 && min(&tally.coverage) >= 0.973, "{:?}", tally.coverage);
}

#[test]
fn house_ceilings_corners_and_floors_are_scanned() {
    // Looking up from inside the rooms: no black slits beside the ceiling
    // beams, no missing patch of ceiling over the shower.
    let scan = full_scan();
    let snap = make_scene(SceneKind::House).snapshot(0.0);
    let (mut tally, mut scores) = (Tally::default(), Vec::new());
    for view in house_look_around_views() {
        assert!(clearance(&snap, view.cam.position()) > 0.3, "{} is not in free space", view.name);
        let score = evaluate(&snap, &scan.layers, &view.cam, scan.config.shadows, None, Renderer::Normalized);
        println!("look around, {:16} {:.2} dB, coverage {:.2} %", view.name, score.psnr, 100.0 * score.coverage);
        assert!(score.coverage >= 0.995, "{}: coverage {:.4}", view.name, score.coverage);
        tally.push(&score);
        scores.push(score);
    }
    save_sheet("house_look_around.png", &scores);
    tally.report("look around, normalised");
    // Measured: every view fully covered; mean 28.51 dB, worst 23.15 dB (the
    // kitchen ceiling with its beams and lamp cords). A single sensor that
    // only looks ahead leaves holes over 5 to 8 % of three of these views.
    let (mean_psnr, worst, _) = tally.headline();
    assert!(mean_psnr >= 28.0 && worst >= 22.6, "{:?}", tally.psnr);
}

#[test]
fn house_surfaces_are_single_layered_and_walls_do_not_bleed() {
    let scan = full_scan();
    let voxel = scan.config.scan.voxel_size;

    // Floor and ceiling: one layer of splats each, facing into the room.
    let facing = |s: &Splat| s.rotation() * Vec3::Z;
    let floor: Vec<&Splat> = scan.cloud().iter().filter(|s| s.pos[1].abs() < 0.02 && facing(s).y > 0.9).collect();
    let ceiling: Vec<&Splat> =
        scan.cloud().iter().filter(|s| (s.pos[1] - house::CEILING_HEIGHT).abs() < 0.02 && facing(s).y < -0.9).collect();
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
    for s in scan.cloud() {
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
        let score = evaluate(&snap, &scan.layers, &cam, scan.config.shadows, None, Renderer::Normalized);
        println!("wall, {name}: {:.2} dB, coverage {:.3}", score.psnr, score.coverage);
        assert!(score.coverage > 0.99, "{name}: holes in the wall ({:.3})", score.coverage);
        // Measured: 29.5 dB from the hall, 24.7 dB from the bathroom (tiles).
        assert!(score.psnr > 23.8, "{name}: {:.2} dB, the far face bleeds through", score.psnr);
        save_sheet(&format!("wall_{}.png", name.replace(' ', "_")), &[score]);
    }
}
