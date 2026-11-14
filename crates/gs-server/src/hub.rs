//! The pipeline thread.
//!
//! One OS thread owns what viewers are shown: the simulated [`Pipeline`]
//! and, while there is one, the capture of a phone (a
//! [`CaptureSession`]). Everything reaches it as a [`Command`]: viewers
//! joining and leaving, control messages, the hello, the messages and the
//! frames of a phone, an imported capture to show, the record button,
//! shutdown. Commands are handled one at a time, so a snapshot is always
//! consistent with the messages broadcast after it, and the frames of a
//! phone are fused in the order they arrived.
//!
//! **Sources.** Without a capture the thread ticks the pipeline at
//! `config.target_hz` while somebody watches or records, and sleeps on its
//! command channel otherwise. A capture replaces the simulation for every
//! viewer: the pipeline is parked (it does not tick) until a viewer asks
//! for a simulated scene again (`set_scene`), which is refused only while
//! a phone is still capturing.
//!
//! **Recording.** The thread writes the recordings that follow the stream
//! (`gs_core::record`): the one of the record button, fed with everything
//! that is broadcast, and the one of a `live` capture. A fused capture is
//! saved as a still when it finishes.
//!
//! Broadcasting is a non-blocking push into every client's [`Outbox`]; the
//! thread never waits for a socket.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bytes::Bytes;
use gs_core::ingest::{
    CaptureSession, ClientMessage, FrameHeader, Hello, IngestParams, Reply, ServerMessage, SessionError,
    RECOMMENDED_MAX_INFLIGHT,
};
use gs_core::pipeline::{Outgoing, Pipeline};
use gs_core::protocol::{CaptureMode, ControlMsg, PipelineConfig};
use tokio::sync::{mpsc as async_mpsc, oneshot, OwnedSemaphorePermit};

use crate::capture::{capture_stem, capture_title, Imported, RawCapture};
use crate::outbox::{Outbox, Shared};
use crate::recording::{save_still, EndReason, FileRecording, RecordingStatus, Saved, SharedRecordState};
use crate::store::{self, Kind, Store};

/// Longest real time one tick may account for. A longer gap (the process
/// was suspended, the machine slept) must not make the scene jump.
const MAX_TICK_DT: Duration = Duration::from_secs(1);

/// Once a tick is due, pending commands are still served for this long
/// before ticking (at least one is always served). Keeps joins and control
/// messages responsive when the pipeline cannot hold its target rate,
/// without letting a flood of commands starve the ticks.
const LATE_COMMAND_BUDGET: Duration = Duration::from_millis(4);

/// How many times in a row the pipeline is rebuilt after a panic before the
/// thread gives up. A panic is a bug; restarting keeps the server usable,
/// the cap keeps a bug that strikes at every start from spinning forever.
const MAX_RESTARTS: u32 = 5;

/// A pipeline that ran this long before it panicked was not in a crash
/// loop: the count of restarts starts again from zero. A rare bug then
/// costs a reconnection each time it strikes, not the stream for good on
/// its sixth occurrence, days later.
const STABLE_RUN: Duration = Duration::from_secs(if cfg!(test) { 2 } else { 60 });

/// How often a running capture reports its progress to the phone and
/// brings viewers up to date with what changed slowly.
const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);

/// A phone that has been silent for this long while another hello arrives
/// is taken for a dead connection (the phone pings every 5 s): the newcomer
/// gets the capture instead of a refusal.
const GHOST_AFTER: Duration = Duration::from_secs(12);

/// Frames a phone may have unacknowledged, announced in the `welcome`.
pub(crate) const MAX_INFLIGHT: u32 = RECOMMENDED_MAX_INFLIGHT;

/// Error code of a refusal that no fault of the phone explains: another
/// phone is capturing, or a newer connection took this one's place.
pub(crate) const CODE_BUSY: &str = "busy";

/// When a connection was last heard, readable from another thread.
pub(crate) struct LastHeard(AtomicU64);

fn process_epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

impl LastHeard {
    pub fn now() -> Self {
        let heard = Self(AtomicU64::new(0));
        heard.note();
        heard
    }

