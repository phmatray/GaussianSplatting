//! The non-reactive half of the application: the viewer, the transport and
//! everything that happens when a wire message arrives. Views read the
//! signals in [`Ui`] and call the methods of [`Core`].

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::Duration;

use glam::Vec3;
use gs_core::codec::{self, Kind, PreviewMsg};
use gs_core::protocol::{CameraInfo, ControlMsg, ScanState, SessionState, Stats};
use gs_core::scene::SceneKind;
use gs_render::{Applied, GizmoScene, NavMode, RenderParams, SceneHints, Viewer, ViewerOptions, LAYER_DYNAMIC, LAYER_STATIC};
use leptos::prelude::*;
use leptos::task::spawn_local;
use wasm_bindgen::{Clamped, JsCast};
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement, ImageData};

use crate::logic::config::PatchQueue;
use crate::logic::files::{self, FileFormat};
use crate::logic::rate::RateMeter;
use crate::logic::source::{api_url, forced_source, is_loopback_base, ws_url, Link, ServerInfo, SourceKind};
use crate::logic::{depth, present};
use crate::state::{Display, FileInfo, Notice, ScanView, Ui};
use crate::transport::{Sink, Transport};
use crate::{dom, net, tauri};

/// Quiet time after the last change of a control before the configuration
/// patch is sent, in milliseconds.
const PATCH_DEBOUNCE_MS: u64 = 160;
/// Interval between two refreshes of the statistics signal, milliseconds.
const STATS_INTERVAL_MS: f64 = 250.0;
/// How long a notice stays on screen.
const NOTICE_MS: u64 = 7000;
/// Speed of the optional automatic rotation of the orbit view, rad/s.
const AUTO_ROTATE_SPEED: f32 = 0.25;
/// Unexpected errors kept for the debug hook.
const MAX_ERRORS: usize = 50;
/// A scan that starts below this progress is a new scan, not a resumed one.
const NEW_SCAN_PROGRESS: f32 = 0.02;
/// Time given to `GET /api/info` before the page falls back to the worker.
const INFO_TIMEOUT_MS: i32 = 2500;

thread_local! {
    static CORE: RefCell<Option<Rc<Core>>> = const { RefCell::new(None) };
}

/// Runs `f` on the application core, once the viewer exists.
pub fn with_core(f: impl FnOnce(&Core)) {
    let core = CORE.with(|c| c.borrow().clone());
    if let Some(core) = core {
        f(&core);
    }
}

/// Counters of received messages (debug hook and data rate).
#[derive(Clone, Copy, Debug, Default)]
pub struct Counters {
    pub messages: u64,
    pub bytes: u64,
    pub states: u64,
    pub stats: u64,
    pub layers: u64,
    pub previews: u64,
    /// Layer messages ignored while an imported file is shown.
    pub dropped_layers: u64,
}

/// The latest thumbnail of one camera.
struct PreviewTile {
    msg: PreviewMsg,
    /// Smoothed depth range used for the depth colours, meters.
    range: Option<(f32, f32)>,
    /// Times this tile was drawn to its canvas.
    painted: u32,
}

/// What was true before an imported file took over the view.
#[derive(Clone, Copy)]
struct FileSession {
    /// The pipeline was already paused: leave it paused afterwards.
    paused_before: bool,
}

pub struct Core {
    pub ui: Ui,
    pub viewer: Viewer,
    transport: RefCell<Option<Transport>>,
    /// HTTP base of the native server (`http://127.0.0.1:7878`).
    server_base: RefCell<Option<String>>,
    session: RefCell<Option<SessionState>>,
    last_stats: Cell<Option<Stats>>,
    stats_pushed_ms: Cell<f64>,
    /// Ticks per second, from the tick counter of the Stats messages.
    tick_rate: Cell<RateMeter>,
    previews: RefCell<BTreeMap<u8, PreviewTile>>,
    counters: Cell<Counters>,
    /// Start of the current data-rate window and the bytes counted then.
    rate_window: Cell<(f64, u64)>,
    patches: RefCell<PatchQueue>,
    patch_timer: Cell<Option<TimeoutHandle>>,
    file: Cell<Option<FileSession>>,
    /// The next State resets the camera as a scene change does.
    reset_view: Cell<bool>,
    /// Camera to return to when the dollhouse view is left.
    saved_view: Cell<Option<(Vec3, Vec3, NavMode)>>,
    /// The house was just shown: its first Stats decide whether it opens
    /// on the overview (scan under way) or inside (scan done).
    overview_pending: Cell<bool>,
    /// Scan state and handheld pose the gizmos were last built for.
    handheld: Cell<Option<(ScanState, CameraInfo)>>,
    /// The viewer stands inside the scan head, whose gizmos are hidden.
    in_scan_head: Cell<bool>,
    errors: RefCell<Vec<String>>,
    notice_serial: Cell<u32>,
    /// RGBA pixels of the thumbnail being painted.
    scratch: RefCell<Vec<u8>>,
}

