//! Per-client delivery queue between the pipeline thread and one WebSocket.
//!
//! The pipeline thread pushes, the client's socket task pops. Pushing never
//! blocks and never waits for the client: a slow or stalled viewer only
//! affects its own [`Outbox`].
//!
//! Two kinds of messages live here (see `gs_core::pipeline` for the
//! contract):
//!
//! * *ordered* messages (State, everything on layer 0, and the messages of a
//!   snapshot) are queued and sent in order;
//! * *latest-wins* messages (live layer, Stats, previews) sit in one mailbox
//!   per slot, where a newer message replaces an unsent older one.
//!
//! Ordered messages go first, so a latest-wins message is never sent before
//! a reliable message that preceded it in the pipeline. The mailboxes are
//! then served in the order in which they became non-empty; replacing the
//! content of a mailbox does not move it back. A client that keeps up
//! therefore receives everything in pipeline order, and a client slower
//! than the tick rate still gets every slot in turn (always its newest
//! content) instead of, say, nothing but previews.
//!
//! The ordered backlog is bounded. When a push would exceed the bound the
//! queue is dropped and the outbox waits for a fresh snapshot, which the
//! socket task requests once it is able to send again. A client that reads
//! nothing therefore holds no memory at all after the overflow.

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};

use bytes::Bytes;
use tokio::sync::Notify;

/// Bound of the ordered backlog of one client.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueLimits {
    /// Most reliable messages waiting for one client.
    pub max_messages: usize,
    /// Most bytes of reliable messages waiting for one client. A single
    /// message is always accepted on an empty backlog, whatever its size.
    pub max_bytes: usize,
}

impl Default for QueueLimits {
    fn default() -> Self {
        // A house scan appends about 30 small chunks per second: the count
        // bound tolerates a stall of several seconds. The byte bound covers
        // bursts of whole-layer `Replace` messages (a few megabytes each).
        Self { max_messages: 256, max_bytes: 32 << 20 }
    }
}

/// One encoded message as the pipeline thread hands it to every client.
/// Cloning shares the bytes.
#[derive(Clone, Debug)]
pub(crate) struct Shared {
    pub bytes: Bytes,
    pub reliable: bool,
    pub slot: u8,
}

/// What the socket task has to do next.
#[derive(Debug)]
pub(crate) enum Next {
    /// Send this message.
    Message(Bytes),
    /// The backlog overflowed and was dropped: ask the pipeline thread for
    /// a snapshot. Returned once per overflow.
    Resync,
    /// The pipeline is gone; nothing more will come.
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Waiting for a snapshot; broadcasts are skipped because the snapshot
    /// will contain their effect.
    Syncing,
    /// Receiving broadcasts.
    Live,
    /// The backlog was dropped; the socket task has not asked for a
    /// snapshot yet.
    Overflowed,
}

struct Entry {
    bytes: Bytes,
    /// Counts against [`QueueLimits`]. Snapshot messages do not: a snapshot
    /// is the smallest thing that brings a client up to date, so it must
    /// always fit.
    counted: bool,
}

struct State {
    ordered: VecDeque<Entry>,
    backlog_messages: usize,
    backlog_bytes: usize,
    /// Non-empty mailboxes as `(slot, newest message)`, in the order in
    /// which they became non-empty. There are a few dozen slots at most.
    latest: VecDeque<(u8, Bytes)>,
    phase: Phase,
    closed: bool,
}

/// See the module documentation.
pub(crate) struct Outbox {
    id: u64,
    limits: QueueLimits,
    state: Mutex<State>,
    wake: Notify,
}

