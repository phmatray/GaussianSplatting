//! An offline refinement as a job: prepare, run, report, checkpoint.
//!
//! ```no_run
//! use gs_refine::job::{CancelToken, Preset, RefineConfig, RefineJob};
//! let cancel = CancelToken::default();
//! let mut job = RefineJob::prepare("salon.gscap".as_ref(), RefineConfig::preset(Preset::Standard), &cancel, &mut |_| {}).unwrap();
//! job.run(&cancel, &mut |p| eprintln!("{} / {}", p.iteration, p.iterations)).unwrap();
//! std::fs::write("salon.ply", gs_core::io::write_ply(&job.splats())).unwrap();
//! ```
//!
//! A host (the command-line tool, a server route, a button in the
//! application) runs [`RefineJob::run`] on a thread of its own, reads
//! [`Progress`] values from the callback and stops the job through the
//! [`CancelToken`]. A stopped job is still a valid result: [`RefineJob::splats`]
//! returns the best state seen, and a checkpoint lets a later process go on
//! with [`RefineJob::resume`].

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use gs_core::experimental::wblend::{self, Layer, WblendOptions};
use gs_core::raster::Image;
use gs_core::Splat;
use serde::{Deserialize, Serialize};

use crate::dataset::{select_views, views_of, Capture, DatasetError, RegionMask, SelectParams, Selection, ViewCache, ViewInfo};
use crate::init::{fuse, InitParams};
use crate::loss::{fit_correction, scene_mask};
use crate::model::{Gaussians, PARAMS};
use crate::raster::{CpuRasterizer, Rasterizer, Rendered, View};
use crate::train::{DensifyStat, Moments, TrainConfig, Trainer, ViewState};

/// Background of evaluation pictures (the one of the project's quality
/// tests), `0..=255`.
pub const EVAL_BACKGROUND: [u8; 3] = [18, 20, 28];

/// Asks a running job to stop. Cheap to clone and to hand to a signal
/// handler or another thread.
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Time budgets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Preset {
    Rapide,
    Standard,
    Fin,
}

impl Preset {
    pub const ALL: [Self; 3] = [Self::Rapide, Self::Standard, Self::Fin];

    pub fn name(self) -> &'static str {
        match self {
            Self::Rapide => "rapide",
            Self::Standard => "standard",
            Self::Fin => "fin",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|p| p.name() == name)
    }
}

/// Everything that defines a job, apart from the capture.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RefineConfig {
    /// `train.iterations` of `0` means "as many as the capture needs":
    /// [`RefineConfig::passes`] times the number of training views, at
    /// least [`RefineConfig::min_iterations`].
    pub train: TrainConfig,
    pub passes: f32,
    pub min_iterations: u32,
    pub select: SelectParams,
    pub init: InitParams,
    /// Worker threads (`0`: one per processor).
    pub threads: usize,
    /// Memory for decoded photographs, megabytes.
    pub cache_mb: usize,
    /// Held-out views scored at each evaluation, at most.
    pub eval_views: usize,
    /// Evaluate on the held-out views this many times during the run (the
    /// start and the end are always scored).
    pub evals: u32,
    /// Seconds between two checkpoints (when a checkpoint path is set).
    pub checkpoint_every_s: f32,
}

impl Default for RefineConfig {
    fn default() -> Self {
        Self::preset(Preset::Standard)
    }
}

impl RefineConfig {
    /// The settings of a time budget. See the crate documentation for what
    /// each costs and gains.
    pub fn preset(preset: Preset) -> Self {
        let (passes, min_iterations, width, max_gaussians, max_views) = match preset {
            Preset::Rapide => (6.0, 1_500, 480, 1_000_000, 300),
            Preset::Standard => (15.0, 4_000, 640, 2_000_000, 600),
            Preset::Fin => (30.0, 12_000, 960, 4_000_000, 1_200),
        };
        // A room fused at 1.6 cm instead of the real-time 2.4 cm gave +0.8 dB
        // near the phone's path for a quarter more time per iteration; the
        // quick preset keeps the coarser start.
        let room_voxel_size = if preset == Preset::Rapide { 0.0 } else { 0.016 };
        Self {
            train: TrainConfig { iterations: 0, width, max_gaussians, ..TrainConfig::default() },
            passes,
            min_iterations,
            select: SelectParams { max_views, ..SelectParams::default() },
            init: InitParams { room_voxel_size, ..InitParams::default() },
            threads: 0,
            cache_mb: 1024,
            eval_views: 12,
            evals: 4,
            checkpoint_every_s: 300.0,
        }
    }
}

