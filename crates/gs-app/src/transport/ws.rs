//! Transport to `gs-server`: binary frames in, `ControlMsg` text frames
//! out, automatic reconnection with backoff.
//!
//! The WebSocket itself normally lives in the relay worker (see
//! `gs_worker::relay`): a page that renders heavily reads a socket of its
//! own too slowly, and the picture would fall seconds behind the pipeline.
//! Through the worker the page receives acknowledged, coalesced batches
//! and is never more than two of them late. When the worker cannot start,
//! the socket is opened on the main thread instead.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::{Rc, Weak};
use std::time::Duration;

use gs_worker::{WorkerEvent, WorkerRequest};
use leptos::prelude::{set_timeout, set_timeout_with_handle, TimeoutHandle};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{BinaryType, CloseEvent, ErrorEvent, MessageEvent, WebSocket, Worker};

use super::worker::spawn_worker;
use super::Sink;
use crate::logic::source::{reconnect_delay_ms, Link};

/// Control messages kept while the socket is down. Older ones are dropped:
/// after a reconnection the server's State is the truth anyway.
const MAX_OUTBOX: usize = 16;
/// Time given to the relay worker to load before the main thread takes over.
const RELAY_BOOT_TIMEOUT: Duration = Duration::from_secs(10);
/// A relay that has never opened a connection is given up after this many
/// failed attempts: where a worker may not open sockets (a restrictive
/// policy of the host), the main thread still can.
const RELAY_ATTEMPTS: u32 = 2;

/// Handlers of a socket on the main thread. They outlive the sockets: the
/// same three closures serve every reconnection.
struct SocketHandlers {
    open: Closure<dyn FnMut()>,
    message: Closure<dyn FnMut(MessageEvent)>,
    close: Closure<dyn FnMut(CloseEvent)>,
}

/// Where the WebSocket lives.
enum Channel {
    /// In the relay worker.
    Relay {
        worker: Worker,
        _on_message: Closure<dyn FnMut(MessageEvent)>,
        _on_error: Closure<dyn FnMut(ErrorEvent)>,
    },
    /// On the main thread.
    Direct { socket: Option<WebSocket>, handlers: SocketHandlers },
}

struct Inner {
    url: String,
    sink: Sink,
    channel: RefCell<Channel>,
    /// The relay worker is initialised and takes requests.
    relay_ready: Cell<bool>,
    /// Number of the latest `connect` sent to the relay.
    relay_link: Cell<u32>,
    /// Batches of the relay are stale until the latest `connect` has opened.
    relay_stale: Cell<bool>,
    /// The relay has opened at least one connection.
    relay_proven: Cell<bool>,
    /// The link is open: control messages can be sent.
    open: Cell<bool>,
    /// Failed attempts since the last successful connection.
    attempt: Cell<u32>,
    retry: Cell<Option<TimeoutHandle>>,
    outbox: RefCell<VecDeque<String>>,
    received: Cell<u64>,
}

/// Handle to the connection. Dropping it closes the socket for good.
pub struct WsTransport {
    inner: Rc<Inner>,
}

impl WsTransport {
    pub fn connect(url: String, sink: Sink) -> Self {
        let inner = Rc::new_cyclic(|weak: &Weak<Inner>| {
            let channel = Inner::relay_channel(weak).unwrap_or_else(|| Inner::direct_channel(weak));
            Inner {
                url,
                sink,
                channel: RefCell::new(channel),
                relay_ready: Cell::new(false),
                relay_link: Cell::new(0),
                relay_stale: Cell::new(true),
                relay_proven: Cell::new(false),
                open: Cell::new(false),
                attempt: Cell::new(0),
                retry: Cell::new(None),
                outbox: RefCell::new(VecDeque::new()),
                received: Cell::new(0),
            }
        });
        if inner.is_relayed() {
            // A worker that never reports (a loader stuck on the network)
            // must not keep the application waiting for ever.
            let weak = Rc::downgrade(&inner);
            set_timeout(
                move || {
                    if let Some(inner) = weak.upgrade() {
                        if inner.is_relayed() && !inner.relay_ready.get() {
                            inner.fall_back();
                        }
                    }
                },
                RELAY_BOOT_TIMEOUT,
            );
        }
        inner.open_link();
        Self { inner }
    }

    pub fn send(&self, json: String) {
        if self.inner.open.get() {
            self.inner.transmit(&json);
        } else {
            let mut outbox = self.inner.outbox.borrow_mut();
            if outbox.len() == MAX_OUTBOX {
                outbox.pop_front();
            }
            outbox.push_back(json);
        }
    }

    /// Drops the connection and opens a new one right away; the server
    /// answers a new connection with a fresh snapshot.
    pub fn reconnect_now(&self) {
        self.inner.cancel_retry();
        self.inner.open.set(false);
        self.inner.attempt.set(0);
        self.inner.sink.on_link.as_ref()(Link::Connecting);
        self.inner.open_link();
    }