    pub fn note(&self) {
        self.0.store(process_epoch().elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    fn silence(&self) -> Duration {
        let now = process_epoch().elapsed().as_millis() as u64;
        Duration::from_millis(now.saturating_sub(self.0.load(Ordering::Relaxed)))
    }
}

/// What the pipeline thread asks the task of a phone connection to send.
#[derive(Debug)]
pub(crate) enum PhoneOut {
    /// A text frame (`welcome`, `ack`, `progress`).
    Text(String),
    /// An `error` message, then a close with `close_code`.
    Refuse { code: String, message: String, close_code: u16 },
    /// A close without an error: the capture this phone fed is over.
    Close,
}

impl PhoneOut {
    pub fn error(error: &SessionError) -> Self {
        Self::Refuse {
            code: error.code.as_str().to_string(),
            message: error.message.clone(),
            close_code: error.code.close_code(),
        }
    }

    fn busy(message: &str) -> Self {
        Self::Refuse { code: CODE_BUSY.to_string(), message: message.to_string(), close_code: 1008 }
    }
}

/// The pipeline thread's end of one phone connection.
pub(crate) struct PhoneLink {
    /// Session id of the connection.
    pub conn: u64,
    pub tx: async_mpsc::UnboundedSender<PhoneOut>,
    /// Frames read from the socket whose `ack` has not been queued yet.
    /// The connection counts up, the pipeline thread down.
    pub inflight: Arc<AtomicU32>,
    pub heard: Arc<LastHeard>,
}

impl PhoneLink {
    fn send(&self, message: &ServerMessage) {
        if matches!(message, ServerMessage::Ack { .. }) {
            // Before the ack is queued: the phone can only send its next
            // frame after it has read this one, so the count never runs
            // ahead of the phone's own.
            let _ = self.inflight.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| Some(n.saturating_sub(1)));
        }
        let _ = self.tx.send(PhoneOut::Text(message.to_json()));
    }

    fn out(&self, message: PhoneOut) {
        let _ = self.tx.send(message);
    }
}

/// Why the record button could not do what was asked.
#[derive(Debug)]
pub(crate) enum RecordRefusal {
    /// A recording is already running.
    AlreadyRecording(RecordingStatus),
    /// No recording is running.
    NotRecording,
    /// The server was started without a recordings directory.
    NoDirectory,
    /// The file could not be created.
    Disk(String),
}

/// A request to the pipeline thread.
pub(crate) enum Command {
    /// A new client: register it and give it a snapshot.
    Join(Arc<Outbox>),
    /// The client with this id dropped its backlog and wants a snapshot.
    Resync(u64),
    /// The client with this id is gone.
    Leave(u64),
    /// Apply a control message and broadcast the result. The permit is the
    /// sender's flow-control token; dropping it lets the client send more.
    Control(ControlMsg, Option<OwnedSemaphorePermit>),
    /// A phone said hello (token already checked and removed). The answer,
    /// `welcome` or a refusal, goes through the link.
    PhoneHello { hello: Hello, link: PhoneLink },
    /// A text message of the phone with this connection id.
    PhoneText { conn: u64, message: ClientMessage, permit: Option<OwnedSemaphorePermit> },
    /// A capture frame of the phone with this connection id.
    PhoneFrame { conn: u64, bytes: Bytes },
    /// The connection of a phone ended.
    PhoneGone(u64),
    /// Show an imported capture to every viewer. Answers `false` when a
    /// phone is capturing: the import is kept on disk but not shown.
    Show { imported: Box<Imported>, reply: oneshot::Sender<bool> },
    /// Start recording the stream under this title.
    RecordStart { title: Option<String>, reply: oneshot::Sender<Result<RecordingStatus, RecordRefusal>> },
    /// Stop the recording of the stream.
    RecordStop { reply: oneshot::Sender<Result<Saved, RecordRefusal>> },
    /// Broadcast arbitrary messages as if the pipeline had produced them.
    #[cfg(test)]
    Inject(Vec<Outgoing>),
    /// Panic, as a bug in the pipeline would.
    #[cfg(test)]
    Panic,
    /// Stop the thread.
    Shutdown,
}

/// Counters published by the pipeline thread.
#[derive(Default)]
pub(crate) struct HubMetrics {
    pub ticks: AtomicU64,
    pub resyncs: AtomicU64,
    pub controls: AtomicU64,
    /// `f32` bits of [`Pipeline::scene_time`] after the latest tick or
    /// control message.
    pub scene_time: AtomicU32,
    /// Clients registered with the pipeline thread.
    pub registered: AtomicU64,
    /// Bytes held in the outboxes of all clients right after the latest
    /// broadcast or snapshot.
    pub queued_bytes: AtomicU64,
    /// Capture frames of phones handled so far.
    pub capture_frames: AtomicU64,
    /// Viewers are shown a capture instead of the simulation.
    pub capture_shown: AtomicBool,
    /// A phone is connected and its capture is not finished.
    pub capturing: AtomicBool,
}

/// What the pipeline thread is started with.
pub(crate) struct HubSetup {
    pub pipeline: PipelineConfig,
    pub store: Arc<Store>,
    /// State of the record button, for the request handlers.
    pub record: SharedRecordState,
    /// Reconstruction parameters of phone captures.
    pub ingest: IngestParams,
    /// Size limit of one recording.
    pub max_recording_bytes: u64,
    /// Keep the stream of a phone as a `.gscap`.
    pub keep_captures: bool,
}

/// Owner side of the pipeline thread.
pub(crate) struct Hub {
    tx: Sender<Command>,
    thread: std::sync::Mutex<Option<JoinHandle<()>>>,
    pub metrics: Arc<HubMetrics>,
}

impl Hub {
    /// A hub without directories: nothing is recorded or kept.
    #[cfg(test)]
    pub fn start(config: PipelineConfig) -> std::io::Result<Self> {
        Self::start_with(HubSetup {
            pipeline: config,
            store: Arc::new(Store::new(None, None)),
            record: SharedRecordState::default(),
            ingest: IngestParams::default(),
            max_recording_bytes: u64::MAX,
            keep_captures: false,
        })
    }

    /// Starts the thread. The pipeline is built on it, so this returns at
    /// once even for a heavy configuration; clients that join meanwhile
    /// get their snapshot as soon as it is ready.
    pub fn start_with(setup: HubSetup) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let metrics = Arc::new(HubMetrics::default());
        let thread = {
            let metrics = metrics.clone();
            std::thread::Builder::new().name("gs-pipeline".into()).spawn(move || {
                let mut clients = Vec::new();
                let mut restarts = 0;
                loop {
                    let started = Instant::now();
                    let outcome = catch_unwind(AssertUnwindSafe(|| run(&setup, &rx, &metrics, &mut clients)));
                    // However the run ended, its viewers are let go: no
                    // socket may wait forever on a pipeline that is gone,
                    // and after a panic their stream cannot be trusted.
                    for client in clients.drain(..) {
                        client.close();
                    }
                    metrics.registered.store(0, Ordering::Relaxed);
                    metrics.queued_bytes.store(0, Ordering::Relaxed);
                    // Neither does a capture or a recording outlive its run
                    // (their files are closed, cut short after a panic).
                    metrics.capture_shown.store(false, Ordering::Relaxed);
                    metrics.capturing.store(false, Ordering::Relaxed);
                    setup.record.lock().unwrap_or_else(|p| p.into_inner()).recording = None;
                    if started.elapsed() >= STABLE_RUN {
                        restarts = 0;
                    }
                    match outcome {
                        Ok(()) => break,
                        // Start again from the initial configuration (the
                        // one that led to the panic is not replayed);
                        // viewers reconnect by themselves.
                        Err(_) if restarts < MAX_RESTARTS => {
                            restarts += 1;
                            log::error!("le pipeline s'est arrêté sur une erreur interne ; redémarrage ({restarts}/{MAX_RESTARTS})");
                        }
                        Err(_) => {
                            log::error!("le pipeline s'est arrêté sur une erreur interne ; il ne sera plus redémarré");
                            break;
                        }
                    }
                }
                // Dropping `rx` here makes every later `send` fail, which
                // is how sessions learn that the pipeline is gone for good.
            })?
        };
        Ok(Self { tx, thread: std::sync::Mutex::new(Some(thread)), metrics })
    }

