//! The `/ingest` endpoint: the WebSocket a capturing phone talks to.
//!
//! LAN listener only. The phone is a native application: it sends no
//! `Origin`, so none is checked; what lets it in is the pairing token in
//! its `hello`. One task per connection reads the socket and forwards
//! what it reads to the pipeline thread, which fuses the frames in the
//! order they arrived and answers through a channel; nothing here waits
//! for the reconstruction.
//!
//! Bounds on what a connection can make the server hold: a message is at
//! most [`MAX_INGEST_MESSAGE_BYTES`], at most `max_inflight` frames may be
//! unacknowledged (one more is a `flow_control` error), and text messages
//! are forwarded through a small window, so a flood of them only slows its
//! own socket down.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::ws::rejection::WebSocketUpgradeRejection;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use gs_core::ingest::message::{IngestErrorCode, MAX_CLIENT_TEXT_BYTES};
use gs_core::ingest::{ClientMessage, Hello, SessionError, MAX_INGEST_MESSAGE_BYTES};
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};

use crate::http::{json_error, AppState, Listener};
use crate::hub::{Command, LastHeard, PhoneLink, PhoneOut, MAX_INFLIGHT};

/// How long a phone may take to say hello once the WebSocket is open. The
/// phone itself gives up when no `welcome` came within 8 s.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

/// Text messages one phone may have waiting in the pipeline thread; beyond
/// that its socket is not read until the thread catches up.
const TEXT_WINDOW: usize = 16;

/// Time allowed for the closing handshake, during which the phone reads
/// the error it was sent.
const CLOSE_GRACE: Duration = Duration::from_secs(1);

// WebSocket close codes (RFC 6455).
const CLOSE_NORMAL: u16 = 1000;
const CLOSE_GOING_AWAY: u16 = 1001;
const CLOSE_POLICY: u16 = 1008;
const CLOSE_TOO_BIG: u16 = 1009;
const CLOSE_INTERNAL: u16 = 1011;

/// Why a connection ended.
#[derive(Debug)]
enum End {
    /// The phone sent a close frame.
    ClientClosed,
    /// The connection dropped or broke.
    ConnectionLost,
    /// The phone broke the protocol: it gets this error, then a close.
    Refused { code: String, message: String, close_code: u16 },
    /// The capture this phone fed was closed on the server.
    CaptureClosed,
    /// A message over the size limit.
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

impl End {
    fn refused(error: &SessionError) -> Self {
        Self::Refused {
            code: error.code.as_str().to_string(),
            message: error.message.clone(),
            close_code: error.code.close_code(),
        }
    }
}

fn violation(code: IngestErrorCode, message: &str, detail: &str) -> SessionError {
    SessionError { code, message: message.to_string(), detail: detail.to_string() }
}

pub(crate) async fn upgrade(
    State(state): State<AppState>,
    upgrade: Result<WebSocketUpgrade, WebSocketUpgradeRejection>,
) -> Response {
    // A capture comes from another device by definition; on the machine
    // itself there is nothing to pair with.
    if state.listener != Listener::Lan {
        return json_error(StatusCode::FORBIDDEN, "lan_only", "captures are received on the LAN listener only");
    }
    let upgrade = match upgrade {
        Ok(upgrade) => upgrade,
        Err(rejection) => return rejection.into_response(),
    };
    // Held until the hello has been judged.
    let Ok(pending) = state.core.pending_hellos.clone().try_acquire_owned() else {
        return json_error(StatusCode::SERVICE_UNAVAILABLE, "busy", "too many phones are connecting; try again");
    };
    upgrade
        .max_message_size(MAX_INGEST_MESSAGE_BYTES)
        .max_frame_size(MAX_INGEST_MESSAGE_BYTES)
        .on_failed_upgrade(|err| log::debug!("ingest WebSocket upgrade failed: {err}"))
        .on_upgrade(move |socket| session(socket, state, pending))
}

/// Tells the pipeline thread that the phone is gone when the session ends,
/// however it ends.
struct Attached<'a> {
    core: &'a crate::Core,
    conn: u64,
}

impl Drop for Attached<'_> {
    fn drop(&mut self) {
        self.core.hub.send(Command::PhoneGone(self.conn));
    }
}

