//! The `/ws` endpoint: one task per viewer.
//!
//! Server to client: the binary messages of the viewer's [`Outbox`], one
//! per frame. Client to server: JSON [`ControlMsg`] text frames, forwarded
//! to the pipeline thread. Anything else a client sends is ignored.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use gs_core::pipeline::MAX_CONTROL_JSON;
use gs_core::protocol::ControlMsg;
use tokio::sync::Semaphore;

use crate::http::{origin_allowed, AppState};
use crate::hub::Command;
use crate::outbox::{Next, Outbox};

/// Control messages one client may have waiting in the pipeline thread.
/// Beyond that its socket is simply not read until the pipeline catches
/// up: a burst is applied in full and in order, but cannot pile up without
/// bound.
const CONTROL_WINDOW: usize = 32;

/// Time allowed for the closing handshake.
const CLOSE_GRACE: Duration = Duration::from_millis(500);

// WebSocket close codes (RFC 6455).
const CLOSE_GOING_AWAY: u16 = 1001;
const CLOSE_TOO_BIG: u16 = 1009;
const CLOSE_INTERNAL: u16 = 1011;

/// Counts the frames received from the client. Shared by the two halves
/// of a session: the reader counts, the writer checks that the count moved
/// after a ping.
#[derive(Default)]
struct Heard(AtomicU64);

impl Heard {
    fn note(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    fn count(&self) -> u64 {
        self.0.load(Ordering::Relaxed)
    }
}

/// Why a session ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum End {
    /// The client sent a close frame.
    ClientClosed,
    /// The connection dropped or broke.
    ConnectionLost,
    /// The client sent a frame larger than a control message can be.
    TooBig,
    /// The socket did not accept a message in time.
    SendTimeout,
    /// A keepalive ping stayed unanswered.
    Unresponsive,
    /// The listener is closing.
    ListenerStopped,
    /// The pipeline thread is gone.
    PipelineGone,
}

pub(crate) async fn upgrade(
    State(state): State<AppState>,
    headers: HeaderMap,
    uri: Uri,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !origin_allowed(&state, &headers, &uri) {
        return (StatusCode::FORBIDDEN, "origin not allowed").into_response();
    }
    // These limits apply to what the client sends. Outgoing messages are
    // not limited here: a layer of a million splats is one 16 MB frame.
    upgrade
        .max_message_size(MAX_CONTROL_JSON)
        .max_frame_size(MAX_CONTROL_JSON)
        .on_failed_upgrade(|err| log::debug!("WebSocket upgrade failed: {err}"))
        .on_upgrade(move |socket| session(socket, state))
}

/// Takes a viewer off the pipeline thread's list when the session ends,
/// however it ends: a viewer left registered would keep the pipeline
/// ticking for nobody.
struct Membership<'a> {
    core: &'a crate::Core,
    id: u64,
}

impl Drop for Membership<'_> {
    fn drop(&mut self) {
        self.core.hub.send(Command::Leave(self.id));
    }
}

async fn session(socket: WebSocket, state: AppState) {
    let core = &state.core;
    let (id, _guard) = core.enter_session();
    let outbox = Arc::new(Outbox::new(id, core.settings.queue));
    let (mut sink, mut stream) = socket.split();

    let end = if core.hub.send(Command::Join(outbox.clone())) {
        let _member = Membership { core, id };
        log::debug!("client {id} connected ({:?})", state.listener);
        let heard = Heard::default();
        let mut stop = state.stop.clone();
        let end = tokio::select! {
            end = write(&mut sink, &outbox, &state, &heard) => end,
            end = read(&mut stream, &state, &heard) => end,
            _ = stop.wait_for(|stop| *stop) => End::ListenerStopped,
        };
        // A server that stops tells its sessions first and its pipeline
        // thread second, but a session busy sending may notice the closed
        // outbox before it looks at the stop signal again. The viewer is
        // told what really happened: the server is going away (1001), not
        // that the pipeline broke (1011).
        if end == End::PipelineGone && *stop.borrow() {
            End::ListenerStopped
        } else {
            end
        }
    } else {
        End::PipelineGone
    };

    // Finish the closing handshake where there is one to finish, so that
    // browsers report a clean close instead of code 1006.
    let farewell = match end {
        End::ListenerStopped => Some(close(CLOSE_GOING_AWAY, "server closing")),
        End::PipelineGone => Some(close(CLOSE_INTERNAL, "pipeline stopped")),
        End::TooBig => Some(close(CLOSE_TOO_BIG, "message too big")),
        End::ClientClosed | End::ConnectionLost | End::SendTimeout | End::Unresponsive => None,
    };
    let _ = tokio::time::timeout(CLOSE_GRACE, async {
        match farewell {
            Some(frame) => {
                let _ = sink.send(frame).await;
                if end == End::TooBig {
                    // The client is probably still sending the rest of its
                    // oversized message. Hanging up at once would answer
                    // that with a TCP reset, and a browser that gets a
                    // reset reports an abnormal closure (1006) instead of
                    // the close frame it had not read yet. The connection
                    // is kept, unread, for the rest of the grace period.
                    std::future::pending::<()>().await;
                }
            }
            // The reply to the client's close frame is queued by the
            // protocol layer; it still has to be flushed.
            None if end == End::ClientClosed => {
                let _ = sink.close().await;
            }
            None => {}
        }
    })
    .await;
    log::debug!("client {id} disconnected: {end:?}");
}