/// Creates the viewer on `canvas` and starts everything else. Called once,
/// when the canvas is mounted.
pub fn start(canvas: HtmlCanvasElement, ui: Ui) {
    let viewer = match Viewer::new(canvas, ViewerOptions::default()) {
        Ok(viewer) => viewer,
        Err(error) => {
            let detail = error.as_string().unwrap_or_else(|| format!("{error:?}"));
            ui.fatal.set(Some(format!("L'affichage 3D n'a pas pu démarrer\u{a0}: ce navigateur ne propose pas WebGL2 ({detail}).")));
            ui.link.set(Link::Failed);
            return;
        }
    };
    let core = Rc::new(Core {
        ui,
        viewer,
        transport: RefCell::new(None),
        server_base: RefCell::new(None),
        session: RefCell::new(None),
        last_stats: Cell::new(None),
        stats_pushed_ms: Cell::new(f64::NEG_INFINITY),
        tick_rate: Cell::new(RateMeter::default()),
        previews: RefCell::new(BTreeMap::new()),
        counters: Cell::new(Counters::default()),
        rate_window: Cell::new((dom::now_ms(), 0)),
        patches: RefCell::new(PatchQueue::default()),
        patch_timer: Cell::new(None),
        file: Cell::new(None),
        reset_view: Cell::new(false),
        saved_view: Cell::new(None),
        overview_pending: Cell::new(false),
        handheld: Cell::new(None),
        in_scan_head: Cell::new(false),
        errors: RefCell::new(Vec::new()),
        notice_serial: Cell::new(0),
        scratch: RefCell::new(Vec::new()),
    });
    CORE.with(|c| *c.borrow_mut() = Some(core.clone()));

    core.viewer.on_xr_change(|active| with_core(|core| core.on_xr_change(active)));
    spawn_local(async move {
        let supported = Viewer::xr_supported().await;
        ui.xr_supported.set(Some(supported));
    });
    crate::debug::install();
    let _ = set_interval_with_handle(|| with_core(Core::on_timer), Duration::from_millis(500));
    spawn_local(connect());
}

/// How the page found its source.
enum Resolved {
    Server { base: String, info: ServerInfo },
    Worker,
}

/// The order of "Server interface" in `docs/ARCHITECTURE.md`: the Tauri
/// command, then `GET /api/info` on the page's origin, then the worker.
/// `?source=worker|server` forces one.
async fn resolve(forced: Option<SourceKind>) -> Result<Resolved, String> {
    if forced == Some(SourceKind::Worker) {
        return Ok(Resolved::Worker);
    }
    if tauri::is_tauri() {
        if let Ok(info) = tauri::server_info().await {
            return Ok(Resolved::Server { base: info.http_url.clone(), info });
        }
    }
    let origin = web_sys::window().and_then(|w| w.location().origin().ok()).unwrap_or_default();
    let probe = async {
        let answer = net::request(&api_url(&origin, "/api/info"), None, INFO_TIMEOUT_MS).await?;
        if !answer.ok() {
            return Err(format!("réponse HTTP {}", answer.status));
        }
        ServerInfo::parse(&answer.body)
    };
    match probe.await {
        Ok(info) => Ok(Resolved::Server { base: origin, info }),
        Err(error) if forced == Some(SourceKind::Server) => Err(error),
        Err(_) => Ok(Resolved::Worker),
    }
}

async fn connect() {
    let forced = forced_source(dom::query_param("source").as_deref());
    let scene = match dom::query_param("scene").as_deref() {
        Some("house") => SceneKind::House,
        _ => SceneKind::Stage,
    };
    loop {
        match resolve(forced).await {
            Ok(Resolved::Server { base, info }) => {
                with_core(|core| core.attach_server(base, info));
                return;
            }
            Ok(Resolved::Worker) => {
                with_core(|core| core.attach_worker(scene));
                return;
            }
            Err(error) => {
                // Only when the server is forced: keep trying, visibly.
                with_core(|core| {
                    core.ui.source.set(Some(SourceKind::Server));
                    Ui::set_if_changed(core.ui.link, Link::Failed);
                    core.notify(true, format!("Serveur natif injoignable ({error}). Nouvel essai dans quelques secondes."));
                });
                dom::sleep_ms(3000).await;
            }
        }
    }
}

fn sink() -> Sink {
    Sink {
        on_message: Rc::new(|bytes| with_core(|core| core.on_bytes(bytes))),
        on_link: Rc::new(|link| with_core(|core| core.on_link(link))),
        on_error: Rc::new(|message| with_core(|core| core.report_error(message))),
    }
}

