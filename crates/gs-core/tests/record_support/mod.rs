//! Shared by the recording tests: a model of a viewer, a small synthetic
//! session with everything in it, and the loops that feed a player.

#![allow(dead_code)]

use std::collections::BTreeMap;

use gs_core::codec::{self, encode, encode_layer, Body, LayerMode, Message, PreviewMsg};
use gs_core::record::{
    classify, ByteRange, KeyframePolicy, MessageClass, Player, Pushed, Recorder, RecorderOptions, Update,
    HEADER_JSON_SIZE,
};
use gs_core::Splat;

pub type Stream = Vec<(f64, Vec<u8>)>;

/// What a viewer holds: the session state, both layers, the latest
/// statistics and the latest thumbnail of every camera.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Viewer {
    pub state: Option<String>,
    pub layers: [Vec<Splat>; 2],
    pub stats: Option<String>,
    pub previews: BTreeMap<u8, PreviewMsg>,
    /// Layer 0 messages applied, to tell a replay that re-sends it from
    /// one that does not.
    pub static_messages: usize,
    pub state_messages: usize,
}

impl Viewer {
    pub fn apply(&mut self, bytes: &[u8]) {
        match codec::decode(bytes).expect("a viewer only ever gets valid messages").body {
            Body::State(json) => {
                // Another scene has other cameras: what is on screen of
                // the old one goes, as in the app.
                if self.state.as_deref().map(scene_of) != Some(scene_of(&json)) {
                    self.stats = None;
                    self.previews.clear();
                }
                self.state = Some(json);
                self.state_messages += 1;
            }
            Body::Stats(json) => self.stats = Some(json),
            Body::Preview(preview) => {
                self.previews.insert(preview.camera_id, preview);
            }
            Body::Layer(layer) => {
                let target = &mut self.layers[layer.layer_id as usize];
                if layer.mode == LayerMode::Replace {
                    target.clear();
                }
                target.extend_from_slice(&layer.splats);
                self.static_messages += usize::from(layer.layer_id == 0);
            }
        }
    }

    pub fn apply_all(&mut self, update: &Update) {
        if update.reset {
            // The play head jumped: the statistics and thumbnails on screen
            // belong to another moment. The update brings the right ones.
            self.stats = None;
            self.previews.clear();
        }
        for message in &update.messages {
            self.apply(message);
        }
    }

    /// The live viewer at time `t`: every message sent up to then.
    pub fn live(stream: &Stream, t: f64) -> Self {
        Self::live_after(stream, stream.partition_point(|(time, _)| *time <= t))
    }

    /// The live viewer after the first `upto` messages.
    pub fn live_after(stream: &Stream, upto: usize) -> Self {
        let mut viewer = Self::default();
        // Only what survives matters; skip the decoding of live layers
        // that a later one replaces.
        let last_live = stream[..upto]
            .iter()
            .rposition(|(_, m)| classify(m) == Some(MessageClass::Layer { layer: 1, replace: true }));
        for (i, (_, message)) in stream[..upto].iter().enumerate() {
            let class = classify(message).unwrap();
            if matches!(class, MessageClass::Layer { layer: 1, replace: true }) && Some(i) != last_live {
                continue;
            }
            viewer.apply(message);
        }
        viewer
    }

    /// Same picture: everything equal, except that positions of layer 0 may
    /// differ by `tolerance` (the requantization of a merged keyframe).
    pub fn assert_shows(&self, live: &Viewer, tolerance: f32, context: &str) {
        assert_eq!(self.state, live.state, "{context}: state");
        assert_eq!(self.stats, live.stats, "{context}: stats");
        // No `assert_eq`: a mismatch would print megabytes.
        assert!(
            self.previews == live.previews,
            "{context}: previews of {:?} against {:?}",
            self.previews.keys(),
            live.previews.keys()
        );
        assert!(
            self.layers[1] == live.layers[1],
            "{context}: live layer, {} splats against {}",
            self.layers[1].len(),
            live.layers[1].len()
        );
        assert_eq!(self.layers[0].len(), live.layers[0].len(), "{context}: static layer size");
        if tolerance == 0.0 {
            assert!(self.layers[0] == live.layers[0], "{context}: static layer differs");
            return;
        }
        for (i, (a, b)) in self.layers[0].iter().zip(&live.layers[0]).enumerate() {
            assert_eq!((a.scale, a.rot, a.rgba), (b.scale, b.rot, b.rgba), "{context}: splat {i}");
            let apart = (0..3).map(|k| (a.pos[k] - b.pos[k]).abs()).fold(0.0f32, f32::max);
            assert!(apart <= tolerance, "{context}: splat {i} moved by {apart}");
        }
    }
}

