//! What a recording from a real session can do to the offline mode: stops
//! and restarts at any moment, captures with almost nothing in them, depth
//! that is missing over large areas, files cut short. Every capture here is
//! simulated.

mod common;

use std::io::{BufWriter, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use common::ingest_sim::{Phone, PhoneSim};
use common::{scratch, small_sim, write_capture};
use gs_core::glam::Vec3;
use gs_core::ingest::frame::{encode_frame, CaptureFrame};
use gs_core::ingest::{GscapHeader, GscapWriter};
use gs_core::raster::{rasterize, RasterOptions};
use gs_core::scene::{make_scene, SceneKind};
use gs_core::{Camera, RgbdFrame};
use gs_refine::job::{CancelToken, Preset, RefineConfig, RefineError, RefineJob, Stage};
use gs_refine::model::Gaussians;
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
    config.evals = 3;
    config.cache_mb = 16;
    config
}

fn prepare(path: &Path, config: RefineConfig) -> RefineJob {
    RefineJob::prepare(path, config, &CancelToken::default(), &mut |_| {}).expect("the job prepares")
}

/// What a finished job hands to its user, without the clock: the result,
/// the final state, the scores and the loss curve.
#[derive(Debug, PartialEq)]
struct Product {
    result_iteration: u32,
    result: Gaussians,
    last: Gaussians,
    evals: Vec<(u32, f32, f32)>,
    curve: Vec<(u32, f32, usize, u32)>,
    refined: Option<(f32, f32)>,
}

fn product(job: &RefineJob) -> Product {
    let report = job.report();
    Product {
        result_iteration: job.result().1,
        result: job.result().0.clone(),
        last: job.trainer.model.clone(),
        evals: report.evals.iter().map(|e| (e.iteration, e.score.psnr, e.score.ssim)).collect(),
        curve: report.curve.iter().map(|p| (p.iteration, p.loss, p.gaussians, p.width)).collect(),
        refined: report.refined.map(|s| (s.psnr, s.ssim)),
    }
}

/// Runs `config` on `capture` in as many sessions as it takes: a session
/// ends at the next of the iterations `stops` (a pause), or when `cancel_at`
/// says so on a progress report (the token, as Ctrl-C does); the next one
/// resumes from the checkpoint the stopped job wrote, in a new job.
fn run_with_stops(capture: &Path, config: &RefineConfig, checkpoint: &Path, stops: &[u32], mut cancel_at: impl FnMut(&gs_refine::job::Progress) -> bool) -> (RefineJob, usize) {
    let mut job = prepare(capture, config.clone());
    job.checkpoint_to(Some(checkpoint.to_path_buf()));
    let mut sessions = 0;
    loop {
        sessions += 1;
        assert!(sessions < 200, "the job does not get anywhere");
        let cancel = CancelToken::default();
        let token = cancel.clone();
        let next = stops.iter().copied().find(|s| *s > job.trainer.iteration).unwrap_or(u32::MAX);
        let outcome = job
            .run_to(next, &cancel, &mut |p| {
                if cancel_at(p) {
                    token.cancel();
                }
            })
            .expect("the run");
        if outcome.completed {
            return (job, sessions);
        }
        drop(job);
        job = RefineJob::resume(checkpoint, None).expect("the checkpoint resumes");
    }
}

