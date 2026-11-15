//! What a recorder and a player need to know about a wire message without
//! decoding it: its delivery class, whether a viewer would accept it, and
//! how to merge the chunks of a layer into one `Replace`.

use crate::codec::{self, Kind, LayerMode, HEADER_SIZE, LAYER_HEADER_SIZE, PREVIEW_HEADER_SIZE, SPLAT_WIRE_SIZE};

/// Bytes of a message that [`peek_class`] looks at.
pub(crate) const CLASS_PREFIX: usize = HEADER_SIZE + 2;
/// Wire ids of the two layers a viewer has.
pub(crate) const LAYER_COUNT: usize = 2;

/// What a wire message does to a viewer, as far as the delivery contract
/// of `docs/ARCHITECTURE.md` is concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MessageClass {
    /// The session state.
    State,
    /// Splats of layer 0 (static) or 1 (live), replacing the layer or
    /// appended to it.
    Layer { layer: u8, replace: bool },
    /// Statistics of a tick.
    Stats,
    /// The thumbnail of one camera.
    Preview(u8),
}

/// The class of a message from its first [`CLASS_PREFIX`] bytes. Nothing
/// past them is looked at.
pub(crate) fn peek_class(prefix: &[u8]) -> Option<MessageClass> {
    match codec::peek_kind(prefix).ok()? {
        Kind::State => Some(MessageClass::State),
        Kind::Stats => Some(MessageClass::Stats),
        Kind::Preview => Some(MessageClass::Preview(*prefix.get(HEADER_SIZE)?)),
        Kind::Layer => {
            let layer = *prefix.get(HEADER_SIZE)?;
            let replace = match *prefix.get(HEADER_SIZE + 1)? {
                mode if mode == LayerMode::Replace as u8 => true,
                mode if mode == LayerMode::Append as u8 => false,
                _ => return None,
            };
            ((layer as usize) < LAYER_COUNT).then_some(MessageClass::Layer { layer, replace })
        }
    }
}

/// The class of a whole message, or `None` for one a viewer would refuse:
/// a bad header, a layer or a preview whose size does not match what it
/// announces, a layer id other than 0 or 1, a bounding box that is not
/// finite, text that is not UTF-8. Constant time apart from the UTF-8
/// check of the (small) JSON messages.
pub fn classify(message: &[u8]) -> Option<MessageClass> {
    let class = peek_class(message)?;
    let valid = match class {
        MessageClass::State | MessageClass::Stats => std::str::from_utf8(&message[HEADER_SIZE..]).is_ok(),
        MessageClass::Layer { .. } => LayerView::parse(message).is_some(),
        MessageClass::Preview(_) => {
            let fixed = HEADER_SIZE + PREVIEW_HEADER_SIZE;
            message.len() >= fixed && {
                let width = u16::from_le_bytes([message[HEADER_SIZE + 2], message[HEADER_SIZE + 3]]) as u64;
                let height = u16::from_le_bytes([message[HEADER_SIZE + 4], message[HEADER_SIZE + 5]]) as u64;
                message.len() as u64 == fixed as u64 + width * height * 5
            }
        }
    };
    valid.then_some(class)
}

/// A validated layer message: its bounding box and its quantized splats.
pub(crate) struct LayerView<'a> {
    pub min: [f32; 3],
    pub max: [f32; 3],
    /// `count × 16` bytes.
    pub splats: &'a [u8],
}

