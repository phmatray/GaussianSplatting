//! Offline work on a kept capture: the refinement of `gs_refine` and the
//! export of a capture as a dataset for other trainers.
//!
//! One job at a time, on a thread of its own: an optimisation keeps its
//! processor cores busy for minutes to hours, far too long for the blocking
//! pool of the async runtime, and it must leave the pipeline and the
//! listeners a core to breathe ([`RefineSetup::threads`]). The job runs in
//! this process rather than in a child: the desktop application is one
//! binary, and the optimiser checks its [`CancelToken`] at every iteration
//! and every sixteen frames of its preparation, which makes a stop a matter
//! of seconds.
//!
//! * A refinement writes `<capture stem>-<preset>.ply` (INRIA 3DGS) into
//!   the directory of [`Kind::Refined`], and next to it a `.json` that
//!   says where it comes from, with the optimiser's own report inside.
//! * A cancelled refinement keeps nothing. A run stopped in its first
//!   hundreds of iterations is worse than the real-time fusion, and there
//!   is no telling a user which side of that line a stop fell on.
//! * A dataset export writes a directory `<capture stem>-<format>` under
//!   the datasets directory; the interface shows where it is.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gs_refine::dataset::Capture;
use gs_refine::export::{export_dataset, DatasetFormat};
use gs_refine::job::{CancelToken, Preset, Progress, RefineConfig, RefineError, RefineJob, Stage};
use serde::Serialize;

use crate::store::{self, Kind, RefinedEntry, RefinedMeta, Store};

/// A refinement is not started with less than this free where its result
/// goes: a result is tens of megabytes, and a disk that full has other
/// troubles coming.
const MIN_FREE_BYTES: u64 = 256 << 20;

/// What a job does.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    /// Optimises Gaussians against the photographs of the capture.
    Refine,
    /// Writes the capture as a COLMAP or nerfstudio dataset.
    Export,
}

/// The job in progress, as `GET /api/refine` reports it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct JobStatus {
    /// Grows with every job of this run of the server.
    pub id: u64,
    pub kind: JobKind,
    /// File name of the capture (`<stem>.gscap`).
    pub capture: String,
    /// What the user called the capture.
    pub title: String,
    /// `rapide`, `standard` or `fin` (a refinement).
    pub preset: Option<String>,
    /// `colmap` or `nerfstudio` (an export).
    pub format: Option<String>,
    /// `prepare`, `select`, `fuse`, `exposure`, `poses`, `evaluate`,
    /// `optimise`, `write` or `export`.
    pub stage: &'static str,
    /// Within the stages that walk through frames: frames done and to do.
    pub done: u64,
    pub total: u64,
    /// Iterations done and planned (`optimise`).
    pub iteration: u32,
    pub iterations: u32,
    /// Seconds since the job started.
    pub elapsed_s: f64,
    /// The optimiser's estimate of the seconds left, once it has one.
    pub eta_s: Option<f64>,
    /// Smoothed loss of the optimiser (not comparable between levels).
    pub loss: Option<f32>,
    /// Latest PSNR on the photographs held out, dB. They are neighbours of
    /// the training photographs: this is a sign of life, not the quality of
    /// the result from another viewpoint.
    pub holdout_psnr: Option<f32>,
    pub gaussians: u64,
    /// Width of the pictures being optimised, pixels.
    pub width: u32,
    /// Worker threads of the job.
    pub threads: usize,
    /// A stop was asked for and the job has not ended yet.
    pub cancelling: bool,
    /// Start of the job, RFC 3339.
    pub started: String,
}

/// How a job ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Done,
    /// Stopped on request: nothing was kept.
    Cancelled,
    Failed,
}

/// Why a job failed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct JobError {
    /// `bad_capture`, `no_views`, `disk_full`, `disk_error` or `internal`.
    pub code: &'static str,
    /// English sentence for logs and developers.
    pub message: String,
}