fn close(code: u16, reason: &'static str) -> Message {
    Message::Close(Some(CloseFrame { code, reason: reason.into() }))
}

/// Sends what the outbox holds, and a ping now and then.
async fn write(sink: &mut SplitSink<WebSocket, Message>, outbox: &Outbox, state: &AppState, heard: &Heard) -> End {
    let settings = &state.core.settings;
    let mut ping =
        tokio::time::interval_at(tokio::time::Instant::now() + settings.ping_interval, settings.ping_interval);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The ping that is still unanswered: when it was sent, and how many
    // frames the client had sent by then.
    let mut unanswered: Option<(Instant, u64)> = None;

    loop {
        let next = tokio::select! {
            next = outbox.next() => Some(next),
            _ = ping.tick() => None,
        };
        let (message, is_ping) = match next {
            Some(Next::Message(bytes)) => (Message::Binary(bytes), false),
            Some(Next::Resync) => {
                // The backlog was dropped while this client was slow; now
                // that it can take data again, bring it up to date.
                if !state.core.hub.send(Command::Resync(outbox.id())) {
                    return End::PipelineGone;
                }
                continue;
            }
            Some(Next::Closed) => return End::PipelineGone,
            None => {
                match unanswered {
                    // Any frame counts as a sign of life, not only the pong.
                    Some((_, frames)) if heard.count() > frames => unanswered = None,
                    Some((sent, _)) if sent.elapsed() >= settings.idle_timeout => return End::Unresponsive,
                    // Still waiting; one ping in flight is enough.
                    Some(_) => continue,
                    None => {}
                }
                (Message::Ping(Bytes::new()), true)
            }
        };
        let frames_before = heard.count();
        // `send` resolves once the whole message has been handed to the
        // kernel. While a slow client keeps this task waiting here, the
        // pipeline keeps overwriting the mailboxes of the outbox, so the
        // next message taken is the newest one.
        match tokio::time::timeout(settings.send_timeout, sink.send(message)).await {
            Ok(Ok(())) => {}
            Ok(Err(_)) => return End::ConnectionLost,
            Err(_) => return End::SendTimeout,
        }
        if is_ping {
            unanswered = Some((Instant::now(), frames_before));
        }
    }
}

/// Forwards control messages to the pipeline thread.
async fn read(stream: &mut SplitStream<WebSocket>, state: &AppState, heard: &Heard) -> End {
    let core = &state.core;
    let window = Arc::new(Semaphore::new(CONTROL_WINDOW));
    loop {
        let message = match stream.next().await {
            None => return End::ConnectionLost,
            Some(Err(err)) => {
                // The protocol layer refuses a message over the size limit
                // without buffering it; everything else is a broken peer.
                let too_big = std::error::Error::source(&err)
                    .and_then(|source| source.downcast_ref::<tungstenite::Error>())
                    .is_some_and(|source| matches!(source, tungstenite::Error::Capacity(_)));
                return if too_big { End::TooBig } else { End::ConnectionLost };
            }
            Some(Ok(message)) => message,
        };
        heard.note();
        match message {
            Message::Text(text) => {
                let parsed = if text.len() <= MAX_CONTROL_JSON {
                    serde_json::from_str::<ControlMsg>(text.as_str()).ok()
                } else {
                    None
                };
                let Some(control) = parsed else {
                    // Garbage is dropped here and never reaches the
                    // pipeline thread.
                    core.rejected_controls.fetch_add(1, Ordering::Relaxed);
                    continue;
                };
                // Never fails: the semaphore is not closed.
                let permit = window.clone().acquire_owned().await.ok();
                if !core.hub.send(Command::Control(control, permit)) {
                    return End::PipelineGone;
                }
            }
            Message::Close(_) => return End::ClientClosed,
            // Viewers have nothing binary to say; pings are answered by the
            // protocol layer and pongs only matter as a sign of life.
            Message::Binary(_) | Message::Ping(_) | Message::Pong(_) => {}
        }
    }
}