/// Why a job cannot start or go on.
#[derive(Debug)]
pub enum RefineError {
    Dataset(DatasetError),
    Io(std::io::Error),
    /// The checkpoint is damaged or belongs to another capture.
    Checkpoint(String),
    /// Nothing to optimise: no usable view, or an empty fused cloud.
    Empty(&'static str),
    /// Stopped through the [`CancelToken`] before the optimisation began.
    Cancelled,
}

impl fmt::Display for RefineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dataset(e) => write!(f, "{e}"),
            Self::Io(e) => write!(f, "{e}"),
            Self::Checkpoint(why) => write!(f, "checkpoint: {why}"),
            Self::Empty(what) => f.write_str(what),
            Self::Cancelled => f.write_str("cancelled"),
        }
    }
}

impl std::error::Error for RefineError {}

impl From<DatasetError> for RefineError {
    fn from(e: DatasetError) -> Self {
        Self::Dataset(e)
    }
}

impl From<std::io::Error> for RefineError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

/// What a job is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    /// Reading the capture and choosing the views.
    Select,
    /// Building the starting cloud with the real-time fusion.
    Fuse,
    /// Measuring the exposure of every view against the starting cloud.
    Exposure,
    /// Registering every view on the cloud (pose refinement).
    Poses,
    Evaluate,
    Optimise,
}

/// A progress report.
#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Progress {
    pub stage: Stage,
    /// Within `Select`, `Fuse`, `Exposure` and `Poses`: frames done and
    /// to do.
    pub done: u64,
    pub total: u64,
    pub iteration: u32,
    pub iterations: u32,
    /// Smoothed loss.
    pub loss: f32,
    pub gaussians: usize,
    /// Width of the pictures being optimised right now.
    pub width: u32,
    /// Seconds spent optimising so far (all sessions of a resumed job).
    pub elapsed_s: f64,
    /// Estimated seconds left.
    pub eta_s: Option<f64>,
    /// Latest held-out PSNR, dB.
    pub psnr: Option<f32>,
}

/// Scores against the held-out photographs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct EvalScore {
    /// Mean PSNR over the views after fitting a gain and an offset per
    /// channel to each photograph (its exposure is its own), dB.
    pub psnr: f32,
    /// The same without the fit.
    pub psnr_raw: f32,
    /// Mean SSIM of the luma, after the fit.
    pub ssim: f32,
    pub views: usize,
}

/// One point of the loss curve.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct CurvePoint {
    pub iteration: u32,
    pub wall_s: f64,
    pub cpu_s: f64,
    pub loss: f32,
    pub gaussians: usize,
    pub width: u32,
}

/// One evaluation during the run.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct EvalPoint {
    pub iteration: u32,
    pub wall_s: f64,
    pub score: EvalScore,
}

/// What the job knows about its capture and its start.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CaptureInfo {
    pub path: String,
    pub name: String,
    pub mode: String,
    pub bytes: u64,
    pub frames: usize,
    pub truncated: bool,
    pub selection: Selection,
    /// Height of the floor in capture coordinates; the output has it at 0.
    pub floor_y: f32,
    pub floor_known: bool,
    /// Region of interest in output coordinates: center and radius.
    pub region: Option<([f32; 3], f32)>,
    pub fused_splats: usize,
    pub voxel_size: f32,
}

/// The JSON report written next to the result.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub config: RefineConfig,
    pub capture: CaptureInfo,
    pub threads: usize,
    pub iterations_done: u32,
    pub completed: bool,
    /// Seconds spent optimising (wall clock and processor time).
    pub wall_s: f64,
    pub cpu_s: f64,
    /// Seconds spent before the optimisation: selection, fusion.
    pub prepare_s: f64,
    pub gaussians: usize,
    /// Held-out scores: the fused cloud as the viewer shows a capture (its
    /// normalised blending), the starting point of the optimiser, and the
    /// result. `None` without held-out views.
    pub fusion: Option<EvalScore>,
    pub start: Option<EvalScore>,
    pub refined: Option<EvalScore>,
    /// Iteration whose state is the result (the best on the held-out views).
    pub result_iteration: u32,
    pub evals: Vec<EvalPoint>,
    pub curve: Vec<CurvePoint>,
}