async fn session(socket: WebSocket, state: AppState, pending: OwnedSemaphorePermit) {
    let core = &state.core;
    let (conn, _guard) = core.enter_phone();
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel();
    let heard = Arc::new(LastHeard::now());
    let frames_heard = AtomicU64::new(0);
    log::debug!("phone {conn} connected");

    let end = {
        let _attached = Attached { core, conn };
        let mut stop = state.stop.clone();
        let end = tokio::select! {
            end = read(&mut stream, &state, conn, tx, &heard, &frames_heard, pending) => end,
            end = write(&mut sink, &mut rx, &state, &frames_heard) => end,
            _ = stop.wait_for(|stop| *stop) => End::ListenerStopped,
        };
        if matches!(end, End::PipelineGone) && *stop.borrow() {
            End::ListenerStopped
        } else {
            end
        }
    };

    let farewell = match &end {
        End::Refused { code, message, close_code } => {
            let error = gs_core::ingest::ServerMessage::Error { code: code.clone(), message: message.clone() };
            Some((Some(error.to_json()), *close_code, code.clone()))
        }
        End::CaptureClosed => Some((None, CLOSE_NORMAL, "capture closed".to_string())),
        End::ListenerStopped => Some((None, CLOSE_GOING_AWAY, "server closing".to_string())),
        End::PipelineGone => Some((None, CLOSE_INTERNAL, "pipeline stopped".to_string())),
        End::TooBig => Some((None, CLOSE_TOO_BIG, "message too big".to_string())),
        End::Unresponsive => Some((None, CLOSE_POLICY, "no sign of life".to_string())),
        End::ClientClosed | End::ConnectionLost | End::SendTimeout => None,
    };
    let _ = tokio::time::timeout(CLOSE_GRACE, async {
        match farewell {
            Some((error, code, reason)) => {
                if let Some(error) = error {
                    if sink.send(Message::Text(error.into())).await.is_err() {
                        return;
                    }
                }
                let _ = sink.send(Message::Close(Some(CloseFrame { code, reason: reason.into() }))).await;
                // Reading on lets the phone's own close arrive, and keeps a
                // phone that is still sending from being answered with a
                // TCP reset before it has read the error.
                while let Some(Ok(_)) = stream.next().await {}
            }
            None if matches!(end, End::ClientClosed) => {
                let _ = sink.close().await;
            }
            None => {}
        }
    })
    .await;
    log::debug!("phone {conn} disconnected: {end:?}");
}

/// Reads the next message; `Err` with the reason when there is none.
async fn next_message(stream: &mut SplitStream<WebSocket>) -> Result<Message, End> {
    match stream.next().await {
        None => Err(End::ConnectionLost),
        Some(Err(err)) => {
            let too_big = std::error::Error::source(&err)
                .and_then(|source| source.downcast_ref::<tungstenite::Error>())
                .is_some_and(|source| matches!(source, tungstenite::Error::Capacity(_)));
            Err(if too_big { End::TooBig } else { End::ConnectionLost })
        }
        Some(Ok(message)) => Ok(message),
    }
}

