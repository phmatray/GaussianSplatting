//! The pipeline thread.
//!
//! One OS thread owns the [`Pipeline`]. It ticks at `config.target_hz` while
//! at least one client is connected and sleeps on its command channel
//! otherwise. Everything else reaches it as a [`Command`]: clients joining
//! and leaving, snapshot requests, control messages, shutdown. Commands are
//! handled between two ticks, so a snapshot is always consistent with the
//! messages broadcast after it.
//!
//! Broadcasting is a non-blocking push into every client's [`Outbox`]; the
//! thread never waits for a socket.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bytes::Bytes;
use gs_core::pipeline::{Outgoing, Pipeline};
use gs_core::protocol::{ControlMsg, PipelineConfig};
use tokio::sync::OwnedSemaphorePermit;

use crate::outbox::{Outbox, Shared};

/// Longest real time one tick may account for. A longer gap (the process
/// was suspended, the machine slept) must not make the scene jump.
const MAX_TICK_DT: Duration = Duration::from_secs(1);

/// Once a tick is due, pending commands are still served for this long
/// before ticking (at least one is always served). Keeps joins and control
/// messages responsive when the pipeline cannot hold its target rate,
/// without letting a flood of commands starve the ticks.
const LATE_COMMAND_BUDGET: Duration = Duration::from_millis(4);

/// How many times the pipeline is rebuilt after a panic before the thread
/// gives up. A panic is a bug; restarting keeps the server usable, the cap
/// keeps a bug that strikes at every start from spinning forever.
const MAX_RESTARTS: u32 = 5;

/// A request to the pipeline thread.
pub(crate) enum Command {
    /// A new client: register it and give it a snapshot.
    Join(Arc<Outbox>),
    /// The client with this id dropped its backlog and wants a snapshot.
    Resync(u64),
    /// The client with this id is gone.
    Leave(u64),
    /// Apply a control message and broadcast the result. The permit is the
    /// sender's flow-control token; dropping it lets the client send more.
    Control(ControlMsg, Option<OwnedSemaphorePermit>),
    /// Broadcast arbitrary messages as if the pipeline had produced them.
    #[cfg(test)]
    Inject(Vec<Outgoing>),
    /// Panic, as a bug in the pipeline would.
    #[cfg(test)]
    Panic,
    /// Stop the thread.
    Shutdown,
}

/// Counters published by the pipeline thread.
#[derive(Default)]
pub(crate) struct HubMetrics {
    pub ticks: AtomicU64,
    pub resyncs: AtomicU64,
    pub controls: AtomicU64,
    /// `f32` bits of [`Pipeline::scene_time`] after the latest tick or
    /// control message.
    pub scene_time: AtomicU32,
    /// Clients registered with the pipeline thread.
    pub registered: AtomicU64,
}

/// Owner side of the pipeline thread.
pub(crate) struct Hub {
    tx: Sender<Command>,
    thread: std::sync::Mutex<Option<JoinHandle<()>>>,
    pub metrics: Arc<HubMetrics>,
}

impl Hub {
    /// Starts the thread. The pipeline is built on it, so this returns at
    /// once even for a heavy configuration; clients that join meanwhile
    /// get their snapshot as soon as it is ready.
    pub fn start(config: PipelineConfig) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        let metrics = Arc::new(HubMetrics::default());
        let thread = {
            let metrics = metrics.clone();
            std::thread::Builder::new().name("gs-pipeline".into()).spawn(move || {
                let mut clients = Vec::new();
                let mut restarts = 0;
                loop {
                    let outcome = catch_unwind(AssertUnwindSafe(|| run(config, &rx, &metrics, &mut clients)));
                    // However the run ended, its viewers are let go: no
                    // socket may wait forever on a pipeline that is gone,
                    // and after a panic their stream cannot be trusted.
                    for client in clients.drain(..) {
                        client.close();
                    }
                    metrics.registered.store(0, Ordering::Relaxed);
                    match outcome {
                        Ok(()) => break,
                        // Start again from the initial configuration (the
                        // one that led to the panic is not replayed);
                        // viewers reconnect by themselves.
                        Err(_) if restarts < MAX_RESTARTS => {
                            restarts += 1;
                            log::error!("le pipeline s'est arrêté sur une erreur interne ; redémarrage ({restarts}/{MAX_RESTARTS})");
                        }
                        Err(_) => {
                            log::error!("le pipeline s'est arrêté sur une erreur interne ; il ne sera plus redémarré");
                            break;
                        }
                    }
                }
                // Dropping `rx` here makes every later `send` fail, which
                // is how sessions learn that the pipeline is gone for good.
            })?
        };
        Ok(Self { tx, thread: std::sync::Mutex::new(Some(thread)), metrics })
    }

    /// Sends a command. `false` when the thread is gone.
    pub fn send(&self, command: Command) -> bool {
        self.tx.send(command).is_ok()
    }

    /// Asks the thread to stop and returns its join handle (once).
    pub fn stop(&self) -> Option<JoinHandle<()>> {
        let _ = self.tx.send(Command::Shutdown);
        self.thread.lock().unwrap_or_else(|p| p.into_inner()).take()
    }
}

