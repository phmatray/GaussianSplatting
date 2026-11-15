//! Relay of a native server's WebSocket stream through the worker.
//!
//! A page that renders heavily dispatches network events slowly: frames of
//! a WebSocket opened on the main thread then pile up in the browser and in
//! the TCP buffers, and the picture falls seconds behind the pipeline. The
//! server's own latest-wins queue cannot help, it only acts once all those
//! buffers are full.
//!
//! So the socket lives here, on a thread that is always free to read it.
//! [`Relay`] keeps what the page has not taken yet, coalesced by the
//! delivery contract of `docs/ARCHITECTURE.md` (State and layer 0 are
//! reliable and ordered; layer 1, Stats and each camera's Preview are
//! latest wins), and hands it over in acknowledged batches, the same flow
//! control the local pipeline uses. However slow the page, it is at most
//! [`crate::MAX_UNACKED_TICKS`] batches behind, and each batch holds the
//! newest data known when it left.

use gs_core::codec::{self, Kind, LayerMode, HEADER_SIZE};

use crate::{Batch, FlowControl};

/// The relay gives up on a connection whose backlog exceeds this many
/// bytes (reliable messages a page stuck for minutes never took): the
/// caller reconnects and the server's fresh snapshot replaces the backlog.
pub const MAX_RELAY_BYTES: usize = 96 * 1024 * 1024;

/// How a codec message may be coalesced while it waits for the page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Delivery {
    /// Must reach the page, in order.
    Reliable,
    /// A full replacement of the reliable layer 0: must reach the page,
    /// and makes every earlier layer 0 message pointless.
    ReplaceStatic,
    /// Only the newest message of this slot matters.
    Latest(u16),
}

/// Classifies one `gs_core::codec` message. Anything unrecognised is
/// reliable: the page gets it and reports what is wrong with it.
pub fn delivery_of(bytes: &[u8]) -> Delivery {
    let Ok(kind) = codec::peek_kind(bytes) else { return Delivery::Reliable };
    // First byte after the common header: the layer or the camera id.
    let id = bytes.get(HEADER_SIZE).copied().unwrap_or(0);
    match kind {
        Kind::State => Delivery::Reliable,
        Kind::Layer if id == 0 => {
            let replace = bytes.get(HEADER_SIZE + 1).copied() == Some(LayerMode::Replace as u8);
            if replace { Delivery::ReplaceStatic } else { Delivery::Reliable }
        }
        Kind::Layer => Delivery::Latest(0x100 | u16::from(id)),
        Kind::Stats => Delivery::Latest(0x200),
        Kind::Preview => Delivery::Latest(0x300 | u16::from(id)),
    }
}

/// True for a message that belongs to the reliable layer 0.
fn is_static_layer(bytes: &[u8]) -> bool {
    codec::peek_kind(bytes) == Ok(Kind::Layer) && bytes.get(HEADER_SIZE).copied() == Some(0)
}

/// The backlog outgrew [`MAX_RELAY_BYTES`]; the queue has been emptied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RelayOverflow;

struct Pending {
    delivery: Delivery,
    bytes: Vec<u8>,
}

/// Messages of the server that wait for the page.
#[derive(Default)]
pub struct Relay {
    queue: Vec<Pending>,
    queued_bytes: usize,
    flow: FlowControl,
    /// Messages dropped because a newer one took their place.
    superseded: u64,
}

impl Relay {
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues one frame received from the server.
    pub fn push(&mut self, bytes: Vec<u8>) -> Result<(), RelayOverflow> {
        let delivery = delivery_of(&bytes);
        let before = self.queue.len();
        match delivery {
            Delivery::Reliable => {}
            Delivery::ReplaceStatic => self.queue.retain(|p| !is_static_layer(&p.bytes)),
            Delivery::Latest(_) => self.queue.retain(|p| p.delivery != delivery),
        }
        if self.queue.len() != before {
            self.superseded += (before - self.queue.len()) as u64;
            self.queued_bytes = self.queue.iter().map(|p| p.bytes.len()).sum();
        }
        self.queued_bytes += bytes.len();
        self.queue.push(Pending { delivery, bytes });
        if self.queued_bytes > MAX_RELAY_BYTES {
            self.clear();
            return Err(RelayOverflow);
        }
        Ok(())
    }

    /// Records the acknowledgement of every batch up to `batch`.
    pub fn ack(&mut self, batch: u32) {
        self.flow.ack(batch);
    }

    /// Everything that waits, as one batch, if the page has kept up.
    pub fn next_batch(&mut self) -> Option<Batch> {
        if self.queue.is_empty() || !self.flow.can_tick() {
            return None;
        }
        self.queued_bytes = 0;
        let messages = self.queue.drain(..).map(|p| p.bytes).collect();
        Some(Batch { id: self.flow.next_id(), messages })
    }

