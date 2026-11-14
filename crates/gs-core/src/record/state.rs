//! The state of a session as its stream describes it.

use std::collections::BTreeMap;

use super::wire::{classify, merge_layer, MessageClass, LAYER_COUNT};
use super::RecordError;
use crate::codec::HEADER_SIZE;

/// What a viewer that followed a stream from its start holds: the session
/// state, both layers, the latest statistics and the latest thumbnail of
/// every camera. [`StreamState::snapshot`] turns it into the messages a
/// viewer that joins now needs, which is what a keyframe stores.
///
/// A recorder keeps one. A host that may start recording in the middle of
/// a session (the worker, which cannot ask a remote pipeline for a
/// snapshot) keeps one from the first message on and hands a copy to
/// [`super::Recorder::with_state`]. It costs the memory of the layers:
/// 1.6 MB for the default stage, 12 MB for a finished house scan.
#[derive(Clone, Debug, Default)]
pub struct StreamState {
    state: Option<Vec<u8>>,
    /// Per layer, the messages that built it: a `Replace` (or the first
    /// `Append`) followed by `Append`s.
    layers: [Vec<Vec<u8>>; LAYER_COUNT],
    layer_bytes: usize,
    stats: Option<Vec<u8>>,
    previews: BTreeMap<u8, Vec<u8>>,
}

impl StreamState {
    /// Most bytes of layer messages a state keeps (512 MiB, 33 million
    /// splats); [`StreamState::apply`] refuses what would exceed it.
    pub const MAX_LAYER_BYTES: usize = 512 << 20;

    pub fn new() -> Self {
        Self::default()
    }

    /// The state after `messages`, skipping what [`StreamState::apply`]
    /// refuses: for example from `Pipeline::snapshot()`.
    pub fn from_messages<'a>(messages: impl IntoIterator<Item = &'a [u8]>) -> Self {
        let mut state = Self::new();
        for message in messages {
            let _ = state.apply(message);
        }
        state
    }

    /// Applies one wire message, as a viewer would.
    ///
    /// [`RecordError::BadMessage`] for a message a viewer would refuse
    /// (see [`classify`]), [`RecordError::TooLarge`] when the layers would
    /// outgrow [`StreamState::MAX_LAYER_BYTES`]; the state is unchanged
    /// then.
    pub fn apply(&mut self, message: &[u8]) -> Result<MessageClass, RecordError> {
        let class = classify(message).ok_or(RecordError::BadMessage)?;
        match class {
            MessageClass::State => {
                // Another scene has other cameras: its viewers drop the
                // statistics and thumbnails of the old one.
                let changed = self.state.as_ref().is_none_or(|old| scene_of(old) != scene_of(message));
                if changed {
                    self.stats = None;
                    self.previews.clear();
                }
                self.state = Some(message.to_vec());
            }
            MessageClass::Layer { layer, replace } => {
                let chunks = &mut self.layers[layer as usize];
                let freed = if replace { chunks.iter().map(Vec::len).sum() } else { 0 };
                let total = self.layer_bytes - freed + message.len();
                if total > Self::MAX_LAYER_BYTES {
                    return Err(RecordError::TooLarge);
                }
                if replace {
                    chunks.clear();
                }
                chunks.push(message.to_vec());
                self.layer_bytes = total;
            }
            MessageClass::Stats => self.stats = Some(message.to_vec()),
            MessageClass::Preview(camera) => {
                self.previews.insert(camera, message.to_vec());
            }
        }
        Ok(class)
    }

    /// True once a `State` message has been applied: only then is there a
    /// snapshot.
    pub fn has_state(&self) -> bool {
        self.state.is_some()
    }

    /// The latest `State` message.
    pub fn state_message(&self) -> Option<&[u8]> {
        self.state.as_deref()
    }

    /// The messages that bring an empty viewer to this state, in order:
    /// `State`, layer 0 `Replace`, layer 1 `Replace`, then the latest
    /// `Stats` and the latest `Preview` of every camera, if any. `None`
    /// before the first `State`.
    ///
    /// A layer built from several messages is merged into one (see the
    /// module documentation for what that does to positions).
    pub fn snapshot(&self) -> Option<Vec<Vec<u8>>> {
        let mut out = vec![self.state.clone()?];
        for (layer, chunks) in self.layers.iter().enumerate() {
            out.push(merge_layer(layer as u8, chunks));
        }
        out.extend(self.stats.clone());
        out.extend(self.previews.values().cloned());
        Some(out)
    }

    /// Size of [`StreamState::snapshot`] in bytes, without building it.
    pub fn snapshot_bytes(&self) -> usize {
        let fixed = self.state.as_ref().map_or(0, Vec::len) + self.stats.as_ref().map_or(0, Vec::len);
        fixed + self.layer_bytes + self.previews.values().map(Vec::len).sum::<usize>()
    }

    /// Forgets the latest `Stats` and `Preview` messages (a recorder that
    /// does not record them must not put them into keyframes either).
    pub(crate) fn forget(&mut self, stats: bool, previews: bool) {
        if stats {
            self.stats = None;
        }
        if previews {
            self.previews.clear();
        }
    }
}