impl Core {
    // ------------------------------------------------------------ transport

    fn attach_server(&self, base: String, info: ServerInfo) {
        self.ui.source.set(Some(SourceKind::Server));
        let Some(url) = ws_url(&base, info.ws_path()) else {
            self.ui.link.set(Link::Failed);
            self.report_error(format!("Adresse de serveur inattendue\u{a0}: {base}"));
            return;
        };
        self.ui.lan_admin.set(is_loopback_base(&base));
        *self.server_base.borrow_mut() = Some(base);
        self.ui.server.set(Some(info));
        self.ui.link.set(Link::Connecting);
        *self.transport.borrow_mut() = Some(Transport::server(url, sink()));
    }

    fn attach_worker(&self, scene: SceneKind) {
        self.ui.source.set(Some(SourceKind::Worker));
        self.ui.link.set(Link::Connecting);
        match Transport::worker(scene, sink()) {
            Ok(transport) => *self.transport.borrow_mut() = Some(transport),
            Err(error) => {
                self.ui.link.set(Link::Failed);
                self.report_error(format!("Le calcul local n'a pas pu démarrer\u{a0}: {error}."));
            }
        }
    }

    fn on_link(&self, link: Link) {
        let was_connected = self.ui.link.get_untracked() == Link::Connected;
        Ui::set_if_changed(self.ui.link, link);
        if link != Link::Connected {
            // A mode switch that was under way is lost with the link.
            Ui::set_if_changed(self.ui.pending_scene, None);
        }
        if link == Link::Connected && !was_connected && self.ui.source.get_untracked() == Some(SourceKind::Server) {
            self.refresh_server_info();
        }
    }

    pub fn source(&self) -> Option<SourceKind> {
        self.transport.borrow().as_ref().map(Transport::kind)
    }

    /// Sends a control message to the pipeline.
    pub fn send(&self, msg: ControlMsg) {
        if let Some(transport) = self.transport.borrow().as_ref() {
            transport.send(&msg);
        }
    }

    /// Queues a partial configuration; patches made in quick succession (a
    /// slider being dragged) leave as one `patch_config` message.
    pub fn queue_patch(&self, patch: serde_json::Value) {
        self.patches.borrow_mut().push(patch);
        if let Some(timer) = self.patch_timer.take() {
            timer.clear();
        }
        let timer = set_timeout_with_handle(
            || {
                with_core(|core| {
                    core.patch_timer.set(None);
                    let msg = core.patches.borrow_mut().take();
                    if let Some(msg) = msg {
                        core.send(msg);
                    }
                })
            },
            Duration::from_millis(PATCH_DEBOUNCE_MS),
        );
        self.patch_timer.set(timer.ok());
    }

    // ------------------------------------------------------------- messages

    fn on_bytes(&self, bytes: &[u8]) {
        let mut counters = self.counters.get();
        counters.messages += 1;
        counters.bytes += bytes.len() as u64;
        // While a file is shown its splats own the layers; the stream keeps
        // flowing (State, Stats, thumbnails) and "Revenir au direct" asks
        // for a fresh snapshot.
        if self.file.get().is_some() && codec::peek_kind(bytes) == Ok(Kind::Layer) {
            counters.dropped_layers += 1;
            self.counters.set(counters);
            return;
        }
        let applied = self.viewer.apply_message(bytes);
        match &applied {
            Ok(Applied::State(_)) => counters.states += 1,
            Ok(Applied::Stats(_)) => counters.stats += 1,
            Ok(Applied::Preview(_)) => counters.previews += 1,
            Ok(Applied::Layer { .. }) => counters.layers += 1,
            Err(_) => {}
        }
        self.counters.set(counters);
        match applied {
            Ok(Applied::State(json)) => self.on_state(&json),
            Ok(Applied::Stats(json)) => self.on_stats(&json),
            Ok(Applied::Preview(preview)) => self.on_preview(preview),
            Ok(Applied::Layer { .. }) => {}
            Err(error) => self.report_error(format!("Message illisible\u{a0}: {error}")),
        }
    }