pub fn scene_of(state_json: &str) -> String {
    let value: serde_json::Value = serde_json::from_str(state_json).unwrap_or_default();
    value.get("scene").and_then(|scene| scene.as_str()).unwrap_or_default().to_owned()
}

pub fn record(stream: &Stream, options: RecorderOptions) -> Vec<u8> {
    let lean = !(options.stats && options.previews);
    let mut recorder = Recorder::new(Vec::new(), options);
    for (time, message) in stream {
        let pushed = recorder.push(*time, message).unwrap();
        assert!(
            matches!(pushed, Pushed::Recorded | Pushed::Keyframe) || (lean && pushed == Pushed::Skipped),
            "{pushed:?}"
        );
    }
    let (file, summary) = recorder.finish().unwrap();
    assert_eq!(summary.bytes, file.len() as u64);
    file
}

pub fn options(name: &str) -> RecorderOptions {
    RecorderOptions { created: "2026-11-14T10:00:00Z".into(), ..RecorderOptions::named(name) }
}

/// A deterministic sequence in `[0, 1)`.
pub struct Lcg(pub u64);

impl Lcg {
    pub fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

pub fn splats(n: usize, seed: u32) -> Vec<Splat> {
    (0..n)
        .map(|i| {
            let f = (i as u32 * 31 + seed * 17) as f32;
            Splat {
                pos: [(f * 0.37).sin() * 3.0, (f * 0.11).cos() + 1.0, (f * 0.23).sin() * 2.0 + seed as f32 * 0.1],
                scale: [0.01 + (i % 5) as f32 * 0.002, 0.02, 0.001],
                rot: gs_core::glam::Quat::from_rotation_y(f * 0.1).to_array(),
                rgba: [(i * 7) as u8, seed as u8, 200, 216],
            }
        })
        .collect()
}

pub fn layer(seq: u32, id: u8, mode: LayerMode, splats: &[Splat]) -> Vec<u8> {
    let mut out = Vec::new();
    encode_layer(seq, seq * 10, id, mode, splats, &mut out);
    out
}

pub fn text(seq: u32, state: bool, json: String) -> Vec<u8> {
    encode(&Message { seq, time_ms: seq * 10, body: if state { Body::State(json) } else { Body::Stats(json) } })
}

pub fn preview(seq: u32, camera: u8) -> Vec<u8> {
    let body = Body::Preview(PreviewMsg {
        camera_id: camera,
        width: 4,
        height: 3,
        rgb: (0..36).map(|i| (i * seq) as u8).collect(),
        depth_mm: (0..12).map(|i| (i * seq) as u16).collect(),
    });
    encode(&Message { seq, time_ms: seq * 10, body })
}

/// A small session with everything in it: both layers, appends, a replace
/// of the static layer, thumbnails of two cameras, a second State.
pub fn tiny_stream() -> Stream {
    let mut stream: Stream = vec![
        (0.0, text(0, true, r#"{"scene":"tiny"}"#.into())),
        (0.0, layer(1, 0, LayerMode::Replace, &splats(40, 1))),
        (0.0, layer(2, 1, LayerMode::Replace, &splats(9, 2))),
        (0.0, text(3, false, r#"{"tick":0}"#.into())),
    ];
    for tick in 1..=24u32 {
        let time = tick as f64 * 0.25;
        // The sequence number of a message is its place in the stream.
        let mut push = |make: &dyn Fn(u32) -> Vec<u8>| {
            let seq = stream.len() as u32;
            stream.push((time, make(seq)));
        };
        if tick == 14 {
            push(&|seq| text(seq, true, r#"{"scene":"tiny too","shadows":false}"#.into()));
            push(&|seq| layer(seq, 0, LayerMode::Replace, &splats(25, tick)));
        }
        if tick % 3 == 0 {
            push(&|seq| layer(seq, 0, LayerMode::Append, &splats(6, tick)));
        }
        push(&|seq| layer(seq, 1, LayerMode::Replace, &splats(5 + tick as usize % 4, tick)));
        push(&|seq| preview(seq, (tick % 2) as u8));
        push(&|seq| text(seq, false, format!(r#"{{"tick":{tick},"padding":"{}"}}"#, "x".repeat(tick as usize * 5))));
    }
    stream
}

pub fn tiny_options() -> RecorderOptions {
    RecorderOptions { keyframes: KeyframePolicy { min_interval_s: 1.0, ratio: 1.0 }, ..options("tiny") }
}

/// Positions of a merged keyframe of the tiny session move by less than
/// this.
pub const TINY_TOLERANCE: f32 = 1.0e-4;

/// Feeds a ranged player from `file` until it wants nothing more.
pub fn pump(player: &mut Player, file: &[u8], requests: &mut Vec<ByteRange>) {
    while let Some(range) = player.wanted() {
        assert!(range.start < range.end && range.end <= file.len() as u64, "{range:?} of {}", file.len());
        requests.push(range);
        assert!(requests.len() < 200_000, "the player keeps asking");
        match player.supply(range.start, &file[range.start as usize..range.end as usize]) {
            Ok(used) => assert!(used, "{range:?} was asked for and then refused"),
            // The file header is bad: the player says so and wants no more.
            Err(error) => assert_eq!(player.error(), Some(error)),
        }
    }
}

/// Brings a ranged player to its play head, fetching what it asks for.
pub fn settle(player: &mut Player, file: &[u8], viewer: &mut Viewer, requests: &mut Vec<ByteRange>) -> Update {
    for _ in 0..100_000 {
        pump(player, file, requests);
        let update = player.update();
        viewer.apply_all(&update);
        if !update.buffering {
            return update;
        }
    }
    panic!("the player never stops buffering");
}

/// End offset, time and kind of every record of a recording made by this
/// crate's writer (walks the framing directly: length, kind, time).
pub fn records_of(file: &[u8]) -> Vec<(u64, f64, bool)> {
    let mut out = Vec::new();
    let mut pos = 12 + HEADER_JSON_SIZE;
    while pos + 16 <= file.len() {
        let len = u32::from_le_bytes(file[pos..pos + 4].try_into().unwrap()) as usize;
        let kind = file[pos + 4];
        let time = f64::from_le_bytes(file[pos + 8..pos + 16].try_into().unwrap());
        if kind == 2 || pos + 4 + len > file.len() {
            break;
        }
        pos += 4 + len;
        out.push((pos as u64, time, kind == 1));
    }
    out
}

/// The stream a recording was made from, read back from the file without
/// the code under test: the messages of the first keyframe at time 0, then
/// every message record. Only for files with uncompressed payloads.
pub fn stream_of(file: &[u8]) -> Stream {
    let u32_at = |at: usize| u32::from_le_bytes(file[at..at + 4].try_into().unwrap()) as usize;
    let mut stream = Stream::new();
    let mut pos = 12 + u32_at(8);
    while pos + 16 <= file.len() {
        let len = u32_at(pos);
        let (kind, flags) = (file[pos + 4], file[pos + 5]);
        let time = f64::from_le_bytes(file[pos + 8..pos + 16].try_into().unwrap());
        assert_eq!(flags, 0, "stream_of reads uncompressed recordings only");
        let payload = &file[pos + 16..pos + 4 + len];
        match kind {
            0 => stream.push((time, payload.to_vec())),
            1 if stream.is_empty() => {
                let mut at = 0;
                while at < payload.len() {
                    let len = u32::from_le_bytes(payload[at..at + 4].try_into().unwrap()) as usize;
                    stream.push((time, payload[at + 4..at + 4 + len].to_vec()));
                    at += 4 + len;
                }
            }
            1 => {}
            _ => break,
        }
        pos += 4 + len;
    }
    stream
}