    pub fn received(&self) -> u64 {
        self.inner.received.get()
    }

    /// True while the socket lives in the relay worker.
    pub fn relayed(&self) -> bool {
        self.inner.is_relayed()
    }
}

impl Drop for WsTransport {
    fn drop(&mut self) {
        self.inner.cancel_retry();
        self.inner.close_channel();
    }
}

impl Inner {
    // ------------------------------------------------------------- channels

    /// Starts the relay worker; `None` when the browser refuses.
    fn relay_channel(weak: &Weak<Self>) -> Option<Channel> {
        let worker = spawn_worker().ok()?;
        let on_message = {
            let weak = weak.clone();
            Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
                if let Some(inner) = weak.upgrade() {
                    inner.on_relay_message(&event);
                }
            })
        };
        let on_error = {
            let weak = weak.clone();
            Closure::<dyn FnMut(ErrorEvent)>::new(move |event: ErrorEvent| {
                if let Some(inner) = weak.upgrade() {
                    // Handled here: without this the browser also logs an
                    // uncaught error.
                    event.prevent_default();
                    inner.fall_back();
                }
            })
        };
        worker.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        worker.set_onerror(Some(on_error.as_ref().unchecked_ref()));
        Some(Channel::Relay { worker, _on_message: on_message, _on_error: on_error })
    }

    fn direct_channel(weak: &Weak<Self>) -> Channel {
        let with = |f: fn(&Rc<Self>)| {
            let weak = weak.clone();
            move || {
                if let Some(inner) = weak.upgrade() {
                    f(&inner);
                }
            }
        };
        let handlers = SocketHandlers {
            open: Closure::new(with(Self::on_open)),
            message: {
                let weak = weak.clone();
                Closure::new(move |event: MessageEvent| {
                    // Text frames are not part of the protocol in this direction.
                    let (Some(inner), Ok(buffer)) = (weak.upgrade(), event.data().dyn_into::<js_sys::ArrayBuffer>()) else { return };
                    inner.on_frame(&js_sys::Uint8Array::new(&buffer).to_vec());
                })
            },
            close: {
                let weak = weak.clone();
                Closure::new(move |_event: CloseEvent| {
                    if let Some(inner) = weak.upgrade() {
                        if let Channel::Direct { socket, .. } = &mut *inner.channel.borrow_mut() {
                            socket.take();
                        }
                        inner.on_close();
                    }
                })
            },
        };
        Channel::Direct { socket: None, handlers }
    }

    fn is_relayed(&self) -> bool {
        matches!(*self.channel.borrow(), Channel::Relay { .. })
    }

    /// Closes whatever carries the connection, without any further event.
    fn close_channel(&self) {
        match &mut *self.channel.borrow_mut() {
            Channel::Relay { worker, .. } => {
                worker.set_onmessage(None);
                worker.set_onerror(None);
                worker.terminate();
            }
            Channel::Direct { socket, .. } => {
                if let Some(socket) = socket.take() {
                    socket.set_onopen(None);
                    socket.set_onmessage(None);
                    socket.set_onclose(None);
                    let _ = socket.close();
                }
            }
        }
    }

    /// The relay worker is unusable (it failed to load, or it crashed):
    /// carry on with a socket on the main thread.
    fn fall_back(self: &Rc<Self>) {
        if !self.is_relayed() {
            return;
        }
        self.close_channel();
        let direct = Self::direct_channel(&Rc::downgrade(self));
        // The worker's handlers may be running right now (this is called
        // from them): wasm-bindgen keeps a closure alive until it returns.
        *self.channel.borrow_mut() = direct;
        self.relay_ready.set(false);
        self.cancel_retry();
        if self.open.replace(false) {
            self.sink.on_link.as_ref()(Link::Connecting);
        }
        self.open_link();
    }

    fn post_to_relay(&self, text: &str) {
        if let Channel::Relay { worker, .. } = &*self.channel.borrow() {
            let _ = worker.post_message(&JsValue::from_str(text));
        }
    }

    // ----------------------------------------------------------------- link

    fn cancel_retry(&self) {
        if let Some(handle) = self.retry.take() {
            handle.clear();
        }
    }

    /// Starts a connection attempt, replacing the current connection.
    fn open_link(self: &Rc<Self>) {
        let failed = match &mut *self.channel.borrow_mut() {
            Channel::Relay { .. } => {
                self.relay_link.set(self.relay_link.get().wrapping_add(1));
                self.relay_stale.set(true);
                false
            }
            Channel::Direct { socket, handlers } => {
                if let Some(old) = socket.take() {
                    old.set_onopen(None);
                    old.set_onmessage(None);
                    old.set_onclose(None);
                    let _ = old.close();
                }
                // The constructor throws when the URL is refused (a Content
                // Security Policy without the host, say): that is a failed
                // attempt like any other.
                match WebSocket::new(&self.url) {
                    Ok(new) => {
                        new.set_binary_type(BinaryType::Arraybuffer);
                        new.set_onopen(Some(handlers.open.as_ref().unchecked_ref()));
                        new.set_onmessage(Some(handlers.message.as_ref().unchecked_ref()));
                        new.set_onclose(Some(handlers.close.as_ref().unchecked_ref()));
                        *socket = Some(new);
                        false
                    }
                    Err(_) => true,
                }
            }
        };
        if failed {
            self.schedule_retry();
        } else if self.relay_ready.get() {
            self.post_connect();
        }
    }

    /// Asks the relay for the connection `open_link` numbered. Before the
    /// worker is ready this waits for its `ready` event.
    fn post_connect(&self) {
        self.post_to_relay(&WorkerRequest::Connect { url: self.url.clone(), link: self.relay_link.get() }.to_json());
    }

    fn transmit(&self, json: &str) {
        match &*self.channel.borrow() {
            Channel::Relay { worker, .. } => {
                let _ = worker.post_message(&JsValue::from_str(json));
            }
            Channel::Direct { socket, .. } => {
                if let Some(socket) = socket {
                    let _ = socket.send_with_str(json);
                }
            }
        }
    }

    fn on_open(self: &Rc<Self>) {
        self.attempt.set(0);
        self.open.set(true);
        self.sink.on_link.as_ref()(Link::Connected);
        let pending: Vec<String> = self.outbox.borrow_mut().drain(..).collect();
        for json in pending {
            self.transmit(&json);
        }
    }

    fn on_frame(&self, bytes: &[u8]) {
        self.received.set(self.received.get() + 1);
        self.sink.on_message.as_ref()(bytes);
    }

    fn on_close(self: &Rc<Self>) {
        self.open.set(false);
        self.schedule_retry();
    }

    fn schedule_retry(self: &Rc<Self>) {
        let attempt = self.attempt.get() + 1;
        self.attempt.set(attempt);
        self.sink.on_link.as_ref()(Link::Reconnecting { attempt });
        let weak: Weak<Self> = Rc::downgrade(self);
        self.cancel_retry();
        let retry = set_timeout_with_handle(
            move || {
                if let Some(inner) = weak.upgrade() {
                    inner.retry.set(None);
                    inner.open_link();
                }
            },
            Duration::from_millis(u64::from(reconnect_delay_ms(attempt))),
        );
        self.retry.set(retry.ok());
    }

    // ---------------------------------------------------------------- relay

    fn on_relay_message(self: &Rc<Self>, event: &MessageEvent) {
        let data = event.data();
        if let Some(text) = data.as_string() {
            match serde_json::from_str::<WorkerEvent>(&text) {
                Ok(WorkerEvent::Ready) => {
                    self.relay_ready.set(true);
                    self.post_connect();
                }
                // Events of a connection that was replaced meanwhile say
                // nothing about the current one.
                Ok(WorkerEvent::Link { link, .. }) if link != self.relay_link.get() => {}
                Ok(WorkerEvent::Link { open: true, .. }) => {
                    self.relay_stale.set(false);
                    self.relay_proven.set(true);
                    self.on_open();
                }
                Ok(WorkerEvent::Link { open: false, .. }) => {
                    self.relay_stale.set(true);
                    if !self.relay_proven.get() && self.attempt.get() + 1 >= RELAY_ATTEMPTS {
                        self.fall_back();
                    } else {
                        self.on_close();
                    }
                }
                // Before it is ready the worker cannot have rejected a
                // request: it failed to load.
                Ok(WorkerEvent::Error { .. }) if !self.relay_ready.get() => self.fall_back(),
                // A control message that met a closing socket. The State
                // that follows the reconnection is the truth.
                Ok(WorkerEvent::Error { .. }) | Err(_) => {}
            }
            return;
        }
        // A batch: [id, buffer, buffer, ...].
        let Ok(batch) = data.dyn_into::<js_sys::Array>() else { return };
        let Some(id) = batch.get(0).as_f64() else { return };
        if !self.relay_stale.get() {
            for item in batch.iter().skip(1) {
                if let Ok(buffer) = item.dyn_into::<js_sys::ArrayBuffer>() {
                    self.on_frame(&js_sys::Uint8Array::new(&buffer).to_vec());
                }
            }
        }
        // Acknowledged only now that everything is applied: while the page
        // is busy the relay keeps coalescing instead of queueing here.
        self.post_to_relay(&WorkerRequest::Ack { batch: id as u32 }.to_json());
    }
}
