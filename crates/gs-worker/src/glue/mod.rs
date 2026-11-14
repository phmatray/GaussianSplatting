//! Glue between the library (`gs_worker::Driver`, `Relay`, `Replay`,
//! `LocalCapture`, `Tape`) and the worker's `postMessage`, `setTimeout`,
//! `WebSocket`, `fetch` and `Blob`. See the library documentation for the
//! protocol.
//!
//! The worker is in one mode at a time: a local pipeline, the relay of a
//! server's WebSocket, the import of a capture file, or the replay of a
//! recording. A request that starts another mode ends the current one.

mod capture;
mod io;
mod replay;

use std::cell::{Cell, RefCell};

use gs_core::protocol::ControlMsg;
use gs_core::record::RecorderOptions;
use gs_worker::{Batch, Driver, Recording, Relay, Tape, WorkerEvent, WorkerRequest};
use js_sys::{Array, ArrayBuffer, Uint8Array};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{BinaryType, Blob, DedicatedWorkerGlobalScope, MessageEvent, WebSocket};

/// While a recording runs, its progress goes to the page this often.
const RECORD_STATUS_MS: f64 = 500.0;

/// What the worker is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Nothing yet.
    Idle,
    /// A local pipeline (`gs_worker::Driver`).
    Pipeline,
    /// The WebSocket of a native server (`gs_worker::Relay`).
    Relay,
    /// A capture file being ingested, then its finished session.
    Capture,
    /// A recording being replayed.
    Replay,
}

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
    mode: Cell<Mode>,
    /// Incremented whenever the mode or its source changes: asynchronous
    /// work (a download, a file read) that finishes later finds out that
    /// it is stale.
    epoch: Cell<u32>,
    driver: RefCell<Driver>,
    /// Handle of the pending `setTimeout`, if any.
    timer: Cell<Option<i32>>,
    on_timer: Closure<dyn FnMut()>,
    /// Messages waiting for the page, in relay and capture mode.
    relay: RefCell<Relay>,
    link: RefCell<Option<Link>>,
    /// Number of the latest `connect`, echoed by the link events.
    link_id: Cell<u32>,
    /// The session so far and the in-memory recording.
    tape: RefCell<Tape>,
    record_reported_ms: Cell<f64>,
    replay: RefCell<Option<replay::Host>>,
    capture: RefCell<Option<capture::Host>>,
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

    fn reject(&self, message: &str) {
        self.post_event(&WorkerEvent::Error { message: message.to_string() });
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
            self.reject("postMessage failed");
        }
    }

    /// Ends the current mode: its socket, its download, its file.
    fn leave_mode(&self) {
        self.epoch.set(self.epoch.get().wrapping_add(1));
        self.schedule(None);
        if let Some(link) = self.link.borrow_mut().take() {
            link.shut();
        }
        self.relay.borrow_mut().clear();
        self.driver.borrow_mut().stop();
        self.close_replay_source();
        *self.capture.borrow_mut() = None;
        self.mode.set(Mode::Idle);
    }

    fn on_message(&self, event: &MessageEvent) {
        let data = event.data();
        if let Some(text) = data.as_string() {
            self.on_text(&text);
            return;
        }
        // A request that carries a file: [json, Blob].
        let Ok(parts) = data.dyn_into::<Array>() else { return };
        let (Some(text), Ok(blob)) = (parts.get(0).as_string(), parts.get(1).dyn_into::<Blob>()) else { return };
        match serde_json::from_str::<WorkerRequest>(&text) {
            Ok(WorkerRequest::ReplayFile { .. }) => self.open_replay_blob(blob),
            Ok(WorkerRequest::CaptureFile { .. }) => self.open_capture(blob),
            _ => self.reject("this request takes no file"),
        }
    }

    fn on_text(&self, text: &str) {
        let Ok(request) = serde_json::from_str::<WorkerRequest>(text) else {
            // A control message for whatever produces the stream.
            match self.mode.get() {
                Mode::Relay => self.send_to_server(text),
                Mode::Pipeline | Mode::Idle => self.drive(text),
                Mode::Capture => self.control_during_capture(text),
                Mode::Replay => {}
            }
            return;
        };
        match request {
            WorkerRequest::Connect { url, link } => self.connect(&url, link),
            WorkerRequest::Ack { batch } => match self.mode.get() {
                Mode::Replay => self.ack_replay(batch),
                Mode::Relay => {
                    self.relay.borrow_mut().ack(batch);
                    self.flush_relay();
                }
                Mode::Capture => {
                    self.relay.borrow_mut().ack(batch);
                    self.flush_relay();
                }
                Mode::Pipeline | Mode::Idle => self.drive(text),
            },
            WorkerRequest::Start { .. } => {
                self.leave_mode();
                self.mode.set(Mode::Pipeline);
                self.drive(text);
            }
            WorkerRequest::Snapshot => match self.mode.get() {
                Mode::Capture => self.capture_snapshot(),
                Mode::Pipeline | Mode::Idle => self.drive(text),
                // A server sends its snapshot to a new connection, and a
                // replay re-sends its picture when asked to seek.
                Mode::Relay | Mode::Replay => {}
            },
            WorkerRequest::Hold { .. } => {
                if self.mode.get() == Mode::Pipeline {
                    self.drive(text);
                }
            }
            WorkerRequest::RecordStart { name } => self.record_start(&name),
            WorkerRequest::RecordStop => {
                let finished = self.tape.borrow_mut().stop();
                match finished {
                    Some(recording) => self.post_recording(recording),
                    None => self.report_recording(true),
                }
            }
            WorkerRequest::RecordStill { name } => {
                let still = self.tape.borrow().still(&name);
                match still {
                    Some(recording) => self.post_recording(recording),
                    None => self.reject("there is nothing to record yet"),
                }
            }
            WorkerRequest::ReplayOpen { url } => self.open_replay_url(url),
            WorkerRequest::ReplayClose => {
                if self.mode.get() == Mode::Replay {
                    self.leave_mode();
                }
            }
            WorkerRequest::ReplayPlay
            | WorkerRequest::ReplayPause
            | WorkerRequest::ReplaySeek { .. }
            | WorkerRequest::ReplaySpeed { .. }
            | WorkerRequest::ReplayLoop { .. } => self.replay_command(&request),
            WorkerRequest::CaptureCancel => self.cancel_capture(),
            WorkerRequest::ReplayFile { .. } | WorkerRequest::CaptureFile { .. } => self.reject("this request needs its file"),
        }
    }

    // --------------------------------------------------------------- timer

    /// Whatever the current mode does on its own clock.
    fn pump(&self) {
        match self.mode.get() {
            Mode::Pipeline => self.pump_pipeline(),
            Mode::Replay => self.pump_replay(),
            Mode::Capture => self.pump_capture(),
            Mode::Relay | Mode::Idle => {}
        }
    }

    fn schedule(&self, wait_ms: Option<f64>) {
        if let Some(handle) = self.timer.take() {
            self.scope.clear_timeout_with_handle(handle);
        }
        let Some(wait_ms) = wait_ms else { return };
        // Always through the event loop, even when overdue: pending
        // acknowledgements and control messages must get their turn.
        let handle =
            self.scope.set_timeout_with_callback_and_timeout_and_arguments_0(self.on_timer.as_ref().unchecked_ref(), wait_ms.ceil() as i32);
        self.timer.set(handle.ok());
    }

    // ------------------------------------------------------ local pipeline

    fn drive(&self, text: &str) {
        let result = self.driver.borrow_mut().handle_text(text, self.now());
        match result {
            Ok(Some(batch)) => {
                self.record(&batch.messages);
                self.post_batch(batch);
            }
            Ok(None) => {}
            Err(message) => self.post_event(&WorkerEvent::Error { message }),
        }
        // An acknowledgement may have unblocked a tick that is due.
        self.pump();
    }

    /// Runs a tick if one is due, then sleeps until the next one.
    fn pump_pipeline(&self) {
        let poll = self.driver.borrow_mut().poll(self.now());
        let ticked = poll.batch.is_some();
        if let Some(batch) = poll.batch {
            self.record(&batch.messages);
            self.post_batch(batch);
        }
        // After a tick, measure again: the tick itself took time.
        let wait = if ticked { self.driver.borrow().wait_ms(self.now()) } else { poll.wait_ms };
        self.schedule(wait);
    }

    // ---------------------------------------------------------------- relay

    /// The relayed connection is over (closed by the server, refused, or
    /// dropped here): the page decides when to try again.
    fn link_lost(&self) {
        if let Some(link) = self.link.borrow_mut().take() {
            link.shut();
        }
        self.relay.borrow_mut().clear();
        self.post_event(&WorkerEvent::Link { open: false, link: self.link_id.get() });
    }

    fn connect(&self, url: &str, link: u32) {
        self.leave_mode();
        self.mode.set(Mode::Relay);
        self.link_id.set(link);
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

    /// One frame of the server. Text frames are not part of the protocol
    /// in this direction.
    fn on_frame(&self, event: &MessageEvent) {
        let Ok(buffer) = event.data().dyn_into::<ArrayBuffer>() else { return };
        let bytes = Uint8Array::new(&buffer).to_vec();
        // Recorded as received: what the page is too slow to take is on
        // the recording all the same.
        self.record(std::slice::from_ref(&bytes));
        let pushed = self.relay.borrow_mut().push(bytes);
        if pushed.is_err() {
            // The page took nothing for too long. A new connection starts
            // from a fresh snapshot instead of this backlog.
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
        let sent =
            link.as_ref().filter(|link| link.socket.ready_state() == WebSocket::OPEN).is_some_and(|link| link.socket.send_with_str(text).is_ok());
        if !sent {
            self.reject("the server connection is not open");
        }
    }

    // ------------------------------------------------------------ recording

    /// Feeds the tape with messages on their way to the page.
    fn record(&self, messages: &[Vec<u8>]) {
        let now = self.now();
        let mut finished = None;
        {
            let mut tape = self.tape.borrow_mut();
            for message in messages {
                if let Some(recording) = tape.observe(now, message) {
                    finished = Some(recording);
                }
            }
        }
        match finished {
            // The memory bound, or a session that cannot be recorded further.
            Some(recording) => self.post_recording(recording),
            None => self.report_recording(false),
        }
    }

    fn record_start(&self, name: &str) {
        let started = self.tape.borrow_mut().start(name, RecorderOptions::MEMORY_BOUND);
        match started {
            Ok(()) => self.report_recording(true),
            Err(reason) => self.reject(reason),
        }
    }

    /// Tells the page how the recording is doing: at once when `now`,
    /// otherwise only while one runs and at a bounded rate.
    fn report_recording(&self, now: bool) {
        let time = self.now();
        let tape = self.tape.borrow();
        if now || (tape.is_recording() && time - self.record_reported_ms.get() >= RECORD_STATUS_MS) {
            self.record_reported_ms.set(time);
            self.post_event(&WorkerEvent::Record(tape.status()));
        }
    }

    /// Posts `["recording", json, buffer, ...]`, transferring the pieces of
    /// the file, then the status that says recording is over.
    fn post_recording(&self, recording: Recording) {
        let message = Array::new();
        let transfer = Array::new();
        message.push(&JsValue::from_str("recording"));
        message.push(&JsValue::from_str(&serde_json::to_string(&recording.meta).unwrap_or_default()));
        // One piece at a time, each freed as soon as JavaScript has its copy.
        for chunk in recording.chunks {
            let buffer = Uint8Array::from(chunk.as_slice()).buffer();
            message.push(&buffer);
            transfer.push(&buffer);
        }
        if self.scope.post_message_with_transfer(&message, &transfer).is_err() {
            self.reject("postMessage failed");
        }
        self.report_recording(true);
    }

    // --------------------------------------------------------------- capture

    /// A control message while a capture is shown: leaving for a scene of
    /// the simulation restarts the pipeline; everything else addresses a
    /// pipeline that is not there.
    fn control_during_capture(&self, text: &str) {
        if let Ok(ControlMsg::SetScene { scene }) = serde_json::from_str::<ControlMsg>(text) {
            self.leave_mode();
            self.mode.set(Mode::Pipeline);
            self.drive(&WorkerRequest::Start { scene }.to_json());
        }
    }
}

pub fn run() {
    console_error_panic_hook::set_once();
    let scope: DedicatedWorkerGlobalScope = js_sys::global().unchecked_into();
    // The worker lives as long as its global scope: leak it.
    let instance: &'static Worker = Box::leak(Box::new(Worker {
        scope: scope.clone(),
        mode: Cell::new(Mode::Idle),
        epoch: Cell::new(0),
        driver: RefCell::new(Driver::new()),
        timer: Cell::new(None),
        on_timer: Closure::new(|| {
            worker().timer.set(None);
            worker().pump();
        }),
        relay: RefCell::new(Relay::new()),
        link: RefCell::new(None),
        link_id: Cell::new(0),
        tape: RefCell::new(Tape::new()),
        record_reported_ms: Cell::new(f64::NEG_INFINITY),
        replay: RefCell::new(None),
        capture: RefCell::new(None),
    }));
    WORKER.with(|w| *w.borrow_mut() = Some(instance));

    let on_message = Closure::<dyn FnMut(MessageEvent)>::new(|event: MessageEvent| worker().on_message(&event));
    scope.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    on_message.forget();
    instance.post_event(&WorkerEvent::Ready);
}