impl<'a> LayerView<'a> {
    /// The same checks as the decoder of [`crate::codec`], without
    /// touching the splats.
    pub fn parse(message: &'a [u8]) -> Option<Self> {
        let fixed = HEADER_SIZE + LAYER_HEADER_SIZE;
        if message.len() < fixed || codec::peek_kind(message) != Ok(Kind::Layer) {
            return None;
        }
        let body = &message[HEADER_SIZE..];
        if body[0] as usize >= LAYER_COUNT || body[1] > LayerMode::Append as u8 {
            return None;
        }
        let count = super::u32_at(body, 4) as u64;
        if message.len() as u64 != fixed as u64 + count * SPLAT_WIRE_SIZE as u64 {
            return None;
        }
        let f32_at = |offset: usize| f32::from_bits(super::u32_at(body, offset));
        let min = [f32_at(8), f32_at(12), f32_at(16)];
        let max = [f32_at(20), f32_at(24), f32_at(28)];
        let ordered = (0..3).all(|k| min[k].is_finite() && max[k].is_finite() && min[k] <= max[k]);
        ordered.then_some(Self { min, max, splats: &message[fixed..] })
    }

    pub fn count(&self) -> usize {
        self.splats.len() / SPLAT_WIRE_SIZE
    }
}

/// An empty `Replace` of `layer`.
pub(crate) fn empty_layer(layer: u8) -> Vec<u8> {
    let mut out = Vec::new();
    codec::encode_layer(0, 0, layer, LayerMode::Replace, &[], &mut out);
    out
}