    fn on_state(&self, json: &str) {
        let state: SessionState = match serde_json::from_str(json) {
            Ok(state) => state,
            Err(error) => {
                self.report_error(format!("État de session illisible\u{a0}: {error}"));
                return;
            }
        };
        let scene = state.config.scene;
        let previous = self.session.borrow().as_ref().map(|s| s.config.scene);
        let scene_changed = previous != Some(scene);
        let camera_ids = present::camera_ids(&state);
        if scene_changed {
            // Statistics and thumbnails of the other scene are stale.
            self.last_stats.set(None);
            self.handheld.set(None);
            self.previews.borrow_mut().clear();
            self.tick_rate.set(RateMeter::default());
            self.ui.pipeline_hz.set(None);
            self.ui.stats.set(None);
            self.ui.scan.set(None);
        } else {
            self.previews.borrow_mut().retain(|id, _| camera_ids.contains(&u32::from(*id)));
        }
        let config = state.config;
        *self.session.borrow_mut() = Some(state);

        self.ui.scene.set(Some(scene));
        self.ui.pending_scene.set(None);
        Ui::set_if_changed(self.ui.config, Some(config));
        Ui::set_if_changed(self.ui.camera_ids, camera_ids);
        self.ui.state_epoch.update(|epoch| *epoch = epoch.wrapping_add(1));
        if self.file.get().is_none() {
            let reset_view = self.reset_view.replace(false) || scene_changed;
            self.present(reset_view);
        }
    }

    /// Shows the session in the viewer: hints, gizmos and, when the scene
    /// is new, the camera and the navigation mode it starts with.
    fn present(&self, reset_view: bool) {
        let session = self.session.borrow();
        let Some(state) = session.as_ref() else { return };
        self.viewer.set_scene_hints(present::scene_hints(state));
        if reset_view {
            let nav = present::default_nav(state.config.scene);
            self.saved_view.set(None);
            self.viewer.set_nav_mode(nav);
            self.viewer.set_view(Vec3::from_array(state.viewer_start.eye), Vec3::from_array(state.viewer_start.target));
            self.ui.display.update(|d| {
                d.nav = nav;
                d.dollhouse = false;
                d.splat_scale = present::default_splat_scale(state.config.scene);
            });
            self.overview_pending.set(state.config.scene == SceneKind::House);
        }
        drop(session);
        self.update_gizmos();
        self.apply_params();
    }

    /// Rebuilds the gizmos of the live session for the latest scan
    /// statistics and the place the viewer stands at.
    fn update_gizmos(&self) {
        let session = self.session.borrow();
        let Some(state) = session.as_ref() else { return };
        if self.file.get().is_some() {
            return;
        }
        let scan = self.last_stats.get().and_then(|s| s.scan);
        let inside = self.viewer_in_scan_head();
        self.in_scan_head.set(inside);
        self.viewer.set_gizmos(&present::gizmos(state, scan.as_ref(), !inside));
    }

    fn viewer_in_scan_head(&self) -> bool {
        let scan = self.last_stats.get().and_then(|s| s.scan);
        scan.is_some_and(|scan| present::inside_scan_head(&scan, self.viewer.eye()))
    }

    fn on_stats(&self, json: &str) {
        let stats: Stats = match serde_json::from_str(json) {
            Ok(stats) => stats,
            Err(error) => {
                self.report_error(format!("Statistiques illisibles\u{a0}: {error}"));
                return;
            }
        };
        let previous = self.last_stats.replace(Some(stats));
        let scan = stats.scan.map(|s| ScanView { state: s.state, progress: s.progress });
        let scan_changed = previous.and_then(|p| p.scan).map(|s| s.state) != stats.scan.map(|s| s.state);
        Ui::set_if_changed(self.ui.scan, scan);

        // The handheld camera gizmo follows the scan.
        let handheld = stats.scan.map(|s| (s.state, s.camera));
        if handheld != self.handheld.replace(handheld) {
            self.update_gizmos();
        }

        // A scan is best watched from above: the rooms appear one after the
        // other, and the place where a visit starts is scanned last. The
        // overview is the dollhouse view; leaving it lands inside the house.
        if let Some(scan) = stats.scan {
            let first_look = self.overview_pending.replace(false) && scan.state != ScanState::Done;
            let new_scan = scan_changed && scan.state == ScanState::Scanning && scan.progress < NEW_SCAN_PROGRESS;
            if (first_look || new_scan) && self.file.get().is_none() {
                self.set_display(|d| d.dollhouse = true);
            }
        }

        let now = dom::now_ms();
        let mut tick_rate = self.tick_rate.get();
        let pipeline_hz = tick_rate.push(now, stats.tick);
        self.tick_rate.set(tick_rate);
        if scan_changed || now - self.stats_pushed_ms.get() >= STATS_INTERVAL_MS {
            self.stats_pushed_ms.set(now);
            self.ui.stats.set(Some(stats));
            Ui::set_if_changed(self.ui.pipeline_hz, pipeline_hz);
        }
    }