/// How a run ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// `false` when the job was stopped before its last iteration.
    pub completed: bool,
    pub iterations_done: u32,
}

/// An offline refinement.
pub struct RefineJob {
    pub config: RefineConfig,
    pub capture: Capture,
    pub info: CaptureInfo,
    pub trainer: Trainer<CpuRasterizer>,
    holdout: Vec<ViewInfo>,
    fused: Vec<Splat>,
    pool: rayon::ThreadPool,
    cache: ViewCache,
    checkpoint: Option<PathBuf>,
    wall_s: f64,
    cpu_s: f64,
    prepare_s: f64,
    loss_ema: f32,
    curve: Vec<CurvePoint>,
    evals: Vec<EvalPoint>,
    fusion_score: Option<EvalScore>,
    best: Option<(f32, u32, Gaussians)>,
    completed: bool,
}

fn build_pool(threads: usize) -> Result<rayon::ThreadPool, RefineError> {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("gs-refine-{i}"))
        .build()
        .map_err(|e| RefineError::Io(std::io::Error::other(e.to_string())))
}

/// PSNR in dB of a mean squared error on `0..=1` values.
fn psnr_of(mse: f64) -> f32 {
    if mse <= 0.0 {
        return 99.0;
    }
    (10.0 * (1.0 / mse).log10()).min(99.0) as f32
}

/// Scores one rendering (`0..=1` floats) against a decoded held-out view.
/// Returns PSNR with the fitted correction, PSNR without, SSIM.
fn score_view(rendered: &[f32], data: &crate::dataset::ViewData) -> (f32, f32, f32) {
    let target = data.target();
    let pixels = (data.width * data.height) as usize;
    let mask = scene_mask(&target);
    let correction = fit_correction(rendered, target.rgb, mask.as_deref());
    let (mut raw, mut fitted, mut n) = (0.0f64, 0.0f64, 0usize);
    let mut shown = Image { width: data.width, height: data.height, rgb: target.rgb.to_vec() };
    for i in 0..pixels {
        if mask.as_ref().is_some_and(|m| !m[i]) {
            continue;
        }
        n += 1;
        for c in 0..3 {
            let truth = target.rgb[i * 3 + c] as f64 / 255.0;
            let plain = rendered[i * 3 + c].clamp(0.0, 1.0);
            let corrected = (correction.gain[c] * rendered[i * 3 + c] + correction.offset[c]).clamp(0.0, 1.0);
            raw += (plain as f64 - truth).powi(2);
            fitted += (corrected as f64 - truth).powi(2);
            shown.rgb[i * 3 + c] = (corrected * 255.0 + 0.5) as u8;
        }
    }
    if n == 0 {
        return (99.0, 99.0, 1.0);
    }
    // Outside the mask both pictures hold the photograph: no difference.
    let reference = Image { width: data.width, height: data.height, rgb: target.rgb.to_vec() };
    let ssim = gs_core::experimental::metrics::ssim(&shown, &reference);
    (psnr_of(fitted / (3 * n) as f64), psnr_of(raw / (3 * n) as f64), ssim)
}

