//! Independent checks of `gs_core::ingest`, written against the
//! specification (`docs/ARCHITECTURE.md`, "Real capture") and not against
//! the encoder of the crate or its simulated phone:
//!
//! * frames assembled byte by byte at the documented offsets, with deflate
//!   streams written by hand;
//! * a scene whose world positions are computed on paper, asymmetric so that
//!   a mirrored image, a transposed pose or swapped axes cannot pass;
//! * the parsers under hostile input (truncations, bit flips, extreme
//!   lengths, deflate bombs, JPEGs of every flavour);
//! * session rules a real phone exercises: a viewer that joins between a
//!   change and its broadcast, a region that arrives late, an application
//!   that was restarted, frame indices with gaps.

mod ingest_sim;

use gs_core::codec::{self, Body, LayerMode};
use gs_core::glam::{Quat, Vec3};
use gs_core::ingest::file::{split_gscap, FILE_MAGIC};
use gs_core::ingest::frame::{decode_frame, encode_frame, CaptureFrame, ErrorCode, FrameHeader, Orientation, Tracking};
use gs_core::ingest::image::{decode_jpeg, ImageError};
use gs_core::ingest::message::{ClientMessage, ServerMessage};
use gs_core::ingest::prepare::SkipReason;
use gs_core::ingest::{
    parse_pairing_uri, CaptureRegion, CaptureSession, GscapHeader, GscapImporter, GscapParser, GscapWriter, Hello,
    IngestParams, ParsedItem,
};
use gs_core::pipeline::Outgoing;
use gs_core::protocol::{CaptureMode, SessionState, Stats};
use gs_core::scene::{make_scene, SceneKind};
use gs_core::Splat;
use ingest_sim::{Phone, PhoneSim, Rng, Shot};
use std::time::Instant;

// ---------------------------------------------------------------------------
// Bytes written by hand
// ---------------------------------------------------------------------------

/// A raw deflate stream (RFC 1951) made of stored blocks only: nothing of
/// the crate's compressor is involved.
fn stored_deflate(data: &[u8]) -> Vec<u8> {
    if data.is_empty() {
        return vec![1, 0, 0, 0xFF, 0xFF];
    }
    let mut out = Vec::new();
    let blocks: Vec<&[u8]> = data.chunks(65535).collect();
    for (i, block) in blocks.iter().enumerate() {
        out.push(u8::from(i + 1 == blocks.len()));
        let length = block.len() as u16;
        out.extend_from_slice(&length.to_le_bytes());
        out.extend_from_slice(&(!length).to_le_bytes());
        out.extend_from_slice(block);
    }
    out
}

/// The fields of a frame as the table of the specification lists them.
#[derive(Clone)]
struct Raw {
    flags: u8,
    tracking: u8,
    orientation: u8,
    index: u32,
    reserved: u32,
    timestamp: f64,
    pose: [f32; 16],
    intrinsics: [f32; 4],
    color_size: (u16, u16),
    color: Vec<u8>,
    depth_size: (u16, u16),
    /// The planes as stored (deflated or not).
    depth: Vec<u8>,
    confidence: Vec<u8>,
    mask: Vec<u8>,
}

impl Raw {
    /// The frame, every field put at its documented offset.
    fn bytes(&self) -> Vec<u8> {
        let mut b = vec![0u8; 128];
        b[0..4].copy_from_slice(b"GSCF");
        b[4] = 1;
        b[5] = self.flags;
        b[6] = self.tracking;
        b[7] = self.orientation;
        b[8..12].copy_from_slice(&self.index.to_le_bytes());
        b[12..16].copy_from_slice(&self.reserved.to_le_bytes());
        b[16..24].copy_from_slice(&self.timestamp.to_le_bytes());
        for (i, v) in self.pose.iter().enumerate() {
            b[24 + 4 * i..28 + 4 * i].copy_from_slice(&v.to_le_bytes());
        }
        for (i, v) in self.intrinsics.iter().enumerate() {
            b[88 + 4 * i..92 + 4 * i].copy_from_slice(&v.to_le_bytes());
        }
        b[104..106].copy_from_slice(&self.color_size.0.to_le_bytes());
        b[106..108].copy_from_slice(&self.color_size.1.to_le_bytes());
        b[108..112].copy_from_slice(&(self.color.len() as u32).to_le_bytes());
        b[112..114].copy_from_slice(&self.depth_size.0.to_le_bytes());
        b[114..116].copy_from_slice(&self.depth_size.1.to_le_bytes());
        b[116..120].copy_from_slice(&(self.depth.len() as u32).to_le_bytes());
        b[120..124].copy_from_slice(&(self.confidence.len() as u32).to_le_bytes());
        b[124..128].copy_from_slice(&(self.mask.len() as u32).to_le_bytes());
        b.extend_from_slice(&self.color);
        b.extend_from_slice(&self.depth);
        b.extend_from_slice(&self.confidence);
        b.extend_from_slice(&self.mask);
        b
    }
}

