//! The replay worker: a second `splat-worker` instance in replay mode (see
//! `gs_worker::replay`). It plays a recording from a URL or from a `Blob`
//! and hands the page the same acknowledged batches as a live source, so a
//! replay is drawn by the very code that draws the live stream.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gs_worker::{ReplayStatus, WorkerEvent, WorkerRequest};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{Blob, ErrorEvent, MessageEvent, Worker};

use super::worker::{post_request, spawn_worker};
use super::MessageHandler;

/// Callbacks of a replay, all invoked on the main thread.
#[derive(Clone)]
pub struct ReplaySink {
    /// One codec message.
    pub on_message: MessageHandler,
    /// The messages that follow start from a keyframe: statistics and
    /// thumbnails on screen belong to another moment.
    pub on_reset: Rc<dyn Fn()>,
    pub on_status: Rc<dyn Fn(ReplayStatus)>,
    /// The worker could not start or stopped.
    pub on_error: Rc<dyn Fn(String)>,
}

struct Inner {
    worker: Worker,
    sink: ReplaySink,
    ready: Cell<bool>,
    /// Requests made before the worker was ready, with their file.
    outbox: RefCell<Vec<(WorkerRequest, Option<Blob>)>>,
}

impl Inner {
    fn post(&self, request: WorkerRequest, file: Option<Blob>) {
        if self.ready.get() {
            post_request(&self.worker, &request, file.as_ref());
        } else {
            self.outbox.borrow_mut().push((request, file));
        }
    }

    fn on_message(&self, event: &MessageEvent) {
        let data = event.data();
        if let Some(text) = data.as_string() {
            match serde_json::from_str::<WorkerEvent>(&text) {
                Ok(WorkerEvent::Ready) => {
                    self.ready.set(true);
                    for (request, file) in self.outbox.take() {
                        post_request(&self.worker, &request, file.as_ref());
                    }
                }
                Ok(WorkerEvent::Replay(status)) => self.sink.on_status.as_ref()(status),
                Ok(WorkerEvent::ReplayReset) => self.sink.on_reset.as_ref()(),
                // Before it is ready the worker cannot have rejected a
                // request: it failed to load.
                Ok(WorkerEvent::Error { message }) if !self.ready.get() => {
                    self.sink.on_error.as_ref()(format!("Le lecteur n'a pas pu démarrer ({message})."));
                }
                _ => {}
            }
            return;
        }
        // A batch: [id, buffer, buffer, ...].
        let Ok(batch) = data.dyn_into::<js_sys::Array>() else { return };
        let Some(id) = batch.get(0).as_f64() else { return };
        for item in batch.iter().skip(1) {
            if let Ok(buffer) = item.dyn_into::<js_sys::ArrayBuffer>() {
                self.sink.on_message.as_ref()(&js_sys::Uint8Array::new(&buffer).to_vec());
            }
        }
        // Acknowledged only now that everything is applied: while the page
        // is busy the player skips ahead instead of queueing.
        post_request(&self.worker, &WorkerRequest::Ack { batch: id as u32 }, None);
    }
}

/// Handle to a replay. Dropping it terminates its worker and frees the
/// recording.
pub struct ReplayLink {
    inner: Rc<Inner>,
    _on_message: Closure<dyn FnMut(MessageEvent)>,
    _on_error: Closure<dyn FnMut(ErrorEvent)>,
}

impl ReplayLink {
    fn start(sink: ReplaySink, first: WorkerRequest, file: Option<Blob>) -> Result<Self, String> {
        let worker = spawn_worker().map_err(|_| "le lecteur n'a pas pu être créé".to_string())?;
        let inner = Rc::new(Inner { worker, sink, ready: Cell::new(false), outbox: RefCell::new(vec![(first, file)]) });
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
                    inner.sink.on_error.as_ref()(format!("Le lecteur s'est arrêté ({}).", event.message()));
                }
            })
        };
        inner.worker.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        inner.worker.set_onerror(Some(on_error.as_ref().unchecked_ref()));
        Ok(Self { inner, _on_message: on_message, _on_error: on_error })
    }

    /// Plays the recording at `url` (already validated: see
    /// `logic::share::check_file_url`).
    pub fn open_url(url: String, sink: ReplaySink) -> Result<Self, String> {
        Self::start(sink, WorkerRequest::ReplayOpen { url }, None)
    }

    /// Plays the recording in `blob` (a file the user opened, a recording
    /// just made).
    pub fn open_blob(name: &str, blob: Blob, sink: ReplaySink) -> Result<Self, String> {
        Self::start(sink, WorkerRequest::ReplayFile { name: name.to_string() }, Some(blob))
    }

    pub fn play(&self) {
        self.inner.post(WorkerRequest::ReplayPlay, None);
    }

    pub fn pause(&self) {
        self.inner.post(WorkerRequest::ReplayPause, None);
    }

    /// Moves the play head; `id` comes back in `ReplayStatus::seek`.
    pub fn seek(&self, time_s: f64, id: u32) {
        self.inner.post(WorkerRequest::ReplaySeek { t: time_s, id }, None);
    }

    pub fn set_speed(&self, speed: f64) {
        self.inner.post(WorkerRequest::ReplaySpeed { speed }, None);
    }

    pub fn set_looping(&self, on: bool) {
        self.inner.post(WorkerRequest::ReplayLoop { on }, None);
    }
}

impl Drop for ReplayLink {
    fn drop(&mut self) {
        self.inner.worker.set_onmessage(None);
        self.inner.worker.set_onerror(None);
        self.inner.worker.terminate();
    }
}