impl RefineJob {
    /// Reads the capture, chooses the views and builds the starting cloud.
    /// `progress` is called from this thread; cancelling returns
    /// [`RefineError::Cancelled`].
    pub fn prepare(capture_path: &Path, mut config: RefineConfig, cancel: &CancelToken, progress: &mut dyn FnMut(&Progress)) -> Result<Self, RefineError> {
        let start = Instant::now();
        let pool = build_pool(config.threads)?;
        let capture = Capture::open(capture_path)?;
        let mut report = Progress {
            stage: Stage::Select,
            done: 0,
            total: capture.frames.len() as u64,
            iteration: 0,
            iterations: config.train.iterations,
            loss: 0.0,
            gaussians: 0,
            width: 0,
            elapsed_s: 0.0,
            eta_s: None,
            psnr: None,
        };
        progress(&report);
        let selection = pool.install(|| select_views(&capture, &config.select));
        if selection.train.is_empty() {
            return Err(RefineError::Empty("no usable view in the capture"));
        }
        if cancel.is_cancelled() {
            return Err(RefineError::Cancelled);
        }
        if config.train.iterations == 0 {
            config.train.iterations = ((config.passes.max(0.0) * selection.train.len() as f32) as u32).max(config.min_iterations).max(1);
        }

        report.stage = Stage::Fuse;
        report.total = selection.fusion.len() as u64;
        // On the calling thread: the fusion is serial for the most part, and
        // the callback need not be `Send`.
        let fused = fuse(&capture, &selection.fusion, &config.init, |done, _| {
            report.done = done as u64;
            progress(&report);
            !cancel.is_cancelled()
        })?;
        let Some(fused) = fused else { return Err(RefineError::Cancelled) };
        if fused.splats.is_empty() {
            return Err(RefineError::Empty("the fusion produced no splat from this capture"));
        }
        let region = fused.region.map(|r| RegionMask::new(&r, fused.floor_y, capture.mode));
        let model = Gaussians::from_splats(&fused.splats, config.init.splat_scale, config.init.opacity, 0.1 * fused.voxel_size * 0.45);
        let views = views_of(&capture, &selection.train, fused.floor_y);
        let holdout = views_of(&capture, &selection.holdout, fused.floor_y);
        let info = CaptureInfo {
            path: capture_path.display().to_string(),
            name: capture.header.name.clone(),
            mode: capture.mode.name().to_string(),
            bytes: capture.bytes,
            frames: capture.frames.len(),
            truncated: capture.truncated,
            selection,
            floor_y: fused.floor_y,
            floor_known: fused.floor_known,
            region: region.map(|r| (r.center.to_array(), r.radius)),
            fused_splats: fused.splats.len(),
            voxel_size: fused.voxel_size,
        };
        let trainer = Trainer::new(config.train.clone(), model, views, region, CpuRasterizer::default());
        Ok(Self {
            cache: ViewCache::new(config.cache_mb.max(16) << 20),
            config,
            capture,
            info,
            trainer,
            holdout,
            fused: fused.splats,
            pool,
            checkpoint: None,
            wall_s: 0.0,
            cpu_s: 0.0,
            prepare_s: start.elapsed().as_secs_f64(),
            loss_ema: 0.0,
            curve: Vec::new(),
            evals: Vec::new(),
            fusion_score: None,
            best: None,
            completed: false,
        })
    }

    /// Writes a checkpoint to `path` every [`RefineConfig::checkpoint_every_s`]
    /// seconds while running, and when the run ends or is stopped.
    pub fn checkpoint_to(&mut self, path: Option<PathBuf>) {
        self.checkpoint = path;
    }

    /// Number of worker threads.
    pub fn threads(&self) -> usize {
        self.pool.current_num_threads()
    }

    /// The fused cloud the optimisation started from (viewer coordinates).
    pub fn fused(&self) -> &[Splat] {
        &self.fused
    }

    /// The views held out for evaluation.
    pub fn holdout(&self) -> &[ViewInfo] {
        &self.holdout
    }

    /// The Gaussians that are the result: the best state on the held-out
    /// views if one was recorded, the current state otherwise.
    pub fn result(&self) -> (&Gaussians, u32) {
        match &self.best {
            Some((_, iteration, model)) => (model, *iteration),
            None => (&self.trainer.model, self.trainer.iteration),
        }
    }

    /// The result as viewer splats, for `gs_core::io::write_ply` and
    /// `write_splat`.
    pub fn splats(&self) -> Vec<Splat> {
        self.result().0.to_splats()
    }

    /// Width and views of an evaluation.
    fn eval_plan(&self) -> (u32, Vec<ViewInfo>) {
        let n = self.holdout.len();
        let take = self.config.eval_views.min(n);
        let views = (0..take).map(|k| self.holdout[k * n / take.max(1)]).collect();
        (self.config.train.width, views)
    }

    /// Scores `draw(view, width, height) -> colour floats` on the held-out
    /// views.
    fn evaluate_with(&self, mut draw: impl FnMut(&ViewInfo, u32, u32) -> Vec<f32>) -> Result<Option<EvalScore>, RefineError> {
        let (width, views) = self.eval_plan();
        if views.is_empty() {
            return Ok(None);
        }
        let region = self.trainer.region;
        let mut sum = EvalScore::default();
        for view in &views {
            let width = width.min(view.cam.width);
            // Not through the cache: evaluations are rare and must not push
            // training views out.
            let data = crate::dataset::decode_view(&self.capture.frame(view.frame)?, &view.cam, width, region.as_ref())?;
            let rendered = draw(view, data.width, data.height);
            let (psnr, raw, ssim) = score_view(&rendered, &data);
            sum.psnr += psnr;
            sum.psnr_raw += raw;
            sum.ssim += ssim;
            sum.views += 1;
        }
        let n = sum.views as f32;
        Ok(Some(EvalScore { psnr: sum.psnr / n, psnr_raw: sum.psnr_raw / n, ssim: sum.ssim / n, views: sum.views }))
    }