    /// Forgets what waits: the connection was replaced, and a snapshot of
    /// the new one supersedes it all. Batch ids keep counting, so that the
    /// acknowledgements of batches still on their way stay meaningful.
    pub fn clear(&mut self) {
        self.queue.clear();
        self.queued_bytes = 0;
    }

    /// Messages waiting for the page.
    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// Bytes waiting for the page.
    pub fn queued_bytes(&self) -> usize {
        self.queued_bytes
    }

    /// Messages dropped so far because a newer one replaced them.
    pub fn superseded(&self) -> u64 {
        self.superseded
    }

    pub fn flow(&self) -> FlowControl {
        self.flow
    }
}

#[cfg(test)]
mod tests {
    use gs_core::codec::{encode, encode_layer, Body, Message, PreviewMsg};
    use gs_core::pipeline::Pipeline;
    use gs_core::protocol::PipelineConfig;
    use gs_core::scene::SceneKind;
    use gs_core::Splat;

    use super::*;
    use crate::MAX_UNACKED_TICKS;

    fn splats(n: usize) -> Vec<Splat> {
        (0..n).map(|i| Splat { pos: [i as f32, 0.0, 0.0], scale: [0.01; 3], rot: [0.0, 0.0, 0.0, 1.0], rgba: [200; 4] }).collect()
    }

    fn layer(seq: u32, layer: u8, mode: LayerMode, n: usize) -> Vec<u8> {
        let mut out = Vec::new();
        encode_layer(seq, 0, layer, mode, &splats(n), &mut out);
        out
    }

    fn state(seq: u32) -> Vec<u8> {
        encode(&Message { seq, time_ms: 0, body: Body::State("{}".into()) })
    }

    fn stats(seq: u32) -> Vec<u8> {
        encode(&Message { seq, time_ms: 0, body: Body::Stats("{}".into()) })
    }

    fn preview(seq: u32, camera: u8) -> Vec<u8> {
        encode(&Message { seq, time_ms: 0, body: Body::Preview(PreviewMsg { camera_id: camera, ..PreviewMsg::default() }) })
    }

    fn seqs(batch: &Batch) -> Vec<u32> {
        batch.messages.iter().map(|m| codec::peek_header(m).unwrap().seq).collect()
    }

    #[test]
    fn classification_follows_the_delivery_contract() {
        assert_eq!(delivery_of(&state(0)), Delivery::Reliable);
        assert_eq!(delivery_of(&layer(0, 0, LayerMode::Append, 3)), Delivery::Reliable);
        assert_eq!(delivery_of(&layer(0, 0, LayerMode::Replace, 3)), Delivery::ReplaceStatic);
        assert_eq!(delivery_of(&layer(0, 1, LayerMode::Replace, 3)), Delivery::Latest(0x101));
        assert_eq!(delivery_of(&stats(0)), Delivery::Latest(0x200));
        assert_eq!(delivery_of(&preview(0, 4)), Delivery::Latest(0x304));
        assert_ne!(delivery_of(&preview(0, 4)), delivery_of(&preview(0, 5)));
        // Garbage is passed on untouched.
        assert_eq!(delivery_of(b"junk"), Delivery::Reliable);
        assert_eq!(delivery_of(&[]), Delivery::Reliable);
    }

    #[test]
    fn it_agrees_with_what_the_pipeline_marks_reliable() {
        for scene in [SceneKind::Stage, SceneKind::House] {
            let mut pipeline = Pipeline::new(PipelineConfig::light(scene));
            let mut messages = pipeline.snapshot();
            for _ in 0..12 {
                messages.extend(pipeline.tick(0.05));
            }
            assert!(messages.len() > 12);
            for message in &messages {
                let reliable = !matches!(delivery_of(&message.bytes), Delivery::Latest(_));
                assert_eq!(reliable, message.reliable, "{scene:?}: {:?}", codec::peek_header(&message.bytes));
            }
            // Two latest-wins messages share a relay slot exactly when the
            // pipeline gives them the same slot.
            let latest: Vec<_> = messages.iter().filter(|m| !m.reliable).collect();
            for a in &latest {
                for b in &latest {
                    assert_eq!(delivery_of(&a.bytes) == delivery_of(&b.bytes), a.slot == b.slot);
                }
            }
        }
    }

    #[test]
    fn a_fast_page_gets_every_message_in_order() {
        let mut relay = Relay::new();
        for seq in 0..20 {
            relay.push(if seq % 2 == 0 { layer(seq, 1, LayerMode::Replace, 4) } else { stats(seq) }).unwrap();
            let batch = relay.next_batch().unwrap();
            assert_eq!(seqs(&batch), [seq]);
            relay.ack(batch.id);
        }
        assert_eq!(relay.superseded(), 0);
        assert_eq!((relay.queued(), relay.queued_bytes()), (0, 0));
    }

