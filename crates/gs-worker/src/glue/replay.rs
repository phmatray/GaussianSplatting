//! Replay mode: where the bytes of a recording come from.
//!
//! * A `Blob` (a file the user opened, or a recording just made in
//!   memory): read whole when it is small, piece by piece through the
//!   `Range` player when it is not.
//! * A URL: a first request asks for the head of the file with a `Range`
//!   header. A server that answers `206` for a large file is then asked for
//!   the pieces the player wants; anything else is downloaded front to
//!   back and plays while it arrives.

use gs_worker::{Replay, WorkerEvent, WorkerRequest};
use wasm_bindgen_futures::spawn_local;
use web_sys::{AbortController, Blob};

use super::io::{self, BodyReader};
use super::{worker, Mode, Worker};

/// Up to this size a recording that is downloaded is held in memory as a
/// whole: every seek is then immediate. Beyond it, the bounded cache of
/// the `Range` player keeps the memory of the worker in check.
const WHOLE_LIMIT: u64 = 64 << 20;
/// The same for a recording that is already on this machine (a file the
/// user opened, a recording just made in memory, which is at most this
/// large): reading it costs no network, only the memory.
const WHOLE_FILE_LIMIT: u64 = 256 << 20;
/// Size of one request of a `Range` replay. Twice the player's default:
/// a seek on the stage needs the keyframe and what follows it, a few
/// megabytes, and every request is a round trip.
const FETCH_BLOCK: u64 = 4 << 20;
/// Largest recording downloaded front to back (a server without `Range`
/// support): it ends up in memory entirely.
const STREAM_LIMIT: u64 = 1 << 30;
/// The probe request asks for this much of the head of the file.
const PROBE_RANGE: &str = "bytes=0-4095";

/// Where the pieces of a `Range` replay are read from.
enum Pieces {
    None,
    Blob(Blob),
    Url(String),
}

pub(super) struct Host {
    replay: Replay,
    pieces: Pieces,
    /// A piece has been asked for and has not arrived.
    in_flight: bool,
    /// Cancels the download in progress.
    abort: Option<AbortController>,
}

impl Host {
    fn new(replay: Replay, pieces: Pieces) -> Self {
        Self { replay, pieces, in_flight: false, abort: None }
    }
}

impl Worker {
    /// Stops the download of the replay, if any, and forgets the recording.
    pub(super) fn close_replay_source(&self) {
        if let Some(host) = self.replay.borrow_mut().take() {
            if let Some(abort) = host.abort {
                abort.abort();
            }
        }
    }

    fn enter_replay(&self) -> u32 {
        self.leave_mode();
        self.mode.set(Mode::Replay);
        self.epoch.get()
    }

    /// True while the replay opened at `epoch` is still the current one.
    fn replay_is(&self, epoch: u32) -> bool {
        self.mode.get() == Mode::Replay && self.epoch.get() == epoch
    }

    fn set_replay(&self, host: Host) {
        *self.replay.borrow_mut() = Some(host);
        self.pump_replay();
    }

    /// The source could not be opened at all.
    fn replay_failed(&self, epoch: u32, code: &str) {
        if self.replay_is(epoch) {
            self.set_replay(Host::new(Replay::failed(code), Pieces::None));
        }
    }

    pub(super) fn open_replay_blob(&self, blob: Blob) {
        let epoch = self.enter_replay();
        let size = blob.size();
        if size > WHOLE_FILE_LIMIT as f64 {
            self.set_replay(Host::new(Replay::ranged(size as u64).with_fetch_block(FETCH_BLOCK), Pieces::Blob(blob)));
            return;
        }
        spawn_local(async move {
            let bytes = io::read_blob(&blob, 0.0, size).await;
            let this = worker();
            if !this.replay_is(epoch) {
                return;
            }
            match bytes.map_err(|_| "read").and_then(|bytes| Replay::from_bytes(bytes).map_err(|e| e.code())) {
                Ok(replay) => this.set_replay(Host::new(replay, Pieces::None)),
                Err(code) => this.replay_failed(epoch, code),
            }
        });
    }

