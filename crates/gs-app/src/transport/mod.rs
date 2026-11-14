//! Where the wire messages come from. Both transports deliver the same
//! `gs_core::codec` bytes and accept the same `ControlMsg` JSON, so the rest
//! of the application does not care which one is active.

mod replay;
mod worker;
mod ws;

use std::rc::Rc;

use gs_core::protocol::ControlMsg;
use gs_core::scene::SceneKind;
use gs_worker::{RecordingMeta, WorkerEvent, WorkerRequest};
use wasm_bindgen::JsCast;

use crate::logic::source::{Link, SourceKind};

pub use replay::{ReplayLink, ReplaySink};
pub use worker::WorkerTransport;
pub use ws::WsTransport;

/// Receives one `gs_core::codec` message.
pub type MessageHandler = Rc<dyn Fn(&[u8])>;

/// Callbacks of a transport, all invoked on the main thread.
#[derive(Clone)]
pub struct Sink {
    /// One codec message.
    pub on_message: MessageHandler,
    /// The link state changed.
    pub on_link: Rc<dyn Fn(Link)>,
    /// Something went wrong that the user should know about.
    pub on_error: Rc<dyn Fn(String)>,
    /// Progress of what the worker does besides streaming: an in-memory
    /// recording, the import of a capture file.
    pub on_event: Rc<dyn Fn(WorkerEvent)>,
    /// A finished in-memory recording: its description and the pieces of
    /// the `.gsrec` file, in order.
    pub on_recording: Rc<dyn Fn(RecordingMeta, js_sys::Array)>,
}

/// Reads a `["recording", json, buffer, ...]` message of the worker: the
/// description and the array of the file's pieces (ready for a `Blob`).
/// `None` for any other message.
pub(super) fn parse_recording(message: &js_sys::Array) -> Option<(RecordingMeta, js_sys::Array)> {
    if message.get(0).as_string().as_deref() != Some("recording") {
        return None;
    }
    let meta = serde_json::from_str(&message.get(1).as_string()?).ok()?;
    let parts = js_sys::Array::new();
    for item in message.iter().skip(2) {
        if item.is_instance_of::<js_sys::ArrayBuffer>() {
            parts.push(&item);
        }
    }
    Some((meta, parts))
}

/// The active source of splats.
pub enum Transport {
    /// WebSocket to a native `gs-server`.
    Server(WsTransport),
    /// The pipeline in a Web Worker.
    Worker(WorkerTransport),
}

impl Transport {
    /// Connects to the WebSocket at `url` (reconnecting forever).
    pub fn server(url: String, sink: Sink) -> Self {
        Self::Server(WsTransport::connect(url, sink))
    }

    /// Starts the local worker with `scene`.
    pub fn worker(scene: SceneKind, sink: Sink) -> Result<Self, String> {
        WorkerTransport::start(scene, sink).map(Self::Worker)
    }

    pub fn kind(&self) -> SourceKind {
        match self {
            Self::Server(_) => SourceKind::Server,
            Self::Worker(_) => SourceKind::Worker,
        }
    }

    /// Sends a control message to the pipeline. While the link is down the
    /// last few messages are kept and sent once it is back.
    pub fn send(&self, msg: &ControlMsg) {
        let Ok(json) = serde_json::to_string(msg) else { return };
        match self {
            Self::Server(ws) => ws.send(json),
            Self::Worker(worker) => worker.send(json),
        }
    }

    /// Sends a request to the worker behind the transport (recording,
    /// hold, capture import). `false` when there is no worker: a WebSocket
    /// opened by the page itself.
    pub fn request(&self, request: &WorkerRequest) -> bool {
        match self {
            Self::Server(ws) => ws.request(request, None),
            Self::Worker(worker) => worker.request(request, None),
        }
    }

    /// The same for a request that carries a file.
    pub fn request_with_file(&self, request: &WorkerRequest, file: &web_sys::Blob) -> bool {
        match self {
            Self::Server(ws) => ws.request(request, Some(file)),
            Self::Worker(worker) => worker.request(request, Some(file)),
        }
    }

    /// Asks for a full snapshot (State and both layers), as after a fresh
    /// connection.
    pub fn resync(&self) {
        match self {
            // The server sends a snapshot to every new connection.
            Self::Server(ws) => ws.reconnect_now(),
            Self::Worker(worker) => worker.request_snapshot(),
        }
    }

    /// How the bytes reach the page: `worker` (local pipeline), `relay`
    /// (WebSocket in the relay worker) or `direct` (WebSocket on the main
    /// thread). For the debug hook.
    pub fn channel(&self) -> &'static str {
        match self {
            Self::Server(ws) if ws.relayed() => "relay",
            Self::Server(_) => "direct",
            Self::Worker(_) => "worker",
        }
    }

    /// Batches or frames received so far (for the debug hook).
    pub fn received(&self) -> u64 {
        match self {
            Self::Server(ws) => ws.received(),
            Self::Worker(worker) => worker.received(),
        }
    }
}