    /// Sends a command. `false` when the thread is gone.
    pub fn send(&self, command: Command) -> bool {
        self.tx.send(command).is_ok()
    }

    /// Asks the thread to stop and returns its join handle (once).
    pub fn stop(&self) -> Option<JoinHandle<()>> {
        let _ = self.tx.send(Command::Shutdown);
        self.thread.lock().unwrap_or_else(|p| p.into_inner()).take()
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        // Not joined here: a tick may take a while and drop must not block.
        let _ = self.tx.send(Command::Shutdown);
    }
}

fn tick_period(pipeline: &Pipeline) -> Duration {
    // `target_hz` is sanitized to [1, 120] by the pipeline; the clamp only
    // protects the division.
    Duration::from_secs_f64(1.0 / f64::from(pipeline.config().target_hz).clamp(0.1, 1000.0))
}

fn share(messages: Vec<Outgoing>) -> Vec<Shared> {
    messages.into_iter().map(|m| Shared { bytes: Bytes::from(m.bytes), reliable: m.reliable, slot: m.slot }).collect()
}

/// The phone that feeds a capture.
struct Phone {
    link: PhoneLink,
    /// Its hello, without the token.
    hello: Hello,
}

/// A capture shown to the viewers in place of the simulation.
struct Capture {
    session: CaptureSession,
    /// Fed by a phone (as opposed to imported from a file): only such a
    /// capture is continued by a phone that says hello again.
    from_phone: bool,
    /// The phone feeding it, while it is connected.
    phone: Option<Phone>,
    /// The stream of the phone, kept on disk.
    raw: Option<RawCapture>,
    /// The recording that follows a `live` capture.
    stream: Option<FileRecording>,
    /// Stem of the files this capture leaves behind.
    stem: String,
    /// File name of the still saved when the capture last finished: a
    /// capture that is taken up again and finished again replaces it.
    still: Option<String>,
    /// When the next progress report is due.
    next_progress: Instant,
    /// Highest frame index the phone has sent in this capture.
    last_index: Option<u32>,
    /// Set while a live capture plays in front of an earlier scan and has
    /// not received a frame yet: the highest frame index of that scan.
    /// The app's index only grows while the app lives, and so does its AR
    /// world; a first frame at or below this value comes from a relaunched
    /// app, whose world is another one: the scan must go.
    backdrop_until: Option<u32>,
    /// The floor height the phone told since its latest hello.
    floor_told: Option<f32>,
}

impl Capture {
    /// A phone is connected and has not finished: the capture is going on.
    fn is_running(&self) -> bool {
        self.phone.is_some() && !self.session.is_finished()
    }
}

struct Loop<'a> {
    pipeline: Pipeline,
    clients: &'a mut Vec<Arc<Outbox>>,
    metrics: &'a HubMetrics,
    setup: &'a HubSetup,
    /// The snapshot of the current state, encoded at most once between two
    /// changes and shared by every client that asks for it.
    snapshot: Option<Vec<Bytes>>,
    /// The capture that replaces the simulation, if any.
    capture: Option<Box<Capture>>,
    /// The recording of the record button.
    manual: Option<FileRecording>,
}