/// The `scene` of a `State` message, or nothing.
pub(crate) fn scene_of(state: &[u8]) -> String {
    #[derive(serde::Deserialize)]
    struct Scene {
        #[serde(default)]
        scene: String,
    }
    state
        .get(HEADER_SIZE..)
        .and_then(|json| serde_json::from_slice::<Scene>(json).ok())
        .map(|s| s.scene.chars().take(64).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode, encode, encode_layer, Body, LayerMode, Message, PreviewMsg};
    use crate::Splat;

    fn splats(n: usize, x: f32) -> Vec<Splat> {
        (0..n)
            .map(|i| Splat {
                pos: [x + i as f32 * 0.1, 0.0, 1.0],
                scale: [0.01; 3],
                rot: [0.0, 0.0, 0.0, 1.0],
                rgba: [i as u8; 4],
            })
            .collect()
    }

    fn layer(id: u8, mode: LayerMode, splats: &[Splat]) -> Vec<u8> {
        let mut out = Vec::new();
        encode_layer(1, 2, id, mode, splats, &mut out);
        out
    }

    fn text(state: bool, json: &str) -> Vec<u8> {
        let body = if state { Body::State(json.into()) } else { Body::Stats(json.into()) };
        encode(&Message { seq: 0, time_ms: 0, body })
    }

    fn preview(camera: u8, shade: u8) -> Vec<u8> {
        let body = Body::Preview(PreviewMsg {
            camera_id: camera,
            width: 2,
            height: 1,
            rgb: vec![shade; 6],
            depth_mm: vec![1; 2],
        });
        encode(&Message { seq: 0, time_ms: 0, body })
    }

    fn count(message: &[u8]) -> usize {
        match decode(message).unwrap().body {
            Body::Layer(l) => l.splats.len(),
            _ => panic!(),
        }
    }

    #[test]
    fn there_is_no_snapshot_before_the_state() {
        let mut state = StreamState::new();
        assert!(state.snapshot().is_none() && !state.has_state());
        state.apply(&layer(0, LayerMode::Replace, &splats(3, 0.0))).unwrap();
        state.apply(&text(false, "{}")).unwrap();
        assert!(state.snapshot().is_none());
        state.apply(&text(true, r#"{"scene":"stage"}"#)).unwrap();
        // The layer that came before the State is part of the snapshot;
        // the statistics of whatever session came before are not.
        let snapshot = state.snapshot().unwrap();
        assert_eq!(snapshot.len(), 3);
        assert_eq!((count(&snapshot[1]), count(&snapshot[2])), (3, 0));
        assert_eq!(state.snapshot_bytes(), snapshot[0].len() + snapshot[1].len());
    }

    #[test]
    fn the_snapshot_follows_the_stream() {
        let mut state = StreamState::new();
        let session = text(true, r#"{"scene":"house"}"#);
        state.apply(&session).unwrap();
        state.apply(&layer(0, LayerMode::Replace, &splats(10, 0.0))).unwrap();
        state.apply(&layer(0, LayerMode::Append, &splats(5, 3.0))).unwrap();
        state.apply(&layer(1, LayerMode::Replace, &splats(7, 0.0))).unwrap();
        state.apply(&layer(1, LayerMode::Replace, &splats(2, 0.0))).unwrap();
        state.apply(&text(false, r#"{"tick":1}"#)).unwrap();
        state.apply(&text(false, r#"{"tick":2}"#)).unwrap();
        state.apply(&preview(3, 10)).unwrap();
        state.apply(&preview(1, 20)).unwrap();
        state.apply(&preview(3, 30)).unwrap();

        let snapshot = state.snapshot().unwrap();
        assert_eq!(snapshot.len(), 6);
        assert_eq!(snapshot[0], session);
        assert_eq!((count(&snapshot[1]), count(&snapshot[2])), (15, 2));
        assert_eq!(snapshot[3], text(false, r#"{"tick":2}"#));
        // Previews in camera order, the latest of each.
        assert_eq!((&snapshot[4], &snapshot[5]), (&preview(1, 20), &preview(3, 30)));
        assert_eq!(state.snapshot_bytes(), snapshot.iter().map(Vec::len).sum::<usize>() + 48);

        // A State of the same scene keeps statistics and thumbnails; one
        // of another scene drops them. A Replace drops the chunks before it.
        state.apply(&text(true, r#"{"scene":"house","shadows":false}"#)).unwrap();
        assert_eq!(state.snapshot().unwrap().len(), 6);
        state.apply(&text(true, r#"{"scene":"stage"}"#)).unwrap();
        state.apply(&layer(0, LayerMode::Replace, &splats(4, 0.0))).unwrap();
        let snapshot = state.snapshot().unwrap();
        assert_eq!(snapshot.len(), 3);
        assert_eq!(snapshot[1], layer(0, LayerMode::Replace, &splats(4, 0.0)));

        let rebuilt = StreamState::from_messages(snapshot.iter().map(Vec::as_slice));
        assert_eq!(rebuilt.snapshot().unwrap(), snapshot);
    }

    #[test]
    fn refused_messages_leave_the_state_alone() {
        let mut state = StreamState::from_messages([
            text(true, "{}").as_slice(),
            layer(0, LayerMode::Replace, &splats(3, 0.0)).as_slice(),
        ]);
        let before = state.snapshot().unwrap();
        assert_eq!(state.apply(b"junk"), Err(RecordError::BadMessage));
        let mut bad = layer(0, LayerMode::Append, &splats(3, 0.0));
        bad.pop();
        assert_eq!(state.apply(&bad), Err(RecordError::BadMessage));
        assert_eq!(state.snapshot().unwrap(), before);
    }
}
