//! The pipeline in a Web Worker.
//!
//! When the page is opened without a native `gs-server` (static hosting, a
//! standalone headset), the same [`gs_core::pipeline::Pipeline`] runs here,
//! compiled to WebAssembly, in a dedicated worker next to the page. The page
//! receives exactly the bytes a WebSocket would deliver, so the viewer code
//! does not know the difference.
//!
//! This library holds everything that does not touch the browser: the tiny
//! message protocol between page and worker and the [`Driver`] that decides
//! when to tick. The `splat-worker` binary is the glue to `postMessage` and
//! `setTimeout`.
//!
//! # Protocol
//!
//! Page to worker: always a **string** holding one JSON object.
//!
//! | Message | Meaning |
//! |---|---|
//! | `{"type":"start","scene":"stage"}` | Build the pipeline from `PipelineConfig::light(scene)` and start ticking. Answered with a batch holding `Pipeline::snapshot()`. A second `start` rebuilds the pipeline. |
//! | `{"type":"connect","url":"ws://...","link":N}` | Relay mode (see below): open a WebSocket to a native server, replacing the previous one, and forward its frames. |
//! | `{"type":"ack","batch":N}` | The page has applied every batch up to id `N`. |
//! | `{"type":"snapshot"}` | Ask for a fresh snapshot (full resynchronisation): `Pipeline::snapshot()`, or the capture session's. Not in relay mode. |
//! | `{"type":"hold","on":true}` | Suspend (or resume) the local pipeline without touching its configuration: the page shows something else for a while. |
//! | `{"type":"record_start","name":"…"}`, `{"type":"record_stop"}` | Record the stream in memory (see [`tape`]), in pipeline, capture and relay mode alike. |
//! | `{"type":"record_still","name":"…"}` | A recording of the present moment alone (a finished scan saved as a file). |
//! | `{"type":"replay_open","url":"…"}` | Replay mode (see [`replay`]): play the `.gsrec` at `url`, streamed while it downloads, or through `Range` requests when it is large and the server supports them. |
//! | `{"type":"replay_play"}`, `replay_pause`, `{"type":"replay_seek","t":12.5,"id":7}`, `{"type":"replay_speed","speed":2}`, `{"type":"replay_loop","on":true}`, `replay_close` | Transport of the replay. |
//! | `{"type":"capture_cancel"}` | Stop the import of a capture file. |
//! | anything else | A `gs_core::protocol::ControlMsg`, exactly as it would be sent over the WebSocket. With a local pipeline the reply messages, if any, come back as a batch; in relay mode the text is sent to the server as it is. |
//!
//! Two requests carry a file and are therefore an **array** `[string, Blob]`
//! instead of a string: `{"type":"replay_file","name":"…"}` plays the
//! recording in the `Blob`, and `{"type":"capture_file","name":"…"}`
//! ingests the `.gscap` in it (see [`capture`]). A `Blob` (a `File` is
//! one) is cloned by reference, so nothing is copied until the worker
//! reads it, piece by piece.
//!
//! Worker to page: either a **string** holding a JSON [`WorkerEvent`]
//! (`{"type":"ready"}` once the WebAssembly module is initialised and
//! messages can be received, `{"type":"error","message":"..."}` for a
//! rejected request, `{"type":"link","open":true|false,"link":N}` when the
//! WebSocket of `connect` number `N` opens or closes; the loader script posts the same error event
//! when the module cannot be loaded at all), or a **batch**: a JavaScript
//! array `[id, buffer, buffer, ...]` where `id` is a number and every
//! `buffer` is an `ArrayBuffer` holding one `gs_core::codec` message. The
//! buffers are transferred, not copied. One tick of the pipeline is one
//! batch, so the messages of a tick always arrive together and in order.
//!
//! Further events: `{"type":"replay", ...}` ([`ReplayStatus`]) and
//! `{"type":"replay_reset"}` (posted right before a batch that starts from
//! a keyframe: the page forgets the statistics and thumbnails it shows);
//! `{"type":"record", ...}` ([`RecordStatus`]); `{"type":"capture", ...}`
//! ([`CaptureProgress`]). A finished recording arrives as the array
//! `["recording", json, buffer, buffer, ...]`: `json` is a
//! [`RecordingMeta`] and the buffers, in order, are the `.gsrec` file.
//!
//! # Flow control
//!
//! Every batch must be acknowledged. The worker never has more than
//! [`MAX_UNACKED_TICKS`] batches outstanding when it starts a tick: if the
//! page is slow (software rendering, a long garbage collection, a hidden
//! tab), ticks are skipped and the next one covers the elapsed time, so the
//! animation jumps ahead instead of megabytes queueing up in the page's
//! message queue. Replies to control messages and snapshots are reliable
//! and are sent regardless; they count as outstanding until acknowledged.
//!
//! # Relay mode
//!
//! With a native server the pipeline does not run here, but its WebSocket
//! does: see [`relay`]. The page gets the server's frames in the same
//! acknowledged batches, coalesced while it is busy, so a slow page shows a
//! lower rate instead of an ever older picture. After a `connect` the page
//! ignores batches until the `link` event that opens it: they belong to the
//! connection that was replaced.