impl Loop<'_> {
    /// Hands `messages` to the recording of the record button and to
    /// every viewer.
    fn broadcast(&mut self, messages: Vec<Outgoing>) {
        if messages.is_empty() {
            return;
        }
        if let Some(recording) = self.manual.as_mut() {
            match messages.iter().find_map(|message| recording.push(&message.bytes)) {
                Some(reason) => {
                    self.end_manual(reason);
                }
                None => self.publish_recording(),
            }
        }
        if self.clients.is_empty() {
            return;
        }
        let batch = share(messages);
        for client in self.clients.iter() {
            if client.push(&batch) {
                log::debug!("client {}: reliable backlog over its bound, resynchronising", client.id());
            }
        }
        self.publish_queued();
    }

    /// A handful of short locks per broadcast: the price of being able to
    /// see that a viewer which reads nothing holds next to nothing.
    fn publish_queued(&self) {
        let queued: usize = self.clients.iter().map(|client| client.held().1).sum();
        self.metrics.queued_bytes.store(queued as u64, Ordering::Relaxed);
    }

    /// What a viewer joining now must receive.
    fn source_snapshot(&mut self) -> Vec<Outgoing> {
        match self.capture.as_mut() {
            Some(capture) => capture.session.snapshot(),
            None => self.pipeline.snapshot(),
        }
    }

    fn sync(&mut self, client: &Outbox) -> bool {
        if self.snapshot.is_none() {
            let snapshot = self.source_snapshot().into_iter().map(|m| Bytes::from(m.bytes)).collect();
            self.snapshot = Some(snapshot);
        }
        client.sync(self.snapshot.as_deref().unwrap_or_default())
    }

    fn publish_scene_time(&self) {
        self.metrics.scene_time.store(self.pipeline.scene_time().to_bits(), Ordering::Relaxed);
    }

    fn publish_capture(&self) {
        self.publish_capture_as(self.capture.as_deref());
    }

    /// Tells the request handlers whether a capture is shown and whether a
    /// phone is at it.
    fn publish_capture_as(&self, capture: Option<&Capture>) {
        self.metrics.capture_shown.store(capture.is_some(), Ordering::Relaxed);
        self.metrics.capturing.store(capture.is_some_and(Capture::is_running), Ordering::Relaxed);
    }

    /// The simulation runs: nobody is shown a capture, and somebody
    /// watches or records.
    fn ticking(&self) -> bool {
        self.capture.is_none() && (!self.clients.is_empty() || self.manual.is_some())
    }

    // -- the record button -----------------------------------------------

    fn record_state(&self) -> std::sync::MutexGuard<'_, crate::recording::RecordState> {
        self.setup.record.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn publish_recording(&self) {
        let status = self.manual.as_ref().map(FileRecording::status);
        self.record_state().recording = status;
    }

    fn start_manual(&mut self, title: Option<String>) -> Result<RecordingStatus, RecordRefusal> {
        if let Some(recording) = &self.manual {
            return Err(RecordRefusal::AlreadyRecording(recording.status()));
        }
        if self.setup.store.dir(Kind::Recording).is_none() {
            return Err(RecordRefusal::NoDirectory);
        }
        let (stem, title) = match title.map(|t| t.trim().to_string()).filter(|t| !t.is_empty()) {
            Some(title) => (store::slug(&title, "enregistrement"), title),
            None => {
                let stem = format!("enregistrement-{}", store::timestamp());
                (stem.clone(), stem)
            }
        };
        let snapshot = self.source_snapshot();
        let recording =
            FileRecording::start(&self.setup.store, &stem, &title, self.setup.max_recording_bytes, &snapshot)
                .map_err(|err| RecordRefusal::Disk(err.to_string()))?;
        let status = recording.status();
        log::info!("recording started: {}", status.file);
        self.manual = Some(recording);
        self.publish_recording();
        Ok(status)
    }

    /// Ends the recording of the record button, if one runs.
    fn end_manual(&mut self, reason: EndReason) -> Option<Saved> {
        let saved = self.manual.take()?.finish(reason);
        log::info!("recording {} ended ({reason:?}): {} bytes, {:.1} s", saved.file, saved.size, saved.duration_s);
        let mut state = self.record_state();
        state.recording = None;
        state.last = Some(saved.clone());
        Some(saved)
    }

    // -- captures ------------------------------------------------------------

    /// Sends what a call into the capture session produced: to the phone,
    /// into the recording of a `live` capture, to the viewers.
    fn emit(&mut self, capture: &mut Capture, reply: Reply) {
        if let Some(phone) = &capture.phone {
            for message in &reply.to_phone {
                phone.link.send(message);
            }
        }
        if let Some(recording) = capture.stream.as_mut() {
            if let Some(reason) = reply.to_viewers.iter().find_map(|message| recording.push(&message.bytes)) {
                Self::end_stream(capture, reason);
            }
        }
        self.snapshot = None;
        self.broadcast(reply.to_viewers);
    }

    fn end_stream(capture: &mut Capture, reason: EndReason) {
        if let Some(recording) = capture.stream.take() {
            let saved = recording.finish(reason);
            log::info!("capture recording {} ended ({reason:?}): {} bytes", saved.file, saved.size);
        }
    }

    /// Starts the recording that follows a `live` capture.
    fn start_stream(&self, capture: &mut Capture, snapshot: &[Outgoing]) {
        if capture.session.mode() != CaptureMode::Live || self.setup.store.dir(Kind::Recording).is_none() {
            return;
        }
        let title = capture_title(capture.session.name()).to_string();
        match FileRecording::start(&self.setup.store, &capture.stem, &title, self.setup.max_recording_bytes, snapshot) {
            Ok(recording) => capture.stream = Some(recording),
            Err(err) => log::error!("la capture en direct ne sera pas enregistrée : {err}"),
        }
    }

    /// The capture finished: what it leaves on disk is brought up to date
    /// and it is saved as a recording.
    fn capture_finished(&self, capture: &mut Capture) {
        if let Some(raw) = capture.raw.as_mut() {
            raw.checkpoint();
        }
        if capture.stream.is_some() {
            Self::end_stream(capture, EndReason::Stopped);
        } else if capture.session.mode().fuses()
            && capture.session.splat_count() > 0
            && self.setup.store.dir(Kind::Recording).is_some()
        {
            let title = capture_title(capture.session.name()).to_string();
            let snapshot = capture.session.snapshot();
            match save_still(&self.setup.store, &capture.stem, capture.still.as_deref(), &title, &snapshot) {
                Ok(saved) => {
                    log::info!("capture saved as {} ({} bytes)", saved.file, saved.size);
                    capture.still = Some(saved.file);
                }
                Err(err) => log::error!("la capture n'a pas pu être enregistrée : {err}"),
            }
        }
    }

    /// Lets go of a capture: its files are closed, its phone is told.
    fn close_capture(&self, mut capture: Box<Capture>) -> CaptureSession {
        // A scan whose phone never said `finish` (the app was closed, the
        // phone left the network) is finished here: it is saved as a
        // recording like any other instead of surviving only as a raw file.
        if capture.from_phone && !capture.session.is_finished() && capture.session.frames_used() > 0 {
            // Nobody is shown this capture any longer: the reply is dropped.
            let _ = capture.session.finish();
            self.capture_finished(&mut capture);
        }
        let Capture { session, phone, raw, stream, .. } = *capture;
        if let Some(phone) = phone {
            phone.link.out(PhoneOut::Close);
        }
        if let Some(raw) = raw {
            raw.close();
        }
        if let Some(recording) = stream {
            recording.finish(EndReason::Stopped);
        }
        session
    }

    fn open_raw(&self, stem: &str, hello: &Hello) -> Option<RawCapture> {
        if !self.setup.keep_captures || self.setup.store.dir(Kind::Capture).is_none() {
            return None;
        }
        match RawCapture::create(&self.setup.store, stem, hello) {
            Ok(raw) => Some(raw),
            Err(err) => {
                log::error!("la capture ne sera pas conservée sur le disque : {err}");
                None
            }
        }
    }

    fn phone_hello(&mut self, hello: Hello, link: PhoneLink) {
        let mut previous = self.capture.take();
        if let Some(capture) = previous.as_mut() {
            if let Some(old) = &capture.phone {
                // The phone itself coming back (it sends the same hello on
                // every connection), or a connection that died unnoticed,
                // must never lock the capture; another phone must not
                // break into one that is going on.
                let same = old.hello == hello;
                let silent = old.link.heard.silence() > GHOST_AFTER;
                if capture.is_running() && !same && !silent {
                    log::info!("phone {}: refused, another one is capturing", link.conn);
                    link.out(PhoneOut::busy("Un autre appareil est déjà en train de capturer sur ce serveur."));
                    self.capture = previous;
                    return;
                }
                old.link.out(if capture.is_running() || same {
                    PhoneOut::busy("Cette connexion a été remplacée par une connexion plus récente.")
                } else {
                    // A phone that had finished: its capture gives way.
                    PhoneOut::Close
                });
                capture.phone = None;
            }
        }

        let welcome = ServerMessage::welcome(MAX_INFLIGHT);
        // The same capture goes on when the phone returns with the name
        // and mode it had. Another model of device is another phone, with
        // another AR world: its frames must not land in this scan.
        let continues = |capture: &Capture| {
            capture.from_phone && capture.session.matches(&hello) && capture.session.hello().device == hello.device
        };
        let capture = match previous {
            Some(mut capture) if continues(&capture) => {
                log::info!("phone {}: continues the {} capture", link.conn, hello.mode.name());
                let restarted = capture.session.is_finished();
                let reply = capture.session.resume();
                link.send(&welcome);
                capture.phone = Some(Phone { link, hello });
                capture.floor_told = None;
                // Published before anybody hears of it, here and below: a
                // request made on seeing the capture must find it.
                self.publish_capture_as(Some(&capture));
                if restarted {
                    let snapshot = capture.session.snapshot();
                    self.start_stream(&mut capture, &snapshot);
                }
                self.emit(&mut capture, reply);
                capture
            }
            previous => {
                log::info!("phone {}: new {} capture from {:?}", link.conn, hello.mode.name(), hello.device);
                // A live capture right after a scan by the same phone
                // plays in front of that scan: the app keeps its AR
                // session, hence its world, between two captures. (A
                // relaunched app starts its frame index again, which makes
                // the session start from nothing.)
                let backdrop_until = previous.as_ref().and_then(|old| {
                    let fits = old.from_phone
                        && hello.mode == CaptureMode::Live
                        && old.session.mode().fuses()
                        && old.session.is_finished()
                        && old.session.splat_count() > 0
                        && old.session.hello().device == hello.device;
                    old.last_index.filter(|_| fits)
                });
                let old = previous.map(|old| self.close_capture(old));
                let session = match old {
                    Some(old) if backdrop_until.is_some() => old.into_live(&hello),
                    _ => CaptureSession::new(&hello, self.setup.ingest),
                };
                let mut stem = capture_stem(&hello.name);
                let raw = self.open_raw(&stem, &hello);
                // The recordings of this capture take the name its raw file
                // got (numbered when the plain name was taken), so that the
                // files of one capture go together.
                if let Some(raw) = &raw {
                    stem = raw.stem().to_string();
                }
                link.send(&welcome);
                let mut capture = Box::new(Capture {
                    session,
                    from_phone: true,
                    phone: Some(Phone { link, hello }),
                    raw,
                    stream: None,
                    stem,
                    still: None,
                    next_progress: Instant::now() + PROGRESS_INTERVAL,
                    last_index: None,
                    backdrop_until,
                    floor_told: None,
                });
                // Every viewer changes source: the whole session, as for
                // one that joins.
                let snapshot = capture.session.snapshot();
                self.start_stream(&mut capture, &snapshot);
                self.publish_capture_as(Some(&capture));
                self.snapshot = None;
                self.broadcast(snapshot);
                capture
            }
        };
        self.capture = Some(capture);
        self.publish_capture();
    }

    /// The capture, if `conn` is the phone feeding it.
    fn take_capture_of(&mut self, conn: u64) -> Option<Box<Capture>> {
        let fed_by_conn = self.capture.as_ref()?.phone.as_ref().is_some_and(|phone| phone.link.conn == conn);
        if fed_by_conn {
            self.capture.take()
        } else {
            None
        }
    }

    fn phone_text(&mut self, conn: u64, message: ClientMessage) {
        // A message of a connection that was refused or replaced.
        let Some(mut capture) = self.take_capture_of(conn) else { return };
        let was_finished = capture.session.is_finished();
        let reply = capture.session.handle_message(&message);
        match message {
            ClientMessage::Region(region) => {
                if let Some(raw) = capture.raw.as_mut() {
                    raw.set_region(region);
                }
            }
            ClientMessage::Floor { y } => {
                capture.floor_told = Some(y);
                if let Some(raw) = capture.raw.as_mut() {
                    raw.set_floor(y);
                }
            }
            ClientMessage::Reset => {
                // The phone threw everything away and starts a new world:
                // what is on disk belongs to the old one and must not be
                // fused with what follows when the file is imported again.
                if let Some(raw) = capture.raw.take() {
                    raw.discard();
                }
                if let Some(phone) = &capture.phone {
                    capture.raw = self.open_raw(&capture.stem, &phone.hello);
                }
            }
            _ => {}
        }
        let finished = capture.session.is_finished();
        // Before the phone has its answer (and long before the files are
        // written): whoever acts on that answer must find the capture as
        // it now is.
        self.publish_capture_as(Some(&capture));
        self.emit(&mut capture, reply);
        if finished && !was_finished {
            self.capture_finished(&mut capture);
        } else if was_finished && !finished {
            // Taken up again after its end: a live capture is recorded
            // again, as a new file.
            let snapshot = capture.session.snapshot();
            self.start_stream(&mut capture, &snapshot);
        }
        self.capture = Some(capture);
        self.publish_capture();
    }

    fn phone_frame(&mut self, conn: u64, bytes: &[u8]) {
        let Some(mut capture) = self.take_capture_of(conn) else { return };
        self.metrics.capture_frames.fetch_add(1, Ordering::Relaxed);
        let index = FrameHeader::parse(bytes).ok().map(|header| header.index);
        if let (Some(index), Some(scan_end)) = (index, capture.backdrop_until.take()) {
            if index <= scan_end {
                self.drop_backdrop(&mut capture);
            }
        }
        match capture.session.handle_frame(bytes) {
            Ok(reply) => {
                capture.last_index = capture.last_index.max(index);
                if let Some(raw) = capture.raw.as_mut() {
                    raw.append(bytes);
                }
                self.emit(&mut capture, reply);
            }
            Err(error) => {
                log::warn!("phone {conn}: {error}");
                if let Some(phone) = capture.phone.take() {
                    phone.link.out(PhoneOut::error(&error));
                }
            }
        }
        self.capture = Some(capture);
        self.publish_capture();
    }

    /// The live capture turned out to come from a relaunched app: the scan
    /// behind it belongs to another world. Viewers get the capture again,
    /// empty.
    fn drop_backdrop(&mut self, capture: &mut Capture) {
        let Some(phone) = &capture.phone else { return };
        log::info!("phone {}: the app was relaunched, the scan behind the live capture is dropped", phone.link.conn);
        let mut session = CaptureSession::new(&phone.hello, self.setup.ingest);
        // The floor the phone told since this hello belongs to the new
        // world; what the old scan knew does not.
        if let Some(y) = capture.floor_told {
            session.handle_message(&ClientMessage::Floor { y });
        }
        capture.session = session;
        let snapshot = capture.session.snapshot();
        if let Some(recording) = capture.stream.as_mut() {
            if let Some(reason) = snapshot.iter().find_map(|message| recording.push(&message.bytes)) {
                Self::end_stream(capture, reason);
            }
        }
        self.snapshot = None;
        self.broadcast(snapshot);
    }

    fn phone_gone(&mut self, conn: u64) {
        let Some(mut capture) = self.take_capture_of(conn) else { return };
        log::info!("phone {conn}: disconnected");
        capture.phone = None;
        // The file is complete up to here, should the phone not return.
        if let Some(raw) = capture.raw.as_mut() {
            raw.checkpoint();
        }
        self.capture = Some(capture);
        self.publish_capture();
    }

    fn capture_progress(&mut self) {
        let Some(mut capture) = self.capture.take() else { return };
        capture.next_progress = Instant::now() + PROGRESS_INTERVAL;
        let reply = capture.session.progress();
        self.emit(&mut capture, reply);
        self.capture = Some(capture);
    }

    /// When the running capture wants its next progress report.
    fn progress_due(&self) -> Option<Instant> {
        self.capture.as_ref().filter(|capture| capture.is_running()).map(|capture| capture.next_progress)
    }

    fn show(&mut self, imported: Imported) -> bool {
        if self.capture.as_ref().is_some_and(|capture| capture.is_running()) {
            return false;
        }
        if let Some(old) = self.capture.take() {
            self.close_capture(old);
        }
        let Imported { mut session, .. } = imported;
        let snapshot = session.snapshot();
        self.capture = Some(Box::new(Capture {
            session,
            from_phone: false,
            phone: None,
            raw: None,
            stream: None,
            stem: String::new(),
            still: None,
            next_progress: Instant::now(),
            last_index: None,
            backdrop_until: None,
            floor_told: None,
        }));
        self.publish_capture();
        self.snapshot = None;
        self.broadcast(snapshot);
        true
    }

    fn control(&mut self, msg: ControlMsg) {
        if let Some(capture) = self.capture.as_ref() {
            // The controls drive the simulation, which is parked. Asking
            // for a simulated scene is how viewers leave a capture; while
            // the phone is still at it, the capture stays.
            if !matches!(msg, ControlMsg::SetScene { .. }) || capture.is_running() {
                return;
            }
            if let Some(old) = self.capture.take() {
                self.close_capture(old);
            }
            self.publish_capture();
            let _ = self.pipeline.handle_control(msg);
            self.snapshot = None;
            self.metrics.controls.fetch_add(1, Ordering::Relaxed);
            self.publish_scene_time();
            // Every viewer changes source: the whole session.
            let snapshot = self.pipeline.snapshot();
            self.broadcast(snapshot);
            return;
        }
        let out = self.pipeline.handle_control(msg);
        self.snapshot = None;
        self.metrics.controls.fetch_add(1, Ordering::Relaxed);
        self.publish_scene_time();
        self.broadcast(out);
    }

    /// Returns `false` on [`Command::Shutdown`].
    fn handle(&mut self, command: Command) -> bool {
        match command {
            Command::Join(client) => {
                self.sync(&client);
                self.clients.push(client);
                self.metrics.registered.store(self.clients.len() as u64, Ordering::Relaxed);
                self.publish_queued();
            }
            Command::Resync(id) => {
                if let Some(client) = self.clients.iter().find(|c| c.id() == id).cloned() {
                    if self.sync(&client) {
                        self.metrics.resyncs.fetch_add(1, Ordering::Relaxed);
                        self.publish_queued();
                    }
                }
            }
            Command::Leave(id) => {
                self.clients.retain(|c| c.id() != id);
                self.metrics.registered.store(self.clients.len() as u64, Ordering::Relaxed);
                self.publish_queued();
            }
            Command::Control(msg, permit) => {
                self.control(msg);
                drop(permit);
            }
            Command::PhoneHello { hello, link } => self.phone_hello(hello, link),
            Command::PhoneText { conn, message, permit } => {
                self.phone_text(conn, message);
                drop(permit);
            }
            Command::PhoneFrame { conn, bytes } => self.phone_frame(conn, &bytes),
            Command::PhoneGone(conn) => self.phone_gone(conn),
            Command::Show { imported, reply } => {
                let shown = self.show(*imported);
                let _ = reply.send(shown);
            }
            Command::RecordStart { title, reply } => {
                let _ = reply.send(self.start_manual(title));
            }
            Command::RecordStop { reply } => {
                let _ = reply.send(self.end_manual(EndReason::Stopped).ok_or(RecordRefusal::NotRecording));
            }
            #[cfg(test)]
            Command::Inject(messages) => self.broadcast(messages),
            #[cfg(test)]
            Command::Panic => panic!("deliberate panic of the pipeline thread (test)"),
            Command::Shutdown => return false,
        }
        true
    }

    fn tick(&mut self, dt: Duration) {
        let out = self.pipeline.tick(dt.as_secs_f32());
        self.snapshot = None;
        self.metrics.ticks.fetch_add(1, Ordering::Relaxed);
        self.publish_scene_time();
        self.broadcast(out);
    }

    /// Runs until shutdown or until every sender is gone.
    fn serve(&mut self, rx: &Receiver<Command>) {
        // Start of the previous tick, and the deadline of the next one.
        // Ticks are scheduled on absolute deadlines (`deadline += period`)
        // so that sleep jitter does not accumulate, while `dt` is the
        // measured time between two ticks so that the scene follows the
        // wall clock.
        let mut last_tick = Instant::now();
        let mut deadline = last_tick;

        loop {
            if !self.ticking() {
                // No ticking: block until something happens, or until the
                // running capture is due for its progress report.
                let command = match self.progress_due() {
                    Some(due) => {
                        let now = Instant::now();
                        if due <= now {
                            self.capture_progress();
                            continue;
                        }
                        match rx.recv_timeout(due - now) {
                            Ok(command) => command,
                            Err(RecvTimeoutError::Timeout) => continue,
                            Err(RecvTimeoutError::Disconnected) => return,
                        }
                    }
                    None => match rx.recv() {
                        Ok(command) => command,
                        Err(_) => return,
                    },
                };
                if !self.handle(command) {
                    return;
                }
                if self.ticking() {
                    // The scene resumes where it stopped.
                    last_tick = Instant::now();
                    deadline = last_tick + tick_period(&self.pipeline);
                }
                continue;
            }

            let now = Instant::now();
            if now < deadline {
                match rx.recv_timeout(deadline - now) {
                    Ok(command) => {
                        if !self.handle(command) {
                            return;
                        }
                        // A control message may have raised the tick rate.
                        deadline = deadline.min(Instant::now() + tick_period(&self.pipeline));
                        continue;
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            } else {
                // Running late: serve what is already queued, briefly.
                let mut served = 0;
                while served == 0 || now.elapsed() < LATE_COMMAND_BUDGET {
                    match rx.try_recv() {
                        Ok(command) => {
                            if !self.handle(command) {
                                return;
                            }
                            served += 1;
                        }
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => return,
                    }
                }
                if !self.ticking() {
                    continue;
                }
            }

            let start = Instant::now();
            self.tick(start.duration_since(last_tick).min(MAX_TICK_DT));
            last_tick = start;
            deadline += tick_period(&self.pipeline);
            // Too slow for the target rate: tick back to back, without
            // trying to catch up on the deadlines that were missed.
            let now = Instant::now();
            if deadline < now {
                deadline = now;
            }
        }
    }

    /// Closes what is being written, so that a stopped server leaves
    /// complete files.
    fn finalize(&mut self) {
        self.end_manual(EndReason::Stopped);
        if let Some(capture) = self.capture.take() {
            self.close_capture(capture);
        }
        self.publish_capture();
    }
}

fn run(setup: &HubSetup, rx: &Receiver<Command>, metrics: &HubMetrics, clients: &mut Vec<Arc<Outbox>>) {
    let mut hub = Loop {
        pipeline: Pipeline::new(setup.pipeline),
        clients,
        metrics,
        setup,
        snapshot: None,
        capture: None,
        manual: None,
    };
    hub.publish_scene_time();
    hub.serve(rx);
    hub.finalize();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outbox::{Next, QueueLimits};
    use gs_core::codec::{self, Kind};
    use gs_core::rig::RingRig;
    use gs_core::scene::SceneKind;

    fn small_stage(hz: f32) -> PipelineConfig {
        PipelineConfig {
            rig: RingRig { count: 3, width: 48, height: 36, ..RingRig::default() },
            target_hz: hz,
            ..PipelineConfig::for_scene(SceneKind::Stage)
        }
    }

    async fn next_kind(outbox: &Outbox) -> Kind {
        match tokio::time::timeout(Duration::from_secs(20), outbox.next()).await.expect("nothing arrived") {
            Next::Message(bytes) => codec::peek_kind(&bytes).unwrap(),
            other => panic!("expected a message, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn join_gets_the_snapshot_then_the_live_stream() {
        let hub = Hub::start(small_stage(50.0)).unwrap();
        assert_eq!(hub.metrics.ticks.load(Ordering::Relaxed), 0);
        let outbox = Arc::new(Outbox::new(1, QueueLimits::default()));
        assert!(hub.send(Command::Join(outbox.clone())));
        let mut kinds = Vec::new();
        for _ in 0..4 {
            kinds.push(next_kind(&outbox).await);
        }
        assert_eq!(kinds, [Kind::State, Kind::Layer, Kind::Layer, Kind::Stats]);
        // The live stream follows.
        let mut layers = 0;
        for _ in 0..12 {
            if next_kind(&outbox).await == Kind::Layer {
                layers += 1;
            }
        }
        assert!(layers >= 3, "only {layers} live layers in 12 messages");
        hub.stop().unwrap().join().unwrap();
        assert!(matches!(outbox.next().await, Next::Closed));
        assert!(!hub.send(Command::Leave(1)));
    }

    #[tokio::test]
    async fn idle_without_clients_and_paced_with_one() {
        let hub = Hub::start(small_stage(40.0)).unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(hub.metrics.ticks.load(Ordering::Relaxed), 0, "ticked without a client");

        let outbox = Arc::new(Outbox::new(1, QueueLimits::default()));
        hub.send(Command::Join(outbox.clone()));
        // Let the stream start, then measure over a fixed window.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let (t0, n0) = (Instant::now(), hub.metrics.ticks.load(Ordering::Relaxed));
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let (elapsed, n1) = (t0.elapsed().as_secs_f64(), hub.metrics.ticks.load(Ordering::Relaxed));
        let hz = (n1 - n0) as f64 / elapsed;
        assert!((36.0..=42.0).contains(&hz), "ticked at {hz:.1} Hz instead of 40");

        hub.send(Command::Leave(1));
        tokio::time::sleep(Duration::from_millis(200)).await;
        let n2 = hub.metrics.ticks.load(Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(hub.metrics.ticks.load(Ordering::Relaxed), n2, "kept ticking after the last client left");
        hub.stop().unwrap().join().unwrap();
    }

    #[tokio::test]
    async fn a_panic_disconnects_the_viewers_and_the_pipeline_restarts() {
        let hub = Hub::start(small_stage(50.0)).unwrap();
        let first = Arc::new(Outbox::new(1, QueueLimits::default()));
        hub.send(Command::Join(first.clone()));
        assert_eq!(next_kind(&first).await, Kind::State);

        hub.send(Command::Panic);
        // The viewer of the crashed run is told that its stream is over.
        loop {
            match tokio::time::timeout(Duration::from_secs(20), first.next()).await.expect("left hanging") {
                Next::Closed => break,
                Next::Message(_) | Next::Resync => {}
            }
        }
        // Messages about that viewer no longer mean anything; a new one is
        // served by the rebuilt pipeline.
        assert!(hub.send(Command::Leave(1)));
        let second = Arc::new(Outbox::new(2, QueueLimits::default()));
        assert!(hub.send(Command::Join(second.clone())));
        let mut kinds = Vec::new();
        for _ in 0..4 {
            kinds.push(next_kind(&second).await);
        }
        assert_eq!(kinds, [Kind::State, Kind::Layer, Kind::Layer, Kind::Stats]);
        let ticks = hub.metrics.ticks.load(Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(hub.metrics.ticks.load(Ordering::Relaxed) > ticks + 5, "the rebuilt pipeline does not tick");

        // A bug that strikes again and again ends the thread for good.
        for _ in 0..MAX_RESTARTS {
            hub.send(Command::Panic);
        }
        hub.stop().unwrap().join().unwrap();
        assert!(matches!(second.next().await, Next::Closed));
        assert!(!hub.send(Command::Leave(2)));
    }

    /// Whether the pipeline thread still serves: a new client gets its
    /// snapshot.
    async fn serves(hub: &Hub, id: u64) -> bool {
        let outbox = Arc::new(Outbox::new(id, QueueLimits::default()));
        if !hub.send(Command::Join(outbox.clone())) {
            return false;
        }
        let first = tokio::time::timeout(Duration::from_secs(20), outbox.next()).await.expect("nothing arrived");
        hub.send(Command::Leave(id));
        matches!(first, Next::Message(_))
    }

    #[tokio::test]
    async fn only_panics_in_a_row_count_towards_giving_up() {
        let hub = Hub::start(small_stage(50.0)).unwrap();
        // Many more panics than the cap in total, but never more than four
        // in a row without a stable run in between: the thread carries on.
        for round in 0..3 {
            for _ in 0..MAX_RESTARTS - 1 {
                hub.send(Command::Panic);
            }
            assert!(serves(&hub, round).await, "gave up in round {round}");
            tokio::time::sleep(STABLE_RUN + Duration::from_millis(200)).await;
        }
        assert!(serves(&hub, 10).await);

        // A crash loop is still a crash loop: one panic more than the cap,
        // back to back, and it is over.
        for _ in 0..=MAX_RESTARTS {
            hub.send(Command::Panic);
        }
        hub.stop().unwrap().join().unwrap();
        assert!(!serves(&hub, 11).await);
    }
}
