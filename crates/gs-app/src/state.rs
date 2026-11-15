//! Reactive state of the interface. Everything here is a signal, so the
//! handle is `Copy` and can be captured by any view or event handler; the
//! objects that are not reactive (viewer, transport) live in
//! [`crate::core::Core`].

use gs_core::protocol::{CaptureInfo, CaptureStats, PipelineConfig, ScanState, Stats};
use gs_core::scene::SceneKind;
use gs_render::{BlendStatus, NavMode, ViewerStats};
use gs_worker::{CaptureProgress, ReplayStatus};
use leptos::prelude::*;

use crate::logic::refine::{Job, JobEnd, KeptCapture, Refined, Storage};
use crate::logic::render::BlendChoice;
use crate::logic::source::{Link, ServerInfo, SourceKind};

/// Local display settings (never sent to the pipeline).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Display {
    pub nav: NavMode,
    /// Blending the viewer is asked for.
    pub blend: BlendChoice,
    pub splat_scale: f32,
    pub point_mode: bool,
    pub gizmos: bool,
    pub show_static: bool,
    pub show_dynamic: bool,
    /// House: ceiling clipped away, seen from above.
    pub dollhouse: bool,
    pub auto_rotate: bool,
}

impl Default for Display {
    fn default() -> Self {
        Self {
            nav: NavMode::Orbit,
            blend: BlendChoice::Auto,
            splat_scale: 1.0,
            point_mode: false,
            gizmos: true,
            show_static: true,
            show_dynamic: true,
            dollhouse: false,
            auto_rotate: false,
        }
    }
}

/// Scan progress as the interface shows it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScanView {
    pub state: ScanState,
    /// `0..=1`.
    pub progress: f32,
}

/// An imported file that replaces the live view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileInfo {
    pub name: String,
    pub splats: usize,
}

/// Where a replay reads its recording.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplayOrigin {
    /// A URL: the `file` parameter of a link, or a recording of the server.
    /// The value is what a share link would name.
    Url(String),
    /// A file the user opened.
    File,
    /// A recording made in memory during this visit, by its take id.
    Memory(u32),
}

/// The recording being replayed.
#[derive(Clone, Debug, PartialEq)]
pub struct ReplayView {
    /// Name shown while the file header has not been read.
    pub title: String,
    pub origin: ReplayOrigin,
    pub status: ReplayStatus,
}

impl ReplayView {
    /// Name of the recording: the one in its header, else the file's.
    pub fn name(&self) -> &str {
        if self.status.name.is_empty() { &self.title } else { &self.status.name }
    }
}

/// Where a finished recording lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TakePlace {
    /// In the memory of this page (a `Blob`), until the page is closed.
    Memory,
    /// In the recordings directory of the native server.
    Server,
}

/// A finished recording the interface lists.
#[derive(Clone, Debug, PartialEq)]
pub struct Take {
    /// Stable within the page: the key of the list.
    pub id: u32,
    pub place: TakePlace,
    pub title: String,
    /// File name (`danse.gsrec`).
    pub file: String,
    pub bytes: u64,
    pub duration_s: f64,
    pub scene: String,
}

/// What the share dialog is open on.
#[derive(Clone, Debug, PartialEq)]
pub enum Shared {
    /// A recording of the server.
    Take(Take),
    /// A refined result of the server: a still scene.
    Refined(Refined),
}

impl Shared {
    /// File name of what is shared.
    pub fn file(&self) -> &str {
        match self {
            Self::Take(take) => &take.file,
            Self::Refined(result) => &result.file,
        }
    }

    pub fn title(&self) -> &str {
        match self {
            Self::Take(take) => &take.title,
            Self::Refined(result) => &result.title,
        }
    }
}

/// A recording in progress.
#[derive(Clone, Debug, PartialEq)]
pub struct RecordingView {
    pub name: String,
    pub bytes: u64,
    /// The bound of an in-memory recording; `None` on the server's disk.
    pub max_bytes: Option<u64>,
    pub duration_s: f64,
}