pub mod capture;
pub mod relay;
pub mod replay;
pub mod tape;

use gs_core::pipeline::{Outgoing, Pipeline};
use gs_core::protocol::PipelineConfig;
use gs_core::scene::SceneKind;
use serde::{Deserialize, Serialize};

pub use capture::{CaptureProgress, LocalCapture};
pub use relay::{Delivery, Relay, RelayOverflow, MAX_RELAY_BYTES};
pub use replay::{Replay, ReplayPoll, ReplayStatus};
pub use tape::{RecordStatus, Recording, RecordingMeta, Tape};

/// A tick is skipped while this many batches are waiting for their
/// acknowledgement.
pub const MAX_UNACKED_TICKS: u32 = 2;

/// Longest real time one tick may cover, in milliseconds. After a stall (a
/// suspended tab, a debugger) the scene resumes instead of leaping.
pub const MAX_TICK_INTERVAL_MS: f64 = 500.0;

/// How long the driver sleeps while it waits for acknowledgements, in
/// milliseconds. An acknowledgement wakes it earlier.
const STALL_POLL_MS: f64 = 50.0;

/// Requests of the page that are addressed to the worker itself rather than
/// to the pipeline.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerRequest {
    /// Build the pipeline for `scene` and start ticking.
    Start { scene: SceneKind },
    /// Relay the WebSocket at `url` (replacing the one relayed so far).
    /// `link` is echoed by the [`WorkerEvent::Link`] events of this
    /// connection, which tells them from those of a replaced one.
    Connect { url: String, link: u32 },
    /// Every batch up to `batch` has been applied.
    Ack { batch: u32 },
    /// Send a full snapshot again.
    Snapshot,
    /// Suspend or resume the local pipeline.
    Hold { on: bool },
    /// Start recording the stream in memory.
    RecordStart { name: String },
    /// End the recording; it comes back as a `recording` message.
    RecordStop,
    /// Record the present moment alone.
    RecordStill { name: String },
    /// Replay the recording at `url`.
    ReplayOpen { url: String },
    /// Replay the recording in the `Blob` that accompanies this request.
    ReplayFile { name: String },
    ReplayPlay,
    ReplayPause,
    /// Move the play head to `t` seconds; `id` comes back in
    /// [`ReplayStatus::seek`].
    ReplaySeek { t: f64, id: u32 },
    ReplaySpeed { speed: f64 },
    ReplayLoop { on: bool },
    /// Leave replay mode and free the recording.
    ReplayClose,
    /// Ingest the `.gscap` in the `Blob` that accompanies this request.
    CaptureFile { name: String },
    /// Stop the import of a capture file; what was fused stays.
    CaptureCancel,
}

impl WorkerRequest {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// String messages from the worker to the page.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WorkerEvent {
    /// The worker is initialised and listening.
    Ready,
    /// A request was rejected; the pipeline is unchanged.
    Error { message: String },
    /// The relayed WebSocket of `connect` number `link` opened (`open`) or
    /// closed.
    Link { open: bool, link: u32 },
    /// State of the replay.
    Replay(ReplayStatus),
    /// The next batch starts from a keyframe.
    ReplayReset,
    /// Progress of the in-memory recording.
    Record(RecordStatus),
    /// Progress of the import of a capture file.
    Capture(CaptureProgress),
}

impl WorkerEvent {
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// Codec messages that belong together, in delivery order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Batch {
    /// Identifier to acknowledge; ids grow by one per batch, starting at 1.
    pub id: u32,
    /// One `gs_core::codec` message each.
    pub messages: Vec<Vec<u8>>,
}

/// Bookkeeping of batches sent and acknowledged.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FlowControl {
    sent: u32,
    acked: u32,
}