impl Drop for Hub {
    fn drop(&mut self) {
        // Not joined here: a tick may take a while and drop must not block.
        let _ = self.tx.send(Command::Shutdown);
    }
}

fn tick_period(pipeline: &Pipeline) -> Duration {
    // `target_hz` is sanitized to [1, 120] by the pipeline; the clamp only
    // protects the division.
    Duration::from_secs_f64(1.0 / f64::from(pipeline.config().target_hz).clamp(0.1, 1000.0))
}

fn share(messages: Vec<Outgoing>) -> Vec<Shared> {
    messages.into_iter().map(|m| Shared { bytes: Bytes::from(m.bytes), reliable: m.reliable, slot: m.slot }).collect()
}

struct Loop<'a> {
    pipeline: Pipeline,
    clients: &'a mut Vec<Arc<Outbox>>,
    metrics: &'a HubMetrics,
    /// The snapshot of the current pipeline state, encoded at most once
    /// between two changes and shared by every client that asks for it.
    snapshot: Option<Vec<Bytes>>,
}

impl Loop<'_> {
    fn broadcast(&mut self, messages: Vec<Outgoing>) {
        if messages.is_empty() || self.clients.is_empty() {
            return;
        }
        let batch = share(messages);
        for client in self.clients.iter() {
            if client.push(&batch) {
                log::debug!("client {}: reliable backlog over its bound, resynchronising", client.id());
            }
        }
    }

    fn sync(&mut self, client: &Outbox) -> bool {
        let pipeline = &self.pipeline;
        let snapshot = self
            .snapshot
            .get_or_insert_with(|| pipeline.snapshot().into_iter().map(|m| Bytes::from(m.bytes)).collect());
        client.sync(snapshot)
    }

    fn publish_scene_time(&self) {
        self.metrics.scene_time.store(self.pipeline.scene_time().to_bits(), Ordering::Relaxed);
    }

    /// Returns `false` on [`Command::Shutdown`].
    fn handle(&mut self, command: Command) -> bool {
        match command {
            Command::Join(client) => {
                self.sync(&client);
                self.clients.push(client);
                self.metrics.registered.store(self.clients.len() as u64, Ordering::Relaxed);
            }
            Command::Resync(id) => {
                if let Some(client) = self.clients.iter().find(|c| c.id() == id).cloned() {
                    if self.sync(&client) {
                        self.metrics.resyncs.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            Command::Leave(id) => {
                self.clients.retain(|c| c.id() != id);
                self.metrics.registered.store(self.clients.len() as u64, Ordering::Relaxed);
            }
            Command::Control(msg, permit) => {
                let out = self.pipeline.handle_control(msg);
                self.snapshot = None;
                self.metrics.controls.fetch_add(1, Ordering::Relaxed);
                self.publish_scene_time();
                self.broadcast(out);
                drop(permit);
            }
            #[cfg(test)]
            Command::Inject(messages) => self.broadcast(messages),
            #[cfg(test)]
            Command::Panic => panic!("deliberate panic of the pipeline thread (test)"),
            Command::Shutdown => return false,
        }
        true
    }

    fn tick(&mut self, dt: Duration) {
        let out = self.pipeline.tick(dt.as_secs_f32());
        self.snapshot = None;
        self.metrics.ticks.fetch_add(1, Ordering::Relaxed);
        self.publish_scene_time();
        self.broadcast(out);
    }
}

fn run(config: PipelineConfig, rx: &Receiver<Command>, metrics: &HubMetrics, clients: &mut Vec<Arc<Outbox>>) {
    let mut hub = Loop { pipeline: Pipeline::new(config), clients, metrics, snapshot: None };
    hub.publish_scene_time();

    // Start of the previous tick, and the deadline of the next one. Ticks
    // are scheduled on absolute deadlines (`deadline += period`) so that
    // sleep jitter does not accumulate, while `dt` is the measured time
    // between two ticks so that the scene follows the wall clock.
    let mut last_tick = Instant::now();
    let mut deadline = last_tick;

    loop {
        if hub.clients.is_empty() {
            // Idle: no ticking, block until something happens.
            let Ok(command) = rx.recv() else { return };
            if !hub.handle(command) {
                return;
            }
            if !hub.clients.is_empty() {
                // The scene resumes where it stopped.
                last_tick = Instant::now();
                deadline = last_tick + tick_period(&hub.pipeline);
            }
            continue;
        }

        let now = Instant::now();
        if now < deadline {
            match rx.recv_timeout(deadline - now) {
                Ok(command) => {
                    if !hub.handle(command) {
                        return;
                    }
                    // A control message may have raised the tick rate.
                    deadline = deadline.min(Instant::now() + tick_period(&hub.pipeline));
                    continue;
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        } else {
            // Running late: serve what is already queued, briefly.
            let mut served = 0;
            while served == 0 || now.elapsed() < LATE_COMMAND_BUDGET {
                match rx.try_recv() {
                    Ok(command) => {
                        if !hub.handle(command) {
                            return;
                        }
                        served += 1;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return,
                }
            }
            if hub.clients.is_empty() {
                continue;
            }
        }

        let start = Instant::now();
        hub.tick(start.duration_since(last_tick).min(MAX_TICK_DT));
        last_tick = start;
        deadline += tick_period(&hub.pipeline);
        // Too slow for the target rate: tick back to back, without trying
        // to catch up on the deadlines that were missed.
        let now = Instant::now();
        if deadline < now {
            deadline = now;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::outbox::{Next, QueueLimits};
    use gs_core::codec::{self, Kind};
    use gs_core::rig::RingRig;
    use gs_core::scene::SceneKind;

    fn small_stage(hz: f32) -> PipelineConfig {
        PipelineConfig {
            rig: RingRig { count: 3, width: 48, height: 36, ..RingRig::default() },
            target_hz: hz,
            ..PipelineConfig::for_scene(SceneKind::Stage)
        }
    }

    async fn next_kind(outbox: &Outbox) -> Kind {
        match tokio::time::timeout(Duration::from_secs(20), outbox.next()).await.expect("nothing arrived") {
            Next::Message(bytes) => codec::peek_kind(&bytes).unwrap(),
            other => panic!("expected a message, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn join_gets_the_snapshot_then_the_live_stream() {
        let hub = Hub::start(small_stage(50.0)).unwrap();
        assert_eq!(hub.metrics.ticks.load(Ordering::Relaxed), 0);
        let outbox = Arc::new(Outbox::new(1, QueueLimits::default()));
        assert!(hub.send(Command::Join(outbox.clone())));
        let mut kinds = Vec::new();
        for _ in 0..4 {
            kinds.push(next_kind(&outbox).await);
        }
        assert_eq!(kinds, [Kind::State, Kind::Layer, Kind::Layer, Kind::Stats]);
        // The live stream follows.
        let mut layers = 0;
        for _ in 0..12 {
            if next_kind(&outbox).await == Kind::Layer {
                layers += 1;
            }
        }
        assert!(layers >= 3, "only {layers} live layers in 12 messages");
        hub.stop().unwrap().join().unwrap();
        assert!(matches!(outbox.next().await, Next::Closed));
        assert!(!hub.send(Command::Leave(1)));
    }

    #[tokio::test]
    async fn idle_without_clients_and_paced_with_one() {
        let hub = Hub::start(small_stage(40.0)).unwrap();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(hub.metrics.ticks.load(Ordering::Relaxed), 0, "ticked without a client");

        let outbox = Arc::new(Outbox::new(1, QueueLimits::default()));
        hub.send(Command::Join(outbox.clone()));
        // Let the stream start, then measure over a fixed window.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let (t0, n0) = (Instant::now(), hub.metrics.ticks.load(Ordering::Relaxed));
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let (elapsed, n1) = (t0.elapsed().as_secs_f64(), hub.metrics.ticks.load(Ordering::Relaxed));
        let hz = (n1 - n0) as f64 / elapsed;
        assert!((36.0..=42.0).contains(&hz), "ticked at {hz:.1} Hz instead of 40");

        hub.send(Command::Leave(1));
        tokio::time::sleep(Duration::from_millis(200)).await;
        let n2 = hub.metrics.ticks.load(Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(hub.metrics.ticks.load(Ordering::Relaxed), n2, "kept ticking after the last client left");
        hub.stop().unwrap().join().unwrap();
    }

    #[tokio::test]
    async fn a_panic_disconnects_the_viewers_and_the_pipeline_restarts() {
        let hub = Hub::start(small_stage(50.0)).unwrap();
        let first = Arc::new(Outbox::new(1, QueueLimits::default()));
        hub.send(Command::Join(first.clone()));
        assert_eq!(next_kind(&first).await, Kind::State);

        hub.send(Command::Panic);
        // The viewer of the crashed run is told that its stream is over.
        loop {
            match tokio::time::timeout(Duration::from_secs(20), first.next()).await.expect("left hanging") {
                Next::Closed => break,
                Next::Message(_) | Next::Resync => {}
            }
        }
        // Messages about that viewer no longer mean anything; a new one is
        // served by the rebuilt pipeline.
        assert!(hub.send(Command::Leave(1)));
        let second = Arc::new(Outbox::new(2, QueueLimits::default()));
        assert!(hub.send(Command::Join(second.clone())));
        let mut kinds = Vec::new();
        for _ in 0..4 {
            kinds.push(next_kind(&second).await);
        }
        assert_eq!(kinds, [Kind::State, Kind::Layer, Kind::Layer, Kind::Stats]);
        let ticks = hub.metrics.ticks.load(Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(hub.metrics.ticks.load(Ordering::Relaxed) > ticks + 5, "the rebuilt pipeline does not tick");

        // A bug that strikes again and again ends the thread for good.
        for _ in 0..MAX_RESTARTS {
            hub.send(Command::Panic);
        }
        hub.stop().unwrap().join().unwrap();
        assert!(matches!(second.next().await, Next::Closed));
        assert!(!hub.send(Command::Leave(2)));
    }
}