/// The hello, then everything else, forwarded to the pipeline thread.
async fn read(
    stream: &mut SplitStream<WebSocket>,
    state: &AppState,
    conn: u64,
    tx: mpsc::UnboundedSender<PhoneOut>,
    heard: &Arc<LastHeard>,
    frames_heard: &AtomicU64,
    pending: OwnedSemaphorePermit,
) -> End {
    let core = &state.core;
    let note = || {
        heard.note();
        frames_heard.fetch_add(1, Ordering::Relaxed);
    };

    // Nothing but a hello may come first, and it must come soon.
    let deadline = tokio::time::Instant::now() + HELLO_TIMEOUT;
    let hello = loop {
        let message = match tokio::time::timeout_at(deadline, next_message(stream)).await {
            Ok(Ok(message)) => message,
            Ok(Err(end)) => return end,
            Err(_) => {
                return End::refused(&violation(
                    IngestErrorCode::BadHello,
                    "Le serveur n'a pas reçu de hello à temps.",
                    "no hello in time",
                ))
            }
        };
        note();
        match message {
            Message::Text(text) => match Hello::parse(text.as_str(), core.pairing_token()) {
                // The token has done its work: it goes no further.
                Ok(hello) => break Hello { token: String::new(), ..hello },
                Err(error) => {
                    log::info!("phone {conn}: hello refused ({})", error.code);
                    return End::refused(&error);
                }
            },
            Message::Binary(_) => {
                return End::refused(&violation(
                    IngestErrorCode::BadHello,
                    "Le premier message doit être un hello.",
                    "a frame before the hello",
                ))
            }
            Message::Close(_) => return End::ClientClosed,
            Message::Ping(_) | Message::Pong(_) => {}
        }
    };

    // No longer a stranger.
    drop(pending);
    let inflight = Arc::new(AtomicU32::new(0));
    let link = PhoneLink { conn, tx, inflight: inflight.clone(), heard: heard.clone() };
    if !core.hub.send(Command::PhoneHello { hello, link }) {
        return End::PipelineGone;
    }

    let window = Arc::new(Semaphore::new(TEXT_WINDOW));
    loop {
        let message = match next_message(stream).await {
            Ok(message) => message,
            Err(end) => return end,
        };
        note();
        match message {
            Message::Text(text) => {
                if text.len() > MAX_CLIENT_TEXT_BYTES {
                    return End::refused(&violation(
                        IngestErrorCode::BadMessage,
                        "Message incompris par le serveur.",
                        "text message over the size limit",
                    ));
                }
                let message = match ClientMessage::parse(text.as_str()) {
                    Ok(message) => message,
                    Err(error) => return End::refused(&error),
                };
                // Never fails: the semaphore is not closed.
                let permit = window.clone().acquire_owned().await.ok();
                if !core.hub.send(Command::PhoneText { conn, message, permit }) {
                    return End::PipelineGone;
                }
            }
            Message::Binary(bytes) => {
                // Counted here, uncounted by the pipeline thread when it
                // queues the ack. A phone that respects `max_inflight`
                // never gets past it, whatever the timing.
                if inflight.fetch_add(1, Ordering::AcqRel) >= MAX_INFLIGHT {
                    return End::refused(&violation(
                        IngestErrorCode::FlowControl,
                        "Trop d'images envoyées sans attendre les accusés de réception.",
                        "more than max_inflight frames unacknowledged",
                    ));
                }
                if !core.hub.send(Command::PhoneFrame { conn, bytes }) {
                    return End::PipelineGone;
                }
            }
            Message::Close(_) => return End::ClientClosed,
            Message::Ping(_) | Message::Pong(_) => {}
        }
    }
}

/// Sends what the pipeline thread answers, and a ping now and then.
async fn write(
    sink: &mut SplitSink<WebSocket, Message>,
    rx: &mut mpsc::UnboundedReceiver<PhoneOut>,
    state: &AppState,
    frames_heard: &AtomicU64,
) -> End {
    let settings = &state.core.settings;
    let mut ping =
        tokio::time::interval_at(tokio::time::Instant::now() + settings.ping_interval, settings.ping_interval);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    // The ping that is still unanswered: when it was sent, and how many
    // frames the phone had sent by then.
    let mut unanswered: Option<(Instant, u64)> = None;

    loop {
        let next = tokio::select! {
            next = rx.recv() => Some(next),
            _ = ping.tick() => None,
        };
        let (message, is_ping) = match next {
            Some(Some(PhoneOut::Text(text))) => (Message::Text(text.into()), false),
            Some(Some(PhoneOut::Refuse { code, message, close_code })) => {
                return End::Refused { code, message, close_code }
            }
            Some(Some(PhoneOut::Close)) => return End::CaptureClosed,
            // Every sender is gone: the reader ended (its own reason wins
            // the select) or the pipeline thread dropped the link.
            Some(None) => return End::PipelineGone,
            None => {
                let heard = frames_heard.load(Ordering::Relaxed);
                match unanswered {
                    // Any frame counts as a sign of life, not only the pong.
                    Some((_, frames)) if heard > frames => unanswered = None,
                    Some((sent, _)) if sent.elapsed() >= settings.idle_timeout => return End::Unresponsive,
                    Some(_) => continue,
                    None => {}
                }
                (Message::Ping(Bytes::new()), true)
            }
        };
        let frames_before = frames_heard.load(Ordering::Relaxed);
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