impl FlowControl {
    /// Id for the next batch; it counts as outstanding from now on.
    pub fn next_id(&mut self) -> u32 {
        self.sent = self.sent.wrapping_add(1);
        self.sent
    }

    /// Records the acknowledgement of every batch up to `batch`. Stale and
    /// impossible ids (never sent) are ignored.
    pub fn ack(&mut self, batch: u32) {
        let ahead = batch.wrapping_sub(self.acked);
        if ahead <= self.outstanding() {
            self.acked = batch;
        }
    }

    /// Batches sent and not yet acknowledged.
    pub fn outstanding(&self) -> u32 {
        self.sent.wrapping_sub(self.acked)
    }

    /// Whether a new tick may be produced.
    pub fn can_tick(&self) -> bool {
        self.outstanding() < MAX_UNACKED_TICKS
    }
}

/// What [`Driver::poll`] decided.
#[derive(Clone, Debug, PartialEq)]
pub struct Poll {
    /// The tick that ran, to be posted to the page.
    pub batch: Option<Batch>,
    /// Call [`Driver::poll`] again after this many milliseconds (or earlier,
    /// when a message arrives). `None` while no pipeline is running.
    pub wait_ms: Option<f64>,
}

/// Owns the pipeline and decides when it ticks. Time is passed in by the
/// caller as milliseconds of a monotonic clock, which keeps this type free
/// of browser APIs.
#[derive(Default)]
pub struct Driver {
    pipeline: Option<Pipeline>,
    flow: FlowControl,
    /// Clock value of the last tick that ran.
    last_tick_ms: Option<f64>,
    /// Clock value at which the next tick is due.
    next_due_ms: f64,
    /// Ticks that were due but not run because the page was behind.
    skipped: u64,
    /// The page shows something else: no tick runs.
    held: bool,
}

impl Driver {
    pub fn new() -> Self {
        Self::default()
    }

    /// The running pipeline, if `start` was received.
    pub fn pipeline(&self) -> Option<&Pipeline> {
        self.pipeline.as_ref()
    }

    pub fn flow(&self) -> FlowControl {
        self.flow
    }

    /// Ticks skipped so far because acknowledgements were missing.
    pub fn skipped_ticks(&self) -> u64 {
        self.skipped
    }

    /// True while [`WorkerRequest::Hold`] suspends the pipeline.
    pub fn is_held(&self) -> bool {
        self.held
    }

    /// Wraps messages that did not come from the pipeline (a capture
    /// session) into the next batch of the same numbering.
    pub fn wrap(&mut self, messages: Vec<Vec<u8>>) -> Option<Batch> {
        if messages.is_empty() {
            return None;
        }
        Some(Batch { id: self.flow.next_id(), messages })
    }

    /// Drops the pipeline (another source takes over); batch numbering and
    /// acknowledgements go on.
    pub fn stop(&mut self) {
        self.pipeline = None;
        self.last_tick_ms = None;
        self.held = false;
    }

    fn batch(&mut self, messages: Vec<Outgoing>) -> Option<Batch> {
        if messages.is_empty() {
            return None;
        }
        Some(Batch { id: self.flow.next_id(), messages: messages.into_iter().map(|m| m.bytes).collect() })
    }

    /// Handles one string message of the page. Returns the batch to post
    /// back, if the message produced one, or a description of why it was
    /// rejected.
    pub fn handle_text(&mut self, text: &str, now_ms: f64) -> Result<Option<Batch>, String> {
        if let Ok(request) = serde_json::from_str::<WorkerRequest>(text) {
            return match request {
                WorkerRequest::Start { scene } => {
                    let pipeline = Pipeline::new(PipelineConfig::light(scene));
                    let snapshot = pipeline.snapshot();
                    self.pipeline = Some(pipeline);
                    self.last_tick_ms = None;
                    self.next_due_ms = now_ms;
                    self.held = false;
                    Ok(self.batch(snapshot))
                }
                WorkerRequest::Hold { on } => {
                    if self.held && !on {
                        // The time spent on hold is not played back.
                        self.last_tick_ms = None;
                        self.next_due_ms = now_ms;
                    }
                    self.held = on;
                    Ok(None)
                }
                WorkerRequest::Ack { batch } => {
                    self.flow.ack(batch);
                    Ok(None)
                }
                WorkerRequest::Snapshot => {
                    let snapshot = self.pipeline.as_ref().ok_or("the pipeline is not started")?.snapshot();
                    Ok(self.batch(snapshot))
                }
                // The relay, the replay, recordings and capture files are
                // the business of the glue, which owns sockets and files.
                _ => Err("not a request for the pipeline".to_string()),
            };
        }
        let pipeline = self.pipeline.as_mut().ok_or("the pipeline is not started")?;
        let replies = pipeline.handle_control_json(text)?;
        Ok(self.batch(replies))
    }

