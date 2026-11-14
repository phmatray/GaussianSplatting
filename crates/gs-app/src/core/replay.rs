//! The third source: a recording. Opening one (a link, a file, a take just
//! recorded), the transport of its time line, and the way back to the
//! live source.

use std::rc::Rc;

use glam::Vec3;
use gs_core::protocol::ControlMsg;
use gs_render::NavMode;
use gs_worker::{ReplayStatus, WorkerRequest};
use leptos::prelude::*;
use leptos::task::spawn_local;

use super::{with_core, Core, Suspended};
use crate::dom;
use crate::logic::replay::{self, seek_target};
use crate::logic::share::{FileKind, FileRef, View, ViewerParams};
use crate::logic::source::Link;
use crate::net::{self, DownloadError};
use crate::state::{ReplayOrigin, ReplayView};
use crate::transport::{ReplayLink, ReplaySink, Transport};

/// What a link asks of a replay once its first picture is there.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ReplayStart {
    pub time: Option<f64>,
    pub autoplay: bool,
    pub looping: bool,
    pub view: Option<View>,
    pub nav: Option<NavMode>,
}

impl ReplayStart {
    pub fn of(params: &ViewerParams) -> Self {
        Self { time: params.time, autoplay: params.autoplay, looping: params.looping, view: params.view, nav: params.nav }
    }

    /// A recording opened from the interface simply plays.
    pub fn play() -> Self {
        Self { autoplay: true, ..Self::default() }
    }
}

/// How long seeks take, from the request to the picture being applied.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SeekStats {
    pub count: u32,
    pub last_ms: f64,
    pub max_ms: f64,
    pub total_ms: f64,
}

/// Time given to the download of a `.ply` or `.splat` a link names.
const SPLAT_TIMEOUT_MS: i32 = 120_000;

fn sink() -> ReplaySink {
    ReplaySink {
        on_message: Rc::new(|bytes| with_core(|core| core.apply_bytes(bytes))),
        on_reset: Rc::new(|| with_core(Core::forget_moment)),
        on_status: Rc::new(|status| with_core(|core| core.on_replay_status(status))),
        on_error: Rc::new(|message| {
            with_core(|core| {
                core.report_error(message);
                core.ui.replay.update(|view| {
                    if let Some(view) = view {
                        view.status.error.get_or_insert_with(|| "worker".to_string());
                    }
                });
            })
        }),
    }
}

impl Core {
    pub fn is_replay(&self) -> bool {
        self.replay.borrow().is_some()
    }

    /// Opens what a link (or a `share.json`) names: a recording is
    /// replayed, a splat file is shown as an import.
    pub fn open_link_file(&self, file: &FileRef, start: ReplayStart) {
        let Some(url) = dom::resolve_url(&file.url) else {
            self.notify(true, "Le fichier du lien ne peut pas être ouvert\u{a0}: son adresse n'est ni http ni https.".to_string());
            return;
        };
        match file.kind {
            FileKind::Recording => {
                let link = ReplayLink::open_url(url, sink());
                self.begin_replay(link, &file.name, ReplayOrigin::Url(file.url.clone()), start);
            }
            FileKind::Splats => {
                let name = file.name.clone();
                self.ui.link.set(Link::Connecting);
                spawn_local(async move {
                    let bytes = net::download(&url, crate::logic::files::MAX_IMPORT_BYTES, SPLAT_TIMEOUT_MS).await;
                    with_core(|core| match bytes {
                        Ok(bytes) => {
                            core.import_bytes(&name, &bytes);
                            // A link that asks for a walk without saying
                            // from where starts inside the scene (the link
                            // of a refined room).
                            if start.nav == Some(NavMode::Fly) && start.view.is_none() {
                                core.walk_into_file();
                            }
                            core.apply_start_view(start);
                        }
                        Err(DownloadError::Status(status)) => {
                            let text = replay::error_text(&format!("http_{status}"));
                            core.import_failed(text.replace("L'enregistrement", "Le fichier").replace("cet enregistrement", "ce fichier"))
                        }
                        Err(DownloadError::TooLarge) => core.import_failed("Le fichier du lien est trop volumineux.".to_string()),
                        Err(DownloadError::Network(reason)) => core.import_failed(format!(
                            "Le fichier du lien n'a pas pu être téléchargé ({reason}). Vérifiez l'adresse, et pour un autre site que celui-ci, qu'il autorise le partage (CORS)."
                        )),
                    });
                });
            }
        }
    }

