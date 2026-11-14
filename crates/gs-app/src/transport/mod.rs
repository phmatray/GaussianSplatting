//! Where the wire messages come from. Both transports deliver the same
//! `gs_core::codec` bytes and accept the same `ControlMsg` JSON, so the rest
//! of the application does not care which one is active.

mod worker;
mod ws;

use std::rc::Rc;

use gs_core::protocol::ControlMsg;
use gs_core::scene::SceneKind;

use crate::logic::source::{Link, SourceKind};

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

    /// Asks for a full snapshot (State and both layers), as after a fresh
    /// connection.
    pub fn resync(&self) {
        match self {
            // The server sends a snapshot to every new connection.
            Self::Server(ws) => ws.reconnect_now(),
            Self::Worker(worker) => worker.request_snapshot(),
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