fn depth_plane(mm: &[u16]) -> Vec<u8> {
    mm.iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn jpeg(width: u16, height: u16, pixel: impl Fn(u32, u32) -> [u8; 3]) -> Vec<u8> {
    let rgb: Vec<u8> = (0..height as u32).flat_map(|y| (0..width as u32).map(move |x| (x, y))).flat_map(|(x, y)| pixel(x, y)).collect();
    let mut out = Vec::new();
    let mut encoder = jpeg_encoder::Encoder::new(&mut out, 95);
    encoder.set_sampling_factor(jpeg_encoder::SamplingFactor::R_4_4_4);
    encoder.encode(&rgb, width, height, jpeg_encoder::ColorType::Rgb).unwrap();
    out
}

fn session(mode: CaptureMode) -> CaptureSession {
    CaptureSession::new(&Hello::offline(mode, "Essai", "iPhone17,2", "0.1.0"), IngestParams::default())
}

#[test]
fn a_frame_assembled_from_the_table_of_the_specification_is_read_field_by_field() {
    let mm: Vec<u16> = (0..12).map(|i| 500 + 250 * i).collect();
    let confidence: Vec<u8> = (0..12).map(|i| (i % 3) as u8).collect();
    let mask: Vec<u8> = (0..12).map(|i| if i < 5 { 255 } else { 0 }).collect();
    let pose: [f32; 16] = std::array::from_fn(|i| 0.25 * i as f32 - 1.0);
    let plain = Raw {
        flags: 0b101,
        tracking: 1,
        orientation: 3,
        index: 0x0102_0304,
        reserved: 0xDEAD_BEEF,
        timestamp: 123_456.789,
        pose,
        intrinsics: [1400.5, 1401.25, 960.75, 719.5],
        color_size: (1920, 1440),
        color: vec![0xFF, 0xD8, 0xFF, 0xD9, 7],
        depth_size: (4, 3),
        depth: depth_plane(&mm),
        confidence: confidence.clone(),
        mask: mask.clone(),
    };
    let deflated = Raw {
        flags: 0b111,
        depth: stored_deflate(&plain.depth),
        confidence: stored_deflate(&confidence),
        mask: stored_deflate(&mask),
        ..plain.clone()
    };
    for (raw, compressed) in [(&plain, false), (&deflated, true)] {
        let bytes = raw.bytes();
        let frame = decode_frame(&bytes).unwrap();
        assert_eq!(frame.index, 0x0102_0304);
        assert_eq!(frame.reserved, 0xDEAD_BEEF, "kept, but means nothing");
        assert_eq!(frame.timestamp, 123_456.789);
        assert_eq!(frame.world_from_cam, pose);
        assert_eq!(frame.intrinsics, [1400.5, 1401.25, 960.75, 719.5]);
        assert_eq!((frame.tracking, frame.orientation), (Tracking::Limited, Orientation::PortraitUpsideDown));
        assert_eq!((frame.color_width, frame.color_height, frame.color.as_slice()), (1920, 1440, &[0xFF, 0xD8, 0xFF, 0xD9, 7][..]));
        assert_eq!((frame.depth_width, frame.depth_height), (4, 3));
        assert_eq!(frame.depth_mm, mm, "row-major from the top-left, millimeters");
        assert_eq!(frame.confidence.as_deref(), Some(confidence.as_slice()));
        assert_eq!(frame.mask.as_deref(), Some(mask.as_slice()));
        assert_eq!(frame.compressed, compressed);
        // What the crate writes is read back by the same table.
        let again = encode_frame(&frame).unwrap();
        assert_eq!(&again[..116], &bytes[..116], "everything before the plane lengths is identical");
        assert_eq!(decode_frame(&again).unwrap(), frame);
        if !compressed {
            assert_eq!(again, bytes);
        }
    }

    // Each plane is its own stream: two planes in one stream are refused,
    // and so is a stream that ends early or late.
    let mut joined = deflated.clone();
    joined.depth = stored_deflate(&[plain.depth.clone(), confidence.clone()].concat());
    assert_eq!(decode_frame(&joined.bytes()).unwrap_err().code, ErrorCode::BadDeflate);
    let mut short = deflated.clone();
    short.mask = stored_deflate(&mask[..11]);
    assert_eq!(decode_frame(&short.bytes()).unwrap_err().code, ErrorCode::BadDeflate);
    let mut long = deflated.clone();
    long.confidence = stored_deflate(&[confidence.clone(), vec![0]].concat());
    assert_eq!(decode_frame(&long.bytes()).unwrap_err().code, ErrorCode::BadDeflate);
    // A zlib wrapper (RFC 1950) is not raw deflate.
    let mut zlib = deflated.clone();
    zlib.depth = [vec![0x78, 0x01], stored_deflate(&plain.depth), vec![0, 0, 0, 0]].concat();
    assert_eq!(decode_frame(&zlib.bytes()).unwrap_err().code, ErrorCode::BadDeflate);

    // The file: magic, length, JSON, then length-prefixed records.
    let json = br#"{"protocol":1,"device":"iPhone17,2","app":"0.1.0","mode":"object","name":"Vase","created":"2026-11-14T10:00:00Z","region":{"center":[0.5,-0.25,-1.5],"radius":0.4},"floor":-1.25}   "#;
    let mut file = b"GSCAP1\0\0".to_vec();
    assert_eq!(file, FILE_MAGIC);
    file.extend_from_slice(&(json.len() as u32).to_le_bytes());
    file.extend_from_slice(json);
    for raw in [&plain, &deflated] {
        let record = raw.bytes();
        file.extend_from_slice(&(record.len() as u32).to_le_bytes());
        file.extend_from_slice(&record);
    }
    let contents = split_gscap(&file).unwrap();
    assert_eq!(contents.frames.len(), 2);
    assert_eq!(contents.header.capture_mode(), Some(CaptureMode::Object));
    assert_eq!(contents.header.floor, Some(-1.25), "a bare number is accepted for the floor");
    assert_eq!(contents.header.region, Some(CaptureRegion { center: [0.5, -0.25, -1.5], radius: 0.4 }));
    assert_eq!(decode_frame(contents.frames[1]).unwrap().depth_mm, mm);
}

// ---------------------------------------------------------------------------
// Geometry computed on paper
// ---------------------------------------------------------------------------

/// A flat wall 2 m in front of the camera with a red patch up and to the
/// right and a green one down and to the left, as a frame of `depth` pixels
/// whose JPEG really is `jpeg_size` while its header says 128x96.
fn wall_frame(depth: (u16, u16), jpeg_size: (u16, u16)) -> Raw {
    let (sx, sy) = (jpeg_size.0 as f32 / 128.0, jpeg_size.1 as f32 / 96.0);
    let color = jpeg(jpeg_size.0, jpeg_size.1, |x, y| {
        let (u, v) = ((x as f32 + 0.5) / sx, (y as f32 + 0.5) / sy);
        if (96.0..112.0).contains(&u) && (16.0..32.0).contains(&v) {
            [225, 30, 30]
        } else if (16.0..32.0).contains(&u) && (64.0..80.0).contains(&v) {
            [30, 215, 40]
        } else {
            [128, 128, 128]
        }
    });
    // world_from_cam, column-major: a quarter turn about +Y (the camera's
    // right is world -Z, its up is world +Y, its back is world +X, so it
    // looks along world -X), standing at (3, -1.2, 5).
    #[rustfmt::skip]
    let pose = [
        0.0, 0.0, -1.0, 0.0,
        0.0, 1.0, 0.0, 0.0,
        1.0, 0.0, 0.0, 0.0,
        3.0, -1.2, 5.0, 1.0,
    ];
    Raw {
        flags: 0,
        tracking: 2,
        orientation: 0,
        index: 40,
        reserved: 0,
        timestamp: 10.0,
        pose,
        // Off-centre on purpose, in pixels of the 128x96 colour image.
        intrinsics: [100.0, 100.0, 70.0, 40.0],
        color_size: (128, 96),
        color,
        depth_size: depth,
        depth: depth_plane(&vec![2000u16; depth.0 as usize * depth.1 as usize]),
        confidence: Vec::new(),
        mask: Vec::new(),
    }
}

fn centroid(splats: &[Splat], pick: impl Fn(&Splat) -> bool) -> (Vec3, usize) {
    let picked: Vec<Vec3> = splats.iter().filter(|s| pick(s)).map(Splat::position).collect();
    (picked.iter().copied().sum::<Vec3>() / picked.len().max(1) as f32, picked.len())
}

#[test]
fn a_scene_computed_on_paper_lands_where_the_conventions_put_it() {
    // In camera space the wall is z = -2. A colour pixel (u, v) of it is at
    // x = (u - 70) / 100 * 2, y = -(v - 40) / 100 * 2:
    //   red patch, centre (104, 24):  ( 0.68,  0.32, -2)
    //   green patch, centre (24, 72): (-0.92, -0.64, -2)
    // world = x * (0, 0, -1) + y * (0, 1, 0) + z * (1, 0, 0) + (3, -1.2, 5):
    //   red   (1, -0.88, 4.32)      green (1, -1.84, 5.92)
    // The phone reports the floor at y = -2.5, so viewers see
    //   red   (1,  1.62, 4.32)      green (1,  0.66, 5.92)
    let red_at = Vec3::new(1.0, 1.62, 4.32);
    let green_at = Vec3::new(1.0, 0.66, 5.92);
    // Image corners: u = 0 and 128, v = 0 and 96.
    let (z_range, y_range) = ((5.0 - 1.16, 5.0 + 1.4), (2.5 - 1.2 - 1.12, 2.5 - 1.2 + 0.8));

    for (depth, jpeg_size, what) in [
        ((64, 48), (128, 96), "depth at half the colour size"),
        ((48, 48), (128, 96), "depth scaled differently on each axis"),
        ((64, 48), (256, 192), "a JPEG larger than the header says"),
        ((64, 48), (96, 72), "a JPEG smaller than the header says"),
        ((128, 96), (128, 96), "depth as fine as the colour"),
    ] {
        let mut session = session(CaptureMode::Room);
        session.handle_message(&ClientMessage::Floor { y: -2.5 });
        let reply = session.handle_frame(&wall_frame(depth, jpeg_size).bytes()).unwrap();
        assert_eq!(reply.to_phone, [ServerMessage::Ack { frame: 40, used: true }], "{what}");
        let splats = session.static_splats();
        assert!(splats.len() > 5_000, "{what}: {} splats", splats.len());

        let (red, reds) = centroid(&splats, |s| s.rgba[0] > 170 && s.rgba[1] < 90 && s.rgba[2] < 90);
        let (green, greens) = centroid(&splats, |s| s.rgba[1] > 160 && s.rgba[0] < 90 && s.rgba[2] < 100);
        assert!(reds > 60 && greens > 60, "{what}: {reds} red and {greens} green splats");
        assert!(red.distance(red_at) < 0.03, "{what}: red patch at {red}, expected {red_at}");
        assert!(green.distance(green_at) < 0.03, "{what}: green patch at {green}, expected {green_at}");

        // The whole wall: a plane at x = 1 facing the camera (+X), with the
        // extent the intrinsics give it.
        let (mut lo, mut hi) = (Vec3::splat(f32::INFINITY), Vec3::splat(f32::NEG_INFINITY));
        for s in &splats {
            lo = lo.min(s.position());
            hi = hi.max(s.position());
            let normal = Quat::from_array(s.rot) * Vec3::Z;
            assert!(normal.x > 0.95, "{what}: normal {normal}");
        }
        let cell = 0.05;
        assert!((lo.x - 1.0).abs() < 0.02 && (hi.x - 1.0).abs() < 0.02, "{what}: x {} to {}", lo.x, hi.x);
        assert!((lo.z - z_range.0).abs() < cell && (hi.z - z_range.1).abs() < cell, "{what}: z {} to {}", lo.z, hi.z);
        assert!((lo.y - y_range.0).abs() < cell && (hi.y - y_range.1).abs() < cell, "{what}: y {} to {}", lo.y, hi.y);

        // The phone, as viewers are told: at (3, 1.3, 5), looking along -X.
        let stats = session.stats();
        let camera = stats.scan.unwrap().camera;
        assert_eq!(&camera.world_from_cam[12..15], &[3.0, 1.3, 5.0], "{what}");
        assert_eq!(&camera.world_from_cam[8..11], &[1.0, 0.0, 0.0], "{what}: the camera's +Z (its back)");
    }

    // The same frame with its pose written row-major (the translation in the
    // bottom row) or mirrored is refused, not fused somewhere else.
    let good = wall_frame((64, 48), (128, 96));
    let mut transposed = good.clone();
    for row in 0..4 {
        for col in 0..4 {
            transposed.pose[row * 4 + col] = good.pose[col * 4 + row];
        }
    }
    let mut mirrored = good.clone();
    mirrored.pose[0..3].iter_mut().for_each(|v| *v = -*v);
    for (raw, what) in [(transposed, "transposed"), (mirrored, "mirrored")] {
        let mut session = session(CaptureMode::Room);
        let reply = session.handle_frame(&raw.bytes()).unwrap();
        assert_eq!(reply.frame.unwrap().skipped, Some(SkipReason::Pose), "{what}");
        assert!(session.static_splats().is_empty());
    }
}

#[test]
fn the_mask_and_the_confidence_select_pixels_of_the_depth_grid_row_major() {
    // The wall again, 64x48. The mask marks the top-right quarter of the
    // image, the confidence is low in the left half: person mode keeps the
    // top-right quarter only, which is the part of the wall above the
    // optical axis row (v < 24 of 48) and toward world -Z.
    let mut raw = wall_frame((64, 48), (128, 96));
    raw.flags = 0b101;
    raw.mask = (0..48).flat_map(|y| (0..64).map(move |x| if x >= 32 && y < 24 { 255u8 } else { 0 })).collect();
    raw.confidence = (0..48).flat_map(|_| (0..64).map(|x| if x < 32 { 0u8 } else { 2 })).collect();
    let mut person = session(CaptureMode::Person);
    person.handle_message(&ClientMessage::Floor { y: -2.5 });
    person.handle_message(&ClientMessage::Region(CaptureRegion { center: [1.0, -1.2, 5.0], radius: 3.0 }));
    assert!(person.handle_frame(&raw.bytes()).unwrap().frame.unwrap().used);
    let splats = person.static_splats();
    assert!(splats.len() > 2_000, "{} splats", splats.len());
    // Depth pixel x >= 32 is colour u >= 64: camera x >= -0.12, world
    // z <= 5.12. Depth pixel y < 24 is colour v < 48: camera y > -0.16,
    // viewer y > 1.14. One depth pixel is 4 cm on the wall.
    for s in &splats {
        assert!(s.pos[2] < 5.12 + 0.05 && s.pos[1] > 1.14 - 0.05, "{:?} is outside the masked quarter", s.pos);
    }
    let (lo_z, hi_y) = splats.iter().fold((f32::INFINITY, f32::NEG_INFINITY), |(z, y), s| (z.min(s.pos[2]), y.max(s.pos[1])));
    assert!((lo_z - 3.84).abs() < 0.05 && (hi_y - 2.1).abs() < 0.05, "z from {lo_z}, y up to {hi_y}");

    // Room mode ignores the mask but not the confidence: the right half.
    let mut room = session(CaptureMode::Room);
    room.handle_message(&ClientMessage::Floor { y: -2.5 });
    room.handle_frame(&raw.bytes()).unwrap();
    let splats = room.static_splats();
    let (lo_y, hi_z) = splats.iter().fold((f32::INFINITY, f32::NEG_INFINITY), |(y, z), s| (y.min(s.pos[1]), z.max(s.pos[2])));
    assert!((lo_y - 0.18).abs() < 0.05 && (hi_z - 5.12).abs() < 0.05, "y from {lo_y}, z up to {hi_z}");
}

// ---------------------------------------------------------------------------
// What viewers hold
// ---------------------------------------------------------------------------

/// A viewer: applies the messages of a session like the real client.
#[derive(Default)]
struct Viewer {
    state: Option<SessionState>,
    layers: [Vec<Splat>; 2],
    stats: Option<Stats>,
    replaces: usize,
}

impl Viewer {
    fn apply(&mut self, messages: &[Outgoing]) {
        for message in messages {
            match codec::decode(&message.bytes).unwrap().body {
                Body::State(json) => self.state = Some(serde_json::from_str(&json).unwrap()),
                Body::Stats(json) => self.stats = Some(serde_json::from_str(&json).unwrap()),
                Body::Preview(_) => {}
                Body::Layer(layer) => {
                    let id = layer.layer_id as usize;
                    if layer.mode == LayerMode::Replace {
                        self.layers[id] = layer.splats;
                        self.replaces += 1;
                    } else {
                        self.layers[id].extend_from_slice(&layer.splats);
                    }
                }
            }
        }
    }

    /// Mean height of the level surface within 15 cm of `expected` (the
    /// floor of the living room, not its table).
    fn floor_height(&self, expected: f32) -> f32 {
        let ys: Vec<f32> = self.layers[0]
            .iter()
            .filter(|s| (Quat::from_array(s.rot) * Vec3::Z).y > 0.9 && (s.pos[1] - expected).abs() < 0.15)
            .map(|s| s.pos[1])
            .collect();
        assert!(ys.len() > 300, "{} floor splats near y = {expected}", ys.len());
        ys.iter().sum::<f32>() / ys.len() as f32
    }
}

fn small_sim() -> PhoneSim {
    PhoneSim { color_width: 160, color_height: 120, depth_width: 64, depth_height: 48, ..PhoneSim::default() }
}

/// The phone turning on the spot in the living room of the house.
fn living_room(n: usize, degrees: f32) -> (Phone, Vec<Shot>) {
    let sim = small_sim();
    let snap = make_scene(SceneKind::House).snapshot(0.0);
    let cam = |i: usize| {
        let eye = Vec3::new(-2.6, 1.4, -1.8);
        let yaw = (degrees * i as f32 / n.max(1) as f32).to_radians();
        sim.look_at(eye, eye + Vec3::new(-yaw.sin(), -0.3, -yaw.cos()))
    };
    let mut phone = Phone::new(sim, &cam(0));
    let shots = (0..n).map(|i| phone.shoot(&snap, None, &cam(i), i as f64 * 0.1)).collect();
    (phone, shots)
}

#[test]
fn a_viewer_that_joins_does_not_take_the_broadcast_the_others_wait_for() {
    // The floor moves after frames were fused; before the next frame or
    // progress tick brings layer 0 in the new coordinates, a second viewer
    // joins and gets a snapshot of its own. The first viewer must still get
    // its layer 0 again: its cloud and the newcomer's are the same.
    let (phone, shots) = living_room(10, 100.0);
    let truth = phone.world.floor_y();
    let mut capture = session(CaptureMode::Room);
    let mut first = Viewer::default();
    first.apply(&capture.snapshot());
    for shot in &shots[..5] {
        first.apply(&capture.handle_decoded(&shot.frame).to_viewers);
    }
    first.apply(&capture.handle_message(&ClientMessage::Floor { y: truth + 0.5 }).to_viewers);
    let mut second = Viewer::default();
    second.apply(&capture.snapshot());
    for shot in &shots[5..] {
        let reply = capture.handle_decoded(&shot.frame).to_viewers;
        first.apply(&reply);
        second.apply(&reply);
    }
    let reply = capture.progress().to_viewers;
    first.apply(&reply);
    second.apply(&reply);
    assert_eq!(first.layers[0].len(), second.layers[0].len());
    assert_eq!(first.layers[0].len(), capture.static_splats().len());
    // The phone put the floor half a meter above the true one: for every
    // viewer the true floor is now at y = -0.5, and nowhere else.
    let (a, b) = (first.floor_height(-0.5), second.floor_height(-0.5));
    assert!((a + 0.5).abs() < 0.02 && (b + 0.5).abs() < 0.02, "floor at {a} for the first viewer, {b} for the second");
    // Nothing is left at the old height that the newcomer does not have too
    // (furniture half a meter above the floor).
    let level = |v: &Viewer| v.layers[0].iter().filter(|s| (Quat::from_array(s.rot) * Vec3::Z).y > 0.9 && s.pos[1].abs() < 0.05).count();
    assert_eq!(level(&first), level(&second), "floor splats left at the old height");

    // The same with a region that moves in object mode: the first viewer
    // must not keep what is outside the new region.
    let mut capture = session(CaptureMode::Object);
    let mut first = Viewer::default();
    first.apply(&capture.snapshot());
    for shot in &shots[..5] {
        first.apply(&capture.handle_decoded(&shot.frame).to_viewers);
    }
    let auto = capture.region().unwrap();
    let before = first.layers[0].len();
    assert!(before > 500, "{before} splats");
    let moved = CaptureRegion { center: [auto.center[0] + 0.3, auto.center[1], auto.center[2]], radius: 0.3 };
    first.apply(&capture.handle_message(&ClientMessage::Region(moved)).to_viewers);
    let mut second = Viewer::default();
    second.apply(&capture.snapshot());
    let reply = capture.progress().to_viewers;
    first.apply(&reply);
    second.apply(&reply);
    assert_eq!(second.layers[0].len(), capture.static_splats().len());
    assert_eq!(first.layers[0].len(), second.layers[0].len(), "the first viewer still holds the old region");
    assert!(first.layers[0].len() < before);
    assert_eq!(first.state.as_ref().unwrap().capture.as_ref().unwrap().region.unwrap().radius, 0.3);
}

#[test]
fn a_region_that_arrives_late_is_what_viewers_end_up_with() {
    // Object mode, the region placed after a dozen frames, somewhere else
    // than what the first frame looked at, then moved again while frames
    // keep coming. After every progress tick a viewer holds exactly the
    // splats inside the region in effect.
    let (_, shots) = living_room(24, 60.0);
    let mut capture = session(CaptureMode::Object);
    let mut viewer = Viewer::default();
    viewer.apply(&capture.snapshot());
    let inside = |viewer: &Viewer, capture: &CaptureSession| {
        let region = capture.region().unwrap();
        let state = viewer.state.as_ref().unwrap();
        let told = state.capture.as_ref().unwrap().region.unwrap();
        // Viewer coordinates: the region is told shifted like the splats.
        let center = Vec3::from_array(told.center);
        assert_eq!(told.radius, region.radius);
        assert!((center.y - (region.center[1] - capture.floor().0)).abs() < 1e-4);
        viewer.layers[0].iter().all(|s| s.position().distance(center) <= region.radius + 0.002)
    };
    for shot in &shots[..12] {
        viewer.apply(&capture.handle_decoded(&shot.frame).to_viewers);
    }
    viewer.apply(&capture.progress().to_viewers);
    let auto = capture.region().unwrap();
    assert!(inside(&viewer, &capture) && viewer.layers[0].len() > 1_000);

    // 0.5 m to the side, within the margin kept around the first region.
    let late = CaptureRegion { center: [auto.center[0] + 0.5, auto.center[1], auto.center[2]], radius: 0.5 };
    viewer.apply(&capture.handle_text(&ClientMessage::Region(late).to_json()).unwrap().to_viewers);
    for shot in &shots[12..18] {
        viewer.apply(&capture.handle_decoded(&shot.frame).to_viewers);
    }
    viewer.apply(&capture.progress().to_viewers);
    assert_eq!(capture.region(), Some(late));
    assert!(inside(&viewer, &capture), "splats outside the late region are still shown");
    assert_eq!(viewer.layers[0].len(), capture.static_splats().len());
    assert!(viewer.layers[0].len() > 500, "{}", viewer.layers[0].len());

    // Moved again, and the phone repeats it (as it does on reconnection):
    // the repeat changes nothing and costs no new layer.
    let again = CaptureRegion { center: [late.center[0], late.center[1] + 0.2, late.center[2]], radius: 0.4 };
    viewer.apply(&capture.handle_message(&ClientMessage::Region(again)).to_viewers);
    viewer.apply(&capture.progress().to_viewers);
    let replaces = viewer.replaces;
    viewer.apply(&capture.handle_message(&ClientMessage::Region(again)).to_viewers);
    viewer.apply(&capture.progress().to_viewers);
    assert_eq!(viewer.replaces, replaces, "a repeated region is not a change");
    for shot in &shots[18..] {
        viewer.apply(&capture.handle_decoded(&shot.frame).to_viewers);
    }
    viewer.apply(&capture.finish().to_viewers);
    assert!(inside(&viewer, &capture));
    assert_eq!(viewer.layers[0].len(), capture.static_splats().len());
    assert_eq!(viewer.stats.unwrap().splats_static as usize, viewer.layers[0].len());
}

#[test]
fn frame_indices_with_gaps_are_acknowledged_by_index_and_a_restarted_app_starts_a_new_world() {
    let (_, shots) = living_room(12, 120.0);
    let with_index = |i: usize, index: u32| CaptureFrame { index, ..shots[i].frame.clone() };
    let mut capture = session(CaptureMode::Room);
    let mut viewer = Viewer::default();
    viewer.apply(&capture.snapshot());
    // The index starts wherever the app is in its life and has gaps (frames
    // the phone skipped, frames of an earlier capture).
    for (i, index) in [1000u32, 1001, 1007, 1050, u32::MAX - 1, u32::MAX].into_iter().enumerate() {
        let reply = capture.handle_decoded(&with_index(i, index));
        assert_eq!(reply.to_phone, [ServerMessage::Ack { frame: index, used: true }]);
        viewer.apply(&reply.to_viewers);
    }
    assert_eq!((capture.frames_received(), capture.frames_used()), (6, 6));
    let before = capture.static_splats().len();
    assert!(before > 5_000);

    // The connection drops and comes back (same hello): the capture goes on,
    // with the indices where they were.
    let hello = Hello::offline(CaptureMode::Room, "Essai", "iPhone17,2", "0.1.0");
    assert!(capture.matches(&hello));
    capture.resume();
    let mut capture2 = session(CaptureMode::Room);
    for (i, index) in [1000u32, 1001, 1007].into_iter().enumerate() {
        capture2.handle_decoded(&with_index(i, index));
    }
    capture2.resume();
    let reply = capture2.handle_decoded(&with_index(3, 1008));
    assert!(reply.frame.unwrap().used);
    assert_eq!(capture2.frames_received(), 4, "a reconnection continues the capture");

    // The app was restarted (its index starts again at 0, and with it
    // ARKit's world): what was fused belongs to another world and cannot be
    // continued. The session starts again from nothing, and says so.
    let mut restarted = with_index(6, 0);
    // The new world has its origin elsewhere: the same wall would land two
    // meters away, through the old cloud.
    restarted.world_from_cam[12] += 2.0;
    let reply = capture2.handle_decoded(&restarted);
    assert_eq!(reply.to_phone[0], ServerMessage::Ack { frame: 0, used: true });
    assert_eq!(capture2.frames_received(), 1, "the counters start again, like after a reset");
    let mut late = Viewer::default();
    late.apply(&reply.to_viewers);
    assert!(late.state.is_some(), "viewers are told about the new world");
    assert_eq!(late.layers[0].len(), capture2.static_splats().len());
    assert!(late.replaces >= 1, "layer 0 is replaced, not appended to");
    let lone = {
        let mut alone = session(CaptureMode::Room);
        alone.handle_decoded(&restarted);
        alone.static_splats().len()
    };
    assert_eq!(capture2.static_splats().len(), lone, "nothing of the old world is left");
}

/// The wall of [`wall_frame`] moved toward the camera by `closer` meters.
fn wall_at(closer: f32, index: u32) -> Vec<u8> {
    let mut raw = wall_frame((64, 48), (128, 96));
    raw.index = index;
    raw.timestamp = 10.0 + index as f64 * 0.1;
    raw.depth = depth_plane(&vec![(2000.0 - closer * 1000.0) as u16; 64 * 48]);
    raw.bytes()
}

#[test]
fn layers_a_few_centimeters_apart_are_one_surface_and_farther_ones_are_two() {
    // One wall, seen once: the count every other case is compared with.
    let mut single = session(CaptureMode::Room);
    single.handle_frame(&wall_at(0.0, 0)).unwrap();
    let one = single.static_splats().len();
    single.finish();
    assert_eq!(single.static_splats().len(), one, "a clean wall loses nothing at the end");

    // The same wall measured 4 cm nearer in a second frame (a depth error
    // LiDAR makes at 3 to 4 m): two layers while the capture runs, one
    // surface when layer 0 is next sent as a whole.
    let mut noisy = session(CaptureMode::Room);
    let mut viewer = Viewer::default();
    viewer.apply(&noisy.snapshot());
    viewer.apply(&noisy.handle_frame(&wall_at(0.0, 0)).unwrap().to_viewers);
    viewer.apply(&noisy.handle_frame(&wall_at(0.04, 1)).unwrap().to_viewers);
    let doubled = noisy.static_splats().len();
    assert!(doubled as f32 > 1.7 * one as f32, "{doubled} cells for two layers of {one}");
    viewer.apply(&noisy.finish().to_viewers);
    let merged = noisy.static_splats().len();
    assert!((merged as f32) < 1.08 * one as f32 && merged as f32 > 0.95 * one as f32, "{merged} cells left of {doubled}, one wall is {one}");
    assert_eq!(viewer.layers[0].len(), merged, "viewers get the merged layer");
    // What is left is one sheet: no two cells of a column behind each other.
    let mut columns = std::collections::HashMap::new();
    for s in &viewer.layers[0] {
        *columns.entry(((s.pos[1] / 0.024).floor() as i32, (s.pos[2] / 0.024).floor() as i32)).or_insert(0) += 1;
    }
    let stacked = columns.values().filter(|&&n| n > 1).count();
    assert!(stacked * 50 < columns.len(), "{stacked} of {} columns still hold two cells", columns.len());

    // 10 cm apart they are two surfaces (a board in front of a wall), and
    // stay two.
    let mut two = session(CaptureMode::Room);
    two.handle_frame(&wall_at(0.0, 0)).unwrap();
    two.handle_frame(&wall_at(0.10, 1)).unwrap();
    two.finish();
    assert!(two.static_splats().len() as f32 > 1.8 * one as f32, "{} cells for two walls of {one}", two.static_splats().len());

    // During a capture the merge comes with the periodic whole layer: the
    // viewer never holds a layer 0 that differs from the session's.
    let mut running = session(CaptureMode::Room);
    let mut viewer = Viewer::default();
    viewer.apply(&running.snapshot());
    for i in 0..40u32 {
        // The wall wobbles by up to 4 cm from frame to frame.
        let closer = [0.0, 0.04, 0.01, 0.035, 0.02][i as usize % 5];
        viewer.apply(&running.handle_frame(&wall_at(closer, i)).unwrap().to_viewers);
        if i % 10 == 9 {
            viewer.apply(&running.progress().to_viewers);
        }
        assert_eq!(viewer.layers[0].len(), running.static_splats().len(), "frame {i}");
    }
    for _ in 0..6 {
        viewer.apply(&running.progress().to_viewers);
    }
    assert_eq!(viewer.layers[0].len(), running.static_splats().len());
    let settled = running.static_splats().len();
    assert!((settled as f32) < 1.15 * one as f32, "{settled} cells while running, one wall is {one}");
    // With the merge off, the layers stay.
    let params = IngestParams { layer_window: 0.0, ..IngestParams::default() };
    let mut off = CaptureSession::new(&Hello::offline(CaptureMode::Room, "x", "d", "a"), params);
    off.handle_frame(&wall_at(0.0, 0)).unwrap();
    off.handle_frame(&wall_at(0.04, 1)).unwrap();
    off.finish();
    assert!(off.static_splats().len() as f32 > 1.7 * one as f32);
}

// ---------------------------------------------------------------------------
// Hostile input
// ---------------------------------------------------------------------------

fn fixture(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ios/SplatCapture/Fixtures").join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// A frame that decodes must be self-consistent, whatever it was made of.
fn check_decoded(frame: &CaptureFrame) {
    let pixels = frame.depth_width as usize * frame.depth_height as usize;
    assert!((1..=1 << 22).contains(&pixels));
    assert_eq!(frame.depth_mm.len(), pixels);
    assert!(frame.confidence.as_ref().is_none_or(|p| p.len() == pixels));
    assert!(frame.mask.as_ref().is_none_or(|p| p.len() == pixels));
    // It can be written again, and reads back to the same bytes.
    let again = encode_frame(frame).unwrap();
    assert_eq!(encode_frame(&decode_frame(&again).unwrap()).unwrap(), again);
}

/// Rounds of a mutation test: `base` times `GS_FUZZ` (default 1), for a
/// longer hunt than the test suite can afford every time:
/// `GS_FUZZ=50 cargo test -p gs-core --test ingest_verify mutated`.
fn rounds(base: usize) -> usize {
    base * std::env::var("GS_FUZZ").ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(1).max(1)
}

fn mutate(rng: &mut Rng, bytes: &mut Vec<u8>) {
    let pick = |rng: &mut Rng, n: usize| (rng.next_u64() % n.max(1) as u64) as usize;
    match rng.next_u64() % 8 {
        // Bit flips, biased toward the header where every byte means something.
        0..=2 => {
            for _ in 0..1 + pick(rng, 3) {
                let at = if rng.next_u64().is_multiple_of(2) { pick(rng, bytes.len().min(128)) } else { pick(rng, bytes.len()) };
                if let Some(b) = bytes.get_mut(at) {
                    *b ^= 1 << (rng.next_u64() % 8);
                }
            }
        }
        // A byte, a u16 or a u32 at an extreme.
        3 | 4 => {
            let extremes: [&[u8]; 8] =
                [&[0], &[0xFF], &[0x7F], &[0xFF, 0xFF], &[0, 0x80], &[0xFF, 0xFF, 0xFF, 0xFF], &[0, 0, 0, 0x80], &[1, 0, 0, 0]];
            let value = extremes[pick(rng, extremes.len())];
            let at = pick(rng, bytes.len().min(132));
            for (i, v) in value.iter().enumerate() {
                if let Some(b) = bytes.get_mut(at + i) {
                    *b = *v;
                }
            }
        }
        5 => bytes.truncate(pick(rng, bytes.len() + 1)),
        6 => {
            let extra = pick(rng, 64);
            bytes.extend((0..extra).map(|_| rng.next_u64() as u8));
        }
        // A run of bytes overwritten or removed in the middle.
        _ => {
            let at = pick(rng, bytes.len());
            let run = pick(rng, 40).min(bytes.len() - at);
            if rng.next_u64().is_multiple_of(2) {
                bytes.drain(at..at + run);
            } else {
                bytes[at..at + run].iter_mut().for_each(|b| *b = rng.next_u64() as u8);
            }
        }
    }
}

#[test]
fn mutated_frames_never_panic_and_never_poison_a_session() {
    let (_, shots) = living_room(3, 30.0);
    let mut seeds: Vec<Vec<u8>> = shots.iter().map(|s| encode_frame(&s.frame).unwrap()).collect();
    seeds.push(encode_frame(&CaptureFrame { compressed: false, confidence: None, ..shots[0].frame.clone() }).unwrap());
    for name in ["frame_lidar", "frame_deflate_conf_mask", "frame_conf_mask", "frame_extreme", "frame_plain"] {
        seeds.push(fixture(&format!("frames/{name}.bin")));
    }
    let mut rng = Rng(0xC0FFEE);
    let mut sessions: Vec<CaptureSession> = CaptureMode::ALL.into_iter().map(session).collect();
    let (mut decoded, mut used, mut worst) = (0usize, 0usize, 0.0f64);
    for round in 0..rounds(6_000) {
        let mut bytes = seeds[round % seeds.len()].clone();
        for _ in 0..1 + rng.next_u64() % 3 {
            mutate(&mut rng, &mut bytes);
        }
        let start = Instant::now();
        let _ = FrameHeader::parse(&bytes);
        if let Ok(frame) = decode_frame(&bytes) {
            decoded += 1;
            check_decoded(&frame);
            let session = &mut sessions[round % 4];
            let reply = session.handle_frame(&bytes).unwrap();
            used += usize::from(reply.frame.unwrap().used);
            for message in &reply.to_viewers {
                codec::decode(&message.bytes).unwrap();
            }
        }
        worst = worst.max(start.elapsed().as_secs_f64());
    }
    // The mutations are not all fatal, or this test would test nothing.
    assert!(decoded > 500 && used > 100, "{decoded} decoded, {used} used");
    // Whatever went in, what viewers get is finite and bounded.
    for session in &mut sessions {
        session.progress();
        session.finish();
        for splat in session.static_splats().iter().chain(&session.dynamic_splats()) {
            assert!(splat.pos.iter().chain(&splat.scale).chain(&splat.rot).all(|v| v.is_finite()), "{splat:?}");
        }
        let state = session.state();
        assert!(state.bounds.min.is_finite() && state.bounds.max.is_finite(), "{:?}", state.bounds);
        assert!(state.viewer_start.eye.iter().chain(&state.viewer_start.target).all(|v| v.is_finite()));
        serde_json::to_string(&state).unwrap();
        for message in session.snapshot() {
            codec::decode(&message.bytes).unwrap();
        }
    }
    println!("{decoded} of {} mutated frames decoded, {used} were fused; slowest {:.1} ms", rounds(6_000), worst * 1e3);
    assert!(worst < 2.0, "a mutated frame of a few kB took {worst:.2} s");
}

#[test]
fn every_length_field_at_its_extremes_is_refused_without_allocating_for_it() {
    let base = wall_frame((64, 48), (128, 96));
    let good = base.bytes();
    assert!(decode_frame(&good).is_ok());
    // (offset, size) of every size and length field of the header.
    let fields = [(104, 2), (106, 2), (108, 4), (112, 2), (114, 2), (116, 4), (120, 4), (124, 4)];
    let values: [u32; 9] = [0, 1, 2, 0x7FFF, 0x8000, 0xFFFF, 0x7FFF_FFFF, 0x8000_0000, 0xFFFF_FFFF];
    for flags in 0u8..8 {
        for (offset, size) in fields {
            for value in values {
                let mut bytes = good.clone();
                bytes[5] = flags;
                bytes[offset..offset + size].copy_from_slice(&value.to_le_bytes()[..size]);
                if bytes == good {
                    continue;
                }
                let start = Instant::now();
                if let Ok(frame) = decode_frame(&bytes) {
                    check_decoded(&frame);
                    // Only the colour size may change without the frame
                    // becoming another frame.
                    assert!(offset < 108 && flags == 0, "flags {flags}, field at {offset} = {value:#x} was accepted");
                }
                assert!(start.elapsed().as_secs_f64() < 0.5);
            }
        }
    }
    // All four lengths at once: the sum wraps 32 bits back to something
    // small.
    let mut wrap = good.clone();
    wrap[5] = 0b111;
    for (offset, value) in [(108, 0x4000_0000u32), (116, 0x4000_0000), (120, 0x4000_0000), (124, 0x4000_0000 + 6144 + 6272)] {
        wrap[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
    assert_eq!(decode_frame(&wrap).unwrap_err().code, ErrorCode::BadLength);
}

#[test]
fn deflate_bombs_cost_neither_memory_nor_time() {
    use miniz_oxide::deflate::compress_to_vec;
    // 128 MiB of zeros are half a megabyte of deflate. Announced as the depth of
    // the largest map the format allows (2048 x 2048, 8 MiB inflated).
    let bomb = compress_to_vec(&vec![0u8; 128 << 20], 1);
    assert!(bomb.len() < 2 << 20, "{} bytes", bomb.len());
    let mut raw = wall_frame((2048, 2048), (128, 96));
    raw.flags = 0b010;
    raw.depth = bomb.clone();
    let start = Instant::now();
    let error = decode_frame(&raw.bytes()).unwrap_err();
    assert_eq!(error.code, ErrorCode::BadDeflate);
    assert!(start.elapsed().as_secs_f64() < 2.0, "{:?}", start.elapsed());
    // The same in the confidence and the mask plane of a small frame.
    let mut raw = wall_frame((64, 48), (128, 96));
    raw.flags = 0b111;
    raw.depth = stored_deflate(&raw.depth.clone());
    raw.confidence = bomb.clone();
    raw.mask = stored_deflate(&[0u8; 64 * 48]);
    assert_eq!(decode_frame(&raw.bytes()).unwrap_err().code, ErrorCode::BadDeflate);
    raw.confidence = stored_deflate(&[2u8; 64 * 48]);
    raw.mask = bomb;
    let start = Instant::now();
    assert_eq!(decode_frame(&raw.bytes()).unwrap_err().code, ErrorCode::BadDeflate);
    assert!(start.elapsed().as_secs_f64() < 0.5, "a small plane stops the bomb at once: {:?}", start.elapsed());

    // The largest honest frame: 2048 x 2048 decodes (the format allows it)
    // but is not fused, and 1024 x 1024, a few kB on the wire, is the most
    // a frame can make the reconstruction work on.
    let mut huge = wall_frame((2048, 2048), (128, 96));
    huge.flags = 0b010;
    huge.depth = compress_to_vec(&depth_plane(&vec![1500u16; 2048 * 2048]), 6);
    let bytes = huge.bytes();
    assert!(bytes.len() < 64 << 10, "{} bytes on the wire", bytes.len());
    let mut capture = session(CaptureMode::Room);
    let reply = capture.handle_frame(&bytes).unwrap();
    assert_eq!(reply.frame.unwrap().skipped, Some(SkipReason::DepthSize));

    let mut large = wall_frame((1024, 1024), (128, 96));
    large.flags = 0b010;
    large.depth = compress_to_vec(&depth_plane(&vec![1500u16; 1024 * 1024]), 6);
    let bytes = large.bytes();
    assert!(bytes.len() < 32 << 10, "{} bytes on the wire", bytes.len());
    let start = Instant::now();
    let reply = capture.handle_frame(&bytes).unwrap();
    let seconds = start.elapsed().as_secs_f64();
    let report = reply.frame.unwrap();
    println!("a 1024x1024 depth map in {} bytes: {:.0} ms, {} splats", bytes.len(), seconds * 1e3, report.splats);
    assert!(report.used);
    assert!(seconds < 3.0, "the largest frame that is fused took {seconds:.1} s");
}

/// Imports `bytes` in random pieces; whatever happens, it returns.
fn import_in_pieces(rng: &mut Rng, bytes: &[u8]) -> Option<(usize, u64, bool)> {
    let mut importer = GscapImporter::new(IngestParams::default());
    let mut rest = bytes;
    while !rest.is_empty() {
        let n = (1 + rng.next_u64() as usize % 700).min(rest.len());
        if importer.push(&rest[..n]).is_err() {
            return None;
        }
        rest = &rest[n..];
    }
    let (session, summary) = importer.finish().ok()?;
    Some((session.static_splats().len(), summary.frames, summary.truncated))
}

#[test]
fn mutated_files_never_panic_and_agree_between_the_two_readers() {
    let (phone, shots) = living_room(4, 30.0);
    let header = GscapHeader {
        protocol: Some(1),
        mode: "room".into(),
        name: "Salon".into(),
        floor: Some(phone.world.floor_y()),
        ..GscapHeader::default()
    };
    let mut writer = GscapWriter::new(Vec::new(), &header, 256).unwrap();
    for shot in &shots {
        writer.append(&encode_frame(&shot.frame).unwrap()).unwrap();
    }
    let good = writer.into_inner();
    let mut seeds = vec![good.clone()];
    for name in ["three_frames.gscap", "padded_header.gscap", "truncated.gscap", "empty.gscap"] {
        seeds.push(fixture(name));
    }
    let mut rng = Rng(0xFEED);
    let (mut imported, mut refused) = (0, 0);
    for round in 0..rounds(1_500) {
        let mut bytes = seeds[round % seeds.len()].clone();
        for _ in 0..1 + rng.next_u64() % 2 {
            mutate(&mut rng, &mut bytes);
        }
        // The in-memory splitter and the push parser see the same file.
        let whole = split_gscap(&bytes);
        let mut parser = GscapParser::new();
        let mut frames = 0u64;
        let mut most = 0usize;
        let mut outcome = Ok(());
        'file: for piece in bytes.chunks(1 + round % 300) {
            parser.push(piece);
            loop {
                match parser.next_item() {
                    Ok(Some(ParsedItem::Frame(_))) => frames += 1,
                    Ok(Some(ParsedItem::Header(_))) => {}
                    Ok(None) => break,
                    Err(e) => {
                        outcome = Err(e);
                        break 'file;
                    }
                }
            }
            most = most.max(parser.buffered());
        }
        let outcome = outcome.and_then(|()| parser.finish());
        match (&whole, &outcome) {
            (Ok(whole), Ok(end)) => {
                assert_eq!((whole.frames.len() as u64, whole.truncated, whole.trailing_bytes), (frames, end.truncated, end.trailing_bytes));
            }
            (Err(a), Err(b)) => assert_eq!(a.code, b.code, "round {round}"),
            other => panic!("round {round}: the readers disagree: {other:?}"),
        }
        assert!(most <= bytes.len(), "the parser buffered {most} bytes of a file of {}", bytes.len());
        match import_in_pieces(&mut rng, &bytes) {
            Some(_) => imported += 1,
            None => refused += 1,
        }
    }
    assert!(imported > 100 && refused > 100, "{imported} imported, {refused} refused");

    // A record length that announces 64 MiB at the end of a short file is a
    // recording cut short, and nothing is reserved for it.
    let mut cut = good.clone();
    cut.extend_from_slice(&(64u32 << 20).to_le_bytes());
    cut.extend_from_slice(&[0; 100]);
    let (frames, count, truncated) = import_in_pieces(&mut rng, &cut).unwrap();
    assert!(frames > 0);
    assert_eq!((count, truncated), (4, true));
}

#[test]
fn text_that_is_not_a_message_is_refused_without_a_panic() {
    let mut rng = Rng(42);
    let seeds = [
        r#"{"type":"hello","protocol":1,"token":"0123456789abcdef0123456789abcdef","device":"iPhone17,2","app":"0.1.0","mode":"room","name":"Salon"}"#,
        r#"{"type":"region","center":[0.5,-0.25,-1.5],"radius":0.75}"#,
        r#"{"type":"floor","y":-1.25}"#,
        r#"{"type":"finish"}"#,
        "splatlive://pair?host=192.168.1.20&port=7879&fp=00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff&token=0123456789abcdef0123456789abcdef",
        r#"{"protocol":1,"device":"d","app":"a","mode":"person","name":"n","created":"c","region":{"center":[1,2,3],"radius":1},"floor":{"y":-1}}"#,
    ];
    let token = "0123456789abcdef0123456789abcdef";
    let mut accepted = 0;
    for round in 0..rounds(20_000) {
        let mut bytes = seeds[round % seeds.len()].as_bytes().to_vec();
        for _ in 0..1 + rng.next_u64() % 3 {
            mutate(&mut rng, &mut bytes);
        }
        let text = String::from_utf8_lossy(&bytes);
        accepted += usize::from(Hello::parse(&text, token).is_ok());
        if let Ok(message) = ClientMessage::parse(&text) {
            accepted += 1;
            // What is accepted is usable as it is.
            match message {
                ClientMessage::Region(r) => {
                    assert!(r.center.iter().all(|c| c.is_finite() && c.abs() <= 1.0e4));
                    assert!((CaptureRegion::MIN_RADIUS..=CaptureRegion::MAX_RADIUS).contains(&r.radius));
                }
                ClientMessage::Floor { y } => assert!(y.is_finite() && y.abs() <= 1.0e4),
                _ => {}
            }
        }
        if let Ok(pairing) = parse_pairing_uri(&text) {
            accepted += 1;
            assert_eq!(parse_pairing_uri(&pairing.uri()).unwrap(), pairing, "the canonical form parses to itself");
        }
        if let Ok(header) = GscapHeader::from_json(&text) {
            assert!(header.floor.is_none_or(f32::is_finite));
            let _ = CaptureSession::from_header(&header, IngestParams::default());
        }
        let _ = ServerMessage::parse(&text);
    }
    assert!(accepted > 100, "{accepted} mutated texts were still valid");
    // The numbers JSON cannot carry and the sizes nobody should send.
    for bad in [
        r#"{"type":"floor","y":1e400}"#,
        r#"{"type":"floor","y":-1e400}"#,
        r#"{"type":"region","center":[0,0,0],"radius":1e-50}"#,
        r#"{"type":"region","center":[0,0,0],"radius":null}"#,
    ] {
        assert!(ClientMessage::parse(bad).is_err(), "{bad}");
    }
    let long = format!(r#"{{"type":"pause","pad":"{}"}}"#, "x".repeat(20_000));
    assert!(ClientMessage::parse(&long).is_err());
}

// ---------------------------------------------------------------------------
// JPEGs of every flavour
// ---------------------------------------------------------------------------

fn test_picture(width: u16, height: u16) -> Vec<u8> {
    // Left half reddish, right half bluish, brighter toward the bottom.
    (0..height as u32)
        .flat_map(|y| (0..width as u32).map(move |x| (x, y)))
        .flat_map(|(x, y)| {
            let shade = (60 + 150 * y / height.max(1) as u32) as u8;
            if x < width as u32 / 2 { [shade, 40, 40] } else { [40, 40, shade] }
        })
        .collect()
}

fn encode_with(width: u16, height: u16, setup: impl FnOnce(&mut jpeg_encoder::Encoder<&mut Vec<u8>>)) -> Vec<u8> {
    let mut out = Vec::new();
    let mut encoder = jpeg_encoder::Encoder::new(&mut out, 90);
    setup(&mut encoder);
    encoder.encode(&test_picture(width, height), width, height, jpeg_encoder::ColorType::Rgb).unwrap();
    out
}

#[test]
fn jpegs_of_every_flavour_are_decoded_or_refused_never_misread() {
    use jpeg_encoder::SamplingFactor;
    // The picture must come out the right way round at every scale.
    let check = |data: &[u8], what: &str| {
        for (w, h) in [(256, 192), (64, 48), (1, 1), (4000, 3000)] {
            let image = decode_jpeg(data, w, h).unwrap_or_else(|e| panic!("{what} at {w}x{h}: {e}"));
            assert!(image.width >= 1 && image.height >= 1);
            assert_eq!(image.rgb.len(), (image.width * image.height * 3) as usize, "{what}");
            let at = |x: u32, y: u32| {
                let i = ((y * image.width + x) * 3) as usize;
                [image.rgb[i] as i32, image.rgb[i + 1] as i32, image.rgb[i + 2] as i32]
            };
            if image.width >= 8 && image.height >= 8 {
                let (l, r) = (image.width / 4, image.width * 3 / 4);
                let (t, b) = (image.height / 8, image.height * 7 / 8);
                assert!(at(l, b)[0] > at(l, b)[2] + 60, "{what}: left is red, got {:?}", at(l, b));
                assert!(at(r, b)[2] > at(r, b)[0] + 60, "{what}: right is blue, got {:?}", at(r, b));
                assert!(at(l, b)[0] > at(l, t)[0] + 60, "{what}: bottom is brighter, {:?} over {:?}", at(l, t), at(l, b));
            }
        }
    };
    let subsamplings = [
        (SamplingFactor::R_4_4_4, "4:4:4"),
        (SamplingFactor::R_4_2_2, "4:2:2"),
        (SamplingFactor::R_4_2_0, "4:2:0"),
        (SamplingFactor::R_4_4_0, "4:4:0"),
        (SamplingFactor::R_4_1_1, "4:1:1"),
    ];
    for (w, h) in [(640u16, 480u16), (1920, 1440), (641, 479), (17, 13), (8, 8), (1, 1), (3, 500), (500, 3)] {
        for (sampling, name) in subsamplings {
            for progressive in [false, true] {
                for restart in [0u16, 7] {
                    let data = encode_with(w, h, |e| {
                        e.set_sampling_factor(sampling);
                        e.set_progressive(progressive);
                        if restart > 0 {
                            e.set_restart_interval(restart);
                        }
                    });
                    let what = format!("{w}x{h} {name} progressive={progressive} restart={restart}");
                    if w >= 16 && h >= 16 {
                        check(&data, &what);
                    } else {
                        let image = decode_jpeg(&data, 256, 192).unwrap_or_else(|e| panic!("{what}: {e}"));
                        assert_eq!((image.width, image.height), (w as u32, h as u32), "{what}");
                    }
                }
            }
        }
    }

    // EXIF orientation: the image is in the sensor's orientation whatever
    // the tag says; the geometry depends on it not being turned.
    for orientation in [1u8, 3, 6, 8] {
        let mut exif = b"Exif\0\0MM\0\x2A\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01\0".to_vec();
        exif.extend_from_slice(&[orientation, 0, 0, 0, 0, 0, 0]);
        let data = encode_with(640, 480, |e| e.add_app_segment(1, exif.clone()).unwrap());
        check(&data, &format!("EXIF orientation {orientation}"));
        let image = decode_jpeg(&data, 640, 480).unwrap();
        assert_eq!((image.width, image.height), (640, 480), "an EXIF orientation must not turn the image");
    }
    // An ICC profile and other application segments are skipped.
    let data = encode_with(320, 240, |e| {
        e.add_icc_profile(&vec![7u8; 70_000]).unwrap();
        e.add_app_segment(13, b"Photoshop 3.0\0".to_vec()).unwrap();
    });
    check(&data, "ICC profile");

    // Grey: every channel the same.
    let grey: Vec<u8> = (0..480u32).flat_map(|y| (0..640u32).map(move |x| ((x + y) % 256) as u8)).collect();
    let mut data = Vec::new();
    jpeg_encoder::Encoder::new(&mut data, 90).encode(&grey, 640, 480, jpeg_encoder::ColorType::Luma).unwrap();
    let image = decode_jpeg(&data, 256, 192).unwrap();
    assert!(image.rgb.chunks_exact(3).all(|p| p[0] == p[1] && p[1] == p[2]));
    assert_eq!(image.rgb.len(), (image.width * image.height * 3) as usize);

    // CMYK and YCCK have no plain colour meaning here: refused, and a frame
    // that carries one is acknowledged as not used.
    for kind in [jpeg_encoder::ColorType::Cmyk, jpeg_encoder::ColorType::CmykAsYcck] {
        let cmyk = vec![90u8; 64 * 48 * 4];
        let mut data = Vec::new();
        jpeg_encoder::Encoder::new(&mut data, 90).encode(&cmyk, 64, 48, kind).unwrap();
        assert_eq!(decode_jpeg(&data, 64, 48), Err(ImageError::Unsupported));
        let mut raw = wall_frame((64, 48), (128, 96));
        raw.color = data;
        let mut capture = session(CaptureMode::Room);
        assert_eq!(capture.handle_frame(&raw.bytes()).unwrap().frame.unwrap().skipped, Some(SkipReason::Color));
    }
}

/// A baseline JPEG header that announces `width` x `height` and nothing
/// behind it.
fn jpeg_header_only(width: u16, height: u16, components: u8) -> Vec<u8> {
    let mut out = vec![0xFF, 0xD8, 0xFF, 0xC0];
    out.extend_from_slice(&(8 + 3 * components as u16).to_be_bytes());
    out.push(8);
    out.extend_from_slice(&height.to_be_bytes());
    out.extend_from_slice(&width.to_be_bytes());
    out.push(components);
    for c in 0..components {
        out.extend_from_slice(&[c + 1, 0x11, 0]);
    }
    out
}

#[test]
fn hostile_jpegs_are_refused_quickly() {
    // Sizes that would need gigabytes are refused from the header.
    for (w, h) in [(65535u16, 65535u16), (65535, 1), (1, 65535), (4096, 4096), (0, 480), (640, 0)] {
        let data = jpeg_header_only(w, h, 3);
        let start = Instant::now();
        let outcome = decode_jpeg(&data, 256, 192);
        assert!(outcome.is_err(), "{w}x{h} was decoded");
        if w as u64 * h as u64 > 1 << 23 {
            assert!(matches!(outcome, Err(ImageError::Size { .. })), "{w}x{h}: {outcome:?}");
        }
        assert!(start.elapsed().as_secs_f64() < 0.2, "{w}x{h}: {:?}", start.elapsed());
    }
    // Every start-of-frame marker there is (extended, lossless, arithmetic,
    // hierarchical), with a scan of noise behind it: an error or a picture,
    // never a panic.
    let mut rng = Rng(7);
    for marker in (0xC0u8..=0xCF).filter(|m| ![0xC4, 0xC8, 0xCC].contains(m)) {
        for components in [1u8, 3, 4] {
            let mut data = jpeg_header_only(64, 48, components);
            data[3] = marker;
            data.extend_from_slice(&[0xFF, 0xC4, 0, 20, 0x00, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
            data.extend_from_slice(&[0xFF, 0xDA, 0, 6 + 2 * components, components]);
            for c in 0..components {
                data.extend_from_slice(&[c + 1, 0]);
            }
            data.extend_from_slice(&[0, 63, 0]);
            data.extend((0..400).map(|_| (rng.next_u64() % 255) as u8));
            data.extend_from_slice(&[0xFF, 0xD9]);
            if let Ok(image) = decode_jpeg(&data, 64, 48) {
                assert_eq!(image.rgb.len(), (image.width * image.height * 3) as usize, "marker {marker:#x}");
            }
        }
    }
    // Truncations and corruptions of real JPEGs, baseline and progressive:
    // an error or a picture of the announced size, in bounded time.
    let mut rng = Rng(0xBAD_1DEA);
    let seeds = [
        encode_with(320, 240, |_| {}),
        encode_with(320, 240, |e| e.set_progressive(true)),
        encode_with(161, 119, |e| e.set_restart_interval(3)),
    ];
    let (mut ok, mut failed, mut worst) = (0, 0, 0.0f64);
    for round in 0..rounds(3_000) {
        let mut data = seeds[round % seeds.len()].clone();
        for _ in 0..1 + rng.next_u64() % 4 {
            mutate(&mut rng, &mut data);
        }
        let start = Instant::now();
        match decode_jpeg(&data, 64, 48) {
            Ok(image) => {
                ok += 1;
                assert_eq!(image.rgb.len(), (image.width * image.height * 3) as usize);
                assert!(image.width as u64 * image.height as u64 <= 1 << 23);
            }
            Err(_) => failed += 1,
        }
        worst = worst.max(start.elapsed().as_secs_f64());
    }
    println!("{ok} mutated JPEGs decoded, {failed} refused, slowest {:.1} ms", worst * 1e3);
    assert!(ok > 300 && failed > 300);
    assert!(worst < 2.0, "a mutated JPEG of a few kB took {worst:.2} s");
}

/// A progressive grey JPEG of `side` x `side` pixels written by hand: one
/// DC scan, then `scans` AC scans of 25 bytes each, every one of them a
/// handful of end-of-band runs that span the whole picture.
fn scan_bomb(side: u16, scans: usize) -> Vec<u8> {
    let mut out = vec![0xFF, 0xD8];
    // DQT: table 0, all ones.
    out.extend_from_slice(&[0xFF, 0xDB, 0, 67, 0]);
    out.extend_from_slice(&[1; 64]);
    // SOF2: progressive, 8 bits, one component, sampling 1x1.
    out.extend_from_slice(&[0xFF, 0xC2, 0, 11, 8]);
    out.extend_from_slice(&side.to_be_bytes());
    out.extend_from_slice(&side.to_be_bytes());
    out.extend_from_slice(&[1, 1, 0x11, 0]);
    // DHT: DC table 0 with the single code `0` for "no difference"; AC
    // table 0 with the single code `0` for a run of 2^14 empty blocks
    // (symbol 0xE0, followed by 14 bits that add to the run).
    for (class, symbol) in [(0x00u8, 0x00u8), (0x10, 0xE0)] {
        out.extend_from_slice(&[0xFF, 0xC4, 0, 20, class, 1]);
        out.extend_from_slice(&[0; 15]);
        out.push(symbol);
    }
    let blocks = (side as usize).div_ceil(8).pow(2);
    // DC scan: one zero bit per block.
    out.extend_from_slice(&[0xFF, 0xDA, 0, 8, 1, 1, 0x00, 0, 0, 0]);
    out.extend(std::iter::repeat_n(0u8, blocks.div_ceil(8)));
    // AC scans: 15 zero bits are one run of 16384 blocks.
    let runs = blocks.div_ceil(16384);
    for _ in 0..scans {
        out.extend_from_slice(&[0xFF, 0xDA, 0, 8, 1, 1, 0x00, 1, 63, 0]);
        out.extend(std::iter::repeat_n(0u8, (runs * 15).div_ceil(8)));
    }
    out.extend_from_slice(&[0xFF, 0xD9]);
    out
}

#[test]
fn a_progressive_jpeg_with_thousands_of_scans_does_not_stall_the_ingest() {
    // Every scan of a progressive JPEG is a pass over the whole picture,
    // and a scan that says "nothing here" is a few bytes: the largest
    // picture the ingest accepts with as many scans as fit into one
    // WebSocket message would keep a decoder that does not count them busy
    // for minutes.
    let few = scan_bomb(2880, 8);
    let image = decode_jpeg(&few, 64, 48).expect("the hand-made progressive JPEG is valid");
    assert!(image.rgb.iter().all(|&v| v == 128), "an empty picture is mid-grey");

    let scans = (gs_core::ingest::MAX_INGEST_MESSAGE_BYTES - (1 << 16)) / 25;
    let bomb = scan_bomb(2880, scans);
    assert!(bomb.len() < gs_core::ingest::MAX_INGEST_MESSAGE_BYTES);
    let mut raw = wall_frame((64, 48), (128, 96));
    raw.color = bomb;
    let bytes = raw.bytes();
    let mut capture = session(CaptureMode::Room);
    let start = Instant::now();
    let reply = capture.handle_frame(&bytes).unwrap();
    let seconds = start.elapsed().as_secs_f64();
    println!("{scans} scans in {} bytes: {seconds:.2} s, {:?}", bytes.len(), reply.frame.unwrap().skipped);
    assert!(seconds < 3.0, "one frame of {} bytes kept the ingest busy for {seconds:.1} s", bytes.len());
    assert_eq!(reply.frame.unwrap().skipped, Some(SkipReason::Color));
}