/// An exported dataset.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct DatasetInfo {
    /// Name of its directory.
    pub name: String,
    /// Where it is on this machine.
    pub dir: String,
    pub format: String,
    pub images: usize,
    pub points: usize,
    pub depth_maps: usize,
    pub masks: usize,
}

/// The latest job that ended, as `GET /api/refine` reports it.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct JobEnd {
    pub id: u64,
    pub kind: JobKind,
    pub capture: String,
    pub title: String,
    pub preset: Option<String>,
    pub format: Option<String>,
    pub outcome: Outcome,
    pub error: Option<JobError>,
    /// The refined result that was written.
    pub result: Option<RefinedEntry>,
    /// The dataset that was written.
    pub dataset: Option<DatasetInfo>,
    pub iterations_done: u32,
    pub iterations: u32,
    pub elapsed_s: f64,
}

/// `GET /api/refine`.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct RefineState {
    /// The job in progress.
    pub job: Option<JobStatus>,
    /// The latest job that ended since the server started.
    pub last: Option<JobEnd>,
}

/// Why a job was not started.
#[derive(Debug)]
pub(crate) enum Refusal {
    /// Another job is running.
    Running(Box<JobStatus>),
    /// The server has no directory for the outcome.
    NoDirectory,
    /// No such capture among the kept ones.
    NotFound,
    /// The capture is still being written.
    InUse,
    /// A `live` capture: a moving scene has no single cloud to refine.
    Unsupported,
    /// Not enough room for the outcome.
    DiskFull { free: u64 },
    Disk(io::Error),
}

/// What a job is asked to do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Task {
    Refine {
        preset: Preset,
        /// `TrainConfig::photo_beyond_sensor`.
        beyond_sensor: bool,
    },
    Export(DatasetFormat),
}

/// How jobs are run.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RefineSetup {
    /// Worker threads of a job; 0: every processor but one.
    pub threads: usize,
    /// Iterations of every refinement, whatever its preset (tests and
    /// demonstrations); `None`: what the preset and the capture call for.
    pub iterations: Option<u32>,
}

struct Running {
    status: JobStatus,
    cancel: CancelToken,
    started: Instant,
}

#[derive(Default)]
struct Inner {
    running: Option<Running>,
    last: Option<JobEnd>,
    jobs: u64,
}

/// The one job slot of the server.
pub(crate) struct Refiner {
    store: Arc<Store>,
    setup: RefineSetup,
    inner: Mutex<Inner>,
}

fn format_name(format: DatasetFormat) -> &'static str {
    match format {
        DatasetFormat::Colmap => "colmap",
        DatasetFormat::Nerfstudio => "nerfstudio",
    }
}

fn stage_name(stage: Stage) -> &'static str {
    match stage {
        Stage::Select => "select",
        Stage::Fuse => "fuse",
        Stage::Exposure => "exposure",
        Stage::Poses => "poses",
        Stage::Evaluate => "evaluate",
        Stage::Optimise => "optimise",
    }
}

fn now_rfc3339() -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
    gs_core::record::rfc3339_utc(now)
}

fn disk_error(err: &io::Error) -> JobError {
    let code = if err.kind() == io::ErrorKind::StorageFull { "disk_full" } else { "disk_error" };
    JobError { code, message: err.to_string() }
}

fn job_error(err: &RefineError) -> JobError {
    match err {
        RefineError::Io(err) => disk_error(err),
        RefineError::Dataset(gs_refine::dataset::DatasetError::Io(err)) => disk_error(err),
        RefineError::Dataset(err) => JobError { code: "bad_capture", message: err.to_string() },
        RefineError::Empty(what) => JobError { code: "no_views", message: (*what).to_string() },
        RefineError::Checkpoint(why) => JobError { code: "internal", message: why.clone() },
        RefineError::Cancelled => JobError { code: "internal", message: "cancelled".to_string() },
    }
}

/// How a job thread ended, before it is turned into a [`JobEnd`].
enum Ended {
    Refined { entry: RefinedEntry, iterations: u32 },
    Exported(DatasetInfo),
    Cancelled,
    Failed(JobError),
}