    /// Scores a model on the held-out photographs, drawn as the viewer
    /// draws an imported splat file.
    pub fn evaluate(&self, model: &Gaussians) -> Result<Option<EvalScore>, RefineError> {
        let background = EVAL_BACKGROUND.map(|v| v as f32 / 255.0);
        let mut raster = CpuRasterizer::default();
        let mut out = Rendered::default();
        self.evaluate_with(|view, width, height| {
            let cam = view.cam.resized(width, height);
            self.pool.install(|| raster.forward(model, &View::from_camera(&cam, background), &mut out));
            out.color.clone()
        })
    }

    /// Scores the fused cloud the way a viewer shows a capture session: the
    /// normalised blending of `docs/ARCHITECTURE.md` with soft silhouettes.
    pub fn evaluate_fusion(&self) -> Result<Option<EvalScore>, RefineError> {
        self.evaluate_with(|view, width, height| {
            let cam = view.cam.resized(width, height);
            let layers = [Layer { splats: &self.fused, weight: 1.0 }];
            let image = self.pool.install(|| wblend::render(&layers, &cam, &WblendOptions::normalized_soft())).over(EVAL_BACKGROUND);
            image.rgb.iter().map(|&v| v as f32 / 255.0).collect()
        })
    }

    /// Registers every view on the cloud (when poses are optimised). The
    /// time counts as optimisation time.
    fn align_poses(&mut self, cancel: &CancelToken, progress: &mut dyn FnMut(&Progress)) -> Result<(), RefineError> {
        if !self.config.train.pose || self.config.train.pose_align_steps == 0 {
            return Ok(());
        }
        let (wall, cpu) = (Instant::now(), cpu_time::ProcessTime::now());
        let mut report = self.progress_now(Stage::Poses, 0);
        report.total = self.trainer.views.len() as u64;
        let (iteration, steps) = (self.trainer.iteration, self.config.train.pose_align_steps);
        for index in 0..self.trainer.views.len() {
            if cancel.is_cancelled() {
                break;
            }
            if index % 4 == 0 {
                report.done = index as u64;
                progress(&report);
            }
            let (trainer, capture, cache) = (&mut self.trainer, &self.capture, &mut self.cache);
            self.pool.install(|| trainer.align_pose(index, iteration, capture, cache, steps))?;
        }
        self.wall_s += wall.elapsed().as_secs_f64();
        self.cpu_s += cpu.elapsed().as_secs_f64();
        Ok(())
    }

    fn progress_now(&self, stage: Stage, width: u32) -> Progress {
        let (it, total) = (self.trainer.iteration, self.config.train.iterations);
        let eta = (it > 0 && it < total).then(|| self.wall_s / it as f64 * (total - it) as f64);
        Progress {
            stage,
            done: 0,
            total: 0,
            iteration: it,
            iterations: total,
            loss: self.loss_ema,
            gaussians: self.trainer.model.len(),
            width,
            elapsed_s: self.wall_s,
            eta_s: eta,
            psnr: self.evals.last().map(|e| e.score.psnr),
        }
    }

    fn record_eval(&mut self, score: Option<EvalScore>) {
        let Some(score) = score else { return };
        let iteration = self.trainer.iteration;
        self.evals.retain(|e| e.iteration != iteration);
        self.evals.push(EvalPoint { iteration, wall_s: self.wall_s, score });
        if self.best.as_ref().is_none_or(|(best, ..)| score.psnr > *best) {
            self.best = Some((score.psnr, iteration, self.trainer.model.clone()));
        }
    }

    /// Runs the optimisation to its last iteration, or until cancelled.
    /// Safe to call again after a stop: it goes on where it was.
    pub fn run(&mut self, cancel: &CancelToken, progress: &mut dyn FnMut(&Progress)) -> Result<Outcome, RefineError> {
        self.run_to(u32::MAX, cancel, progress)
    }

