//! Transport to the local pipeline worker (`crates/gs-worker`): batches of
//! transferred `ArrayBuffer`s in, JSON strings out. See the protocol in the
//! `gs_worker` crate documentation.

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;

use gs_core::scene::SceneKind;
use gs_worker::{WorkerEvent, WorkerRequest};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{ErrorEvent, MessageEvent, Worker, WorkerOptions, WorkerType};

use super::Sink;
use crate::logic::source::Link;

/// Loader of the worker, next to the page (relative: the application may be
/// served from any sub-path). The build id keeps stale cached copies out.
const WORKER_URL: &str = concat!("./worker-boot.js?v=", env!("GS_APP_BUILD_ID"));

struct Inner {
    worker: Worker,
    sink: Sink,
    scene: Cell<SceneKind>,
    ready: Cell<bool>,
    /// The first batch has arrived.
    streaming: Cell<bool>,
    /// Control messages sent before the worker was ready.
    outbox: RefCell<VecDeque<String>>,
    received: Cell<u64>,
}

/// Handle to the worker. Dropping it terminates the worker.
pub struct WorkerTransport {
    inner: Rc<Inner>,
    _on_message: Closure<dyn FnMut(MessageEvent)>,
    _on_error: Closure<dyn FnMut(ErrorEvent)>,
}

impl WorkerTransport {
    pub fn start(scene: SceneKind, sink: Sink) -> Result<Self, String> {
        let options = WorkerOptions::new();
        options.set_type(WorkerType::Module);
        let worker = Worker::new_with_options(WORKER_URL, &options)
            .map_err(|e| format!("le worker n'a pas pu être créé ({})", describe(&e)))?;
        let inner = Rc::new(Inner {
            worker,
            sink,
            scene: Cell::new(scene),
            ready: Cell::new(false),
            streaming: Cell::new(false),
            outbox: RefCell::new(VecDeque::new()),
            received: Cell::new(0),
        });
        let on_message = {
            let inner = Rc::downgrade(&inner);
            Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
                if let Some(inner) = inner.upgrade() {
                    inner.on_message(&event);
                }
            })
        };
        let on_error = {
            let inner = Rc::downgrade(&inner);
            Closure::<dyn FnMut(ErrorEvent)>::new(move |event: ErrorEvent| {
                if let Some(inner) = inner.upgrade() {
                    // Without this the browser also logs an uncaught error.
                    event.prevent_default();
                    inner.fail(format!("Le calcul local s'est arrêté ({}).", event.message()));
                }
            })
        };
        inner.worker.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        inner.worker.set_onerror(Some(on_error.as_ref().unchecked_ref()));
        Ok(Self { inner, _on_message: on_message, _on_error: on_error })
    }

    pub fn send(&self, json: String) {
        if self.inner.ready.get() {
            self.inner.post(&json);
        } else {
            self.inner.outbox.borrow_mut().push_back(json);
        }
    }

    pub fn request_snapshot(&self) {
        if self.inner.ready.get() {
            self.inner.post(&WorkerRequest::Snapshot.to_json());
        }
    }

    pub fn received(&self) -> u64 {
        self.inner.received.get()
    }
}

impl Drop for WorkerTransport {
    fn drop(&mut self) {
        self.inner.worker.set_onmessage(None);
        self.inner.worker.set_onerror(None);
        self.inner.worker.terminate();
    }
}

fn describe(error: &JsValue) -> String {
    error
        .dyn_ref::<js_sys::Error>()
        .map(|e| String::from(e.message()))
        .or_else(|| error.as_string())
        .unwrap_or_else(|| "erreur inconnue".to_string())
}

impl Inner {
    fn post(&self, text: &str) {
        let _ = self.worker.post_message(&JsValue::from_str(text));
    }

    fn fail(&self, message: String) {
        self.sink.on_link.as_ref()(Link::Failed);
        self.sink.on_error.as_ref()(message);
    }

    fn on_message(&self, event: &MessageEvent) {
        let data = event.data();
        if let Some(text) = data.as_string() {
            match serde_json::from_str::<WorkerEvent>(&text) {
                Ok(WorkerEvent::Ready) => {
                    self.ready.set(true);
                    self.post(&WorkerRequest::Start { scene: self.scene.get() }.to_json());
                    let pending: Vec<String> = self.outbox.borrow_mut().drain(..).collect();
                    for json in pending {
                        self.post(&json);
                    }
                }
                // Before it is ready the worker cannot have rejected a
                // request: it failed to load.
                Ok(WorkerEvent::Error { message }) if !self.ready.get() => {
                    self.fail(format!("Le calcul local n'a pas pu démarrer ({message})."));
                }
                Ok(WorkerEvent::Error { message }) => self.sink.on_error.as_ref()(format!("Commande refusée: {message}")),
                Err(_) => {}
            }
            return;
        }
        // A batch: [id, buffer, buffer, ...].
        let Ok(batch) = data.dyn_into::<js_sys::Array>() else { return };
        let Some(id) = batch.get(0).as_f64() else { return };
        self.received.set(self.received.get() + 1);
        if !self.streaming.replace(true) {
            self.sink.on_link.as_ref()(Link::Connected);
        }
        for item in batch.iter().skip(1) {
            if let Ok(buffer) = item.dyn_into::<js_sys::ArrayBuffer>() {
                let bytes = js_sys::Uint8Array::new(&buffer).to_vec();
                self.sink.on_message.as_ref()(&bytes);
            }
        }
        // Acknowledged only now that everything is applied: a busy main
        // thread delays this, and the worker skips ticks instead of queueing.
        self.post(&WorkerRequest::Ack { batch: id as u32 }.to_json());
    }
}
