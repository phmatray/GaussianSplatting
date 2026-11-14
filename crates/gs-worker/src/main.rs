//! Entry point of the `splat-worker` Web Worker: glue between
//! [`gs_worker::Driver`] (local pipeline) or [`gs_worker::Relay`] (WebSocket
//! of a native server) and the worker's `postMessage` and `setTimeout`.
//! See the library documentation for the protocol.

#[cfg(target_arch = "wasm32")]
mod glue {
    use std::cell::{Cell, RefCell};

    use gs_worker::{Batch, Driver, Relay, WorkerEvent, WorkerRequest};
    use js_sys::{Array, ArrayBuffer, Uint8Array};
    use wasm_bindgen::prelude::*;
    use wasm_bindgen::JsCast;
    use web_sys::{BinaryType, DedicatedWorkerGlobalScope, MessageEvent, WebSocket};

    /// The relayed WebSocket and its event handlers.
    struct Link {
        socket: WebSocket,
        _on_open: Closure<dyn FnMut()>,
        _on_message: Closure<dyn FnMut(MessageEvent)>,
        _on_close: Closure<dyn FnMut()>,
    }

    impl Link {
        /// Closes the socket without any further event.
        fn shut(self) {
            self.socket.set_onopen(None);
            self.socket.set_onmessage(None);
            self.socket.set_onclose(None);
            let _ = self.socket.close();
        }
    }

    struct Worker {
        scope: DedicatedWorkerGlobalScope,
        driver: RefCell<Driver>,
        /// Handle of the pending `setTimeout`, if any.
        timer: Cell<Option<i32>>,
        on_timer: Closure<dyn FnMut()>,
        relay: RefCell<Relay>,
        link: RefCell<Option<Link>>,
        /// True from the first `connect`: text messages go to the server.
        relaying: Cell<bool>,
        /// Number of the latest `connect`, echoed by the link events.
        link_id: Cell<u32>,
    }

    thread_local! {
        static WORKER: RefCell<Option<&'static Worker>> = const { RefCell::new(None) };
    }