    fn on_preview(&self, msg: PreviewMsg) {
        let id = msg.camera_id;
        if !self.ui.camera_ids.with_untracked(|ids| ids.contains(&u32::from(id))) {
            return;
        }
        {
            let mut tiles = self.previews.borrow_mut();
            let previous = tiles.get(&id);
            let range = match depth::depth_range(&msg.depth_mm) {
                Some(current) => Some(depth::smooth_range(previous.and_then(|t| t.range), current)),
                None => previous.and_then(|t| t.range),
            };
            let painted = previous.map_or(0, |t| t.painted);
            tiles.insert(id, PreviewTile { msg, range, painted });
        }
        self.paint_preview(id);
    }

    /// Draws the latest thumbnail of camera `id` on its canvas, if the tile
    /// is on the page.
    fn paint_preview(&self, id: u8) {
        let mut tiles = self.previews.borrow_mut();
        let Some(tile) = tiles.get_mut(&id) else { return };
        let (width, height) = (u32::from(tile.msg.width), u32::from(tile.msg.height));
        let pixels = width as usize * height as usize;
        if pixels == 0 || tile.msg.rgb.len() < pixels * 3 || tile.msg.depth_mm.len() < pixels {
            return;
        }
        let Some(canvas) = dom::element_by_id::<HtmlCanvasElement>(&format!("preview-{id}")) else { return };
        if canvas.width() != width || canvas.height() != height {
            canvas.set_width(width);
            canvas.set_height(height);
        }
        let Some(context) = canvas.get_context("2d").ok().flatten().and_then(|c| c.dyn_into::<CanvasRenderingContext2d>().ok()) else {
            return;
        };
        let mut rgba = self.scratch.borrow_mut();
        if self.ui.preview_depth.get_untracked() {
            let (near, far) = tile.range.unwrap_or((0.5, 6.0));
            depth::colorize_depth(&tile.msg.depth_mm[..pixels], near, far, &mut rgba);
        } else {
            depth::rgb_to_rgba(&tile.msg.rgb[..pixels * 3], &mut rgba);
        }
        if let Ok(image) = ImageData::new_with_u8_clamped_array_and_sh(Clamped(&rgba[..]), width, height) {
            if context.put_image_data(&image, 0, 0).is_ok() {
                tile.painted += 1;
            }
        }
    }

    /// Redraws every thumbnail (the tiles were just created, or the colour
    /// / depth choice changed).
    pub fn repaint_previews(&self) {
        let ids: Vec<u8> = self.previews.borrow().keys().copied().collect();
        for id in ids {
            self.paint_preview(id);
        }
    }

    pub fn set_preview_depth(&self, depth: bool) {
        Ui::set_if_changed(self.ui.preview_depth, depth);
        self.repaint_previews();
    }

    // ------------------------------------------------------------- pipeline

    /// Switches between the live stage and the home scan.
    pub fn set_scene(&self, scene: SceneKind) {
        if self.file.get().is_some() {
            self.return_to_live();
        }
        if self.ui.scene.get_untracked() != Some(scene) {
            self.ui.pending_scene.set(Some(scene));
        }
        self.send(ControlMsg::SetScene { scene });
    }

    pub fn toggle_pause(&self) {
        // The echoed configuration is the truth; without one there is
        // nothing to pause yet.
        let Some(config) = self.ui.config.get_untracked() else { return };
        if self.file.get().is_some() {
            return;
        }
        self.send(ControlMsg::SetPaused { paused: !config.paused });
    }

    // -------------------------------------------------------------- display

    /// Changes the display settings and applies them to the viewer.
    pub fn set_display(&self, change: impl FnOnce(&mut Display)) {
        let old = self.ui.display.get_untracked();
        let mut new = old;
        change(&mut new);
        if new == old {
            return;
        }
        if new.dollhouse && !old.dollhouse {
            let house = self.session.borrow().as_ref().map(|s| (s.bounds, present::dollhouse_clip(s).unwrap_or(s.bounds.max.y)));
            if let Some((bounds, wall_top)) = house {
                self.saved_view.set(Some((self.viewer.eye(), self.viewer.target(), old.nav)));
                new.nav = NavMode::Orbit;
                self.viewer.set_nav_mode(NavMode::Orbit);
                let (eye, target) =
                    present::dollhouse_view(&bounds, wall_top, self.viewer.fov_y(), self.aspect(), self.covered_by_panel());
                self.viewer.set_view(eye, target);
            } else {
                new.dollhouse = false;
            }
        } else if !new.dollhouse && old.dollhouse {
            if let Some((eye, target, nav)) = self.saved_view.take() {
                new.nav = nav;
                self.viewer.set_view(eye, target);
            }
            self.viewer.set_nav_mode(new.nav);
        } else if new.nav != old.nav {
            self.viewer.set_nav_mode(new.nav);
        }
        if new.auto_rotate != old.auto_rotate {
            self.viewer.set_auto_rotate(new.auto_rotate.then_some(AUTO_ROTATE_SPEED));
        }
        self.ui.display.set(new);
        self.apply_params();
    }