/// One `Replace` message holding the splats of `chunks`, in order: the
/// messages (a `Replace` or an `Append` each, already validated) that built
/// a layer. The header (`seq`, `time_ms`) is that of the last chunk.
///
/// A single chunk is copied as it is. Several chunks quantize positions
/// inside different boxes, so their positions are requantized into the
/// union of the boxes: a position moves by at most half a step of the
/// union box. Scale, colour, opacity and rotation are copied bit for bit.
pub(crate) fn merge_layer(layer: u8, chunks: &[Vec<u8>]) -> Vec<u8> {
    let views: Vec<LayerView> = chunks.iter().filter_map(|c| LayerView::parse(c)).collect();
    let Some(last) = chunks.last().filter(|_| !views.is_empty()) else { return empty_layer(layer) };
    let mode_at = HEADER_SIZE + 1;
    if let [only] = chunks {
        let mut out = only.clone();
        out[mode_at] = LayerMode::Replace as u8;
        return out;
    }

    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    let mut count = 0u64;
    for view in views.iter().filter(|v| v.count() > 0) {
        count += view.count() as u64;
        for k in 0..3 {
            min[k] = min[k].min(view.min[k]);
            max[k] = max[k].max(view.max[k]);
        }
    }
    // The count field is 32 bits wide; a recorder refuses a state anywhere
    // near that size long before (`StreamState::MAX_LAYER_BYTES`).
    let count = count.min(u32::MAX as u64) as u32;
    if count == 0 {
        (min, max) = ([0.0; 3], [0.0; 3]);
    }

    let mut out = Vec::with_capacity(HEADER_SIZE + LAYER_HEADER_SIZE + count as usize * SPLAT_WIRE_SIZE);
    out.extend_from_slice(&last[..HEADER_SIZE]);
    out.extend_from_slice(&[layer, LayerMode::Replace as u8, 0, 0]);
    out.extend_from_slice(&count.to_le_bytes());
    for v in min.iter().chain(&max) {
        out.extend_from_slice(&v.to_le_bytes());
    }

    let mut to_unorm = [0.0f32; 3];
    for k in 0..3 {
        let extent = max[k] - min[k];
        if extent > 0.0 && extent.is_finite() {
            to_unorm[k] = 65535.0 / extent;
        }
    }
    let mut left = count as usize;
    for view in &views {
        let take = view.count().min(left);
        left -= take;
        let splats = &view.splats[..take * SPLAT_WIRE_SIZE];
        if view.min == min && view.max == max {
            out.extend_from_slice(splats);
            continue;
        }
        for wire in splats.chunks_exact(SPLAT_WIRE_SIZE) {
            let mut merged = [0u8; SPLAT_WIRE_SIZE];
            merged.copy_from_slice(wire);
            for k in 0..3 {
                // Decode as `codec::decode_layer_into` does, encode as
                // `codec::encode_layer` does.
                let t = u16::from_le_bytes([wire[2 * k], wire[2 * k + 1]]) as f32 / 65535.0;
                let p = view.min[k] * (1.0 - t) + view.max[k] * t;
                let u = (((p - min[k]) * to_unorm[k] + 0.5) as u32).min(65535) as u16;
                merged[2 * k..2 * k + 2].copy_from_slice(&u.to_le_bytes());
            }
            out.extend_from_slice(&merged);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{decode, encode, encode_layer, Body, Message, PreviewMsg};
    use crate::Splat;

    fn splats(n: usize, origin: [f32; 3]) -> Vec<Splat> {
        (0..n)
            .map(|i| {
                let f = i as f32;
                Splat {
                    pos: [origin[0] + f * 0.013, origin[1] + (f * 0.7).sin(), origin[2] - f * 0.004],
                    scale: [0.01 + f * 1e-4, 0.02, 0.001],
                    rot: glam::Quat::from_rotation_y(f * 0.1).to_array(),
                    rgba: [i as u8, 200, (i * 3) as u8, 216],
                }
            })
            .collect()
    }

    fn layer(layer: u8, mode: LayerMode, splats: &[Splat]) -> Vec<u8> {
        let mut out = Vec::new();
        encode_layer(7, 1234, layer, mode, splats, &mut out);
        out
    }

    fn decoded(message: &[u8]) -> Vec<Splat> {
        match decode(message).unwrap().body {
            Body::Layer(layer) => layer.splats,
            _ => panic!("not a layer"),
        }
    }

    #[test]
    fn classes_follow_the_wire() {
        let state = encode(&Message { seq: 0, time_ms: 0, body: Body::State("{}".into()) });
        let stats = encode(&Message { seq: 0, time_ms: 0, body: Body::Stats("{}".into()) });
        let preview = encode(&Message {
            seq: 0,
            time_ms: 0,
            body: Body::Preview(PreviewMsg {
                camera_id: 4,
                width: 3,
                height: 2,
                rgb: vec![9; 18],
                depth_mm: vec![5; 6],
            }),
        });
        assert_eq!(classify(&state), Some(MessageClass::State));
        assert_eq!(classify(&stats), Some(MessageClass::Stats));
        assert_eq!(classify(&preview), Some(MessageClass::Preview(4)));
        let s = splats(5, [0.0; 3]);
        assert_eq!(classify(&layer(0, LayerMode::Replace, &s)), Some(MessageClass::Layer { layer: 0, replace: true }));
        assert_eq!(classify(&layer(0, LayerMode::Append, &s)), Some(MessageClass::Layer { layer: 0, replace: false }));
        assert_eq!(classify(&layer(1, LayerMode::Replace, &[])), Some(MessageClass::Layer { layer: 1, replace: true }));
        // The prefix is enough to tell the class.
        for message in [&state, &stats, &preview, &layer(1, LayerMode::Append, &s)] {
            assert_eq!(peek_class(&message[..CLASS_PREFIX.min(message.len())]), classify(message));
        }
        assert_eq!(peek_class(&layer(1, LayerMode::Replace, &s)[..CLASS_PREFIX - 1]), None);
    }

    #[test]
    fn what_a_viewer_refuses_has_no_class() {
        let good = layer(0, LayerMode::Replace, &splats(4, [0.0; 3]));
        assert!(classify(&good).is_some());
        let patched = |offset: usize, bytes: &[u8]| {
            let mut m = good.clone();
            m[offset..offset + bytes.len()].copy_from_slice(bytes);
            m
        };
        assert_eq!(classify(&patched(0, b"GSL2")), None);
        assert_eq!(classify(&patched(4, &[9])), None);
        assert_eq!(classify(&patched(16, &[2])), None, "layer id");
        assert_eq!(classify(&patched(17, &[2])), None, "layer mode");
        assert_eq!(classify(&patched(20, &5u32.to_le_bytes())), None, "count");
        assert_eq!(classify(&patched(24, &f32::NAN.to_le_bytes())), None, "box");
        assert_eq!(classify(&patched(24, &1.0e9f32.to_le_bytes())), None, "min above max");
        assert_eq!(classify(&good[..good.len() - 1]), None);
        assert_eq!(classify(&good[..40]), None);
        let mut long = good.clone();
        long.push(0);
        assert_eq!(classify(&long), None);
        // A count that would wrap a 32-bit size.
        assert_eq!(classify(&patched(20, &u32::MAX.to_le_bytes())), None);

        let mut text = encode(&Message { seq: 0, time_ms: 0, body: Body::Stats("{}".into()) });
        text.push(0xff);
        assert_eq!(classify(&text), None);
        let preview = encode(&Message {
            seq: 0,
            time_ms: 0,
            body: Body::Preview(PreviewMsg {
                camera_id: 0,
                width: 2,
                height: 2,
                rgb: vec![0; 12],
                depth_mm: vec![0; 4],
            }),
        });
        assert!(classify(&preview).is_some());
        assert_eq!(classify(&preview[..preview.len() - 1]), None);
        assert_eq!(classify(&preview[..20]), None);
        for junk in [&b""[..], b"GSL1", b"junk junk junk junk junk"] {
            assert_eq!(classify(junk), None);
        }
    }

    #[test]
    fn a_single_chunk_is_kept_verbatim() {
        let s = splats(40, [1.0, 0.0, -2.0]);
        let replace = layer(0, LayerMode::Replace, &s);
        assert_eq!(merge_layer(0, std::slice::from_ref(&replace)), replace);
        // An append onto nothing is a replace.
        let append = layer(0, LayerMode::Append, &s);
        assert_eq!(merge_layer(0, &[append]), replace);
        assert_eq!(merge_layer(1, &[]), empty_layer(1));
        assert_eq!(classify(&empty_layer(1)), Some(MessageClass::Layer { layer: 1, replace: true }));
        assert!(decoded(&empty_layer(0)).is_empty());
    }

    #[test]
    fn merged_chunks_move_positions_by_less_than_a_step_and_nothing_else() {
        let parts = [splats(300, [0.0; 3]), splats(1, [5.0, 2.0, 1.0]), vec![], splats(200, [-7.0, 0.5, 9.0])];
        let chunks: Vec<Vec<u8>> = parts
            .iter()
            .enumerate()
            .map(|(i, s)| layer(0, if i == 0 { LayerMode::Replace } else { LayerMode::Append }, s))
            .collect();
        let merged = merge_layer(0, &chunks);
        assert_eq!(classify(&merged), Some(MessageClass::Layer { layer: 0, replace: true }));
        // Header of the last chunk.
        assert_eq!(merged[..HEADER_SIZE], chunks[3][..HEADER_SIZE]);

        let live: Vec<Splat> = chunks.iter().flat_map(|c| decoded(c)).collect();
        let got = decoded(&merged);
        assert_eq!(got.len(), live.len());
        let view = LayerView::parse(&merged).unwrap();
        for k in 0..3 {
            let step = (view.max[k] - view.min[k]) / 65535.0;
            let worst = got.iter().zip(&live).map(|(a, b)| (a.pos[k] - b.pos[k]).abs()).fold(0.0f32, f32::max);
            assert!(worst <= step * 0.51 + 1e-6, "axis {k}: {worst} against a step of {step}");
            assert!(step < 3.0e-4);
        }
        for (a, b) in got.iter().zip(&live) {
            assert_eq!((a.scale, a.rot, a.rgba), (b.scale, b.rot, b.rgba));
        }
        // Merging what is already merged changes nothing.
        assert_eq!(merge_layer(0, &[merged.clone(), layer(0, LayerMode::Append, &[])]), merged);
        // Chunks that are all empty give an empty layer.
        let empties = [layer(1, LayerMode::Replace, &[]), layer(1, LayerMode::Append, &[])];
        assert!(decoded(&merge_layer(1, &empties)).is_empty());
    }
}