    fn worker() -> &'static Worker {
        WORKER.with(|w| w.borrow().expect("the worker is initialised in main"))
    }

    impl Worker {
        fn now(&self) -> f64 {
            self.scope.performance().map_or_else(js_sys::Date::now, |p| p.now())
        }

        fn post_event(&self, event: &WorkerEvent) {
            let _ = self.scope.post_message(&JsValue::from_str(&event.to_json()));
        }

        /// Posts `[id, buffer, ...]`, transferring the buffers.
        fn post_batch(&self, batch: Batch) {
            let message = Array::new();
            let transfer = Array::new();
            message.push(&JsValue::from(batch.id));
            for bytes in &batch.messages {
                // A copy into a buffer owned by JavaScript: a view of the
                // WebAssembly memory cannot be transferred.
                let buffer = Uint8Array::from(bytes.as_slice()).buffer();
                message.push(&buffer);
                transfer.push(&buffer);
            }
            if self.scope.post_message_with_transfer(&message, &transfer).is_err() {
                self.post_event(&WorkerEvent::Error { message: "postMessage failed".into() });
            }
        }

        fn on_message(&self, event: &MessageEvent) {
            let Some(text) = event.data().as_string() else { return };
            match serde_json::from_str::<WorkerRequest>(&text) {
                Ok(WorkerRequest::Connect { url, link }) => self.connect(&url, link),
                Ok(WorkerRequest::Ack { batch }) if self.relaying.get() => {
                    self.relay.borrow_mut().ack(batch);
                    self.flush_relay();
                }
                Ok(WorkerRequest::Start { .. }) => {
                    // Back to a local pipeline.
                    self.relaying.set(false);
                    self.disconnect();
                    self.drive(&text);
                }
                Ok(_) | Err(_) if self.relaying.get() => self.send_to_server(&text),
                Ok(_) | Err(_) => self.drive(&text),
            }
        }

        // ------------------------------------------------- local pipeline

        fn drive(&self, text: &str) {
            let result = self.driver.borrow_mut().handle_text(text, self.now());
            match result {
                Ok(Some(batch)) => self.post_batch(batch),
                Ok(None) => {}
                Err(message) => self.post_event(&WorkerEvent::Error { message }),
            }
            // An acknowledgement may have unblocked a tick that is due.
            self.pump();
        }

        /// Runs a tick if one is due, then sleeps until the next one.
        fn pump(&self) {
            let poll = self.driver.borrow_mut().poll(self.now());
            let ticked = poll.batch.is_some();
            if let Some(batch) = poll.batch {
                self.post_batch(batch);
            }
            // After a tick, measure again: the tick itself took time.
            let wait = if ticked { self.driver.borrow().wait_ms(self.now()) } else { poll.wait_ms };
            self.schedule(wait);
        }

        fn schedule(&self, wait_ms: Option<f64>) {
            if let Some(handle) = self.timer.take() {
                self.scope.clear_timeout_with_handle(handle);
            }
            let Some(wait_ms) = wait_ms else { return };
            // Always through the event loop, even when overdue: pending
            // acknowledgements and control messages must get their turn.
            let handle = self
                .scope
                .set_timeout_with_callback_and_timeout_and_arguments_0(self.on_timer.as_ref().unchecked_ref(), wait_ms.ceil() as i32);
            self.timer.set(handle.ok());
        }

        // ---------------------------------------------------------- relay

        fn disconnect(&self) {
            if let Some(link) = self.link.borrow_mut().take() {
                link.shut();
            }
            self.relay.borrow_mut().clear();
        }

        /// The relayed connection is over (closed by the server, refused,
        /// or dropped here): the page decides when to try again.
        fn link_lost(&self) {
            self.disconnect();
            self.post_event(&WorkerEvent::Link { open: false, link: self.link_id.get() });
        }

        fn connect(&self, url: &str, link: u32) {
            self.relaying.set(true);
            self.link_id.set(link);
            self.disconnect();
            // No local pipeline next to a relayed one.
            *self.driver.borrow_mut() = Driver::new();
            self.schedule(None);
            let Ok(socket) = WebSocket::new(url) else {
                // A refused URL is a failed attempt like any other.
                self.post_event(&WorkerEvent::Link { open: false, link });
                return;
            };
            socket.set_binary_type(BinaryType::Arraybuffer);
            // The handlers of a replaced socket are detached, so whatever
            // fires belongs to the current link.
            let on_open = Closure::<dyn FnMut()>::new(move || worker().post_event(&WorkerEvent::Link { open: true, link }));
            let on_message = Closure::<dyn FnMut(MessageEvent)>::new(|event: MessageEvent| worker().on_frame(&event));
            let on_close = Closure::<dyn FnMut()>::new(|| worker().link_lost());
            socket.set_onopen(Some(on_open.as_ref().unchecked_ref()));
            socket.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
            socket.set_onclose(Some(on_close.as_ref().unchecked_ref()));
            *self.link.borrow_mut() = Some(Link { socket, _on_open: on_open, _on_message: on_message, _on_close: on_close });
        }

        /// One frame of the server. Text frames are not part of the
        /// protocol in this direction.
        fn on_frame(&self, event: &MessageEvent) {
            let Ok(buffer) = event.data().dyn_into::<ArrayBuffer>() else { return };
            let pushed = self.relay.borrow_mut().push(Uint8Array::new(&buffer).to_vec());
            if pushed.is_err() {
                // The page took nothing for too long. A new connection
                // starts from a fresh snapshot instead of this backlog.
                self.link_lost();
                return;
            }
            self.flush_relay();
        }

        fn flush_relay(&self) {
            let batch = self.relay.borrow_mut().next_batch();
            if let Some(batch) = batch {
                self.post_batch(batch);
            }
        }

        /// A control message of the page, on its way to the server.
        fn send_to_server(&self, text: &str) {
            let link = self.link.borrow();
            let sent = link
                .as_ref()
                .filter(|link| link.socket.ready_state() == WebSocket::OPEN)
                .is_some_and(|link| link.socket.send_with_str(text).is_ok());
            if !sent {
                self.post_event(&WorkerEvent::Error { message: "the server connection is not open".into() });
            }
        }
    }

    pub fn run() {
        console_error_panic_hook::set_once();
        let scope: DedicatedWorkerGlobalScope = js_sys::global().unchecked_into();
        // The worker lives as long as its global scope: leak it.
        let instance: &'static Worker = Box::leak(Box::new(Worker {
            scope: scope.clone(),
            driver: RefCell::new(Driver::new()),
            timer: Cell::new(None),
            on_timer: Closure::new(|| {
                worker().timer.set(None);
                worker().pump();
            }),
            relay: RefCell::new(Relay::new()),
            link: RefCell::new(None),
            relaying: Cell::new(false),
            link_id: Cell::new(0),
        }));
        WORKER.with(|w| *w.borrow_mut() = Some(instance));

        let on_message = Closure::<dyn FnMut(MessageEvent)>::new(|event: MessageEvent| worker().on_message(&event));
        scope.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        on_message.forget();
        instance.post_event(&WorkerEvent::Ready);
    }
}

#[cfg(target_arch = "wasm32")]
fn main() {
    glue::run();
}

/// The worker only exists in the browser.
#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!("splat-worker is a Web Worker: build it for wasm32-unknown-unknown (trunk build in crates/gs-app).");
}