/// A capture session of the phone companion (or of a capture file) as the
/// interface follows it.
#[derive(Clone, Debug, PartialEq)]
pub struct CaptureView {
    pub info: CaptureInfo,
    pub stats: Option<CaptureStats>,
}

/// The import of a capture file: read by the local worker, or sent to the
/// server.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ImportView {
    pub file: String,
    /// The file goes to the server: there is no frame-by-frame progress.
    pub upload: bool,
    pub progress: CaptureProgress,
}

/// The pairing code for the phone companion.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PairView {
    /// `data:` URL of the QR code of the pairing URI.
    pub qr: Option<String>,
    pub busy: bool,
    pub error: Option<String>,
}

/// A transient message at the bottom of the window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    pub error: bool,
    pub text: String,
}

/// Every signal of the interface.
#[derive(Clone, Copy)]
pub struct Ui {
    /// Which source is active; `None` while it is being resolved.
    pub source: RwSignal<Option<SourceKind>>,
    pub link: RwSignal<Link>,
    /// Scene of the last State message.
    pub scene: RwSignal<Option<SceneKind>>,
    /// Scene the user asked for and the pipeline has not confirmed yet.
    pub pending_scene: RwSignal<Option<SceneKind>>,
    /// Configuration echoed by the pipeline: the single source of truth for
    /// every control of the "Capture" section.
    pub config: RwSignal<Option<PipelineConfig>>,
    /// Incremented with every State message; optimistic control values are
    /// dropped when it changes.
    pub state_epoch: RwSignal<u32>,
    /// Ids of the cameras that have a thumbnail tile.
    pub camera_ids: RwSignal<Vec<u32>>,
    /// Latest pipeline statistics (refreshed a few times per second).
    pub stats: RwSignal<Option<Stats>>,
    /// Pipeline ticks per second, measured from the tick counter; `None`
    /// until a first second of statistics has been seen.
    pub pipeline_hz: RwSignal<Option<f32>>,
    pub scan: RwSignal<Option<ScanView>>,
    pub viewer_stats: RwSignal<ViewerStats>,
    /// Blending the viewer really uses, read back regularly: a fallback
    /// can come and go by itself.
    pub blend: RwSignal<BlendStatus>,
    /// Bytes received per second.
    pub transfer_rate: RwSignal<f32>,
    pub display: RwSignal<Display>,
    /// Thumbnails show depth instead of colour.
    pub preview_depth: RwSignal<bool>,
    pub xr_supported: RwSignal<Option<bool>>,
    pub xr_active: RwSignal<bool>,
    pub xr_error: RwSignal<Option<String>>,
    pub panel_open: RwSignal<bool>,
    pub ui_hidden: RwSignal<bool>,
    pub file: RwSignal<Option<FileInfo>>,
    pub file_error: RwSignal<Option<String>>,
    /// A file is being dragged over the window.
    pub dragging: RwSignal<bool>,
    pub notice: RwSignal<Option<Notice>>,
    /// The viewer could not start (no WebGL2).
    pub fatal: RwSignal<Option<String>>,
    /// Info of the native server, when it is the source.
    pub server: RwSignal<Option<ServerInfo>>,
    /// The page talks to the server over loopback, the only side from
    /// which LAN sharing can be switched.
    pub lan_admin: RwSignal<bool>,
    pub lan_busy: RwSignal<bool>,
    pub lan_error: RwSignal<Option<String>>,
    /// `data:` URL of the QR code of the LAN address.
    pub qr: RwSignal<Option<String>>,
    /// The page is the embedded viewer (`embed=1`): canvas and small
    /// playback controls only.
    pub embed: RwSignal<bool>,
    pub fullscreen: RwSignal<bool>,
    /// The recording on screen, when the source is a replay.
    pub replay: RwSignal<Option<ReplayView>>,
    /// The play head while the user drags the time line, seconds.
    pub scrub: RwSignal<Option<f64>>,
    pub recording: RwSignal<Option<RecordingView>>,
    /// A start or stop of the recorder is under way.
    pub record_busy: RwSignal<bool>,
    pub takes: RwSignal<Vec<Take>>,
    /// The recording or refined result whose share dialog is open.
    pub share: RwSignal<Option<Shared>>,
    /// Raw captures the native server keeps (loopback side only).
    pub captures: RwSignal<Vec<KeptCapture>>,
    /// Refined results the native server keeps.
    pub refined: RwSignal<Vec<Refined>>,
    /// The offline job running on the server.
    pub job: RwSignal<Option<Job>>,
    /// The latest job that ended, until it is dismissed.
    pub job_end: RwSignal<Option<JobEnd>>,
    /// A start or stop of a job is under way.
    pub refine_busy: RwSignal<bool>,
    /// File name of the capture whose refinement form is open.
    pub refining: RwSignal<Option<String>>,
    /// File name of the refined result being downloaded for display.
    pub refined_loading: RwSignal<Option<String>>,
    /// State of the server's disk, when it tells.
    pub storage: RwSignal<Option<Storage>>,
    pub capture: RwSignal<Option<CaptureView>>,
    pub import: RwSignal<Option<ImportView>>,
    pub pair: RwSignal<PairView>,
}