    /// Runs a tick if one is due and the page has kept up.
    pub fn poll(&mut self, now_ms: f64) -> Poll {
        let Some(pipeline) = self.pipeline.as_mut() else { return Poll { batch: None, wait_ms: None } };
        if self.held {
            return Poll { batch: None, wait_ms: None };
        }
        let period_ms = 1000.0 / f64::from(pipeline.config().target_hz.max(1.0));
        if now_ms < self.next_due_ms {
            return Poll { batch: None, wait_ms: Some(self.next_due_ms - now_ms) };
        }
        if !self.flow.can_tick() {
            // The tick that was due is dropped; the next one that runs
            // covers its time.
            self.skipped += 1;
            self.next_due_ms = now_ms;
            return Poll { batch: None, wait_ms: Some(STALL_POLL_MS.min(period_ms)) };
        }
        let elapsed_ms = self.last_tick_ms.map_or(period_ms, |last| now_ms - last);
        let dt = (elapsed_ms.clamp(0.0, MAX_TICK_INTERVAL_MS) / 1000.0) as f32;
        let messages = pipeline.tick(dt);
        self.last_tick_ms = Some(now_ms);
        // Keep the cadence when ticks are cheap; never try to catch up when
        // they are not.
        self.next_due_ms = (self.next_due_ms + period_ms).max(now_ms);
        let batch = self.batch(messages);
        Poll { batch, wait_ms: Some(period_ms) }
    }

    /// Milliseconds until the next tick is due at clock value `now_ms`
    /// (`0` when it is overdue), `None` while no pipeline is running. Use it
    /// after a tick, whose own duration `poll` cannot know.
    pub fn wait_ms(&self, now_ms: f64) -> Option<f64> {
        self.pipeline.as_ref().filter(|_| !self.held).map(|_| (self.next_due_ms - now_ms).max(0.0))
    }
}

#[cfg(test)]
mod tests {
    use gs_core::codec::{self, Kind};
    use gs_core::protocol::ControlMsg;

    use super::*;

    fn kinds(batch: &Batch) -> Vec<Kind> {
        batch.messages.iter().map(|m| codec::peek_kind(m).unwrap()).collect()
    }

    /// A started driver with a cheap stage (the light preset shrunk further).
    fn started() -> Driver {
        let mut driver = Driver::new();
        let first = driver.handle_text(&WorkerRequest::Start { scene: SceneKind::Stage }.to_json(), 0.0).unwrap().unwrap();
        assert_eq!(first.id, 1);
        driver.handle_text(&WorkerRequest::Ack { batch: 1 }.to_json(), 0.0).unwrap();
        let patch = r#"{"type":"patch_config","patch":{"rig":{"count":3,"width":32,"height":24}}}"#;
        let reply = driver.handle_text(patch, 0.0).unwrap().unwrap();
        driver.handle_text(&WorkerRequest::Ack { batch: reply.id }.to_json(), 0.0).unwrap();
        driver
    }