impl Refiner {
    pub fn new(store: Arc<Store>, setup: RefineSetup) -> Arc<Self> {
        Arc::new(Self { store, setup, inner: Mutex::new(Inner::default()) })
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn threads(&self) -> usize {
        match self.setup.threads {
            0 => std::thread::available_parallelism().map_or(1, |n| n.get()).saturating_sub(1).max(1),
            n => n,
        }
    }

    /// The job in progress and the latest that ended.
    pub fn state(&self) -> RefineState {
        let inner = self.inner();
        let job = inner.running.as_ref().map(|running| {
            let mut status = running.status.clone();
            status.elapsed_s = running.started.elapsed().as_secs_f64();
            status.cancelling = running.cancel.is_cancelled();
            status
        });
        RefineState { job, last: inner.last.clone() }
    }

    /// Starts `task` on the kept capture `file`. Answers at once; the work
    /// goes on on a thread of its own.
    pub fn start(self: &Arc<Self>, file: &str, task: Task) -> Result<JobStatus, Refusal> {
        let path = self.store.existing(Kind::Capture, file).ok_or(Refusal::NotFound)?;
        // A capture its phone has finished is complete on disk although its
        // file is still held: it is read, not written to.
        if self.store.is_growing(Kind::Capture, file) {
            return Err(Refusal::InUse);
        }
        let header = store::peek_capture_header(&path).unwrap_or_default();
        if header.mode == "live" {
            return Err(Refusal::Unsupported);
        }
        let target = match task {
            Task::Refine { .. } => self.store.dir(Kind::Refined),
            Task::Export(_) => self.store.datasets_dir(),
        }
        .ok_or(Refusal::NoDirectory)?
        .to_path_buf();
        std::fs::create_dir_all(&target).map_err(Refusal::Disk)?;
        let needed = match task {
            Task::Refine { .. } => MIN_FREE_BYTES,
            // The photographs are copied: about the size of the capture.
            Task::Export(_) => std::fs::metadata(&path).map_or(0, |meta| meta.len()).saturating_add(MIN_FREE_BYTES),
        };
        if let Some(free) = store::free_bytes(&target).filter(|free| *free < needed) {
            return Err(Refusal::DiskFull { free });
        }

        let stem = file.strip_suffix(store::CAPTURE_EXT).unwrap_or(file).to_string();
        let title = if header.name.trim().is_empty() { stem.clone() } else { header.name.clone() };
        let (preset, format) = match task {
            Task::Refine { preset, .. } => (Some(preset.name().to_string()), None),
            Task::Export(format) => (None, Some(format_name(format).to_string())),
        };
        let mut inner = self.inner();
        if let Some(running) = &inner.running {
            return Err(Refusal::Running(Box::new(running.status.clone())));
        }
        inner.jobs += 1;
        let status = JobStatus {
            id: inner.jobs,
            kind: if matches!(task, Task::Refine { .. }) { JobKind::Refine } else { JobKind::Export },
            capture: file.to_string(),
            title,
            preset,
            format,
            stage: "prepare",
            done: 0,
            total: 0,
            iteration: 0,
            iterations: 0,
            elapsed_s: 0.0,
            eta_s: None,
            loss: None,
            holdout_psnr: None,
            gaussians: 0,
            width: 0,
            threads: self.threads(),
            cancelling: false,
            started: now_rfc3339(),
        };
        let cancel = CancelToken::default();
        let this = self.clone();
        let (job_status, job_cancel, mode) = (status.clone(), cancel.clone(), header.mode.clone());
        let spawned = std::thread::Builder::new().name("splat-live-refine".to_string()).spawn(move || {
            let id = job_status.id;
            // A panic of the optimiser ends the job, not the server.
            let ended = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match task {
                Task::Refine { preset, beyond_sensor } => this.refine(id, &path, &stem, &job_status, &mode, preset, beyond_sensor, &job_cancel),
                Task::Export(format) => this.export(id, &path, &stem, format, &target, &job_cancel),
            }))
            .unwrap_or_else(|_| Ended::Failed(JobError { code: "internal", message: "the job stopped on an internal error".to_string() }));
            this.finish(id, ended);
        });
        if let Err(err) = spawned {
            return Err(Refusal::Disk(err));
        }
        log::info!("job {} started: {:?} of {file}", status.id, status.kind);
        inner.running = Some(Running { status: status.clone(), cancel, started: Instant::now() });
        Ok(status)
    }

    /// Asks the job in progress to stop. `None`: there is none.
    pub fn cancel(&self) -> Option<JobStatus> {
        let inner = self.inner();
        let running = inner.running.as_ref()?;
        running.cancel.cancel();
        let mut status = running.status.clone();
        status.elapsed_s = running.started.elapsed().as_secs_f64();
        status.cancelling = true;
        Some(status)
    }

    /// Stops the job in progress and waits for its thread to let go, for
    /// `grace` at most (the server is shutting down).
    pub fn stop(&self, grace: Duration) {
        if self.cancel().is_none() {
            return;
        }
        let deadline = Instant::now() + grace;
        while self.inner().running.is_some() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Changes the status of job `id`, if it is still the one running.
    fn update(&self, id: u64, change: impl FnOnce(&mut JobStatus)) {
        let mut inner = self.inner();
        if let Some(running) = inner.running.as_mut().filter(|running| running.status.id == id) {
            change(&mut running.status);
        }
    }

    fn on_progress(&self, id: u64, progress: &Progress) {
        self.update(id, |status| {
            status.stage = stage_name(progress.stage);
            status.done = progress.done;
            status.total = progress.total;
            status.iteration = progress.iteration;
            status.iterations = progress.iterations;
            status.eta_s = progress.eta_s.filter(|eta| eta.is_finite() && *eta >= 0.0);
            status.loss = Some(progress.loss).filter(|loss| loss.is_finite() && progress.iteration > 0);
            status.holdout_psnr = progress.psnr.filter(|psnr| psnr.is_finite());
            status.gaussians = progress.gaussians as u64;
            status.width = progress.width;
        });
    }

    fn finish(&self, id: u64, ended: Ended) {
        let mut inner = self.inner();
        let Some(running) = inner.running.take_if(|running| running.status.id == id) else { return };
        let status = running.status;
        let (outcome, error, result, dataset, result_iterations) = match ended {
            Ended::Refined { entry, iterations } => (Outcome::Done, None, Some(entry), None, Some(iterations)),
            Ended::Exported(info) => (Outcome::Done, None, None, Some(info), None),
            Ended::Cancelled => (Outcome::Cancelled, None, None, None, None),
            Ended::Failed(error) => (Outcome::Failed, Some(error), None, None, None),
        };
        match &error {
            Some(error) => log::error!("la tâche {id} sur {} a échoué ({}) : {}", status.capture, error.code, error.message),
            None => log::info!("job {id} on {} ended: {outcome:?}", status.capture),
        }
        inner.last = Some(JobEnd {
            id,
            kind: status.kind,
            capture: status.capture,
            title: status.title,
            preset: status.preset,
            format: status.format,
            outcome,
            error,
            result,
            dataset,
            iterations_done: result_iterations.unwrap_or(status.iteration),
            iterations: status.iterations,
            elapsed_s: running.started.elapsed().as_secs_f64(),
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn refine(&self, id: u64, capture: &Path, stem: &str, status: &JobStatus, mode: &str, preset: Preset, beyond_sensor: bool, cancel: &CancelToken) -> Ended {
        let mut config = RefineConfig::preset(preset);
        config.threads = self.threads();
        config.train.photo_beyond_sensor = beyond_sensor;
        if let Some(iterations) = self.setup.iterations {
            config.train.iterations = iterations.max(1);
        }
        let mut job = match RefineJob::prepare(capture, config, cancel, &mut |progress| self.on_progress(id, progress)) {
            Ok(job) => job,
            Err(RefineError::Cancelled) => return Ended::Cancelled,
            Err(err) => return Ended::Failed(job_error(&err)),
        };
        let outcome = match job.run(cancel, &mut |progress| self.on_progress(id, progress)) {
            Ok(outcome) => outcome,
            Err(RefineError::Cancelled) => return Ended::Cancelled,
            Err(err) => return Ended::Failed(job_error(&err)),
        };
        // A stop is a stop: half a result is not offered as an improvement.
        if !outcome.completed || cancel.is_cancelled() {
            return Ended::Cancelled;
        }
        self.update(id, |status| status.stage = "write");
        let splats = job.splats();
        let report = job.report();
        // A refinement gains most near the phone's path: a room is first
        // looked at from where the phone started, toward the bulk of what
        // it filmed (as a capture session proposes its own start view).
        let view = (mode == "room").then(|| job.trainer.views.first()).flatten().map(|first| {
            let eye = first.cam.position();
            let count = splats.len().max(1) as f32;
            let centroid = splats.iter().fold([0.0f32; 3], |sum, s| [sum[0] + s.pos[0] / count, sum[1] + s.pos[1] / count, sum[2] + s.pos[2] / count]);
            let into_room = [centroid[0], eye.y - 0.3, centroid[2]];
            let far_enough = (into_room[0] - eye.x).hypot(into_room[2] - eye.z) > 0.75;
            let ahead = (eye + first.cam.forward() * 2.0).to_array();
            let target = if far_enough { into_room } else { ahead };
            // Millimeters: the same digits in every answer that carries it.
            [eye.x, eye.y, eye.z, target[0], target[1], target[2]].map(|c| (f64::from(c) * 1000.0).round() / 1000.0)
        });
        let meta = RefinedMeta {
            capture: status.capture.clone(),
            title: status.title.clone(),
            mode: mode.to_string(),
            preset: preset.name().to_string(),
            created: now_rfc3339(),
            gaussians: splats.len() as u64,
            iterations: report.result_iteration,
            elapsed_s: report.wall_s,
            beyond_sensor,
            view: view.filter(|view| view.iter().all(|c| c.is_finite())),
            psnr: report.refined.as_ref().map(|s| f64::from(s.psnr)).filter(|v| v.is_finite()),
            ssim: report.refined.as_ref().map(|s| f64::from(s.ssim)).filter(|v| v.is_finite()),
        };
        match self.write_result(&format!("{stem}-{}", preset.name()), &gs_core::io::write_ply(&splats), &meta, &report) {
            Ok(entry) => Ended::Refined { entry, iterations: outcome.iterations_done },
            Err(err) => Ended::Failed(disk_error(&err)),
        }
    }

    /// Writes a result under `<stem>.ply` (or the next free name) with its
    /// description beside it. The file appears under its name whole.
    fn write_result(&self, stem: &str, ply: &[u8], meta: &RefinedMeta, report: &gs_refine::job::Report) -> io::Result<RefinedEntry> {
        use std::io::Write;
        let (part, mut file) = self.store.create_part(Kind::Refined)?;
        let written = file.write_all(ply).and_then(|()| file.flush());
        drop(file);
        let adopted = written.and_then(|()| self.store.adopt(Kind::Refined, &part, stem));
        let file = match adopted {
            Ok(file) => file,
            Err(err) => {
                let _ = std::fs::remove_file(&part);
                return Err(err);
            }
        };
        let path = self.store.existing(Kind::Refined, &file).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "the result vanished"))?;
        let mut described = serde_json::to_value(meta).map_err(io::Error::other)?;
        described["report"] = serde_json::to_value(report).map_err(io::Error::other)?;
        // The result is there; a description that cannot be written costs
        // its details in the listing, not the result.
        if let Err(err) = std::fs::write(path.with_extension(&store::REFINED_META_EXT[1..]), format!("{described:#}\n")) {
            log::warn!("la description de {file} n'a pas pu être écrite : {err}");
        }
        self.store.refined(&file).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "the result vanished"))
    }

    fn export(&self, id: u64, capture: &Path, stem: &str, format: DatasetFormat, datasets: &Path, cancel: &CancelToken) -> Ended {
        self.update(id, |status| status.stage = "export");
        let capture = match Capture::open(capture) {
            Ok(capture) => capture,
            Err(err) => return Ended::Failed(job_error(&err.into())),
        };
        let (name, dir) = match new_dataset_dir(datasets, &format!("{stem}-{}", format_name(format))) {
            Ok(created) => created,
            Err(err) => return Ended::Failed(disk_error(&err)),
        };
        // The views and the starting points an optimisation would use.
        let config = RefineConfig::default();
        let exported = export_dataset(&capture, &dir, format, &config.select, &config.init, cancel, &mut |done, total| {
            self.update(id, |status| {
                status.done = done as u64;
                status.total = total as u64;
            });
        });
        let ended = match exported {
            // The export only looks at the token while it fuses.
            Ok(_) if cancel.is_cancelled() => Ended::Cancelled,
            Ok(summary) => Ended::Exported(DatasetInfo {
                name,
                dir: summary.dir.display().to_string(),
                format: format_name(format).to_string(),
                images: summary.images,
                points: summary.points,
                depth_maps: summary.depth_maps,
                masks: summary.masks,
            }),
            Err(RefineError::Cancelled) => Ended::Cancelled,
            Err(err) => Ended::Failed(job_error(&err)),
        };
        if !matches!(ended, Ended::Exported(_)) {
            // Created empty by this job a moment ago: nothing of the user's
            // is in it.
            let _ = std::fs::remove_dir_all(&dir);
        }
        ended
    }
}