impl Ui {
    pub fn new(panel_open: bool) -> Self {
        Self {
            source: RwSignal::new(None),
            link: RwSignal::new(Link::Connecting),
            scene: RwSignal::new(None),
            pending_scene: RwSignal::new(None),
            config: RwSignal::new(None),
            state_epoch: RwSignal::new(0),
            camera_ids: RwSignal::new(Vec::new()),
            stats: RwSignal::new(None),
            pipeline_hz: RwSignal::new(None),
            scan: RwSignal::new(None),
            viewer_stats: RwSignal::new(ViewerStats::default()),
            blend: RwSignal::new(BlendStatus::default()),
            transfer_rate: RwSignal::new(0.0),
            display: RwSignal::new(Display::default()),
            preview_depth: RwSignal::new(false),
            xr_supported: RwSignal::new(None),
            xr_active: RwSignal::new(false),
            xr_error: RwSignal::new(None),
            panel_open: RwSignal::new(panel_open),
            ui_hidden: RwSignal::new(false),
            file: RwSignal::new(None),
            file_error: RwSignal::new(None),
            dragging: RwSignal::new(false),
            notice: RwSignal::new(None),
            fatal: RwSignal::new(None),
            server: RwSignal::new(None),
            lan_admin: RwSignal::new(false),
            lan_busy: RwSignal::new(false),
            lan_error: RwSignal::new(None),
            qr: RwSignal::new(None),
            embed: RwSignal::new(false),
            fullscreen: RwSignal::new(false),
            replay: RwSignal::new(None),
            scrub: RwSignal::new(None),
            recording: RwSignal::new(None),
            record_busy: RwSignal::new(false),
            takes: RwSignal::new(Vec::new()),
            share: RwSignal::new(None),
            captures: RwSignal::new(Vec::new()),
            refined: RwSignal::new(Vec::new()),
            job: RwSignal::new(None),
            job_end: RwSignal::new(None),
            refine_busy: RwSignal::new(false),
            refining: RwSignal::new(None),
            refined_loading: RwSignal::new(None),
            storage: RwSignal::new(None),
            capture: RwSignal::new(None),
            import: RwSignal::new(None),
            pair: RwSignal::new(PairView::default()),
        }
    }

    /// Sets a signal only when the value differs, so that views depending
    /// on it are not re-rendered for nothing.
    pub fn set_if_changed<T: PartialEq + Send + Sync + 'static>(signal: RwSignal<T>, value: T) {
        if signal.with_untracked(|current| *current != value) {
            signal.set(value);
        }
    }
}

/// The interface state of the running application.
pub fn use_ui() -> Ui {
    expect_context::<Ui>()
}