    #[test]
    fn requests_use_the_documented_json() {
        assert_eq!(WorkerRequest::Start { scene: SceneKind::House }.to_json(), r#"{"type":"start","scene":"house"}"#);
        assert_eq!(WorkerRequest::Ack { batch: 7 }.to_json(), r#"{"type":"ack","batch":7}"#);
        assert_eq!(WorkerRequest::Snapshot.to_json(), r#"{"type":"snapshot"}"#);
        assert_eq!(
            WorkerRequest::Connect { url: "ws://127.0.0.1:7878/ws".into(), link: 3 }.to_json(),
            r#"{"type":"connect","url":"ws://127.0.0.1:7878/ws","link":3}"#
        );
        assert_eq!(WorkerEvent::Link { open: true, link: 3 }.to_json(), r#"{"type":"link","open":true,"link":3}"#);
        assert_eq!(WorkerEvent::Ready.to_json(), r#"{"type":"ready"}"#);
        assert_eq!(
            serde_json::from_str::<WorkerEvent>(r#"{"type":"error","message":"x"}"#).unwrap(),
            WorkerEvent::Error { message: "x".into() }
        );
        assert_eq!(WorkerRequest::Hold { on: true }.to_json(), r#"{"type":"hold","on":true}"#);
        assert_eq!(WorkerRequest::RecordStart { name: "a".into() }.to_json(), r#"{"type":"record_start","name":"a"}"#);
        assert_eq!(WorkerRequest::RecordStop.to_json(), r#"{"type":"record_stop"}"#);
        assert_eq!(WorkerRequest::ReplaySeek { t: 1.5, id: 4 }.to_json(), r#"{"type":"replay_seek","t":1.5,"id":4}"#);
        assert_eq!(WorkerRequest::ReplayOpen { url: "a.gsrec".into() }.to_json(), r#"{"type":"replay_open","url":"a.gsrec"}"#);
        assert_eq!(WorkerEvent::ReplayReset.to_json(), r#"{"type":"replay_reset"}"#);
        let status = ReplayStatus { time: 2.0, duration: Some(5.0), playing: true, speed: 1.0, ..ReplayStatus::default() };
        let json = WorkerEvent::Replay(status.clone()).to_json();
        assert!(json.starts_with(r#"{"type":"replay","time":2.0,"duration":5.0,"playing":true"#), "{json}");
        assert_eq!(serde_json::from_str::<WorkerEvent>(&json).unwrap(), WorkerEvent::Replay(status));
        let json = WorkerEvent::Record(RecordStatus { active: true, bytes: 9, ..RecordStatus::default() }).to_json();
        assert!(json.starts_with(r#"{"type":"record","active":true"#), "{json}");
        let json = WorkerEvent::Capture(CaptureProgress { frames: 3, ..CaptureProgress::default() }).to_json();
        assert!(json.starts_with(r#"{"type":"capture","name":"""#), "{json}");
        // Control messages must not parse as worker requests.
        let control = serde_json::to_string(&ControlMsg::SetPaused { paused: true }).unwrap();
        assert!(serde_json::from_str::<WorkerRequest>(&control).is_err());
    }

    #[test]
    fn nothing_happens_before_start() {
        let mut driver = Driver::new();
        assert_eq!(driver.poll(10.0), Poll { batch: None, wait_ms: None });
        assert!(driver.handle_text(r#"{"type":"set_paused","paused":true}"#, 0.0).is_err());
        assert!(driver.handle_text(r#"{"type":"snapshot"}"#, 0.0).is_err());
        assert!(driver.wait_ms(0.0).is_none());
    }

    #[test]
    fn start_answers_with_the_snapshot() {
        let mut driver = Driver::new();
        let batch = driver.handle_text(r#"{"type":"start","scene":"stage"}"#, 5.0).unwrap().unwrap();
        assert_eq!(kinds(&batch), [Kind::State, Kind::Layer, Kind::Layer, Kind::Stats]);
        let config = driver.pipeline().unwrap().config();
        assert_eq!(*config, PipelineConfig::light(SceneKind::Stage));
        assert_eq!(driver.flow().outstanding(), 1);
    }

    #[test]
    fn ticks_follow_the_target_rate() {
        let mut driver = started();
        let period = 1000.0 / f64::from(driver.pipeline().unwrap().config().target_hz);
        let first = driver.poll(0.0);
        let id = first.batch.as_ref().unwrap().id;
        assert_eq!(*kinds(first.batch.as_ref().unwrap()).last().unwrap(), Kind::Stats);
        driver.handle_text(&WorkerRequest::Ack { batch: id }.to_json(), 1.0).unwrap();
        // Not due yet.
        let early = driver.poll(period * 0.5);
        assert!(early.batch.is_none());
        assert!((early.wait_ms.unwrap() - period * 0.5).abs() < 1e-6);
        assert!(driver.poll(period).batch.is_some());
    }

    #[test]
    fn a_slow_page_never_has_more_than_two_ticks_outstanding() {
        let mut driver = started();
        let mut now = 0.0;
        let mut ticks = Vec::new();
        // The page acknowledges nothing for two seconds.
        while now < 2000.0 {
            if let Some(batch) = driver.poll(now).batch {
                ticks.push(batch.id);
            }
            assert!(driver.flow().outstanding() <= MAX_UNACKED_TICKS);
            now += 10.0;
        }
        assert_eq!(ticks.len(), MAX_UNACKED_TICKS as usize);
        assert!(driver.skipped_ticks() > 0);

        // One acknowledgement frees exactly one tick, right away.
        driver.handle_text(&WorkerRequest::Ack { batch: ticks[0] }.to_json(), now).unwrap();
        let time_before = driver.pipeline().unwrap().scene_time();
        assert!(driver.poll(now).batch.is_some());
        assert!(driver.poll(now + 1000.0).batch.is_none());
        // The tick after the stall covers the stall, bounded by the cap.
        let advanced = driver.pipeline().unwrap().scene_time() - time_before;
        assert!(advanced > 0.2 && advanced <= (MAX_TICK_INTERVAL_MS / 1000.0) as f32 + 1e-3, "{advanced}");
    }

    #[test]
    fn replies_are_sent_even_when_the_page_is_behind() {
        let mut driver = started();
        let mut now = 0.0;
        while driver.flow().can_tick() {
            driver.poll(now);
            now += 100.0;
        }
        let reply = driver.handle_text(r#"{"type":"set_scene","scene":"house"}"#, now).unwrap().unwrap();
        assert_eq!(kinds(&reply)[0], Kind::State);
        assert!(driver.flow().outstanding() > MAX_UNACKED_TICKS);
        assert!(driver.poll(now + 100.0).batch.is_none());
        // A cumulative acknowledgement clears everything.
        driver.handle_text(&WorkerRequest::Ack { batch: reply.id }.to_json(), now).unwrap();
        assert_eq!(driver.flow().outstanding(), 0);
        assert!(driver.poll(now + 100.0).batch.is_some());
    }

    #[test]
    fn bogus_acknowledgements_are_ignored() {
        let mut flow = FlowControl::default();
        let a = flow.next_id();
        let b = flow.next_id();
        flow.ack(b + 5);
        assert_eq!(flow.outstanding(), 2);
        flow.ack(b);
        assert_eq!(flow.outstanding(), 0);
        // An old acknowledgement arriving late changes nothing.
        flow.ack(a);
        assert_eq!(flow.outstanding(), 0);
        assert!(flow.can_tick());
    }

    #[test]
    fn invalid_control_text_is_rejected_and_harmless() {
        let mut driver = started();
        let before = *driver.pipeline().unwrap().config();
        assert!(driver.handle_text("not json", 0.0).is_err());
        assert!(driver.handle_text(r#"{"type":"format_disk"}"#, 0.0).is_err());
        assert_eq!(*driver.pipeline().unwrap().config(), before);
    }

    #[test]
    fn a_held_pipeline_does_not_tick_and_resumes_where_it_stopped() {
        let mut driver = started();
        let first = driver.poll(0.0).batch.unwrap();
        driver.handle_text(&WorkerRequest::Ack { batch: first.id }.to_json(), 1.0).unwrap();
        let time = driver.pipeline().unwrap().scene_time();
        driver.handle_text(r#"{"type":"hold","on":true}"#, 10.0).unwrap();
        assert!(driver.is_held());
        for now in [100.0, 5000.0, 60000.0] {
            assert_eq!(driver.poll(now), Poll { batch: None, wait_ms: None });
            assert!(driver.wait_ms(now).is_none());
        }
        assert_eq!(driver.pipeline().unwrap().scene_time(), time);
        // Control messages are still answered (a snapshot for the return).
        assert!(driver.handle_text(r#"{"type":"snapshot"}"#, 60000.0).unwrap().is_some());
        driver.handle_text(r#"{"type":"hold","on":false}"#, 60000.0).unwrap();
        let batch = driver.poll(60000.0).batch.expect("ticks again");
        driver.handle_text(&WorkerRequest::Ack { batch: batch.id }.to_json(), 60000.0).unwrap();
        // The minute on hold is not played back: one period, not a leap.
        let advanced = driver.pipeline().unwrap().scene_time() - time;
        assert!(advanced > 0.0 && advanced < 0.2, "{advanced}");
        // Requests of the other modes are not for the driver.
        assert!(driver.handle_text(r#"{"type":"replay_play"}"#, 0.0).is_err());
        assert!(driver.handle_text(r#"{"type":"record_stop"}"#, 0.0).is_err());
    }

    #[test]
    fn snapshot_request_resynchronises() {
        let mut driver = started();
        let batch = driver.handle_text(r#"{"type":"snapshot"}"#, 0.0).unwrap().unwrap();
        assert_eq!(kinds(&batch), [Kind::State, Kind::Layer, Kind::Layer, Kind::Stats]);
    }
}