    fn aspect(&self) -> f32 {
        let canvas = self.viewer.canvas();
        let (width, height) = (canvas.client_width().max(1) as f32, canvas.client_height().max(1) as f32);
        width / height
    }

    /// Fraction of the view's width hidden by the open side panel (`0` when
    /// it is closed, or when it is a sheet at the bottom of a narrow window).
    fn covered_by_panel(&self) -> f32 {
        if !self.ui.panel_open.get_untracked() {
            return 0.0;
        }
        let Some(panel) = dom::element_by_id::<web_sys::Element>("panel") else { return 0.0 };
        let (panel, view) = (panel.get_bounding_client_rect(), self.viewer.canvas().get_bounding_client_rect());
        let side_panel = panel.height() > view.height() * 0.7 && panel.left() > view.left();
        if !side_panel || view.width() <= 0.0 {
            return 0.0;
        }
        ((view.right() - panel.left()) / view.width()) as f32
    }

    fn apply_params(&self) {
        let display = self.ui.display.get_untracked();
        let clip = if display.dollhouse { self.session.borrow().as_ref().and_then(present::dollhouse_clip) } else { None };
        self.viewer.set_params(RenderParams {
            splat_scale: display.splat_scale,
            point_mode: display.point_mode,
            show_gizmos: display.gizmos,
            show_static: display.show_static,
            show_dynamic: display.show_dynamic,
            clip_y_max: clip,
            ..self.viewer.params()
        });
    }

    /// Leaves the overview for a visit of the scanned house: inside, at
    /// the pose the scene proposes, whatever was looked at before.
    pub fn visit_house(&self) {
        let start = self.session.borrow().as_ref().map(|s| s.viewer_start);
        self.saved_view.set(None);
        self.set_display(|d| {
            d.dollhouse = false;
            d.nav = NavMode::Fly;
        });
        if let Some(start) = start {
            self.viewer.set_view(Vec3::from_array(start.eye), Vec3::from_array(start.target));
        }
    }

    /// Looks at the scene from the camera of a thumbnail: a rig camera
    /// of the stage (in orbit around what it aims at), or a sensor of the
    /// scan head where the scan last saw it (in fly mode).
    pub fn view_from_camera(&self, id: u32) {
        let house = self.ui.scene.get_untracked() == Some(SceneKind::House);
        let view = if house {
            let camera = self.last_stats.get().and_then(|stats| stats.scan).and_then(|scan| present::sensor_camera(&scan, id));
            camera.map(|camera| present::camera_view(&camera, 1.0))
        } else {
            self.session.borrow().as_ref().and_then(|state| {
                let info = state.cameras.iter().find(|c| c.id == id)?;
                let (eye, _) = present::camera_view(info, 1.0);
                Some(present::camera_view(info, (state.config.rig.target - eye).length().max(0.5)))
            })
        };
        if let Some((eye, target)) = view {
            let nav = if house { NavMode::Fly } else { NavMode::Orbit };
            self.set_display(|d| {
                d.dollhouse = false;
                d.nav = nav;
            });
            self.viewer.set_view(eye, target);
            self.update_gizmos();
        }
    }

    // ------------------------------------------------------------------- VR

    fn on_xr_change(&self, active: bool) {
        self.ui.xr_active.set(active);
        if active {
            self.ui.xr_error.set(None);
        }
    }

    /// Must be called synchronously from a click handler: the browser only
    /// grants an immersive session to a user gesture.
    pub fn enter_vr(&self) {
        self.ui.xr_error.set(None);
        // The session starts where the desktop camera stands, life-size:
        // from inside the house, not from the overview above it (the
        // headset has its own miniature mode).
        self.set_display(|d| d.dollhouse = false);
        self.viewer.enter_xr();
    }

    pub fn exit_vr(&self) {
        self.viewer.exit_xr();
    }

    // ---------------------------------------------------------------- files

    /// Downloads the splats the viewer currently shows.
    pub fn export(&self, format: FileFormat) {
        let splats = self.viewer.splats();
        if splats.is_empty() {
            self.notify(true, "Il n'y a encore rien à exporter.".to_string());
            return;
        }
        let scene = match (self.file.get(), self.ui.scene.get_untracked()) {
            (Some(_), _) => "import",
            (None, Some(SceneKind::House)) => "house",
            (None, _) => "stage",
        };
        let name = files::export_name(scene, splats.len(), format);
        let bytes = format.encode(&splats);
        match dom::download(&name, &bytes) {
            Ok(()) => self.notify(false, format!("{} splats exportés dans {name}.", crate::logic::format::count(splats.len() as u64))),
            Err(error) => self.notify(true, format!("L'export a échoué\u{a0}: {error}.")),
        }
    }