#[test]
fn stopping_anywhere_and_resuming_gives_the_uninterrupted_result() {
    let dir = scratch("stops");
    let written = write_capture(&dir.join("a.gscap"), "bedroom", 24, small_sim(), |_, _| {});
    for pose in [false, true] {
        // Sixty iterations: evaluations at 0, 15, 30, 45 and 60, a
        // densification every 10, the second level from 18 on.
        let mut config = tiny_config(60);
        config.train.pose = pose;
        let mut whole = prepare(&written.path, config.clone());
        assert!(whole.run(&CancelToken::default(), &mut |_| {}).unwrap().completed);
        let expected = product(&whole);
        assert!(expected.evals.len() == 5 && expected.curve.len() >= 30, "{:?}", expected.evals);

        // Stops during the exposure pre-pass and the two pose registrations
        // (Ctrl-C), and pauses after the first step, at a densification,
        // just before, at and just after the change of level, at an
        // evaluation and one step before the end.
        let checkpoint = dir.join("a.reprise");
        let mut wanted: Vec<(Stage, u64, u32)> = vec![(Stage::Exposure, 8, 0), (Stage::Poses, 4, 0), (Stage::Poses, 8, 18)];
        let mut hits = 0;
        let (job, sessions) = run_with_stops(&written.path, &config, &checkpoint, &[1, 10, 17, 18, 19, 30, 31, 59], |p| {
            let Some(at) = wanted.iter().position(|w| w.0 == p.stage && p.done >= w.1 && p.iteration == w.2) else { return false };
            wanted.remove(at);
            hits += 1;
            true
        });
        assert_eq!((sessions, hits), if pose { (12, 3) } else { (10, 1) }, "pose {pose}");
        let got = product(&job);
        assert!(got.last == expected.last, "pose {pose}: the final state differs after {sessions} sessions");
        assert!(job.trainer.view_state == whole.trainer.view_state, "pose {pose}: the views differ");
        assert_eq!(got.evals, expected.evals, "pose {pose}");
        assert_eq!(got.result_iteration, expected.result_iteration, "pose {pose}");
        assert!(got.result == expected.result, "pose {pose}: the result differs");
        assert_eq!(got.refined, expected.refined);
        assert_eq!(got.curve, expected.curve, "pose {pose}");
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn the_best_state_survives_a_stop() {
    // Rates that wreck the cloud: the best state on the held-out views is
    // an early one. A stopped and resumed run must still hand that one out.
    let dir = scratch("best");
    let written = write_capture(&dir.join("a.gscap"), "bedroom", 24, small_sim(), |_, _| {});
    let mut config = tiny_config(40);
    config.train.rates.position = 0.2;
    config.train.rates.position_final = 0.2;
    config.train.rates.scale = 0.3;
    config.train.densify.every = 0;
    let mut whole = prepare(&written.path, config.clone());
    whole.run(&CancelToken::default(), &mut |_| {}).unwrap();
    let expected = product(&whole);
    assert!(expected.result_iteration < 40, "the wrecked run did not get worse: best at {}", expected.result_iteration);

    let (job, sessions) = run_with_stops(&written.path, &config, &dir.join("a.reprise"), &[25], |_| false);
    assert_eq!(sessions, 2);
    let got = product(&job);
    assert_eq!(got.result_iteration, expected.result_iteration);
    assert!(got.result == expected.result, "the resumed job hands out another state");

    // A job stopped between two evaluations hands out the better of its
    // best evaluated state and the state it stopped in, and says which.
    let mut paused = prepare(&written.path, config);
    paused.run_to(25, &CancelToken::default(), &mut |_| {}).unwrap();
    let report = paused.report();
    assert_eq!(report.evals.last().unwrap().iteration, 25);
    let best = report.evals.iter().map(|e| e.score.psnr).fold(f32::MIN, f32::max);
    assert_eq!(report.evals.iter().find(|e| e.iteration == report.result_iteration).unwrap().score.psnr, best);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn two_runs_of_one_configuration_are_identical() {
    let dir = scratch("same");
    let written = write_capture(&dir.join("a.gscap"), "bedroom", 20, small_sim(), |_, _| {});
    let run = || {
        let mut job = prepare(&written.path, tiny_config(40));
        job.run(&CancelToken::default(), &mut |_| {}).unwrap();
        (product(&job), gs_core::io::write_ply(&job.splats()))
    };
    let (a, b) = (run(), run());
    assert!(a.0 == b.0 && a.1 == b.1, "two runs differ");
    // Another seed is another run.
    let mut config = tiny_config(40);
    config.train.seed = 2;
    let mut other = prepare(&written.path, config);
    other.run(&CancelToken::default(), &mut |_| {}).unwrap();
    assert!(other.trainer.model != a.0.last);
    let _ = std::fs::remove_dir_all(dir);
}

/// Films `cameras` in the house and writes the capture; `edit` may change
/// each frame. Returns the frames and what the phone truly saw.
fn film(path: &Path, cameras: &[Camera], sim: PhoneSim, mut edit: impl FnMut(usize, &mut CaptureFrame)) -> (common::ingest_sim::CaptureWorld, Vec<RgbdFrame>) {
    let snap = make_scene(SceneKind::House).snapshot(0.0);
    let mut phone = Phone::new(sim, &cameras[0]);
    let world = phone.world;
    let header = GscapHeader {
        protocol: Some(1),
        device: "simulateur".into(),
        app: "test".into(),
        mode: "room".into(),
        name: "Essai".into(),
        created: "2026-11-14T12:00:00Z".into(),
        region: None,
        floor: Some(world.floor_y()),
    };
    let file = std::fs::File::create(path).expect("cannot create the capture");
    let mut writer = GscapWriter::new(BufWriter::new(file), &header, gs_core::ingest::file::HEADER_PAD).expect("header");
    let mut truths = Vec::new();
    for (i, cam) in cameras.iter().enumerate() {
        let shot = phone.shoot(&snap, None, cam, i as f64 / 10.0);
        let mut frame = shot.frame;
        edit(i, &mut frame);
        writer.append(&encode_frame(&frame).expect("frame")).expect("write");
        truths.push(shot.truth);
    }
    (world, truths)
}

/// PSNR of `model` (viewer coordinates of a job) against the true picture
/// of `cam` (scene coordinates), over the pixels where the start cloud is
/// closed: what the capture can be held responsible for.
fn truth_psnr(job: &RefineJob, world: &common::ingest_sim::CaptureWorld, model: &Gaussians, start: &Gaussians, cam: &Camera, width: u32) -> f32 {
    let snap = make_scene(SceneKind::House).snapshot(0.0);
    let cam = cam.resized(width, width * 3 / 4);
    let truth = gs_core::capture::render_reference(&snap, &cam, true, 1, [18, 20, 28]);
    let draw = |m: &Gaussians| {
        let scene = world.to_scene(&m.to_splats(), job.info.floor_y);
        let mut out = Rendered::default();
        CpuRasterizer::default().forward(&Gaussians::from_splats(&scene, 1.0, 1.0, 0.0), &View::from_camera(&cam, [18.0 / 255.0, 20.0 / 255.0, 28.0 / 255.0]), &mut out);
        out
    };
    let (picture, closed) = (draw(model), draw(start));
    let (mut sum, mut n) = (0.0f64, 0usize);
    for i in 0..closed.alpha.len() {
        if closed.alpha[i] < 0.9 {
            continue;
        }
        for c in 0..3 {
            let d = picture.color[i * 3 + c] as f64 - truth.rgb[i * 3 + c] as f64 / 255.0;
            sum += d * d;
            n += 1;
        }
    }
    assert!(n > closed.alpha.len(), "the view is mostly holes");
    (10.0 * (n as f64 / sum.max(1e-12)).log10()) as f32
}

#[test]
fn captures_with_very_few_frames_do_no_harm() {
    let dir = scratch("few");
    let sim = small_sim();
    let eye = Vec3::new(-2.0, 1.4, 2.0);
    let target = Vec3::new(-4.6, 0.7, 3.2);
    let side = (target - eye).cross(Vec3::Y).normalize();
    for frames in [1usize, 2, 3] {
        // One, two or three photographs 12 cm apart.
        let cameras: Vec<Camera> = (0..frames).map(|i| sim.look_at(eye + side * 0.12 * i as f32, target)).collect();
        let path = dir.join(format!("few{frames}.gscap"));
        let (world, _) = film(&path, &cameras, sim, |_, _| {});
        let mut config = tiny_config(120);
        config.train.width = 128;
        let mut job = prepare(&path, config);
        assert_eq!((job.info.selection.train.len(), job.info.selection.holdout.len()), (frames, 0));
        let start = job.trainer.model.clone();
        let outcome = job.run(&CancelToken::default(), &mut |_| {}).unwrap();
        assert!(outcome.completed);
        let report = job.report();
        // Nothing was held out: no score, and the result is the final state.
        assert!(report.refined.is_none() && report.fusion.is_none() && report.result_iteration == 120);
        let result = job.result().0.clone();
        assert!(result.data.iter().flatten().all(|v| v.is_finite()));
        // At the photographs' own poses the fit is better; a step to the
        // side, where nothing constrained it, it must not fall apart.
        let at_pose = (truth_psnr(&job, &world, &start, &start, &cameras[0], 128), truth_psnr(&job, &world, &result, &start, &cameras[0], 128));
        let beside = sim.look_at(eye - side * 0.3 + Vec3::Y * 0.15, target);
        let novel = (truth_psnr(&job, &world, &start, &start, &beside, 128), truth_psnr(&job, &world, &result, &start, &beside, 128));
        eprintln!("{frames} frame(s): at the pose {:.2} -> {:.2} dB, 30 cm beside {:.2} -> {:.2} dB, {} -> {} Gaussians", at_pose.0, at_pose.1, novel.0, novel.1, start.len(), result.len());
        assert!(at_pose.1 > at_pose.0 + 1.0, "{frames} frames: no gain at the pose itself");
        assert!(novel.1 > novel.0 - 0.5, "{frames} frames: the novel view fell from {:.2} to {:.2} dB", novel.0, novel.1);
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_camera_that_barely_moves_does_no_harm() {
    // Forty frames from a hand that trembles: within a centimeter and a
    // fraction of a degree. The selection keeps a handful; the optimiser has
    // no parallax to learn geometry from and must leave it alone.
    let dir = scratch("still");
    let sim = small_sim();
    let eye = Vec3::new(-2.0, 1.4, 2.0);
    let target = Vec3::new(-4.6, 0.7, 3.2);
    let cameras: Vec<Camera> = (0..40)
        .map(|i| {
            let t = i as f32;
            let wobble = Vec3::new((0.9 * t).sin(), (1.3 * t).cos(), (0.7 * t).sin()) * 0.004;
            sim.look_at(eye + wobble, target + wobble * 3.0)
        })
        .collect();
    let path = dir.join("still.gscap");
    let (world, _) = film(&path, &cameras, sim, |_, _| {});
    let mut config = tiny_config(150);
    config.train.width = 128;
    let mut job = prepare(&path, config);
    let s = &job.info.selection;
    assert!(s.dropped_duplicate >= 30 && s.train.len() <= 8, "{} training views, {} repeats dropped", s.train.len(), s.dropped_duplicate);
    // All forty frames still feed the fusion.
    assert!(s.fusion.len() + s.holdout.len() >= s.train.len());
    let start = job.trainer.model.clone();
    assert!(job.run(&CancelToken::default(), &mut |_| {}).unwrap().completed);
    let result = job.result().0.clone();
    let side = (target - eye).cross(Vec3::Y).normalize();
    let beside = sim.look_at(eye - side * 0.3 + Vec3::Y * 0.15, target);
    let at_pose = (truth_psnr(&job, &world, &start, &start, &cameras[0], 128), truth_psnr(&job, &world, &result, &start, &cameras[0], 128));
    let novel = (truth_psnr(&job, &world, &start, &start, &beside, 128), truth_psnr(&job, &world, &result, &start, &beside, 128));
    eprintln!("still camera: at the pose {:.2} -> {:.2} dB, 30 cm beside {:.2} -> {:.2} dB", at_pose.0, at_pose.1, novel.0, novel.1);
    assert!(at_pose.1 > at_pose.0 + 1.0);
    assert!(novel.1 > novel.0 - 0.5, "the novel view fell from {:.2} to {:.2} dB", novel.0, novel.1);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn depth_missing_over_large_areas_is_survived() {
    // A third of every depth map has no value (a window, a mirror, a black
    // sofa), one frame in five has no depth at all and one has zero
    // confidence everywhere.
    let dir = scratch("holes");
    let written = write_capture(&dir.join("a.gscap"), "bedroom", 36, small_sim(), |i, frame| {
        let (w, h) = (frame.depth_width as usize, frame.depth_height as usize);
        for y in h / 4..h * 3 / 4 {
            for x in w / 5..w * 3 / 5 {
                frame.depth_mm[y * w + x] = 0;
            }
        }
        if i % 5 == 2 {
            frame.depth_mm.fill(0);
        }
        if i == 7 {
            if let Some(c) = frame.confidence.as_mut() {
                c.fill(0);
            }
        }
    });
    let mut job = prepare(&written.path, tiny_config(150));
    assert!(job.info.fused_splats > 1000);
    assert!(job.run(&CancelToken::default(), &mut |_| {}).unwrap().completed);
    let report = job.report();
    assert!(report.curve.iter().all(|p| p.loss.is_finite()));
    assert!(job.result().0.data.iter().flatten().all(|v| v.is_finite()));
    let (start, refined) = (report.start.unwrap(), report.refined.unwrap());
    assert!(refined.psnr > start.psnr, "held-out PSNR {:.2} -> {:.2} dB", start.psnr, refined.psnr);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn files_cut_short_are_used_as_far_as_they_go() {
    let dir = scratch("cut");
    let written = write_capture(&dir.join("a.gscap"), "bedroom", 20, small_sim(), |_, _| {});
    let bytes = std::fs::read(&written.path).unwrap();
    let whole = prepare(&written.path, tiny_config(10));
    assert!(!whole.info.truncated && whole.info.frames == 20);

    // Cut inside the last record, inside a record length, and after the
    // third frame.
    let third = whole.capture.frames[3].offset as usize;
    for (cut, frames) in [(bytes.len() - 200, 19), (third - 2, 3), (third - 4, 3)] {
        let path = dir.join("cut.gscap");
        std::fs::write(&path, &bytes[..cut]).unwrap();
        let mut job = prepare(&path, tiny_config(10));
        assert_eq!((job.info.frames, job.info.truncated), (frames, cut != third - 4), "cut at {cut}");
        assert!(job.run(&CancelToken::default(), &mut |_| {}).unwrap().completed);
    }
    // Nothing but the header, half a header, an empty file: errors, not
    // panics, and no job.
    let first = whole.capture.frames[0].offset as usize - 4;
    for cut in [first, first / 2, 12, 5, 0] {
        let path = dir.join("stub.gscap");
        std::fs::write(&path, &bytes[..cut]).unwrap();
        match RefineJob::prepare(&path, tiny_config(10), &CancelToken::default(), &mut |_| {}) {
            Err(RefineError::Empty(_) | RefineError::Dataset(_)) => {}
            Err(other) => panic!("cut at {cut}: {other}"),
            Ok(_) => panic!("cut at {cut}: a job from nothing"),
        }
    }
    // A frame whose bytes are damaged in the middle of the file.
    let mut damaged = bytes.clone();
    let at = whole.capture.frames[5].offset as usize;
    damaged[at..at + 4].copy_from_slice(b"XXXX");
    std::fs::write(dir.join("bad.gscap"), &damaged).unwrap();
    assert!(matches!(RefineJob::prepare(&dir.join("bad.gscap"), tiny_config(10), &CancelToken::default(), &mut |_| {}), Err(RefineError::Dataset(_))));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn the_written_ply_is_the_cloud_the_optimiser_drew() {
    let dir = scratch("ply");
    let written = write_capture(&dir.join("a.gscap"), "bedroom", 30, small_sim(), |_, _| {});
    let mut job = prepare(&written.path, tiny_config(120));
    job.run(&CancelToken::default(), &mut |_| {}).unwrap();
    let splats = job.splats();
    // Through both file formats and back: the same splats.
    let ply = gs_core::io::read_ply(&gs_core::io::write_ply(&splats)).unwrap();
    assert_eq!(ply.len(), splats.len());
    let mut worst = (0u8, 0.0f32, 0.0f32);
    for (a, b) in ply.iter().zip(&splats) {
        for k in 0..4 {
            worst.0 = worst.0.max(a.rgba[k].abs_diff(b.rgba[k]));
        }
        for k in 0..3 {
            worst.1 = worst.1.max((a.pos[k] - b.pos[k]).abs());
            worst.2 = worst.2.max((a.scale[k] / b.scale[k] - 1.0).abs());
        }
    }
    assert!(worst.0 <= 1 && worst.1 == 0.0 && worst.2 < 1e-4, "the .ply changes the splats: {worst:?}");

    // Drawn by the viewer's reference renderer from the file, against the
    // optimiser's own picture of its float parameters, from training and
    // held-out poses.
    let background = [18u8, 20, 28];
    let mut low = f32::INFINITY;
    for view in job.trainer.views.iter().step_by(5).chain(job.holdout()) {
        let cam = view.cam.resized(160, 120);
        let viewer = rasterize(&ply, &cam, &RasterOptions { background, classic_dilation: true, ..Default::default() });
        let mut mine = Rendered::default();
        CpuRasterizer::default().forward(job.result().0, &View::from_camera(&cam, background.map(|v| v as f32 / 255.0)), &mut mine);
        let psnr = gs_core::raster::psnr(&viewer, &mine.to_image());
        low = low.min(psnr);
        // The same picture also where it matters to the eye: no pixel far off.
        let far_off = viewer.rgb.iter().zip(&mine.to_image().rgb).filter(|(a, b)| a.abs_diff(**b) > 24).count();
        assert!(far_off * 500 < viewer.rgb.len(), "{far_off} values differ by more than 24 levels");
    }
    eprintln!("viewer against optimiser, worst view: {low:.1} dB");
    assert!(low > 38.0, "the viewer and the optimiser disagree: {low:.1} dB");
    let _ = std::fs::remove_dir_all(dir);
}

// ---------------------------------------------------------------------------
// The command-line tool under Ctrl-C
// ---------------------------------------------------------------------------

fn tool() -> Command {
    Command::new(env!("CARGO_BIN_EXE_gs-refine"))
}

fn interrupt(pid: u32) {
    let status = Command::new("kill").args(["-INT", &pid.to_string()]).status().expect("kill");
    assert!(status.success());
}

/// Starts a refinement, sends Ctrl-C once `marker` shows on its progress
/// output, and returns its exit code and what it printed.
fn run_and_interrupt(args: &[&str], marker: &str) -> (Option<i32>, String) {
    let mut child = tool().args(args).stderr(Stdio::piped()).stdout(Stdio::null()).spawn().expect("the tool starts");
    let mut stderr = child.stderr.take().unwrap();
    let mut seen = String::new();
    let mut buffer = [0u8; 256];
    let mut sent = false;
    loop {
        let n = stderr.read(&mut buffer).expect("stderr");
        if n == 0 {
            break;
        }
        seen.push_str(&String::from_utf8_lossy(&buffer[..n]));
        if !sent && seen.contains(marker) {
            interrupt(child.id());
            sent = true;
        }
    }
    let status = child.wait().expect("wait");
    assert!(sent, "the marker {marker:?} never showed:\n{seen}");
    (status.code(), seen)
}

#[cfg(unix)]
#[test]
fn ctrl_c_stops_cleanly_and_the_run_resumes_to_the_same_file() {
    let dir = scratch("ctrlc");
    let written = write_capture(&dir.join("a.gscap"), "bedroom", 24, small_sim(), |_, _| {});
    let capture = written.path.to_str().unwrap().to_string();
    let common_args = ["--preset", "rapide", "--iterations", "400", "--largeur", "96", "--threads", "2", "--evaluation", "6", "--splat"];
    let path = |name: &str| -> PathBuf { dir.join(name) };

    // The uninterrupted run.
    let whole = path("whole.ply");
    let output = tool().arg(&capture).args(["-o", whole.to_str().unwrap()]).args(common_args).output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(!path("whole.reprise").exists(), "a finished run leaves no checkpoint");

    // Ctrl-C while optimising: status 130, a result, a report, a checkpoint.
    let stopped = path("stopped.ply");
    let mut args = vec![capture.as_str(), "-o", stopped.to_str().unwrap()];
    args.extend(common_args);
    let (code, text) = run_and_interrupt(&args, "Optimisation : 1");
    assert_eq!(code, Some(130), "{text}");
    assert!(!text.contains("panicked") && text.contains("--reprise"), "{text}");
    assert!(stopped.exists() && path("stopped.json").exists() && path("stopped.reprise").exists() && path("stopped.splat").exists());
    let partial = gs_core::io::read_ply(&std::fs::read(&stopped).unwrap()).unwrap();
    assert!(partial.len() > 1000);
    let report: gs_refine::job::Report = serde_json::from_str(&std::fs::read_to_string(path("stopped.json")).unwrap()).unwrap();
    assert!(!report.completed && report.iterations_done > 0 && report.iterations_done < 400, "stopped at {}", report.iterations_done);

    // Resume twice: once interrupted again, once to the end.
    let reprise = path("stopped.reprise");
    let (code, text) = run_and_interrupt(&["--reprise", reprise.to_str().unwrap(), "--splat"], "Optimisation : ");
    assert!(code == Some(130) || code == Some(0), "{text}");
    if code == Some(130) {
        let output = tool().args(["--reprise", reprise.to_str().unwrap(), "--splat"]).output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }
    assert!(!reprise.exists(), "the checkpoint of a finished run is removed");
    assert!(std::fs::read(&stopped).unwrap() == std::fs::read(&whole).unwrap(), "the resumed run wrote another .ply than the uninterrupted one");
    assert!(std::fs::read(path("stopped.splat")).unwrap() == std::fs::read(path("whole.splat")).unwrap());
    let report: gs_refine::job::Report = serde_json::from_str(&std::fs::read_to_string(path("stopped.json")).unwrap()).unwrap();
    assert!(report.completed && report.iterations_done == 400);

    // Ctrl-C before the optimisation has begun: an error message, no panic,
    // no half-written result.
    let early = path("early.ply");
    let mut args = vec![capture.as_str(), "-o", early.to_str().unwrap()];
    args.extend(common_args);
    let (code, text) = run_and_interrupt(&args, "Lecture");
    assert!(code == Some(1) || code == Some(130), "{code:?}: {text}");
    assert!(!text.contains("panicked"), "{text}");
    if code == Some(1) {
        assert!(!early.exists());
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn mistyped_options_are_refused_before_anything_runs() {
    // A misspelt option must not be dropped in silence: the run it starts
    // is not the one that was asked for, and it can last an hour.
    let dir = scratch("typo");
    let written = write_capture(&dir.join("a.gscap"), "bedroom", 8, small_sim(), |_, _| {});
    let capture = written.path.to_str().unwrap();
    let out = dir.join("out.ply");
    let out = out.to_str().unwrap();
    for (args, expected) in [
        (vec![capture, "-o", out, "--iteration", "5"], "--iteration"),
        (vec![capture, "-o", out, "--largeur=96"], "--largeur=96"),
        (vec![capture, "-o", out, "--iterations"], "--iterations"),
        (vec![capture, "-o", out, "--preset", "rapide", "autre.gscap"], "autre.gscap"),
        (vec!["info", capture, "--vue", "3"], "--vue"),
        (vec!["export", capture, "--format", "colmap", "-o", out, "--poses", "--bogus"], "--bogus"),
    ] {
        let output = tool().args(&args).output().unwrap();
        let text = String::from_utf8_lossy(&output.stderr).to_string();
        assert_eq!(output.status.code(), Some(1), "{args:?}: {text}");
        assert!(text.contains(expected) && text.starts_with("gs-refine : "), "{args:?}: {text}");
        assert!(!text.contains('\u{2014}'), "an em dash in {text}");
    }
    assert!(!dir.join("out.ply").exists() && !dir.join("out.reprise").exists());
    let _ = std::fs::remove_dir_all(dir);
}
