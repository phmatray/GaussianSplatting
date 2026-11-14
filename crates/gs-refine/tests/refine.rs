//! The offline mode end to end on small simulated captures: dataset,
//! optimisation, checkpoints, cancellation, exports, command line.

mod common;

use std::path::Path;

use common::{scratch, small_sim, write_capture, DANCER};
use gs_core::glam::{Mat3, Quat, Vec3};
use gs_core::ingest::frame::Tracking;
use gs_core::raster::{rasterize, RasterOptions};
use gs_refine::dataset::{decode_view, select_views, views_of, Capture, RegionMask, SelectParams, ViewCache};
use gs_refine::export::{export_dataset, DatasetFormat};
use gs_refine::init::InitParams;
use gs_refine::job::{CancelToken, Preset, RefineConfig, RefineJob, Stage};
use gs_refine::loss::{BACKGROUND, IGNORE, SCENE};
use gs_refine::raster::{CpuRasterizer, Rasterizer, Rendered, View};

/// A configuration that runs in a second or two.
fn tiny_config(iterations: u32) -> RefineConfig {
    let mut config = RefineConfig::preset(Preset::Rapide);
    config.train.iterations = iterations;
    config.train.width = 96;
    config.train.max_gaussians = 80_000;
    config.train.densify.every = 10;
    config.train.densify.from = 0.0;
    config.threads = 2;
    config.select.holdout_every = 6;
    config.evals = 1;
    config.cache_mb = 16;
    config
}

fn prepare(path: &Path, config: RefineConfig) -> RefineJob {
    RefineJob::prepare(path, config, &CancelToken::default(), &mut |_| {}).expect("the job prepares")
}

