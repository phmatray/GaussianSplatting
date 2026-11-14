//! Reactive state of the interface. Everything here is a signal, so the
//! handle is `Copy` and can be captured by any view or event handler; the
//! objects that are not reactive (viewer, transport) live in
//! [`crate::core::Core`].

use gs_core::protocol::{PipelineConfig, ScanState, Stats};
use gs_core::scene::SceneKind;
use gs_render::{NavMode, ViewerStats};
use leptos::prelude::*;

use crate::logic::source::{Link, ServerInfo, SourceKind};

/// Local display settings (never sent to the pipeline).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Display {
    pub nav: NavMode,
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