    #[test]
    fn a_stuck_page_only_ever_gets_the_newest_of_each_slot() {
        let mut relay = Relay::new();
        relay.push(state(0)).unwrap();
        relay.push(layer(1, 0, LayerMode::Replace, 50)).unwrap();
        // The page takes two batches and then stops acknowledging.
        let first = relay.next_batch().unwrap();
        assert_eq!(seqs(&first), [0, 1]);
        relay.push(stats(2)).unwrap();
        assert!(relay.next_batch().is_some());
        // Ten seconds of a live stage: 200 ticks of layer 1, Stats and
        // round-robin previews, plus two things that must not be lost.
        let mut seq = 3;
        for tick in 0..200u32 {
            relay.push(layer(seq, 1, LayerMode::Replace, 100)).unwrap();
            relay.push(preview(seq + 1, (tick % 6) as u8)).unwrap();
            relay.push(stats(seq + 2)).unwrap();
            if tick == 50 {
                relay.push(state(1000)).unwrap();
            }
            if tick == 120 {
                relay.push(layer(1001, 0, LayerMode::Append, 10)).unwrap();
            }
            seq += 3;
            assert!(relay.next_batch().is_none(), "nothing leaves while {MAX_UNACKED_TICKS} batches are outstanding");
            assert!(relay.queued() <= 2 + 1 + 6 + 1);
        }
        assert_eq!(relay.flow().outstanding(), MAX_UNACKED_TICKS);
        relay.ack(first.id);
        let batch = relay.next_batch().unwrap();
        // Reliable messages in arrival order, then the newest of each slot.
        let got = seqs(&batch);
        assert_eq!(&got[..2], [1000, 1001]);
        assert_eq!(got.len(), 2 + 6 + 2, "{got:?}");
        assert!(got.contains(&(seq - 3)) && got.contains(&(seq - 1)), "newest layer and stats: {got:?}");
        assert!(got[2..].iter().all(|s| *s >= seq - 3 * 6), "nothing older than the last preview round: {got:?}");
        assert!(relay.superseded() > 500);
    }

    #[test]
    fn a_replace_of_the_static_layer_drops_what_it_makes_pointless() {
        let mut relay = Relay::new();
        relay.push(state(0)).unwrap();
        for seq in 1..=30 {
            relay.push(layer(seq, 0, LayerMode::Append, 20)).unwrap();
        }
        relay.push(layer(40, 1, LayerMode::Replace, 5)).unwrap();
        let before = relay.queued_bytes();
        relay.push(layer(50, 0, LayerMode::Replace, 600)).unwrap();
        relay.push(layer(51, 0, LayerMode::Append, 20)).unwrap();
        assert!(relay.queued_bytes() < before + layer(50, 0, LayerMode::Replace, 600).len());
        let batch = relay.next_batch().unwrap();
        // The State and the live layer stay where they were.
        assert_eq!(seqs(&batch), [0, 40, 50, 51]);
        assert_eq!(relay.superseded(), 30);
    }

    #[test]
    fn byte_count_tracks_the_queue() {
        let mut relay = Relay::new();
        let (a, b) = (stats(1), layer(2, 1, LayerMode::Replace, 7));
        relay.push(a.clone()).unwrap();
        relay.push(b.clone()).unwrap();
        relay.push(a.clone()).unwrap();
        assert_eq!(relay.queued_bytes(), a.len() + b.len());
        relay.clear();
        assert_eq!((relay.queued(), relay.queued_bytes()), (0, 0));
    }

    #[test]
    fn an_endless_reliable_backlog_overflows_instead_of_growing() {
        let mut relay = Relay::new();
        let chunk = layer(0, 0, LayerMode::Append, 60_000);
        let mut pushed = 0usize;
        let overflowed = loop {
            match relay.push(chunk.clone()) {
                Ok(()) => pushed += chunk.len(),
                Err(RelayOverflow) => break true,
            }
            assert!(relay.queued_bytes() <= MAX_RELAY_BYTES);
            if pushed > 2 * MAX_RELAY_BYTES {
                break false;
            }
        };
        assert!(overflowed);
        assert_eq!((relay.queued(), relay.queued_bytes()), (0, 0));
        // The relay is usable again for the next connection.
        relay.push(state(1)).unwrap();
        assert_eq!(seqs(&relay.next_batch().unwrap()), [1]);
    }

    #[test]
    fn clearing_keeps_batch_ids_and_acknowledgements_consistent() {
        let mut relay = Relay::new();
        relay.push(state(0)).unwrap();
        let old = relay.next_batch().unwrap();
        relay.push(stats(1)).unwrap();
        relay.clear();
        relay.push(state(2)).unwrap();
        let new = relay.next_batch().unwrap();
        assert_eq!(new.id, old.id + 1);
        assert_eq!(seqs(&new), [2]);
        // The late acknowledgement of the old batch frees one place only.
        relay.ack(old.id);
        assert_eq!(relay.flow().outstanding(), 1);
        relay.ack(new.id);
        assert_eq!(relay.flow().outstanding(), 0);
    }
}