#[test]
fn the_index_matches_the_file_and_survives_truncation() {
    let dir = scratch("index");
    let written = write_capture(&dir.join("a.gscap"), "bedroom", 6, small_sim(), |_, _| {});
    let capture = Capture::open(&written.path).unwrap();
    assert_eq!((capture.frames.len(), capture.truncated, capture.mode.name()), (6, false, "room"));
    assert_eq!(capture.header.name, "Essai bedroom");
    for (i, frame) in written.frames.iter().enumerate() {
        assert_eq!(&capture.frame(i).unwrap(), frame);
        assert_eq!(capture.jpeg(i).unwrap(), frame.color);
    }
    assert!((capture.duration() - 0.5).abs() < 1e-9);

    // Cut inside the last record: five complete frames remain.
    let bytes = std::fs::read(&written.path).unwrap();
    let cut = dir.join("cut.gscap");
    std::fs::write(&cut, &bytes[..bytes.len() - 100]).unwrap();
    let capture = Capture::open(&cut).unwrap();
    assert_eq!((capture.frames.len(), capture.truncated), (5, true));

    // Not a capture at all, and a record with a wrong length.
    std::fs::write(dir.join("bad.gscap"), b"hello world, this is not a capture").unwrap();
    assert!(Capture::open(&dir.join("bad.gscap")).is_err());
    let mut broken = bytes.clone();
    let first_record = 12 + u32::from_le_bytes(bytes[8..12].try_into().unwrap()) as usize;
    broken[first_record..first_record + 4].copy_from_slice(&50u32.to_le_bytes());
    std::fs::write(dir.join("broken.gscap"), &broken).unwrap();
    assert!(Capture::open(&dir.join("broken.gscap")).is_err());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn selection_drops_what_cannot_teach_anything() {
    let dir = scratch("select");
    let sim = small_sim();
    let mut flat = Vec::new();
    jpeg_encoder::Encoder::new(&mut flat, 80).encode(&vec![128u8; 160 * 120 * 3], 160, 120, jpeg_encoder::ColorType::Rgb).unwrap();
    // The photograph of a frame as a shaking hand would have taken it:
    // every pixel the mean of the 7x7 around it.
    let shaken = |jpeg: &[u8]| {
        let sharp = gs_core::ingest::image::decode_jpeg(jpeg, 160, 120).unwrap();
        let mut rgb = vec![0u8; 160 * 120 * 3];
        for y in 0..120i32 {
            for x in 0..160i32 {
                for c in 0..3 {
                    let mut sum = 0u32;
                    for (dy, dx) in (-3..=3).flat_map(|dy| (-3..=3).map(move |dx| (dy, dx))) {
                        sum += sharp.rgb[((y + dy).clamp(0, 119) * 160 + (x + dx).clamp(0, 159)) as usize * 3 + c] as u32;
                    }
                    rgb[(y * 160 + x) as usize * 3 + c] = (sum / 49) as u8;
                }
            }
        }
        let mut out = Vec::new();
        jpeg_encoder::Encoder::new(&mut out, 80).encode(&rgb, 160, 120, jpeg_encoder::ColorType::Rgb).unwrap();
        out
    };
    let mut still = None;
    let written = write_capture(&dir.join("a.gscap"), "bedroom", 40, sim, |i, frame| {
        match i {
            // The phone lost track.
            5 => frame.tracking = Tracking::Limited,
            // A pose that is no rigid motion.
            9 => frame.world_from_cam[0] = 3.0,
            // No photograph, a blurred one, and one that shows a blank wall
            // (nothing to judge its sharpness by: kept).
            13 => frame.color = b"not a jpeg".to_vec(),
            17 => frame.color = shaken(&frame.color),
            29 => frame.color = flat.clone(),
            // The phone held still: frames 21 to 24 repeat frame 20.
            20 => still = Some(frame.world_from_cam),
            21..=24 => frame.world_from_cam = still.unwrap(),
            _ => {}
        }
    });
    let capture = Capture::open(&written.path).unwrap();
    let all = select_views(&capture, &SelectParams { max_views: 0, holdout_every: 0, ..SelectParams::default() });
    assert_eq!((all.dropped_tracking, all.dropped_pose, all.dropped_color, all.dropped_blurred, all.dropped_duplicate), (1, 1, 1, 1, 4));
    assert_eq!(all.train.len(), 40 - 8);
    assert!(all.holdout.is_empty());
    for dropped in [5, 9, 13, 17, 21, 22, 23, 24] {
        assert!(!all.train.contains(&dropped), "frame {dropped} was kept");
    }
    assert!(all.train.contains(&29));

    // Held-out views are apart from the training views; a cap thins evenly.
    let split = select_views(&capture, &SelectParams { max_views: 10, holdout_every: 4, ..SelectParams::default() });
    assert_eq!((split.train.len(), split.holdout.len()), (10, 8));
    assert!(split.holdout.iter().all(|h| !split.train.contains(h)));
    assert_eq!(split.dropped_thinned, 32 - 8 - 10);
    // The fusion takes every usable frame that is not held out.
    assert_eq!(split.fusion.len(), 32 - 8);
    assert!(split.train.iter().all(|t| split.fusion.contains(t)) && split.holdout.iter().all(|h| !split.fusion.contains(h)));
    assert!(split.train.windows(2).all(|w| w[0] < w[1]) && *split.train.last().unwrap() > 30);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn the_view_cache_stays_within_its_budget() {
    let dir = scratch("cache");
    let written = write_capture(&dir.join("a.gscap"), "bedroom", 8, small_sim(), |_, _| {});
    let capture = Capture::open(&written.path).unwrap();
    let views = views_of(&capture, &(0..8).collect::<Vec<_>>(), 0.0);
    // One view at 96x72 is about 31 kB of pixels and 10 kB of planes.
    let mut cache = ViewCache::new(100_000);
    for round in 0..3 {
        for view in &views {
            let data = cache.get(&capture, view, 96, None).unwrap();
            assert_eq!((data.width, data.height, data.rgb.len()), (96, 72, 96 * 72 * 3));
            assert!(cache.bytes() <= 100_000, "round {round}: {} bytes held", cache.bytes());
        }
    }
    assert_eq!(cache.decodes, 24, "nothing fits twice: every access decodes");
    // With room for everything, the second round is free.
    let mut cache = ViewCache::new(10_000_000);
    for _ in 0..2 {
        for view in &views {
            cache.get(&capture, view, 96, None).unwrap();
        }
    }
    assert_eq!(cache.decodes, 8);
    cache.get(&capture, &views[0], 48, None).unwrap();
    cache.keep_only(48);
    assert!(cache.bytes() < 20_000);
    // The photograph is never enlarged, and the smallest budget still serves.
    let mut tiny = ViewCache::new(1);
    assert_eq!(tiny.get(&capture, &views[0], 4000, None).unwrap().width, 160);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_view_is_decoded_where_its_camera_says() {
    // The decoded photograph, the depth plane and the pose agree: a depth
    // pixel unprojected with the view's camera lands on the splat the
    // fusion made of it, and that splat has the colour of the photograph.
    let dir = scratch("decode");
    let written = write_capture(&dir.join("a.gscap"), "bedroom", 12, small_sim(), |_, _| {});
    let capture = Capture::open(&written.path).unwrap();
    let floor = capture.header.floor.unwrap();
    let views = views_of(&capture, &[4], floor);
    let data = decode_view(&capture.frame(4).unwrap(), &views[0].cam, 160, None).unwrap();
    assert!(data.class.is_empty());
    let depth_cam = views[0].cam.resized(64, 48);
    // In viewer coordinates the floor of the bedroom is y = 0: the lowest
    // depth pixels of a frame that looks down unproject to it.
    let mut lowest = f32::INFINITY;
    for y in 0..48 {
        for x in 0..64 {
            let z = data.depth_mm[y * 64 + x] as f32 * 1e-3;
            if z > 0.0 && data.depth_weight[y * 64 + x] == 255 {
                lowest = lowest.min(depth_cam.unproject(x as f32 + 0.5, y as f32 + 0.5, z).y);
            }
        }
    }
    assert!(lowest > -0.03 && lowest < 1.0, "lowest point at y = {lowest}");
    // And against the truth: the same pixel through the true camera.
    let truth = written.cameras[4].resized(64, 48);
    let (x, y) = (20usize, 30usize);
    let z = data.depth_mm[y * 64 + x] as f32 * 1e-3;
    assert!(z > 0.0);
    let in_scene = truth.unproject(x as f32 + 0.5, y as f32 + 0.5, z);
    let in_view = depth_cam.unproject(x as f32 + 0.5, y as f32 + 0.5, z);
    let back = written.world.capture_from_scene.inverse().transform_point3(in_view + Vec3::new(0.0, floor, 0.0));
    assert!(back.distance(in_scene) < 1e-3, "{back} vs {in_scene}");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn object_captures_are_classified_by_their_region() {
    let dir = scratch("classes");
    let written = write_capture(&dir.join("o.gscap"), "object", 8, small_sim(), |_, _| {});
    let capture = Capture::open(&written.path).unwrap();
    let floor = capture.header.floor.unwrap();
    let region = RegionMask::new(capture.header.region.as_ref().unwrap(), floor, capture.mode);
    assert!(!region.use_person_mask && (region.radius - 1.1).abs() < 1e-6);
    let views = views_of(&capture, &[2], floor);
    let data = decode_view(&capture.frame(2).unwrap(), &views[0].cam, 160, Some(&region)).unwrap();
    let count = |class: u8| data.class.iter().filter(|c| **c == class).count();
    // The dancer in the middle is scene, the far stage is background, and
    // a band in between is left alone.
    assert_eq!(data.class[24 * 64 + 32], SCENE);
    assert!(count(SCENE) > 200 && count(BACKGROUND) > 200 && count(IGNORE) > 20, "{} {} {}", count(SCENE), count(BACKGROUND), count(IGNORE));
    for i in 0..data.class.len() {
        if data.class[i] == BACKGROUND && i % 64 > 0 {
            assert_ne!(data.class[i - 1], SCENE, "background touches scene at {i}");
        }
    }

    // A person capture trusts the mask of its frames.
    let written = write_capture(&dir.join("p.gscap"), "person", 8, small_sim(), |_, _| {});
    let capture = Capture::open(&written.path).unwrap();
    let region = RegionMask::new(capture.header.region.as_ref().unwrap(), floor, capture.mode);
    let frame = capture.frame(2).unwrap();
    let mask = frame.mask.clone().unwrap();
    let data = decode_view(&frame, &views_of(&capture, &[2], floor)[0].cam, 160, Some(&region)).unwrap();
    for (&person, &class) in mask.iter().zip(&data.class) {
        assert_eq!(person >= 128, class == SCENE);
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_short_run_fits_the_photographs_better_and_reports_it() {
    let dir = scratch("run");
    let written = write_capture(&dir.join("a.gscap"), "bedroom", 30, small_sim(), |_, _| {});
    let mut stages = Vec::new();
    let mut job = RefineJob::prepare(&written.path, tiny_config(150), &CancelToken::default(), &mut |p| stages.push(p.stage)).unwrap();
    assert!(stages.contains(&Stage::Select) && stages.contains(&Stage::Fuse));
    assert_eq!((job.info.selection.train.len(), job.info.selection.holdout.len()), (25, 5));
    assert!(job.info.fused_splats > 2000 && job.info.floor_known && job.info.region.is_none());
    let mut last = None;
    let outcome = job.run(&CancelToken::default(), &mut |p| last = Some(*p)).unwrap();
    assert!(outcome.completed && outcome.iterations_done == 150);
    let last = last.unwrap();
    assert_eq!((last.iteration, last.iterations, last.width), (150, 150, 96));

    let report = job.report();
    let (fusion, start, refined) = (report.fusion.unwrap(), report.start.unwrap(), report.refined.unwrap());
    assert_eq!((fusion.views, start.views, refined.views), (5, 5, 5));
    // Thirty frames of a whole room at 96 pixels: the held-out views are
    // far from their neighbours, and still they are predicted better.
    assert!(refined.psnr > start.psnr + 0.5, "held-out PSNR {:.2} -> {:.2} dB", start.psnr, refined.psnr);
    assert!(refined.psnr > fusion.psnr, "fusion {:.2}, refined {:.2} dB", fusion.psnr, refined.psnr);
    assert!(refined.ssim > start.ssim);
    let curve = &report.curve;
    // Within the last level (losses at different sizes do not compare).
    let first = curve.iter().find(|p| p.width == 96).unwrap();
    assert!(curve.len() >= 20 && curve.last().unwrap().loss < first.loss, "{first:?} -> {:?}", curve.last());
    assert!(report.completed && report.result_iteration > 0 && report.wall_s > 0.0 && report.cpu_s > 0.0 && report.threads == 2);
    // The report is plain JSON and reads back.
    let json = serde_json::to_string(&report).unwrap();
    assert_eq!(serde_json::from_str::<gs_refine::job::Report>(&json).unwrap().iterations_done, 150);

    // The result is a splat file the viewer reads, drawn as the optimiser
    // drew it.
    let splats = job.splats();
    assert!(splats.len() > 2000 && splats.len() <= 80_000);
    assert!(splats.iter().all(|s| s.pos.iter().chain(&s.scale).all(|v| v.is_finite()) && s.scale.iter().all(|v| *v > 0.0)));
    let read = gs_core::io::read_ply(&gs_core::io::write_ply(&splats)).unwrap();
    assert_eq!(read.len(), splats.len());
    let cam = job.trainer.views[3].cam.resized(96, 72);
    let viewer = rasterize(&read, &cam, &RasterOptions { background: [18, 20, 28], classic_dilation: true, ..Default::default() });
    let mut mine = Rendered::default();
    CpuRasterizer::default().forward(job.result().0, &View::from_camera(&cam, [18.0 / 255.0, 20.0 / 255.0, 28.0 / 255.0]), &mut mine);
    let psnr = gs_core::raster::psnr(&viewer, &mine.to_image());
    assert!(psnr > 38.0, "the viewer and the optimiser disagree: {psnr:.1} dB");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_run_is_the_same_whatever_the_threads_and_across_a_checkpoint() {
    let dir = scratch("resume");
    let written = write_capture(&dir.join("a.gscap"), "bedroom", 24, small_sim(), |_, _| {});
    let run = |threads: usize| {
        let mut config = tiny_config(36);
        config.threads = threads;
        config.train.pose = true;
        let mut job = prepare(&written.path, config);
        job.run(&CancelToken::default(), &mut |_| {}).unwrap();
        job
    };
    let whole = run(3);
    assert!(run(1).trainer.model == whole.trainer.model, "one and three threads differ");

    // Stop after 17 iterations, write the checkpoint, go on in a new job.
    let mut config = tiny_config(36);
    config.threads = 3;
    config.train.pose = true;
    let mut first = prepare(&written.path, config);
    let checkpoint = dir.join("a.reprise");
    first.checkpoint_to(Some(checkpoint.clone()));
    let outcome = first.run_to(17, &CancelToken::default(), &mut |_| {}).unwrap();
    assert!(!outcome.completed && outcome.iterations_done == 17 && checkpoint.exists());
    drop(first);
    let mut second = RefineJob::resume(&checkpoint, None).unwrap();
    assert_eq!(second.trainer.iteration, 17);
    let outcome = second.run(&CancelToken::default(), &mut |_| {}).unwrap();
    assert!(outcome.completed);
    assert!(second.trainer.model == whole.trainer.model, "the resumed run differs from the uninterrupted one");
    assert!(second.trainer.view_state == whole.trainer.view_state);
    let report = second.report();
    assert!(report.completed && report.start.is_some() && report.fusion.is_some() && report.curve.len() > 20);

    // A checkpoint of another capture, or a damaged one, is refused.
    let other = write_capture(&dir.join("b.gscap"), "bedroom", 12, small_sim(), |_, _| {});
    assert!(RefineJob::resume(&checkpoint, Some(&other.path)).is_err());
    let bytes = std::fs::read(&checkpoint).unwrap();
    std::fs::write(dir.join("cut.reprise"), &bytes[..bytes.len() / 2]).unwrap();
    assert!(RefineJob::resume(&dir.join("cut.reprise"), None).is_err());
    std::fs::write(dir.join("junk.reprise"), b"junk").unwrap();
    assert!(RefineJob::resume(&dir.join("junk.reprise"), None).is_err());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn cancelling_keeps_a_usable_result() {
    let dir = scratch("cancel");
    let written = write_capture(&dir.join("a.gscap"), "bedroom", 24, small_sim(), |_, _| {});
    // Cancelled during the preparation: nothing to keep.
    let cancelled = CancelToken::default();
    cancelled.cancel();
    assert!(matches!(RefineJob::prepare(&written.path, tiny_config(50), &cancelled, &mut |_| {}), Err(gs_refine::job::RefineError::Cancelled)));

    let mut job = prepare(&written.path, tiny_config(100_000));
    let cancel = CancelToken::default();
    let stop = cancel.clone();
    let outcome = job
        .run(&cancel, &mut |p| {
            if p.stage == Stage::Optimise && p.iteration >= 5 {
                stop.cancel();
            }
        })
        .unwrap();
    assert!(!outcome.completed && outcome.iterations_done >= 5 && outcome.iterations_done < 100_000);
    let report = job.report();
    assert!(!report.completed && report.refined.is_some());
    assert!(job.splats().len() > 2000);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn an_object_capture_stays_inside_its_region() {
    let dir = scratch("object");
    let written = write_capture(&dir.join("o.gscap"), "object", 24, small_sim(), |_, _| {});
    let mut job = prepare(&written.path, tiny_config(120));
    let (center, radius) = job.info.region.expect("an object capture has a region");
    // The region in the coordinates of the result: the capture's world
    // with the floor at y = 0.
    let dancer = written.world.to_capture(DANCER) - Vec3::new(0.0, written.world.floor_y(), 0.0);
    assert!((radius - 1.1).abs() < 1e-6 && (Vec3::from_array(center) - dancer).length() < 1e-3, "{center:?} vs {dancer}");
    job.run(&CancelToken::default(), &mut |_| {}).unwrap();
    let splats = job.splats();
    assert!(splats.len() > 500);
    for s in &splats {
        assert!(s.position().distance(Vec3::from_array(center)) <= radius * 1.06, "a Gaussian left the region: {:?}", s.pos);
    }
    let report = job.report();
    assert!(report.refined.unwrap().psnr > report.start.unwrap().psnr, "{:?} -> {:?}", report.start, report.refined);
    // Live captures are refused with a reason.
    let bytes = std::fs::read(&written.path).unwrap();
    let text = String::from_utf8_lossy(&bytes[12..400]).replace("\"object\"", "\"live\"  ");
    let mut live = bytes.clone();
    live[12..400].copy_from_slice(text.as_bytes());
    std::fs::write(dir.join("live.gscap"), &live).unwrap();
    let error = RefineJob::prepare(&dir.join("live.gscap"), tiny_config(10), &CancelToken::default(), &mut |_| {}).err().expect("a live capture is refused");
    assert!(error.to_string().contains("live"), "{error}");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn wandering_exposure_is_absorbed_per_view_and_the_scene_keeps_its_brightness() {
    let dir = scratch("exposure");
    let frames = 30;
    let steady = write_capture(&dir.join("steady.gscap"), "bedroom", frames, small_sim(), |_, _| {});
    let mut sim = small_sim();
    sim.exposure_drift = 0.15;
    let wandering = write_capture(&dir.join("wandering.gscap"), "bedroom", frames, sim, |_, _| {});
    let run = |path: &Path| {
        let mut config = tiny_config(150);
        config.select.holdout_every = 0;
        let mut job = prepare(path, config);
        job.run(&CancelToken::default(), &mut |_| {}).unwrap();
        job
    };
    let (reference, job) = (run(&steady.path), run(&wandering.path));

    // What the simulated phone did to frame i (see `ingest_sim`): the
    // learned gains follow it, up to the common factor the anchor removes.
    let truth: Vec<[f32; 3]> = (0..frames)
        .map(|i| {
            let t = i as f32 / 10.0;
            let (exposure, warmth) = (1.0 + 0.15 * (0.9 * t).sin(), 0.05 * (0.37 * t + 1.0).sin());
            [exposure * (1.0 + warmth), exposure, exposure * (1.0 - warmth)]
        })
        .collect();
    let n = job.trainer.views.len();
    assert_eq!(n, frames);
    for c in 0..3 {
        let mean_truth = truth.iter().map(|g| g[c]).sum::<f32>() / n as f32;
        let mean_learned = job.trainer.view_state.iter().map(|s| s.correction.gain[c]).sum::<f32>() / n as f32;
        assert!((mean_learned - 1.0).abs() < 1e-3, "the gains are anchored: {mean_learned}");
        let (mut worst, mut mean) = (0.0f32, 0.0f32);
        for (view, state) in job.trainer.views.iter().zip(&job.trainer.view_state) {
            let error = (state.correction.gain[c] - truth[view.frame][c] / mean_truth).abs();
            worst = worst.max(error);
            mean += error / n as f32;
        }
        // The phone's gains span 0.85 to 1.2 here. (Thirty small pictures of
        // a whole room; a real capture pins the gains down to a percent.)
        assert!(mean < 0.035 && worst < 0.1, "channel {c}: the learned gains are {mean} off the phone's on average, {worst} at worst");
    }
    // The steady capture needs next to no correction (what is left is the
    // optimiser explaining the errors of a 96-pixel model with a gain).
    let steady_worst = reference.trainer.view_state.iter().flat_map(|s| s.correction.gain).map(|g| (g - 1.0).abs()).fold(0.0, f32::max);
    assert!(steady_worst < 0.06, "a steady capture got a gain {steady_worst} away from one");

    // The scene itself has the brightness of the capture as a whole: its
    // mean colour is the steady one times the mean gain of the phone.
    let mean_color = |job: &RefineJob| {
        let splats = job.splats();
        [0, 1, 2].map(|c| splats.iter().map(|s| s.rgba[c] as f32).sum::<f32>() / splats.len() as f32)
    };
    let (steady_color, wandering_color) = (mean_color(&reference), mean_color(&job));
    for c in 0..3 {
        let mean_truth = truth.iter().map(|g| g[c]).sum::<f32>() / n as f32;
        let ratio = wandering_color[c] / (steady_color[c] * mean_truth);
        assert!((ratio - 1.0).abs() < 0.03, "channel {c}: brightness drifted by {ratio}");
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn depth_that_hangs_between_two_surfaces_is_not_trusted() {
    let dir = scratch("flying");
    let floor = |capture: &Capture| capture.header.floor.unwrap();
    // With a confidence plane: low confidence has no weight, medium some.
    let with = write_capture(&dir.join("with.gscap"), "object", 4, small_sim(), |_, _| {});
    let capture = Capture::open(&with.path).unwrap();
    let frame = capture.frame(1).unwrap();
    let data = decode_view(&frame, &views_of(&capture, &[1], floor(&capture))[0].cam, 160, None).unwrap();
    let confidence = frame.confidence.as_ref().unwrap();
    assert!(confidence.contains(&0) && confidence.contains(&2), "the simulated phone marks silhouettes");
    for i in 0..confidence.len() {
        let expected = if frame.depth_mm[i] == 0 { 0 } else { [0, 96, 255][confidence[i] as usize] };
        assert_eq!(data.depth_weight[i], expected, "pixel {i}");
    }

    // Without one: every pixel next to a depth step loses its weight, and
    // holes (no return) never had any.
    let mut sim = small_sim();
    sim.with_confidence = false;
    let without = write_capture(&dir.join("without.gscap"), "object", 4, sim, |_, frame| {
        // A hole in the depth map.
        for y in 10..14 {
            for x in 40..46 {
                frame.depth_mm[y * 64 + x] = 0;
            }
        }
    });
    let capture = Capture::open(&without.path).unwrap();
    let frame = capture.frame(1).unwrap();
    assert!(frame.confidence.is_none());
    let data = decode_view(&frame, &views_of(&capture, &[1], floor(&capture))[0].cam, 160, None).unwrap();
    let (mut steps, mut trusted) = (0, 0);
    for y in 0..48usize {
        for x in 0..63usize {
            let (a, b) = (frame.depth_mm[y * 64 + x] as f32, frame.depth_mm[y * 64 + x + 1] as f32);
            if a > 0.0 && b > 0.0 && (a - b).abs() > 0.05 * a.min(b) * 1.06 {
                steps += 1;
                assert_eq!((data.depth_weight[y * 64 + x], data.depth_weight[y * 64 + x + 1]), (0, 0), "step at ({x}, {y})");
            }
        }
    }
    for (i, &weight) in data.depth_weight.iter().enumerate() {
        assert!(frame.depth_mm[i] != 0 || weight == 0, "a hole is trusted");
        trusted += (weight == 255) as usize;
    }
    assert_eq!(data.depth_weight[12 * 64 + 43], 0);
    assert!(steps > 20 && trusted > 500, "{steps} steps, {trusted} trusted pixels");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_person_capture_keeps_the_person_only() {
    let dir = scratch("person");
    let written = write_capture(&dir.join("p.gscap"), "person", 24, small_sim(), |_, _| {});
    let mut job = prepare(&written.path, tiny_config(100));
    assert_eq!(job.info.mode, "person");
    let start = job.fused().len();
    job.run(&CancelToken::default(), &mut |_| {}).unwrap();
    let splats = job.splats();
    assert!(splats.len() > 300, "{} splats from {start}", splats.len());
    // Nothing of the stage floor (y = 0 in the result) below the feet: the
    // masks keep the person, and the optimiser grows nothing outside them.
    let (center, radius) = job.info.region.unwrap();
    let low = splats.iter().filter(|s| s.pos[1] < 0.02 && s.rgba[3] > 40).count();
    assert!(low * 50 < splats.len(), "{low} of {} splats lie on the floor", splats.len());
    assert!(splats.iter().all(|s| s.position().distance(Vec3::from_array(center)) <= radius * 1.06));
    let report = job.report();
    assert!(report.refined.is_some_and(|r| r.psnr > report.start.unwrap().psnr), "{:?} -> {:?}", report.start, report.refined);
    let _ = std::fs::remove_dir_all(dir);
}

// ---------------------------------------------------------------------------
// Exports: read our own files back with readers written from the formats'
// documentation, and reproject points whose pixels are known.
// ---------------------------------------------------------------------------

/// A selection that keeps every frame, so that images and frames pair up.
fn every_frame() -> SelectParams {
    SelectParams { max_views: 0, holdout_every: 0, min_move: 0.0, min_turn: 0.0, blur_ratio: 0.0 }
}

/// A pinhole camera as a dataset describes it.
struct ReadCamera {
    name: String,
    /// World-to-camera rotation and translation in the format's own axes.
    rotation: Mat3,
    translation: Vec3,
    intrinsics: [f32; 4],
    size: [u32; 2],
}

fn colmap_text_cameras(model: &Path) -> Vec<ReadCamera> {
    let numbers = |line: &str| line.split_whitespace().map(str::to_string).collect::<Vec<_>>();
    let cameras: Vec<Vec<String>> = std::fs::read_to_string(model.join("cameras.txt")).unwrap().lines().filter(|l| !l.starts_with('#')).map(numbers).collect();
    let text = std::fs::read_to_string(model.join("images.txt")).unwrap();
    let lines: Vec<&str> = text.lines().filter(|l| !l.starts_with('#')).collect();
    // Two lines per image; the second (2D points) is empty.
    lines
        .chunks(2)
        .map(|pair| {
            assert!(pair.get(1).is_none_or(|l| l.trim().is_empty()));
            let f = numbers(pair[0]);
            let v = |i: usize| f[i].parse::<f32>().unwrap();
            let camera = cameras.iter().find(|c| c[0] == f[8]).expect("the image's camera");
            assert_eq!(camera[1], "PINHOLE");
            ReadCamera {
                name: f[9].clone(),
                // QW QX QY QZ.
                rotation: Mat3::from_quat(Quat::from_xyzw(v(2), v(3), v(4), v(1))),
                translation: Vec3::new(v(5), v(6), v(7)),
                intrinsics: [4, 5, 6, 7].map(|i| camera[i].parse().unwrap()),
                size: [camera[2].parse().unwrap(), camera[3].parse().unwrap()],
            }
        })
        .collect()
}

struct Bytes<'a>(&'a [u8]);

impl Bytes<'_> {
    fn take<const N: usize>(&mut self) -> [u8; N] {
        let (head, tail) = self.0.split_at(N);
        self.0 = tail;
        head.try_into().unwrap()
    }
    fn u64(&mut self) -> u64 {
        u64::from_le_bytes(self.take())
    }
    fn i32(&mut self) -> i32 {
        i32::from_le_bytes(self.take())
    }
    fn f64(&mut self) -> f64 {
        f64::from_le_bytes(self.take())
    }
}

fn colmap_binary_cameras(model: &Path) -> Vec<ReadCamera> {
    let data = std::fs::read(model.join("cameras.bin")).unwrap();
    let mut b = Bytes(&data);
    let mut cameras = Vec::new();
    for _ in 0..b.u64() {
        let (id, model_id, width, height) = (b.i32(), b.i32(), b.u64(), b.u64());
        assert_eq!(model_id, 1, "PINHOLE");
        cameras.push((id, [width as u32, height as u32], [0; 4].map(|_| b.f64() as f32)));
    }
    assert!(b.0.is_empty());
    let data = std::fs::read(model.join("images.bin")).unwrap();
    let mut b = Bytes(&data);
    let mut out = Vec::new();
    for _ in 0..b.u64() {
        let _id = b.i32();
        let q = [0; 4].map(|_| b.f64() as f32);
        let t = [0; 3].map(|_| b.f64() as f32);
        let camera_id = b.i32();
        let end = b.0.iter().position(|c| *c == 0).unwrap();
        let name = String::from_utf8(b.0[..end].to_vec()).unwrap();
        b.0 = &b.0[end + 1..];
        assert_eq!(b.u64(), 0, "no 2D points");
        let camera = cameras.iter().find(|c| c.0 == camera_id).unwrap();
        out.push(ReadCamera { name, rotation: Mat3::from_quat(Quat::from_xyzw(q[1], q[2], q[3], q[0])), translation: Vec3::from_array(t), intrinsics: camera.2, size: camera.1 });
    }
    assert!(b.0.is_empty());
    out
}

#[test]
fn the_colmap_export_reprojects_the_depth_maps_onto_their_pixels() {
    let dir = scratch("colmap");
    let written = write_capture(&dir.join("a.gscap"), "bedroom", 14, small_sim(), |_, _| {});
    let capture = Capture::open(&written.path).unwrap();
    let out = dir.join("colmap");
    let mut steps = 0;
    let summary =
        export_dataset(&capture, &out, DatasetFormat::Colmap, &every_frame(), &InitParams::default(), &CancelToken::default(), &mut |_, _| steps += 1).unwrap();
    assert_eq!((summary.images, summary.depth_maps, summary.masks), (14, 0, 0));
    assert!(summary.points > 1000 && steps > 2);
    let model = out.join("sparse").join("0");
    let floor = capture.header.floor.unwrap();

    for cameras in [colmap_text_cameras(&model), colmap_binary_cameras(&model)] {
        assert_eq!(cameras.len(), 14);
        for (i, cam) in cameras.iter().enumerate() {
            let frame = &written.frames[i];
            assert_eq!(cam.name, format!("frame_{:06}.jpg", frame.index));
            assert_eq!(std::fs::read(out.join("images").join(&cam.name)).unwrap(), frame.color, "the photograph is copied as recorded");
            assert_eq!(cam.size, [160, 120]);
            // World points with known pixels: the frame's own depth map,
            // unprojected with the true camera of the simulation and
            // brought into the exported world (floor at y = 0).
            let truth = written.cameras[i].resized(64, 48);
            for (x, y) in [(5usize, 7usize), (32, 24), (60, 40), (11, 44)] {
                let z = frame.depth_mm[y * 64 + x] as f32 * 1e-3;
                if z == 0.0 {
                    continue;
                }
                let scene = truth.unproject(x as f32 + 0.5, y as f32 + 0.5, z);
                let world = written.world.to_capture(scene) - Vec3::new(0.0, floor, 0.0);
                // COLMAP: x_cam = R·X + t, camera looks along +Z, y down,
                // u = fx·x/z + cx, v = fy·y/z + cy.
                let c = cam.rotation * world + cam.translation;
                assert!((c.z - z).abs() < 2e-3, "depth {} vs {z}", c.z);
                let [fx, fy, cx, cy] = cam.intrinsics;
                let (u, v) = (fx * c.x / c.z + cx, fy * c.y / c.z + cy);
                let expected = ((x as f32 + 0.5) * 2.5, (y as f32 + 0.5) * 2.5);
                assert!((u - expected.0).abs() < 0.05 && (v - expected.1).abs() < 0.05, "image {i}: ({u}, {v}) vs {expected:?}");
            }
        }
    }

    // The initial points: as many in both files, inside the room, floor at 0.
    let text = std::fs::read_to_string(model.join("points3D.txt")).unwrap();
    let points: Vec<Vec<f32>> = text.lines().filter(|l| !l.starts_with('#')).map(|l| l.split_whitespace().take(7).map(|v| v.parse().unwrap()).collect()).collect();
    assert_eq!(points.len(), summary.points);
    let lowest = points.iter().map(|p| p[2]).fold(f32::INFINITY, f32::min);
    assert!(lowest > -0.05 && lowest < 0.3, "lowest point at y = {lowest}");
    assert!(points.iter().all(|p| p[4..7].iter().all(|c| (0.0..=255.0).contains(c))));
    let data = std::fs::read(model.join("points3D.bin")).unwrap();
    let mut b = Bytes(&data);
    assert_eq!(b.u64() as usize, summary.points);
    assert_eq!(data.len(), 8 + summary.points * (8 + 24 + 3 + 8 + 8));
    assert_eq!(b.u64(), 1);
    assert!((b.f64() as f32 - points[0][1]).abs() < 1e-4);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn the_nerfstudio_export_reprojects_the_depth_maps_onto_their_pixels() {
    let dir = scratch("nerfstudio");
    let written = write_capture(&dir.join("p.gscap"), "person", 12, small_sim(), |_, _| {});
    let capture = Capture::open(&written.path).unwrap();
    let out = dir.join("ns");
    let summary =
        export_dataset(&capture, &out, DatasetFormat::Nerfstudio, &every_frame(), &InitParams::default(), &CancelToken::default(), &mut |_, _| {}).unwrap();
    assert_eq!((summary.images, summary.depth_maps, summary.masks), (12, 12, 12));
    let floor = capture.header.floor.unwrap();
    let meta: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(out.join("transforms.json")).unwrap()).unwrap();
    assert_eq!(meta["depth_unit_scale_factor"].as_f64(), Some(0.001));
    let frames = meta["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 12);
    for (i, entry) in frames.iter().enumerate() {
        let frame = &written.frames[i];
        let number = |key: &str| entry[key].as_f64().unwrap() as f32;
        assert_eq!(std::fs::read(out.join(entry["file_path"].as_str().unwrap())).unwrap(), frame.color);
        assert_eq!((number("w"), number("h")), (160.0, 120.0));
        // transform_matrix: camera to world, rows; OpenGL axes (x right,
        // y up, z backward).
        let rows: Vec<Vec<f32>> = entry["transform_matrix"].as_array().unwrap().iter().map(|r| r.as_array().unwrap().iter().map(|v| v.as_f64().unwrap() as f32).collect()).collect();
        assert_eq!(rows[3], [0.0, 0.0, 0.0, 1.0]);
        let rotation = Mat3::from_cols(Vec3::new(rows[0][0], rows[1][0], rows[2][0]), Vec3::new(rows[0][1], rows[1][1], rows[2][1]), Vec3::new(rows[0][2], rows[1][2], rows[2][2]));
        let position = Vec3::new(rows[0][3], rows[1][3], rows[2][3]);

        // The depth PNG: 16 bits, millimeters, the frame's own values where
        // the confidence is not low.
        let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(out.join(entry["depth_file_path"].as_str().unwrap())).unwrap()));
        let mut reader = decoder.read_info().unwrap();
        let mut buffer = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buffer).unwrap();
        assert_eq!((info.width, info.height, info.bit_depth, info.color_type), (64, 48, png::BitDepth::Sixteen, png::ColorType::Grayscale));
        let depth: Vec<u16> = buffer.chunks_exact(2).map(|b| u16::from_be_bytes([b[0], b[1]])).collect();
        let confidence = frame.confidence.as_ref().unwrap();
        let truth = written.cameras[i].resized(64, 48);
        let mut checked = 0;
        for at in (0..64 * 48).step_by(13) {
            if confidence[at] == 0 {
                assert_eq!(depth[at], 0);
                continue;
            }
            assert_eq!(depth[at], frame.depth_mm[at]);
            if depth[at] == 0 {
                continue;
            }
            let (x, y) = ((at % 64) as f32 + 0.5, (at / 64) as f32 + 0.5);
            let z = depth[at] as f32 * 0.001;
            let world = written.world.to_capture(truth.unproject(x, y, z)) - Vec3::new(0.0, floor, 0.0);
            // OpenGL camera: the point in camera space, looking along -z.
            let c = rotation.transpose() * (world - position);
            assert!((-c.z - z).abs() < 2e-3);
            let (u, v) = (number("fl_x") * c.x / -c.z + number("cx"), number("cy") - number("fl_y") * c.y / -c.z);
            assert!((u - x * 2.5).abs() < 0.05 && (v - y * 2.5).abs() < 0.05, "image {i}: ({u}, {v}) vs ({}, {})", x * 2.5, y * 2.5);
            checked += 1;
        }
        assert!(checked > 10);

        // The mask at the size of the photograph.
        let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(out.join(entry["mask_path"].as_str().unwrap())).unwrap()));
        let mut reader = decoder.read_info().unwrap();
        let mut buffer = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buffer).unwrap();
        assert_eq!((info.width, info.height), (160, 120));
        let mask = frame.mask.as_ref().unwrap();
        assert_eq!(buffer[60 * 160 + 80] >= 128, mask[24 * 64 + 32] >= 128);
        assert!(buffer.contains(&255) && buffer.contains(&0));
    }

    // The initial points are the fused person, in the same world.
    let ply = std::fs::read(out.join(meta["ply_file_path"].as_str().unwrap())).unwrap();
    let header_end = ply.windows(11).position(|w| w == b"end_header\n").unwrap() + 11;
    assert_eq!((ply.len() - header_end) / 15, summary.points);
    let first = Vec3::new(
        f32::from_le_bytes(ply[header_end..header_end + 4].try_into().unwrap()),
        f32::from_le_bytes(ply[header_end + 4..header_end + 8].try_into().unwrap()),
        f32::from_le_bytes(ply[header_end + 8..header_end + 12].try_into().unwrap()),
    );
    let dancer = written.world.to_capture(DANCER) - Vec3::new(0.0, floor, 0.0);
    assert!(first.distance(dancer) < 1.3, "{first} vs {dancer}");
    let _ = std::fs::remove_dir_all(dir);
}

// ---------------------------------------------------------------------------
// The command-line tool.
// ---------------------------------------------------------------------------

fn tool(args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_gs-refine")).args(args).output().expect("the tool runs")
}

#[test]
fn the_command_line_tool_refines_describes_and_exports() {
    let dir = scratch("cli");
    let written = write_capture(&dir.join("a.gscap"), "bedroom", 20, small_sim(), |_, _| {});
    let capture = written.path.to_str().unwrap();

    let help = tool(&["aide"]);
    assert!(help.status.success() && String::from_utf8_lossy(&help.stdout).contains("gs-refine <capture.gscap> -o <sortie.ply>"));

    let info = tool(&["info", capture]);
    let text = String::from_utf8_lossy(&info.stdout).to_string();
    assert!(info.status.success(), "{text}");
    for expected in ["20 images", "mode : room", "couleur 160x120, profondeur 64x48, confiance", "vues d'entraînement", "sol détecté"] {
        assert!(text.contains(expected), "« {expected} » is missing from:\n{text}");
    }

    let out = dir.join("resultat.ply");
    let run = tool(&[
        capture, "-o", out.to_str().unwrap(), "--preset", "rapide", "--iterations", "25", "--largeur", "96", "--threads", "2", "--evaluation", "5", "--splat", "--set", "evals=0",
    ]);
    let log = String::from_utf8_lossy(&run.stderr).to_string();
    assert!(run.status.success(), "{log}");
    assert!(log.contains("Optimisation terminée") && log.contains("Vues mises de côté : fusion temps réel"), "{log}");
    assert!(!log.contains('\u{2014}'), "no em dash in messages");
    let splats = gs_core::io::read_ply(&std::fs::read(&out).unwrap()).unwrap();
    assert!(splats.len() > 2000);
    assert_eq!(gs_core::io::read_splat(&std::fs::read(dir.join("resultat.splat")).unwrap()).unwrap().len(), splats.len());
    let report: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("resultat.json")).unwrap()).unwrap();
    assert_eq!(report["iterations_done"].as_u64(), Some(25));
    assert_eq!(report["completed"].as_bool(), Some(true));
    assert!(report["refined"]["psnr"].as_f64().unwrap() > report["start"]["psnr"].as_f64().unwrap());
    assert!(!dir.join("resultat.reprise").exists(), "a finished run leaves no checkpoint");

    let export = tool(&["export", capture, "--format", "colmap", "-o", dir.join("colmap").to_str().unwrap(), "--silencieux"]);
    assert!(export.status.success(), "{}", String::from_utf8_lossy(&export.stderr));
    assert!(dir.join("colmap/sparse/0/images.bin").exists() && dir.join("colmap/images/frame_000000.jpg").exists());

    // Mistakes are answered in French with a non-zero status.
    for (args, expected) in [
        (vec![capture], "il manque la sortie"),
        (vec!["absent.gscap", "-o", "x.ply"], "capture illisible"),
        (vec![capture, "-o", "x.ply", "--preset", "turbo"], "preset inconnu"),
        (vec![capture, "-o", "x.ply", "--iterations", "beaucoup"], "valeur incorrecte"),
        (vec!["export", capture, "--format", "obj", "-o", "x"], "format inconnu"),
        (vec![capture, "-o", "x.ply", "--set", "train.nimporte=1"], "n'existe pas"),
    ] {
        let failed = tool(&args);
        let message = String::from_utf8_lossy(&failed.stderr).to_string();
        assert!(!failed.status.success() && message.contains(expected), "{args:?}: {message}");
    }
    let _ = std::fs::remove_dir_all(dir);
}