    /// [`Self::run`] that also stops once `stop_at` iterations are done
    /// (a pause: the schedules still refer to the whole run).
    pub fn run_to(&mut self, stop_at: u32, cancel: &CancelToken, progress: &mut dyn FnMut(&Progress)) -> Result<Outcome, RefineError> {
        let total = self.config.train.iterations;
        if self.trainer.iteration == 0 && self.evals.is_empty() {
            if self.config.train.exposure {
                let mut report = self.progress_now(Stage::Exposure, 0);
                report.total = self.trainer.views.len() as u64;
                for index in 0..self.trainer.views.len() {
                    if cancel.is_cancelled() {
                        break;
                    }
                    if index % 8 == 0 {
                        report.done = index as u64;
                        progress(&report);
                    }
                    let (trainer, capture, cache) = (&mut self.trainer, &self.capture, &mut self.cache);
                    self.pool.install(|| trainer.prefit_exposure(index, capture, cache))?;
                }
                self.trainer.settle_exposures();
            }
            self.align_poses(cancel, progress)?;
            progress(&self.progress_now(Stage::Evaluate, 0));
            if self.fusion_score.is_none() {
                self.fusion_score = self.evaluate_fusion()?;
            }
            let score = self.evaluate(&self.trainer.model)?;
            self.record_eval(score);
        }
        let eval_every = if self.config.evals > 0 { (total / (self.config.evals + 1)).max(1) } else { u32::MAX };
        let curve_every = (total / 200).max(1);
        let (mut last_report, mut last_checkpoint) = (Instant::now(), Instant::now());
        let mut width = 0;
        while self.trainer.iteration < total.min(stop_at) && !cancel.is_cancelled() {
            let native = self.trainer.views.first().map_or(0, |v| v.cam.width);
            if self.trainer.iteration > 0 && self.config.train.level_starts(self.trainer.iteration, native) {
                // The cloud is sharper than at the start: register again.
                self.align_poses(cancel, progress)?;
            }
            let (wall, cpu) = (Instant::now(), cpu_time::ProcessTime::now());
            let (trainer, capture, cache) = (&mut self.trainer, &self.capture, &mut self.cache);
            let step = self.pool.install(|| trainer.step(capture, cache))?;
            self.wall_s += wall.elapsed().as_secs_f64();
            self.cpu_s += cpu.elapsed().as_secs_f64();
            let loss = step.loss.total as f32;
            let it = self.trainer.iteration;
            if step.width != width {
                // The previous level's pictures are of no further use, and
                // its losses do not compare with those of this one.
                self.cache.keep_only(step.width);
                width = step.width;
                self.loss_ema = loss;
            }
            self.loss_ema = if it <= 1 || self.loss_ema == 0.0 { loss } else { 0.98 * self.loss_ema + 0.02 * loss };
            if it.is_multiple_of(curve_every) || it == total {
                self.curve.push(CurvePoint { iteration: it, wall_s: self.wall_s, cpu_s: self.cpu_s, loss: self.loss_ema, gaussians: self.trainer.model.len(), width });
            }
            if it.is_multiple_of(eval_every) && it < total {
                let score = self.evaluate(&self.trainer.model)?;
                self.record_eval(score);
            }
            if last_report.elapsed().as_secs_f32() >= 0.5 || it == total {
                last_report = Instant::now();
                progress(&self.progress_now(Stage::Optimise, width));
            }
            if self.checkpoint.is_some() && last_checkpoint.elapsed().as_secs_f32() >= self.config.checkpoint_every_s.max(1.0) {
                last_checkpoint = Instant::now();
                self.save_checkpoint()?;
            }
        }
        self.completed = self.trainer.iteration >= total;
        // The final state competes with the best one seen on the way.
        progress(&self.progress_now(Stage::Evaluate, width));
        let score = self.evaluate(&self.trainer.model)?;
        self.record_eval(score);
        if self.checkpoint.is_some() {
            self.save_checkpoint()?;
        }
        progress(&self.progress_now(Stage::Optimise, width));
        Ok(Outcome { completed: self.completed, iterations_done: self.trainer.iteration })
    }