    /// Reads a file the user picked or dropped and shows it.
    pub fn import_file(&self, file: web_sys::File) {
        let name = file.name();
        if let Err(message) = files::check_size(file.size() as u64) {
            self.import_failed(message);
            return;
        }
        spawn_local(async move {
            let bytes = dom::read_file(&file).await;
            with_core(|core| match bytes {
                Ok(bytes) => core.import_bytes(&name, &bytes),
                Err(error) => core.import_failed(format!("Le fichier n'a pas pu être lu ({error}).")),
            });
        });
    }

    fn import_failed(&self, message: String) {
        self.ui.file_error.set(Some(message.clone()));
        self.notify(true, message);
    }

    /// Shows an imported file instead of the live reconstruction: the
    /// pipeline is paused, the file becomes layer 0 and layer 1 is emptied.
    pub fn import_bytes(&self, name: &str, bytes: &[u8]) {
        let splats = match files::parse_import(name, bytes) {
            Ok(splats) => splats,
            Err(message) => {
                self.import_failed(message);
                return;
            }
        };
        let was_live = self.file.get().is_none();
        if was_live {
            let paused_before = self.ui.config.get_untracked().is_some_and(|c| c.paused);
            // From here on layer messages of the stream are ignored.
            self.file.set(Some(FileSession { paused_before }));
            if !paused_before {
                self.send(ControlMsg::SetPaused { paused: true });
            }
        }
        let shown = self.viewer.set_layer(LAYER_STATIC, &splats).and_then(|()| self.viewer.set_layer(LAYER_DYNAMIC, &[]));
        if let Err(error) = shown {
            self.return_to_live();
            self.import_failed(format!("Ce fichier ne peut pas être affiché ({error})."));
            return;
        }
        let bounds = present::robust_bounds(&splats);
        self.viewer.set_gizmos(&GizmoScene::default());
        self.viewer.set_scene_hints(SceneHints {
            ceiling_height: None,
            floor_center: bounds.map_or(Vec3::ZERO, |b| Vec3::new(b.center().x, b.min.y, b.center().z)),
            bounds,
        });
        self.saved_view.set(None);
        self.viewer.set_nav_mode(NavMode::Orbit);
        if let Some(bounds) = bounds {
            let (eye, target) = present::frame_view(&bounds, self.viewer.fov_y(), self.aspect());
            self.viewer.set_view(eye, target);
        }
        self.ui.display.update(|d| {
            d.nav = NavMode::Orbit;
            d.dollhouse = false;
            // A file is shown as it was written.
            d.splat_scale = 1.0;
        });
        self.apply_params();
        self.ui.file_error.set(None);
        self.ui.file.set(Some(FileInfo { name: name.to_string(), splats: splats.len() }));
    }

    /// Leaves the imported file and shows the live reconstruction again.
    pub fn return_to_live(&self) {
        let Some(file) = self.file.take() else { return };
        self.ui.file.set(None);
        self.viewer.clear_layers();
        self.reset_view.set(true);
        if let Some(transport) = self.transport.borrow().as_ref() {
            transport.resync();
        }
        if !file.paused_before {
            self.send(ControlMsg::SetPaused { paused: false });
        }
    }

    // ---------------------------------------------------------- LAN sharing

    fn server_url(&self, path: &str) -> Option<String> {
        self.server_base.borrow().as_ref().map(|base| api_url(base, path))
    }

    fn set_server_info(&self, info: ServerInfo) {
        let lan_on = info.lan.enabled && !info.lan.urls.is_empty();
        let changed = self.ui.server.with_untracked(|old| old.as_ref().map(|o| &o.lan) != Some(&info.lan));
        self.ui.server.set(Some(info));
        if !lan_on {
            self.ui.qr.set(None);
        } else if changed || self.ui.qr.with_untracked(Option::is_none) {
            self.load_qr();
        }
    }

    /// Fetches the QR code as text and shows it through a `data:` URL: a
    /// cross-origin `<img>` would need its own entry in the Content
    /// Security Policy of the desktop shell, `fetch` is allowed there anyway.
    fn load_qr(&self) {
        let Some(url) = self.server_url("/api/qr.svg?i=0") else { return };
        spawn_local(async move {
            let answer = net::request(&url, None, 5000).await;
            with_core(|core| match answer {
                Ok(answer) if answer.ok() && answer.body.contains("<svg") => {
                    let encoded = String::from(js_sys::encode_uri_component(&answer.body));
                    core.ui.qr.set(Some(format!("data:image/svg+xml;charset=utf-8,{encoded}")));
                }
                _ => core.ui.qr.set(None),
            });
        });
    }