    /// Replays a recording held in a `Blob`.
    pub fn open_recording_blob(&self, title: &str, blob: web_sys::Blob, origin: ReplayOrigin) {
        let link = ReplayLink::open_blob(title, blob, sink());
        self.begin_replay(link, title, origin, ReplayStart::play());
    }

    /// Replays a recording of the native server.
    pub fn open_server_recording(&self, title: &str, file_name: &str) {
        let Some(url) = self.server_url(&format!("/recordings/{}", crate::logic::share::encode_query_value(file_name))) else { return };
        let link = ReplayLink::open_url(url, sink());
        self.begin_replay(link, title, ReplayOrigin::Url(format!("recordings/{file_name}")), ReplayStart::play());
    }

    fn begin_replay(&self, link: Result<ReplayLink, String>, title: &str, origin: ReplayOrigin, start: ReplayStart) {
        let link = match link {
            Ok(link) => link,
            Err(error) => {
                self.notify(true, format!("L'enregistrement ne peut pas être lu\u{a0}: {error}."));
                return;
            }
        };
        // An imported file gives way without a detour through the live view.
        if self.file.take().is_some() {
            self.ui.file.set(None);
        }
        self.suspend_live();
        if start.looping {
            link.set_looping(true);
        }
        // Dropping the previous link ends the previous replay.
        *self.replay.borrow_mut() = Some(link);
        self.replay_start.set(Some(start));
        self.seek_sent.borrow_mut().clear();
        self.seek_target.set(None);
        self.seek_stats.set(SeekStats::default());
        self.clear_session();
        self.ui.scrub.set(None);
        let status = ReplayStatus { speed: 1.0, looping: start.looping, ..ReplayStatus::default() };
        self.ui.replay.set(Some(ReplayView { title: title.to_string(), origin, status }));
        self.sync_xr_timeline();
    }

    /// Keeps the live source quiet while a recording owns the view. The
    /// local pipeline is put on hold (its scan stays where it is); the
    /// connection to a server is dropped, since nothing here needs its
    /// megabytes per second, and opened again afterwards.
    fn suspend_live(&self) {
        if self.suspended.get() != Suspended::No {
            return;
        }
        let mut transport = self.transport.borrow_mut();
        // A recording in the worker's memory ends here: its stream stops.
        if self.ui.recording.with_untracked(|r| r.as_ref().is_some_and(|r| r.max_bytes.is_some())) {
            if let Some(transport) = transport.as_ref() {
                transport.request(&WorkerRequest::RecordStop);
            }
        }
        match transport.as_ref() {
            Some(Transport::Worker(_)) => {
                if let Some(transport) = transport.as_ref() {
                    transport.request(&WorkerRequest::Hold { on: true });
                }
                self.suspended.set(Suspended::WorkerHeld);
            }
            Some(Transport::Server(_)) => {
                // The relay worker dies with the transport. When it was
                // recording (a client of the LAN side), its take is still on
                // its way: the connection is kept, and what it delivers is
                // ignored while the recording plays.
                if self.ui.recording.with_untracked(|r| r.as_ref().is_some_and(|r| r.max_bytes.is_some())) {
                    self.suspended.set(Suspended::ServerKept);
                } else {
                    *transport = None;
                    self.suspended.set(Suspended::ServerDropped);
                }
            }
            None => {}
        }
    }

    /// Leaves the recording and shows the live source again (or finds one,
    /// when the page started on a link to a recording).
    pub fn close_replay(&self) {
        if self.replay.borrow_mut().take().is_none() {
            return;
        }
        self.ui.replay.set(None);
        self.ui.scrub.set(None);
        self.replay_start.set(None);
        self.sync_xr_timeline();
        self.resume_live();
    }