impl Outbox {
    /// An empty outbox waiting for its first snapshot.
    pub fn new(id: u64, limits: QueueLimits) -> Self {
        Self {
            id,
            limits,
            state: Mutex::new(State {
                ordered: VecDeque::new(),
                backlog_messages: 0,
                backlog_bytes: 0,
                latest: VecDeque::new(),
                phase: Phase::Syncing,
                closed: false,
            }),
            wake: Notify::new(),
        }
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // The critical sections below cannot panic half-way through an
        // update, so a poisoned lock still guards a consistent state.
        self.state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    // -- pipeline side ------------------------------------------------------

    /// Queues the messages of one tick or control reply. Returns `true` when
    /// this push overflowed the backlog (the outbox then drops everything
    /// and waits for a snapshot).
    pub fn push(&self, batch: &[Shared]) -> bool {
        let mut overflowed = false;
        {
            let mut state = self.lock();
            if state.closed || state.phase != Phase::Live {
                return false;
            }
            for msg in batch {
                if !msg.reliable {
                    match state.latest.iter_mut().find(|(slot, _)| *slot == msg.slot) {
                        Some((_, pending)) => *pending = msg.bytes.clone(),
                        None => state.latest.push_back((msg.slot, msg.bytes.clone())),
                    }
                    continue;
                }
                let full = state.backlog_messages >= self.limits.max_messages
                    || (state.backlog_messages > 0
                        && state.backlog_bytes.saturating_add(msg.bytes.len()) > self.limits.max_bytes);
                if full {
                    state.ordered.clear();
                    state.latest.clear();
                    state.backlog_messages = 0;
                    state.backlog_bytes = 0;
                    state.phase = Phase::Overflowed;
                    overflowed = true;
                    break;
                }
                state.backlog_messages += 1;
                state.backlog_bytes += msg.bytes.len();
                state.ordered.push_back(Entry { bytes: msg.bytes.clone(), counted: true });
            }
        }
        self.wake.notify_one();
        overflowed
    }

    /// Replaces everything that is queued by `snapshot` (in order) and
    /// resumes live delivery. Only acts on an outbox that is waiting for a
    /// snapshot; returns whether it was.
    pub fn sync(&self, snapshot: &[Bytes]) -> bool {
        {
            let mut state = self.lock();
            if state.closed || state.phase != Phase::Syncing {
                return false;
            }
            state.ordered.clear();
            state.latest.clear();
            state.backlog_messages = 0;
            state.backlog_bytes = 0;
            state.ordered.extend(snapshot.iter().map(|bytes| Entry { bytes: bytes.clone(), counted: false }));
            state.phase = Phase::Live;
        }
        self.wake.notify_one();
        true
    }

    /// Drops everything and makes [`Outbox::next`] return [`Next::Closed`].
    pub fn close(&self) {
        {
            let mut state = self.lock();
            state.closed = true;
            state.ordered.clear();
            state.latest.clear();
            state.backlog_messages = 0;
            state.backlog_bytes = 0;
        }
        self.wake.notify_one();
    }

    // -- socket side --------------------------------------------------------

    fn try_next(&self) -> Option<Next> {
        let mut state = self.lock();
        if state.closed {
            return Some(Next::Closed);
        }
        if let Some(entry) = state.ordered.pop_front() {
            if entry.counted {
                state.backlog_messages -= 1;
                state.backlog_bytes -= entry.bytes.len();
            }
            return Some(Next::Message(entry.bytes));
        }
        if state.phase == Phase::Overflowed {
            state.phase = Phase::Syncing;
            return Some(Next::Resync);
        }
        state.latest.pop_front().map(|(_, bytes)| Next::Message(bytes))
    }

    /// Waits for the next thing to do. Meant for a single consumer; safe to
    /// drop and call again (nothing is lost).
    pub async fn next(&self) -> Next {
        loop {
            if let Some(next) = self.try_next() {
                return next;
            }
            // `notify_one` stores a permit when nobody waits, so a push
            // between `try_next` and this await is not missed.
            self.wake.notified().await;
        }
    }

    /// Messages and bytes currently held for this client (ordered queue
    /// plus mailboxes): what was produced for it and not yet handed to its
    /// socket.
    pub fn held(&self) -> (usize, usize) {
        let state = self.lock();
        let messages = state.ordered.len() + state.latest.len();
        let bytes = state.ordered.iter().map(|e| e.bytes.len()).sum::<usize>()
            + state.latest.iter().map(|(_, bytes)| bytes.len()).sum::<usize>();
        (messages, bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reliable(tag: u8, len: usize) -> Shared {
        Shared { bytes: Bytes::from(vec![tag; len]), reliable: true, slot: 0 }
    }

    fn latest(slot: u8, tag: u8) -> Shared {
        Shared { bytes: Bytes::from(vec![tag; 4]), reliable: false, slot }
    }

    fn live(limits: QueueLimits) -> Outbox {
        let outbox = Outbox::new(7, limits);
        assert!(outbox.sync(&[]));
        outbox
    }

    fn pop_tag(outbox: &Outbox) -> Option<u8> {
        match outbox.try_next()? {
            Next::Message(bytes) => Some(bytes[0]),
            other => panic!("expected a message, got {other:?}"),
        }
    }

    fn drain_tags(outbox: &Outbox) -> Vec<u8> {
        std::iter::from_fn(|| pop_tag(outbox)).collect()
    }

    #[test]
    fn nothing_is_delivered_before_the_first_snapshot() {
        let outbox = Outbox::new(1, QueueLimits::default());
        assert!(!outbox.push(&[reliable(1, 8), latest(1, 2)]));
        assert!(outbox.try_next().is_none());
        assert_eq!(outbox.held(), (0, 0));

        assert!(outbox.sync(&[Bytes::from_static(&[10]), Bytes::from_static(&[11])]));
        assert!(!outbox.sync(&[Bytes::from_static(&[12])]), "a live outbox must not be reset");
        outbox.push(&[reliable(1, 8)]);
        assert_eq!(drain_tags(&outbox), [10, 11, 1]);
    }

    #[test]
    fn reliable_messages_keep_their_order_and_go_first() {
        let outbox = live(QueueLimits::default());
        outbox.push(&[latest(1, 50), reliable(1, 3), latest(2, 60), reliable(2, 3)]);
        outbox.push(&[reliable(3, 3)]);
        assert_eq!(drain_tags(&outbox), [1, 2, 3, 50, 60]);
    }

    #[test]
    fn a_newer_message_replaces_the_unsent_one_of_its_slot() {
        let outbox = live(QueueLimits::default());
        for tick in 0..100u8 {
            outbox.push(&[latest(1, tick), latest(16 + tick % 4, tick), latest(2, tick)]);
        }
        // One message per slot survives: the newest.
        let (messages, _) = outbox.held();
        assert_eq!(messages, 6);
        // In the order in which the slots first had something to send.
        assert_eq!(drain_tags(&outbox), [99, 96, 99, 97, 98, 99]);
    }

    #[test]
    fn a_client_that_keeps_up_receives_the_pipeline_order() {
        let outbox = live(QueueLimits::default());
        let mut received = Vec::new();
        for tick in 0..5u8 {
            // Live layer, a preview, Stats: the order of a stage tick.
            outbox.push(&[latest(1, 3 * tick), latest(16 + tick % 4, 3 * tick + 1), latest(2, 3 * tick + 2)]);
            received.extend(drain_tags(&outbox));
        }
        assert_eq!(received, (0..15).collect::<Vec<u8>>());
    }

    #[test]
    fn a_slow_client_is_served_every_slot_in_turn() {
        let outbox = live(QueueLimits::default());
        let mut served = Vec::new();
        // A client that manages one send per tick still sees every slot.
        for tick in 0..6u8 {
            outbox.push(&[latest(1, tick), latest(2, 100 + tick), latest(16, 200 + tick)]);
            served.push(pop_tag(&outbox).unwrap());
        }
        assert_eq!(served, [0, 101, 202, 3, 104, 205]);
    }

    #[test]
    fn overflow_drops_the_backlog_and_asks_for_one_resync() {
        let outbox = live(QueueLimits { max_messages: 4, max_bytes: 1 << 20 });
        for tag in 0..4 {
            assert!(!outbox.push(&[reliable(tag, 10), latest(1, tag)]));
        }
        assert!(outbox.push(&[reliable(4, 10), latest(1, 4)]), "the fifth message exceeds the bound");
        assert_eq!(outbox.held(), (0, 0), "an overflowed outbox holds nothing");
        // Later broadcasts are skipped until the snapshot arrives.
        assert!(!outbox.push(&[reliable(5, 10), latest(1, 5)]));
        assert_eq!(outbox.held(), (0, 0));

        assert!(matches!(outbox.try_next(), Some(Next::Resync)));
        assert!(outbox.try_next().is_none(), "the resync is requested once");
        outbox.push(&[reliable(6, 10)]);
        assert!(outbox.try_next().is_none());

        assert!(outbox.sync(&[Bytes::from_static(&[42])]));
        outbox.push(&[reliable(7, 10)]);
        assert_eq!(drain_tags(&outbox), [42, 7]);
    }

    #[test]
    fn byte_bound_overflows_but_one_message_always_fits() {
        let outbox = live(QueueLimits { max_messages: 100, max_bytes: 100 });
        assert!(!outbox.push(&[reliable(1, 500)]), "a single message fits on an empty backlog");
        assert!(outbox.push(&[reliable(2, 1)]));
        assert!(matches!(outbox.try_next(), Some(Next::Resync)));

        let outbox = live(QueueLimits { max_messages: 100, max_bytes: 100 });
        assert!(!outbox.push(&[reliable(1, 60)]));
        assert!(!outbox.push(&[reliable(2, 40)]));
        assert!(outbox.push(&[reliable(3, 1)]));
    }

    #[test]
    fn snapshot_messages_do_not_count_against_the_bound() {
        let outbox = Outbox::new(1, QueueLimits { max_messages: 2, max_bytes: 10 });
        let big = Bytes::from(vec![9u8; 1000]);
        assert!(outbox.sync(&[big.clone(), big.clone(), big.clone(), big]));
        assert!(!outbox.push(&[reliable(1, 5)]));
        assert!(!outbox.push(&[reliable(2, 5)]));
        assert_eq!(drain_tags(&outbox), [9, 9, 9, 9, 1, 2]);
        // Popping released the backlog accounting.
        assert!(!outbox.push(&[reliable(3, 5), reliable(4, 5)]));
        assert!(outbox.push(&[reliable(5, 1)]));
    }

    #[test]
    fn close_wins_over_everything() {
        let outbox = live(QueueLimits::default());
        outbox.push(&[reliable(1, 10), latest(1, 2)]);
        outbox.close();
        assert!(matches!(outbox.try_next(), Some(Next::Closed)));
        assert!(!outbox.push(&[reliable(1, 10)]));
        assert!(!outbox.sync(&[]));
        assert_eq!(outbox.held(), (0, 0));
    }

    #[tokio::test]
    async fn next_wakes_up_on_push_from_another_thread() {
        let outbox = std::sync::Arc::new(live(QueueLimits::default()));
        let pusher = {
            let outbox = outbox.clone();
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(30));
                outbox.push(&[latest(1, 77)]);
            })
        };
        let next = tokio::time::timeout(std::time::Duration::from_secs(5), outbox.next()).await.expect("no wake-up");
        assert!(matches!(next, Next::Message(bytes) if bytes[0] == 77));
        pusher.join().unwrap();
    }
}