    /// The report of the job so far.
    pub fn report(&self) -> Report {
        let (model, result_iteration) = self.result();
        let at = |iteration: u32| self.evals.iter().find(|e| e.iteration == iteration).map(|e| e.score);
        Report {
            config: self.config.clone(),
            capture: self.info.clone(),
            threads: self.threads(),
            iterations_done: self.trainer.iteration,
            completed: self.completed,
            wall_s: self.wall_s,
            cpu_s: self.cpu_s,
            prepare_s: self.prepare_s,
            gaussians: model.len(),
            fusion: self.fusion_score,
            start: at(0),
            refined: at(result_iteration).filter(|_| result_iteration > 0),
            result_iteration,
            evals: self.evals.clone(),
            curve: self.curve.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Checkpoints
// ---------------------------------------------------------------------------

const CHECKPOINT_MAGIC: [u8; 8] = *b"GSRFCK1\0";
/// Floats of one view's state in a checkpoint.
const VIEW_FLOATS: usize = 6 + 9 + 3 + 1;

/// The JSON part of a checkpoint.
#[derive(Serialize, Deserialize)]
struct CheckpointHeader {
    config: RefineConfig,
    info: CaptureInfo,
    iteration: u32,
    gaussians: usize,
    views: usize,
    wall_s: f64,
    cpu_s: f64,
    prepare_s: f64,
    loss_ema: f32,
    curve: Vec<CurvePoint>,
    evals: Vec<EvalPoint>,
    fusion_score: Option<EvalScore>,
    fused_splats: usize,
}

fn view_floats(state: &ViewState) -> [f32; VIEW_FLOATS] {
    let mut out = [0.0f32; VIEW_FLOATS];
    let mut put = |at: &mut usize, values: &[f32]| {
        out[*at..*at + values.len()].copy_from_slice(values);
        *at += values.len();
    };
    let mut at = 0;
    put(&mut at, &state.correction.gain);
    put(&mut at, &state.correction.offset);
    put(&mut at, &state.rot.to_cols_array());
    put(&mut at, &state.trans.to_array());
    put(&mut at, &[state.steps as f32]);
    out
}

fn view_from_floats(f: &[f32]) -> ViewState {
    let take = |from: usize, n: usize| &f[from..from + n];
    ViewState {
        correction: crate::loss::ColorCorrection { gain: take(0, 3).try_into().expect("three"), offset: take(3, 3).try_into().expect("three") },
        rot: glam::Mat3::from_cols_slice(take(6, 9)),
        trans: glam::Vec3::from_slice(take(15, 3)),
        steps: f[18] as u32,
    }
}

fn floats_to_bytes(values: &[f32], out: &mut Vec<u8>) {
    for v in values {
        out.extend_from_slice(&v.to_le_bytes());
    }
}

struct ByteReader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl ByteReader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], RefineError> {
        let end = self.at.checked_add(n).filter(|&e| e <= self.bytes.len()).ok_or_else(|| RefineError::Checkpoint("file cut short".into()))?;
        let slice = &self.bytes[self.at..end];
        self.at = end;
        Ok(slice)
    }

    fn floats(&mut self, n: usize) -> Result<Vec<f32>, RefineError> {
        let bytes = self.take(n.checked_mul(4).ok_or_else(|| RefineError::Checkpoint("size overflow".into()))?)?;
        Ok(bytes.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect())
    }
}

impl RefineJob {
    /// The whole state of the job as bytes: a JSON header, then the raw
    /// arrays (model, moments, densification statistics, view states, fused
    /// cloud), little-endian.
    pub fn checkpoint_bytes(&self) -> Vec<u8> {
        let t = &self.trainer;
        let header = CheckpointHeader {
            config: self.config.clone(),
            info: self.info.clone(),
            iteration: t.iteration,
            gaussians: t.model.len(),
            views: t.view_state.len(),
            wall_s: self.wall_s,
            cpu_s: self.cpu_s,
            prepare_s: self.prepare_s,
            loss_ema: self.loss_ema,
            curve: self.curve.clone(),
            evals: self.evals.clone(),
            fusion_score: self.fusion_score,
            fused_splats: self.fused.len(),
        };
        let json = serde_json::to_vec(&header).expect("the header serialises");
        let mut out = Vec::with_capacity(16 + json.len() + t.model.len() * PARAMS * 12 + self.fused.len() * 44);
        out.extend_from_slice(&CHECKPOINT_MAGIC);
        out.extend_from_slice(&(json.len() as u32).to_le_bytes());
        out.extend_from_slice(&json);
        for g in &t.model.data {
            floats_to_bytes(g, &mut out);
        }
        for m in &t.moments {
            floats_to_bytes(&m.m, &mut out);
            floats_to_bytes(&m.v, &mut out);
        }
        for s in &t.stats {
            floats_to_bytes(&[s.grad_sum, s.views, s.radius_px], &mut out);
        }
        for v in &t.view_state {
            floats_to_bytes(&view_floats(v), &mut out);
        }
        out.extend_from_slice(bytemuck::cast_slice(&self.fused));
        out
    }

