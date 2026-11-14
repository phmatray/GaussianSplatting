//! WebSocket transport to `gs-server`: binary frames in, `ControlMsg` text
//! frames out, automatic reconnection with backoff.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::{Rc, Weak};
use std::time::Duration;

use leptos::prelude::{set_timeout_with_handle, TimeoutHandle};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{BinaryType, CloseEvent, MessageEvent, WebSocket};

use super::Sink;
use crate::logic::source::{reconnect_delay_ms, Link};

/// Control messages kept while the socket is down. Older ones are dropped:
/// after a reconnection the server's State is the truth anyway.
const MAX_OUTBOX: usize = 16;

struct Handlers {
    open: Closure<dyn FnMut()>,
    message: Closure<dyn FnMut(MessageEvent)>,
    close: Closure<dyn FnMut(CloseEvent)>,
}

struct Inner {
    url: String,
    sink: Sink,
    socket: RefCell<Option<WebSocket>>,
    handlers: RefCell<Option<Handlers>>,
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
        let inner = Rc::new(Inner {
            url,
            sink,
            socket: RefCell::new(None),
            handlers: RefCell::new(None),
            attempt: Cell::new(0),
            retry: Cell::new(None),
            outbox: RefCell::new(VecDeque::new()),
            received: Cell::new(0),
        });
        let weak = Rc::downgrade(&inner);
        let with = |f: fn(&Rc<Inner>)| {
            let weak = weak.clone();
            move || {
                if let Some(inner) = weak.upgrade() {
                    f(&inner);
                }
            }
        };
        let handlers = Handlers {
            open: Closure::new(with(Inner::on_open)),
            message: {
                let weak = weak.clone();
                Closure::new(move |event: MessageEvent| {
                    if let Some(inner) = weak.upgrade() {
                        inner.on_message(&event);
                    }
                })
            },
            close: {
                let weak = weak.clone();
                Closure::new(move |_event: CloseEvent| {
                    if let Some(inner) = weak.upgrade() {
                        inner.on_close();
                    }
                })
            },
        };
        *inner.handlers.borrow_mut() = Some(handlers);
        inner.open_socket();
        Self { inner }
    }

    pub fn send(&self, json: String) {
        let socket = self.inner.socket.borrow();
        match socket.as_ref().filter(|s| s.ready_state() == WebSocket::OPEN) {
            Some(socket) => {
                let _ = socket.send_with_str(&json);
            }
            None => {
                let mut outbox = self.inner.outbox.borrow_mut();
                if outbox.len() == MAX_OUTBOX {
                    outbox.pop_front();
                }
                outbox.push_back(json);
            }
        }
    }

    /// Drops the connection and opens a new one right away; the server
    /// answers a new connection with a fresh snapshot.
    pub fn reconnect_now(&self) {
        self.inner.detach();
        self.inner.attempt.set(0);
        self.inner.sink.on_link.as_ref()(Link::Connecting);
        self.inner.open_socket();
    }

    pub fn received(&self) -> u64 {
        self.inner.received.get()
    }
}

impl Drop for WsTransport {
    fn drop(&mut self) {
        self.inner.detach();
    }
}

impl Inner {
    /// Closes the current socket without triggering a reconnection and
    /// cancels a pending retry.
    fn detach(&self) {
        if let Some(handle) = self.retry.take() {
            handle.clear();
        }
        if let Some(socket) = self.socket.borrow_mut().take() {
            socket.set_onopen(None);
            socket.set_onmessage(None);
            socket.set_onclose(None);
            let _ = socket.close();
        }
    }

    fn open_socket(self: &Rc<Self>) {
        // The constructor throws when the URL is refused (a Content Security
        // Policy without the host, say): that is a failed attempt like any other.
        let socket = match WebSocket::new(&self.url) {
            Ok(socket) => socket,
            Err(_) => {
                self.schedule_retry();
                return;
            }
        };
        socket.set_binary_type(BinaryType::Arraybuffer);
        if let Some(handlers) = self.handlers.borrow().as_ref() {
            socket.set_onopen(Some(handlers.open.as_ref().unchecked_ref()));
            socket.set_onmessage(Some(handlers.message.as_ref().unchecked_ref()));
            socket.set_onclose(Some(handlers.close.as_ref().unchecked_ref()));
        }
        *self.socket.borrow_mut() = Some(socket);
    }

    fn on_open(self: &Rc<Self>) {
        self.attempt.set(0);
        self.sink.on_link.as_ref()(Link::Connected);
        let pending: Vec<String> = self.outbox.borrow_mut().drain(..).collect();
        if let Some(socket) = self.socket.borrow().as_ref() {
            for json in pending {
                let _ = socket.send_with_str(&json);
            }
        }
    }

    fn on_message(&self, event: &MessageEvent) {
        // Text frames are not part of the protocol in this direction.
        let Ok(buffer) = event.data().dyn_into::<js_sys::ArrayBuffer>() else { return };
        self.received.set(self.received.get() + 1);
        let bytes = js_sys::Uint8Array::new(&buffer).to_vec();
        self.sink.on_message.as_ref()(&bytes);
    }

    fn on_close(self: &Rc<Self>) {
        self.socket.borrow_mut().take();
        self.schedule_retry();
    }

    fn schedule_retry(self: &Rc<Self>) {
        let attempt = self.attempt.get() + 1;
        self.attempt.set(attempt);
        self.sink.on_link.as_ref()(Link::Reconnecting { attempt });
        let weak: Weak<Self> = Rc::downgrade(self);
        let retry = set_timeout_with_handle(
            move || {
                if let Some(inner) = weak.upgrade() {
                    inner.retry.set(None);
                    inner.open_socket();
                }
            },
            Duration::from_millis(u64::from(reconnect_delay_ms(attempt))),
        );
        self.retry.set(retry.ok());
    }
}
