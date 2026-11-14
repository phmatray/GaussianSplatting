//! Capture mode: a `.gscap` file read from a `Blob` and ingested here,
//! frame by frame, between turns of the event loop.

use gs_core::pipeline::Outgoing;
use gs_worker::{LocalCapture, WorkerEvent};
use wasm_bindgen_futures::spawn_local;
use web_sys::Blob;

use super::io;
use super::{worker, Mode, Worker};

/// The file is read in pieces of this size.
const READ_BYTES: f64 = 1024.0 * 1024.0;
/// More of the file is read only while the parser holds less than this:
/// one piece of the file, plus the frame that straddles its end.
const BUFFER_LOW: usize = 64 * 1024;
/// Frames are fused for about this long before the event loop gets its
/// turn (acknowledgements, a cancel, the page's other requests).
const SLICE_MS: f64 = 30.0;
/// Progress goes to the page at most this often.
const PROGRESS_MS: f64 = 200.0;

pub(super) struct Host {
    capture: LocalCapture,
    blob: Blob,
    /// Bytes of the file read so far.
    offset: f64,
    /// A piece of the file has been asked for and has not arrived.
    reading: bool,
    reported_ms: f64,
}

impl Worker {
    pub(super) fn open_capture(&self, blob: Blob) {
        self.leave_mode();
        self.mode.set(Mode::Capture);
        let capture = LocalCapture::new(blob.size() as u64);
        *self.capture.borrow_mut() = Some(Host { capture, blob, offset: 0.0, reading: false, reported_ms: f64::NEG_INFINITY });
        self.pump_capture();
    }

    /// Stops reading the file. The session built so far stays on screen.
    pub(super) fn cancel_capture(&self) {
        if self.mode.get() != Mode::Capture {
            return;
        }
        let finished = self.capture.borrow_mut().as_mut().map(|host| {
            host.offset = host.blob.size();
            host.capture.finish()
        });
        if let Some(messages) = finished {
            self.send_capture(messages);
            self.report_capture(true);
        }
    }

    /// A resynchronisation of the page: what a viewer that joins now gets.
    pub(super) fn capture_snapshot(&self) {
        let snapshot = self.capture.borrow_mut().as_mut().map(|host| host.capture.snapshot());
        if let Some(messages) = snapshot {
            self.send_capture(messages);
        }
    }

    /// Queues messages of the session for the page, coalesced like a
    /// server's stream while the page is busy, and records them.
    fn send_capture(&self, messages: Vec<Outgoing>) {
        if messages.is_empty() {
            return;
        }
        let bytes: Vec<Vec<u8>> = messages.into_iter().map(|m| m.bytes).collect();
        self.record(&bytes);
        {
            let mut relay = self.relay.borrow_mut();
            for message in bytes {
                // The page acknowledges slowly, not never: what waits is
                // bounded by the size of the capture's map.
                let _ = relay.push(message);
            }
        }
        self.flush_relay();
    }

    fn report_capture(&self, force: bool) {
        let now = self.now();
        let mut guard = self.capture.borrow_mut();
        let Some(host) = guard.as_mut() else { return };
        if force || now - host.reported_ms >= PROGRESS_MS {
            host.reported_ms = now;
            let progress = host.capture.progress();
            drop(guard);
            self.post_event(&WorkerEvent::Capture(progress));
        }
    }

    /// Fuses frames for one time slice, reads more of the file when the
    /// parser runs dry, and finishes the session at the end of the file.
    pub(super) fn pump_capture(&self) {
        let start = self.now();
        let mut messages = Vec::new();
        let mut starved = false;
        {
            let mut guard = self.capture.borrow_mut();
            let Some(host) = guard.as_mut() else { return };
            if host.capture.is_done() {
                return;
            }
            // One frame at a time: a frame takes tens of milliseconds here.
            while !starved && !host.capture.is_done() && self.now() - start < SLICE_MS {
                let step = host.capture.step(1);
                messages.extend(step.messages);
                starved = step.starved;
            }
            if starved && host.offset >= host.blob.size() && !host.reading {
                messages.extend(host.capture.finish());
            }
        }
        self.send_capture(messages);

        let mut guard = self.capture.borrow_mut();
        let Some(host) = guard.as_mut() else { return };
        let done = host.capture.is_done();
        let read = !done && !host.reading && host.capture.buffered() < BUFFER_LOW && host.offset < host.blob.size();
        if read {
            host.reading = true;
            let (blob, from, epoch) = (host.blob.clone(), host.offset, self.epoch.get());
            spawn_local(async move {
                let bytes = io::read_blob(&blob, from, from + READ_BYTES).await;
                let this = worker();
                if this.mode.get() != Mode::Capture || this.epoch.get() != epoch {
                    return;
                }
                if let Some(host) = this.capture.borrow_mut().as_mut() {
                    host.reading = false;
                    match bytes {
                        Ok(bytes) if !bytes.is_empty() => {
                            host.offset += bytes.len() as f64;
                            host.capture.push(&bytes);
                        }
                        // The file cannot be read any further: keep what is there.
                        _ => host.offset = host.blob.size(),
                    }
                }
                this.pump_capture();
            });
        }
        let waiting = starved && host.reading;
        drop(guard);
        self.report_capture(done);
        // While frames are waiting, come back right after the event loop
        // had its turn; while the file is being read, its answer wakes us.
        self.schedule((!done && !waiting).then_some(0.0));
    }
}