/// Creates a new directory `<stem>` under `parent`, or `<stem>-2`,
/// `<stem>-3`… when the name is taken. Returns its name and its path.
fn new_dataset_dir(parent: &Path, stem: &str) -> io::Result<(String, PathBuf)> {
    for attempt in 1..=999u32 {
        let name = if attempt == 1 { stem.to_string() } else { format!("{stem}-{attempt}") };
        let dir = parent.join(&name);
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok((name, dir)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::new(io::ErrorKind::AlreadyExists, "no free directory name"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dataset_directories_never_reuse_a_name() {
        let root = tempfile::tempdir().unwrap();
        let (first, path) = new_dataset_dir(root.path(), "Salon-colmap").unwrap();
        assert_eq!(first, "Salon-colmap");
        assert!(path.is_dir());
        let (second, _) = new_dataset_dir(root.path(), "Salon-colmap").unwrap();
        assert_eq!(second, "Salon-colmap-2");
        std::fs::write(root.path().join("Salon-colmap-3"), b"a file in the way").unwrap();
        let (fourth, _) = new_dataset_dir(root.path(), "Salon-colmap").unwrap();
        assert_eq!(fourth, "Salon-colmap-4");
    }

    #[test]
    fn a_job_needs_a_capture_and_a_directory() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::new(Some(root.path().join("recordings")), None));
        let refiner = Refiner::new(store.clone(), RefineSetup::default());
        let task = Task::Refine { preset: Preset::Rapide, beyond_sensor: true };
        assert!(matches!(refiner.start("nothing.gscap", task), Err(Refusal::NotFound)));
        assert!(matches!(refiner.start("../x.gscap", task), Err(Refusal::NotFound)));
        assert!(refiner.cancel().is_none());
        assert_eq!(refiner.state(), RefineState::default());
        assert!(refiner.threads() >= 1);

        // A server without directories refuses politely.
        let bare = Arc::new(Store::new(None, Some(root.path().join("captures"))));
        std::fs::create_dir_all(root.path().join("captures")).unwrap();
        std::fs::write(root.path().join("captures/x.gscap"), b"GSCAP1\0\0").unwrap();
        let refiner = Refiner::new(bare, RefineSetup::default());
        assert!(matches!(refiner.start("x.gscap", task), Err(Refusal::NoDirectory)));
        assert!(matches!(refiner.start("x.gscap", Task::Export(DatasetFormat::Colmap)), Err(Refusal::NoDirectory)));
    }
}