    pub(super) fn open_replay_url(&self, url: String) {
        let epoch = self.enter_replay();
        let Ok(abort) = AbortController::new() else {
            self.replay_failed(epoch, "network");
            return;
        };
        let signal = abort.signal();
        // The replay exists from now on, so that the status says "loading"
        // and a close can cancel the requests.
        let mut host = Host::new(Replay::streaming(), Pieces::None);
        host.abort = Some(abort);
        self.set_replay(host);
        spawn_local(async move {
            let this = worker();
            let probe = match io::fetch(&this.scope, &url, Some(PROBE_RANGE), Some(&signal)).await {
                Ok(response) => response,
                // Another origin may refuse the Range header in its CORS
                // answer and still serve the file: ask once more, plainly.
                Err(_) => match io::fetch(&this.scope, &url, None, Some(&signal)).await {
                    Ok(response) => response,
                    Err(_) => return this.replay_failed(epoch, "network"),
                },
            };
            if !this.replay_is(epoch) {
                return;
            }
            let response = match probe.status() {
                // The server honours ranges. A large file is played through
                // them; a small one is simply downloaded.
                206 => match io::content_range_total(&probe) {
                    Some(total) if total > WHOLE_LIMIT => {
                        if let Some(host) = this.replay.borrow_mut().as_mut() {
                            host.replay = Replay::ranged(total).with_fetch_block(FETCH_BLOCK).with_flow(host.replay.flow());
                            host.pieces = Pieces::Url(url);
                        }
                        this.pump_replay();
                        return;
                    }
                    _ => match io::fetch(&this.scope, &url, None, Some(&signal)).await {
                        Ok(response) => response,
                        Err(_) => return this.replay_failed(epoch, "network"),
                    },
                },
                // The server ignored the range: this is the whole file.
                200 => probe,
                status => return this.replay_failed(epoch, &format!("http_{status}")),
            };
            if !this.replay_is(epoch) {
                return;
            }
            if response.status() != 200 {
                return this.replay_failed(epoch, &format!("http_{}", response.status()));
            }
            if io::content_length(&response).is_some_and(|length| length > STREAM_LIMIT) {
                return this.replay_failed(epoch, "too_large");
            }
            let Some(body) = BodyReader::new(&response) else { return this.replay_failed(epoch, "network") };
            let mut received = 0u64;
            loop {
                let chunk = body.next().await;
                if !this.replay_is(epoch) {
                    return;
                }
                let mut guard = this.replay.borrow_mut();
                let Some(host) = guard.as_mut() else { return };
                let over = match chunk {
                    Ok(Some(chunk)) => {
                        received += chunk.len() as u64;
                        if received > STREAM_LIMIT {
                            host.replay.fail("too_large");
                            if let Some(abort) = &host.abort {
                                abort.abort();
                            }
                            true
                        } else {
                            host.replay.push_bytes(&chunk);
                            false
                        }
                    }
                    Ok(None) => {
                        host.replay.finish_stream();
                        true
                    }
                    // The connection broke: what has arrived still plays.
                    Err(_) => {
                        host.replay.finish_stream();
                        host.replay.fail("network");
                        true
                    }
                };
                drop(guard);
                this.pump_replay();
                if over {
                    return;
                }
            }
        });
    }

    pub(super) fn ack_replay(&self, batch: u32) {
        if let Some(host) = self.replay.borrow_mut().as_mut() {
            host.replay.ack(batch);
        }
        self.pump_replay();
    }

    pub(super) fn replay_command(&self, request: &WorkerRequest) {
        if self.mode.get() != Mode::Replay {
            self.reject("no recording is open");
            return;
        }
        if let Some(host) = self.replay.borrow_mut().as_mut() {
            match *request {
                WorkerRequest::ReplayPlay => host.replay.play(),
                WorkerRequest::ReplayPause => host.replay.pause(),
                WorkerRequest::ReplaySeek { t, id } => host.replay.seek(t, id),
                WorkerRequest::ReplaySpeed { speed } => host.replay.set_speed(speed),
                WorkerRequest::ReplayLoop { on } => host.replay.set_looping(on),
                _ => {}
            }
        }
        self.pump_replay();
    }

    /// Moves the play head, posts what the viewer needs and the status,
    /// asks for the next piece of the file, and sleeps until the next step.
    pub(super) fn pump_replay(&self) {
        let now = self.now();
        let mut guard = self.replay.borrow_mut();
        let Some(host) = guard.as_mut() else { return };
        let poll = host.replay.poll(now);
        let status = host.replay.take_status(now);
        drop(guard);
        if let Some(batch) = poll.batch {
            if poll.reset {
                self.post_event(&WorkerEvent::ReplayReset);
            }
            self.post_batch(batch);
        }
        if let Some(status) = status {
            self.post_event(&WorkerEvent::Replay(status));
        }
        self.request_piece();
        self.schedule(poll.wait_ms);
    }

    /// Fetches the bytes the `Range` player asks for, one request at a time.
    fn request_piece(&self) {
        let mut guard = self.replay.borrow_mut();
        let Some(host) = guard.as_mut() else { return };
        if host.in_flight {
            return;
        }
        let Some(range) = host.replay.wanted() else { return };
        let epoch = self.epoch.get();
        enum From {
            Blob(Blob),
            Url(String, Option<web_sys::AbortSignal>),
        }
        let from = match &host.pieces {
            Pieces::None => return,
            Pieces::Blob(blob) => From::Blob(blob.clone()),
            Pieces::Url(url) => From::Url(url.clone(), host.abort.as_ref().map(AbortController::signal)),
        };
        host.in_flight = true;
        drop(guard);
        spawn_local(async move {
            let this = worker();
            let bytes = match from {
                From::Blob(blob) => io::read_blob(&blob, range.start as f64, range.end as f64).await.map_err(|_| "read".to_string()),
                From::Url(url, signal) => match io::fetch(&this.scope, &url, Some(&range.http_header()), signal.as_ref()).await {
                    Ok(response) if response.status() == 206 => io::body_bytes(&response).await.map_err(|_| "network".to_string()),
                    Ok(response) => Err(format!("http_{}", response.status())),
                    Err(_) => Err("network".to_string()),
                },
            };
            if !this.replay_is(epoch) {
                return;
            }
            if let Some(host) = this.replay.borrow_mut().as_mut() {
                host.in_flight = false;
                match bytes {
                    // An empty answer would be asked for again at once, forever.
                    Ok(bytes) if bytes.is_empty() => host.replay.fail("read"),
                    Ok(bytes) => host.replay.supply(range.start, &bytes),
                    Err(code) => host.replay.fail(&code),
                }
            }
            this.pump_replay();
        });
    }
}