    /// Shows the live source again after a recording or a file of a link.
    pub(super) fn resume_live(&self) {
        self.clear_session();
        self.reset_view.set(true);
        match self.suspended.replace(Suspended::No) {
            Suspended::WorkerHeld => {
                if let Some(transport) = self.transport.borrow().as_ref() {
                    transport.request(&WorkerRequest::Hold { on: false });
                    transport.resync();
                }
            }
            Suspended::ServerKept => {
                if let Some(transport) = self.transport.borrow().as_ref() {
                    transport.resync();
                }
            }
            Suspended::ServerDropped => {
                let base = self.server_base.borrow().clone();
                match (base, self.ui.server.get_untracked()) {
                    (Some(base), Some(info)) => self.attach_server(base, info),
                    _ => spawn_local(super::connect()),
                }
            }
            Suspended::No if self.transport.borrow().is_none() => {
                // The page opened on a recording: there was no live source yet.
                self.ui.link.set(Link::Connecting);
                spawn_local(super::connect());
            }
            Suspended::No => {
                if let Some(transport) = self.transport.borrow().as_ref() {
                    transport.resync();
                }
            }
        }
    }

    /// Places the camera where a link asks, once the scene has placed it.
    pub(super) fn apply_start_view(&self, start: ReplayStart) {
        if let Some(nav) = start.nav {
            self.set_display(|d| {
                d.dollhouse = false;
                d.nav = nav;
            });
        }
        if let Some((eye, target)) = start.view {
            self.set_display(|d| d.dollhouse = false);
            self.viewer.set_view(Vec3::from_array(eye), Vec3::from_array(target));
        }
    }

    fn on_replay_status(&self, status: ReplayStatus) {
        let now = dom::now_ms();
        // Seek latency: from the request to its picture being applied (the
        // status follows the batch).
        {
            let mut sent = self.seek_sent.borrow_mut();
            let mut answered = None;
            while sent.front().is_some_and(|(id, _)| *id <= status.seek) {
                answered = sent.pop_front();
            }
            if let Some((_, at)) = answered {
                let elapsed = now - at;
                let mut stats = self.seek_stats.get();
                stats.count += 1;
                stats.last_ms = elapsed;
                stats.max_ms = stats.max_ms.max(elapsed);
                stats.total_ms += elapsed;
                self.seek_stats.set(stats);
            }
        }
        let first_picture = status.ready && self.ui.replay.with_untracked(|view| view.as_ref().is_some_and(|v| !v.status.ready));
        let error = status.error.clone().filter(|_| self.ui.replay.with_untracked(|v| v.as_ref().is_some_and(|v| v.status.error.is_none())));
        self.ui.replay.update(|view| {
            if let Some(view) = view {
                view.status = status;
            }
        });
        if let Some(code) = error {
            self.notify(true, replay::error_text(&code));
        }
        if first_picture {
            if let Some(start) = self.replay_start.take() {
                self.apply_start_view(start);
                if let Some(time) = start.time.filter(|t| *t > 0.0) {
                    self.replay_seek(time);
                }
                if start.autoplay {
                    if let Some(link) = self.replay.borrow().as_ref() {
                        link.play();
                    }
                }
            }
        }
        self.sync_xr_timeline();
    }

    // ------------------------------------------------------------ transport

    fn replay_status(&self) -> Option<ReplayStatus> {
        self.ui.replay.with_untracked(|view| view.as_ref().map(|v| v.status.clone()))
    }

    pub fn replay_toggle(&self) {
        let Some(status) = self.replay_status() else { return };
        if let Some(link) = self.replay.borrow().as_ref() {
            if status.playing { link.pause() } else { link.play() }
        }
    }

    /// Moves the play head to `time` seconds.
    pub fn replay_seek(&self, time: f64) {
        if !self.is_replay() {
            return;
        }
        let id = self.seek_serial.get().wrapping_add(1).max(1);
        self.seek_serial.set(id);
        self.seek_target.set(Some((id, time)));
        {
            let mut sent = self.seek_sent.borrow_mut();
            // Seeks the worker collapsed are answered by a later id.
            if sent.len() >= 256 {
                sent.pop_front();
            }
            sent.push_back((id, dom::now_ms()));
        }
        if let Some(link) = self.replay.borrow().as_ref() {
            link.seek(time, id);
        }
    }

