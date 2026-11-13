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
use gs_core::glam::Vec3;
use gs_core::pipeline::Pipeline;
use gs_core::protocol::{ControlMsg, PipelineConfig, ScanState};
use gs_core::raster::{psnr, rasterize, Image, RasterOptions};
use gs_core::scene::{house, make_scene, SceneKind, SceneSnapshot};
use gs_core::Splat;

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
    // Measured: mean 25.12 dB, worst view 22.86 dB (the one from above, which
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
    // Measured: mean 24.55 dB, worst 22.62 dB.
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
    // Measured: light preset mean 25.01 dB (worst 22.26); everything live
    // mean 25.47 dB (worst 23.04).
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
        // shadow: 8.3 / 8.5 levels, against 8.1 / 8.3 for the plate alone.
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
    // Measured: 416 522 splats; PSNR mean 21.46 dB (worst 19.16); coverage
    // mean 99.69 % (worst 97.68 %). On the pixels whose surface point some
    // scan frame has seen: PSNR mean 21.89 dB (worst 19.30), coverage mean
    // 99.92 % (worst 99.41 %). What keeps the PSNR below 22 dB is the cell
    // size: 3.2 cm cells are 2 to 3 pixels wide in these views, which blurs
    // every color edge of the patterned rugs, tiles and pictures. The 0.7 %
    // of the pixels that the scan never saw (floor behind furniture, the
    // niche beside the wardrobe) cost another 0.4 dB.
    assert!(scan.cloud.len() <= 450_000, "{} splats", scan.cloud.len());
    assert!(scan.cloud.len() >= 340_000, "{} splats", scan.cloud.len());
    assert!(mean(&psnrs) >= 21.0, "mean PSNR {:.2} dB", mean(&psnrs));
    assert!(min(&psnrs) >= 18.6, "worst PSNR {:.2} dB", min(&psnrs));
    assert!(mean(&coverages) >= 0.993, "mean coverage {:.4}", mean(&coverages));
    assert!(min(&coverages) >= 0.97, "worst coverage {:.4}", min(&coverages));
    assert!(mean(&psnrs_observed) >= 21.4, "mean observed PSNR {:.2} dB", mean(&psnrs_observed));
    assert!(mean(&coverages_observed) >= 0.997, "mean observed coverage {:.4}", mean(&coverages_observed));
    assert!(min(&coverages_observed) >= 0.988, "worst observed coverage {:.4}", min(&coverages_observed));
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
        assert!(score.coverage >= 0.99, "{}: coverage {:.4}", view.name, score.coverage);
        psnrs.push(score.psnr);
        scores.push(score);
    }
    save_sheet("house_look_around.png", &scores);
    println!("look around: mean {:.2} dB, worst {:.2} dB", mean(&psnrs), min(&psnrs));
    // Measured: every view fully covered; mean 25.63 dB, worst 20.56 dB (the
    // kitchen ceiling with its beams and lamp cords). With the forward-looking
    // sweep of `ScanPath::camera_at` three of these views had holes over 5 to
    // 8 % of the picture and scored 14.6 to 16 dB.
    assert!(mean(&psnrs) >= 25.0 && min(&psnrs) >= 19.9, "{psnrs:?}");
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
        // Measured: 27.6 dB from the hall, 21.7 dB from the bathroom (tiles).
        assert!(score.psnr > 20.8, "{name}: {:.2} dB, the far face bleeds through", score.psnr);
        save_sheet(&format!("wall_{}.png", name.replace(' ', "_")), &[score]);
    }
}