    pub fn refresh_server_info(&self) {
        let Some(url) = self.server_url("/api/info") else { return };
        spawn_local(async move {
            if let Ok(answer) = net::request(&url, None, 5000).await {
                if let (true, Ok(info)) = (answer.ok(), ServerInfo::parse(&answer.body)) {
                    with_core(|core| core.set_server_info(info));
                }
            }
        });
    }

    /// Switches the LAN HTTPS listener of the native server on or off.
    pub fn set_lan(&self, enabled: bool) {
        let Some(url) = self.server_url("/api/lan") else { return };
        if self.ui.lan_busy.get_untracked() {
            return;
        }
        self.ui.lan_busy.set(true);
        self.ui.lan_error.set(None);
        spawn_local(async move {
            let body = serde_json::json!({ "enabled": enabled }).to_string();
            let answer = net::request(&url, Some(&body), 8000).await;
            with_core(|core| {
                core.ui.lan_busy.set(false);
                let result = match answer {
                    Ok(answer) if answer.ok() => ServerInfo::parse(&answer.body),
                    // Only the machine that runs the server may change this.
                    Ok(answer) if answer.status == 403 => {
                        Err("Le partage se règle depuis l'ordinateur qui exécute Splat Live.".to_string())
                    }
                    Ok(answer) => Err(format!("Le serveur a refusé la demande (HTTP {}).", answer.status)),
                    Err(error) => Err(format!("Le serveur n'a pas répondu ({error}).")),
                };
                match result {
                    Ok(info) => core.set_server_info(info),
                    Err(message) => core.ui.lan_error.set(Some(message)),
                }
            });
        });
    }

    // -------------------------------------------------------- housekeeping

    /// Twice per second: viewer statistics, data rate, VR errors.
    fn on_timer(&self) {
        Ui::set_if_changed(self.ui.viewer_stats, self.viewer.stats());
        // The viewer walked into or out of a scan head that stands still.
        if self.viewer_in_scan_head() != self.in_scan_head.get() {
            self.update_gizmos();
        }
        let now = dom::now_ms();
        let (start, bytes_then) = self.rate_window.get();
        let bytes_now = self.counters.get().bytes;
        let elapsed = (now - start).max(1.0);
        let rate = (bytes_now - bytes_then) as f64 * 1000.0 / elapsed;
        self.rate_window.set((now, bytes_now));
        let smoothed = self.ui.transfer_rate.get_untracked() * 0.5 + rate as f32 * 0.5;
        Ui::set_if_changed(self.ui.transfer_rate, if smoothed < 1.0 { 0.0 } else { smoothed });
        // `enter_xr` fails asynchronously and without a callback.
        if !self.viewer.xr_active() {
            Ui::set_if_changed(self.ui.xr_error, self.viewer.xr_error());
        }
    }

    /// Shows a message at the bottom of the window for a few seconds.
    pub fn notify(&self, error: bool, text: String) {
        let serial = self.notice_serial.get().wrapping_add(1);
        self.notice_serial.set(serial);
        self.ui.notice.set(Some(Notice { error, text }));
        set_timeout(
            move || {
                with_core(|core| {
                    if core.notice_serial.get() == serial {
                        core.ui.notice.set(None);
                    }
                })
            },
            Duration::from_millis(NOTICE_MS),
        );
    }

    /// Records something that should not happen (a broken message, a dead
    /// worker) and tells the user.
    pub fn report_error(&self, message: String) {
        {
            let mut errors = self.errors.borrow_mut();
            if errors.len() < MAX_ERRORS {
                errors.push(message.clone());
            }
        }
        self.notify(true, message);
    }

    // ----------------------------------------------------------- debug hook

    pub fn counters(&self) -> Counters {
        self.counters.get()
    }

    pub fn errors(&self) -> Vec<String> {
        self.errors.borrow().clone()
    }

    pub fn last_stats(&self) -> Option<Stats> {
        self.last_stats.get()
    }

    /// The viewer looks through the scan head, whose gizmos are hidden.
    pub fn in_scan_head(&self) -> bool {
        self.in_scan_head.get()
    }

    pub fn is_file_mode(&self) -> bool {
        self.file.get().is_some()
    }

    /// Paint counts of the thumbnails, by camera id.
    pub fn preview_paints(&self) -> BTreeMap<u8, u32> {
        self.previews.borrow().iter().map(|(id, tile)| (*id, tile.painted)).collect()
    }

    pub fn channel(&self) -> Option<&'static str> {
        self.transport.borrow().as_ref().map(Transport::channel)
    }

    pub fn batches_received(&self) -> u64 {
        self.transport.borrow().as_ref().map_or(0, Transport::received)
    }
}