    /// Moves the play head by `delta` seconds from where it is (or from
    /// where the user is dragging it).
    pub fn replay_seek_by(&self, delta: f64) {
        let Some(status) = self.replay_status() else { return };
        // From where the head is going, when a seek is still on its way:
        // key presses in quick succession add up.
        let from = match self.seek_target.get() {
            Some((id, target)) if id > status.seek => target,
            _ => status.time,
        };
        self.replay_seek(seek_target(from, delta, status.duration));
    }

    /// The user drags the time line: the head follows at once on screen,
    /// the picture as fast as the player delivers.
    pub fn replay_scrub(&self, time: f64) {
        self.ui.scrub.set(Some(time));
        self.replay_seek(time);
    }

    pub fn replay_scrub_end(&self) {
        self.ui.scrub.set(None);
    }

    pub fn replay_end(&self) {
        if let Some(duration) = self.replay_status().and_then(|s| s.duration) {
            self.replay_seek(duration);
        }
    }

    pub fn replay_speed(&self, speed: f64) {
        if let Some(link) = self.replay.borrow().as_ref() {
            link.set_speed(speed);
        }
    }

    pub fn replay_loop(&self, on: bool) {
        if let Some(link) = self.replay.borrow().as_ref() {
            link.set_looping(on);
        }
    }

    pub fn seek_stats(&self) -> SeekStats {
        self.seek_stats.get()
    }

    /// The link to this moment and this point of view of a recording that
    /// has an address: the page's own URL with `file`, `t` and `view`.
    pub fn moment_link(&self) -> Option<String> {
        let view = self.ui.replay.get_untracked()?;
        let ReplayOrigin::Url(file) = &view.origin else { return None };
        let (eye, target) = (self.viewer.eye().to_array(), self.viewer.target().to_array());
        let query = crate::logic::share::share_query(file, self.ui.embed.get_untracked(), false, view.status.looping, Some(view.status.time), Some((eye, target)));
        Some(format!("{}{query}", dom::page_base()))
    }

    /// Copies [`Core::moment_link`] to the clipboard.
    pub fn copy_moment_link(&self) {
        let Some(link) = self.moment_link() else { return };
        spawn_local(async move {
            let copied = dom::copy_text(&link).await;
            with_core(|core| {
                if copied {
                    core.notify(false, "Lien vers ce moment et ce point de vue copié.".to_string());
                } else {
                    core.notify(true, format!("La copie n'est pas autorisée ici. Le lien\u{a0}: {link}"));
                }
            });
        });
    }

    // ------------------------------------------------------------------- VR

    /// Shows the time line in the headset and listens to its controllers
    /// while a recording plays.
    pub(super) fn sync_xr_timeline(&self) {
        let timeline = self.ui.replay.with_untracked(|view| {
            view.as_ref().map(|v| {
                let duration = v.status.duration.unwrap_or(0.0);
                let fraction = |t: f64| if duration > 0.0 { (t / duration).clamp(0.0, 1.0) as f32 } else { 0.0 };
                gs_render::XrTimeline { progress: fraction(v.status.time), buffered: fraction(v.status.buffered), playing: v.status.playing }
            })
        });
        self.viewer.set_xr_timeline(timeline);
    }

    /// A controller asked for something of the time line.
    pub(super) fn on_xr_transport(&self, event: gs_render::XrTransport) {
        match event {
            gs_render::XrTransport::TogglePlay => self.replay_toggle(),
            gs_render::XrTransport::Scrub(amount) => {
                let Some(status) = self.replay_status() else { return };
                let rate = replay::scrub_rate(status.duration.unwrap_or(0.0));
                self.replay_seek_by(f64::from(amount) * rate);
            }
        }
    }

    /// Pause and resume, whatever the source: the play head of a
    /// recording, or the pipeline.
    pub fn toggle_pause(&self) {
        if self.is_replay() {
            self.replay_toggle();
            return;
        }
        // The echoed configuration is the truth; without one there is
        // nothing to pause yet.
        let Some(config) = self.ui.config.get_untracked() else { return };
        // An imported file has nothing to pause, and a capture is paused
        // on the phone (the server ignores the control).
        if self.file.get().is_some() || self.ui.capture.with_untracked(Option::is_some) {
            return;
        }
        self.send(ControlMsg::SetPaused { paused: !config.paused });
    }
}