    /// Writes the checkpoint to the path given to [`Self::checkpoint_to`]
    /// (through a temporary file, so that a crash never leaves half of one).
    pub fn save_checkpoint(&self) -> Result<(), RefineError> {
        let Some(path) = &self.checkpoint else { return Ok(()) };
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, self.checkpoint_bytes())?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Restores a job from a checkpoint. The capture is looked for at
    /// `capture_path`, or where it was when the checkpoint was written.
    /// The best state seen before the stop is not part of a checkpoint: the
    /// resumed job judges from its own evaluations.
    pub fn resume(checkpoint: &Path, capture_path: Option<&Path>) -> Result<Self, RefineError> {
        let bad = |why: &str| RefineError::Checkpoint(why.to_string());
        let bytes = std::fs::read(checkpoint)?;
        let mut reader = ByteReader { bytes: &bytes, at: 0 };
        if reader.take(8)? != CHECKPOINT_MAGIC {
            return Err(bad("not a gs-refine checkpoint"));
        }
        let json_length = u32::from_le_bytes(reader.take(4)?.try_into().expect("four bytes")) as usize;
        let header: CheckpointHeader = serde_json::from_slice(reader.take(json_length)?).map_err(|e| RefineError::Checkpoint(e.to_string()))?;
        let path = capture_path.map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from(&header.info.path));
        let capture = Capture::open(&path)?;
        if capture.frames.len() != header.info.frames || capture.bytes != header.info.bytes {
            return Err(bad("the capture is not the one this checkpoint was made from"));
        }
        let n = header.gaussians;
        if n > bytes.len() / (PARAMS * 4) || header.views > bytes.len() / (VIEW_FLOATS * 4) {
            return Err(bad("sizes do not fit the file"));
        }
        let rows = |flat: Vec<f32>| -> Vec<[f32; PARAMS]> { flat.chunks_exact(PARAMS).map(|c| c.try_into().expect("a full row")).collect() };
        let model = Gaussians { data: rows(reader.floats(n * PARAMS)?) };
        let moment_rows = reader.floats(n * PARAMS * 2)?;
        let moments: Vec<Moments<PARAMS>> = moment_rows
            .chunks_exact(PARAMS * 2)
            .map(|c| Moments { m: c[..PARAMS].try_into().expect("a full row"), v: c[PARAMS..].try_into().expect("a full row") })
            .collect();
        let stats: Vec<DensifyStat> = reader.floats(n * 3)?.chunks_exact(3).map(|c| DensifyStat { grad_sum: c[0], views: c[1], radius_px: c[2] }).collect();
        let view_state: Vec<ViewState> = reader.floats(header.views * VIEW_FLOATS)?.chunks_exact(VIEW_FLOATS).map(view_from_floats).collect();
        let fused_bytes = reader.take(header.fused_splats.checked_mul(std::mem::size_of::<Splat>()).ok_or_else(|| bad("size overflow"))?)?;
        let fused: Vec<Splat> = fused_bytes.chunks_exact(std::mem::size_of::<Splat>()).map(bytemuck::pod_read_unaligned).collect();

        let views = views_of(&capture, &header.info.selection.train, header.info.floor_y);
        let holdout = views_of(&capture, &header.info.selection.holdout, header.info.floor_y);
        if views.len() != view_state.len() {
            return Err(bad("the views of the capture changed"));
        }
        let region = header.info.region.map(|(center, radius)| RegionMask {
            center: glam::Vec3::from_array(center),
            radius,
            use_person_mask: capture.mode == gs_core::protocol::CaptureMode::Person,
        });
        let config = header.config;
        let mut trainer = Trainer::new(config.train.clone(), model, views, region, CpuRasterizer::default());
        trainer.moments = moments;
        trainer.stats = stats;
        trainer.view_state = view_state;
        trainer.iteration = header.iteration;
        Ok(Self {
            pool: build_pool(config.threads)?,
            cache: ViewCache::new(config.cache_mb.max(16) << 20),
            config,
            capture,
            info: header.info,
            trainer,
            holdout,
            fused,
            checkpoint: Some(checkpoint.to_path_buf()),
            wall_s: header.wall_s,
            cpu_s: header.cpu_s,
            prepare_s: header.prepare_s,
            loss_ema: header.loss_ema,
            curve: header.curve,
            evals: header.evals,
            fusion_score: header.fusion_score,
            best: None,
            completed: false,
        })
    }
}
