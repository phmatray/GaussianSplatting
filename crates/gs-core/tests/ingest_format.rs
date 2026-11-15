//! The capture frame, the `.gscap` file, the stream messages and the pairing
//! URI against the fixtures shared with the Python reference and the phone
//! (`ios/SplatCapture/Fixtures`, described by its README), cross-checks
//! with `tools/gscap.py` in both directions, and decoding under truncation
//! and corruption.

use std::path::{Path, PathBuf};
use std::process::Command;

use gs_core::ingest::file::{encode_file_header, split_gscap, GscapParser, GscapWriter, ParsedItem, HEADER_PAD};
use gs_core::ingest::frame::{
    decode_frame, depth_meters_to_mm, encode_frame, CaptureFrame, ErrorCode, FrameHeader, Orientation, Tracking,
};
use gs_core::ingest::message::{ClientMessage, Hello, IngestErrorCode, ServerMessage};
use gs_core::ingest::pairing::{normalise_fingerprint, parse_pairing_uri};
use gs_core::ingest::{import_gscap, CaptureRegion, CaptureSession, GscapHeader, IngestParams};
use gs_core::protocol::CaptureMode;
use serde_json::Value;
use sha2::{Digest, Sha256};

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn fixtures() -> PathBuf {
    repo().join("ios/SplatCapture/Fixtures")
}

fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn json(path: &Path) -> Value {
    serde_json::from_slice(&read(path)).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn sha256(data: &[u8]) -> String {
    Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

fn depth_bytes(frame: &CaptureFrame) -> Vec<u8> {
    frame.depth_mm.iter().flat_map(|mm| mm.to_le_bytes()).collect()
}

/// Every `NAME.bin` of `frames/` with its sidecar.
fn frame_fixtures() -> Vec<(String, Vec<u8>, Value)> {
    let dir = fixtures().join("frames");
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "bin") {
            let name = path.file_stem().unwrap().to_string_lossy().into_owned();
            out.push((name, read(&path), json(&path.with_extension("json"))));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    assert!(out.len() >= 40, "only {} frame fixtures found", out.len());
    out
}

fn hex_bits(values: &Value) -> Vec<String> {
    values.as_array().unwrap().iter().map(|v| v.as_str().unwrap().to_string()).collect()
}

#[test]
fn valid_frame_fixtures_decode_to_their_sidecars() {
    let mut valid = 0;
    for (name, bytes, side) in frame_fixtures().into_iter().filter(|(_, _, side)| side["valid"] == true) {
        valid += 1;
        let frame = decode_frame(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
        let header = FrameHeader::parse(&bytes).unwrap();
        assert_eq!(bytes.len() as u64, side["length"].as_u64().unwrap(), "{name}");
        assert_eq!(header.total_length(), bytes.len() as u64, "{name}");
        assert_eq!(frame.flags() as u64, side["flags"].as_u64().unwrap(), "{name}: flags");
        assert_eq!(header.flags, frame.flags(), "{name}");
        assert_eq!(frame.compressed, side["compressed"].as_bool().unwrap(), "{name}");
        assert_eq!(frame.tracking as u64, side["tracking"].as_u64().unwrap(), "{name}: tracking");
        assert_eq!(frame.orientation as u64, side["orientation"].as_u64().unwrap(), "{name}: orientation");
        assert_eq!(frame.index as u64, side["index"].as_u64().unwrap(), "{name}: index");
        assert_eq!(frame.reserved as u64, side["reserved"].as_u64().unwrap(), "{name}: reserved");
        // Bit patterns are exact, -0.0 and subnormals included.
        assert_eq!(format!("{:016x}", frame.timestamp.to_bits()), side["timestamp_bits"].as_str().unwrap(), "{name}");
        let bits = |values: &[f32]| values.iter().map(|v| format!("{:08x}", v.to_bits())).collect::<Vec<_>>();
        assert_eq!(bits(&frame.world_from_cam), hex_bits(&side["world_from_cam_bits"]), "{name}: pose");
        assert_eq!(bits(&frame.intrinsics), hex_bits(&side["intrinsics_bits"]), "{name}: intrinsics");

        let color = &side["color"];
        assert_eq!(frame.color_width as u64, color["width"].as_u64().unwrap(), "{name}");
        assert_eq!(frame.color_height as u64, color["height"].as_u64().unwrap(), "{name}");
        assert_eq!(frame.color.len() as u64, color["length"].as_u64().unwrap(), "{name}");
        assert_eq!(sha256(&frame.color), color["sha256"].as_str().unwrap(), "{name}: colour bytes");

        let depth = &side["depth"];
        assert_eq!(frame.depth_width as u64, depth["width"].as_u64().unwrap(), "{name}");
        assert_eq!(frame.depth_height as u64, depth["height"].as_u64().unwrap(), "{name}");
        assert_eq!(sha256(&depth_bytes(&frame)), depth["sha256"].as_str().unwrap(), "{name}: depth plane");
        if let Some(values) = depth["values"].as_array() {
            let values: Vec<u16> = values.iter().map(|v| v.as_u64().unwrap() as u16).collect();
            assert_eq!(frame.depth_mm, values, "{name}: depth values");
        }
        for (plane, key) in [(&frame.confidence, "confidence"), (&frame.mask, "mask")] {
            match (plane, side[key].is_null()) {
                (None, true) => {}
                (Some(plane), false) => {
                    assert_eq!(sha256(plane), side[key]["sha256"].as_str().unwrap(), "{name}: {key}");
                    assert_eq!(plane.len(), frame.depth_pixels(), "{name}: {key}");
                    if let Some(values) = side[key]["values"].as_array() {
                        let values: Vec<u8> = values.iter().map(|v| v.as_u64().unwrap() as u8).collect();
                        assert_eq!(plane, &values, "{name}: {key} values");
                    }
                }
                _ => panic!("{name}: {key} plane present in one and absent in the other"),
            }
        }
        let stored = &side["stored_lengths"];
        assert_eq!(header.color_length as u64, stored["color_length"].as_u64().unwrap(), "{name}");
        assert_eq!(header.depth_length as u64, stored["depth_length"].as_u64().unwrap(), "{name}");
        assert_eq!(header.confidence_length as u64, stored["confidence_length"].as_u64().unwrap(), "{name}");
        assert_eq!(header.mask_length as u64, stored["mask_length"].as_u64().unwrap(), "{name}");

        // Encoding again: the same bytes where the sidecar promises it, the
        // same content everywhere (deflate streams differ between zlibs).
        let again = encode_frame(&frame).unwrap();
        if side["reencode_identical"] == true {
            assert_eq!(again, bytes, "{name}: re-encoding changed the bytes");
        }
        assert_eq!(decode_frame(&again).unwrap(), frame, "{name}: round trip");
    }
    assert_eq!(valid, 11, "the README lists eleven valid frames");
}

#[test]
fn invalid_frame_fixtures_fail_with_their_code() {
    let mut invalid = 0;
    for (name, bytes, side) in frame_fixtures().into_iter().filter(|(_, _, side)| side["valid"] == false) {
        invalid += 1;
        assert!(name.starts_with("bad_"), "{name}");
        let expected = side["error"].as_str().unwrap();
        let error = decode_frame(&bytes).expect_err(&name);
        assert_eq!(error.code.as_str(), expected, "{name}: {error}");
        // A session answers such a frame with `bad_frame` and survives it.
        let mut session = CaptureSession::new(&Hello::offline(CaptureMode::Room, "t", "d", "a"), IngestParams::default());
        let refused = session.handle_frame(&bytes).expect_err(&name);
        assert_eq!(refused.code, IngestErrorCode::BadFrame, "{name}");
        assert!(refused.detail.starts_with(expected), "{name}: {}", refused.detail);
        assert_eq!(session.frames_received(), 0);
    }
    assert!(invalid >= 29, "only {invalid} invalid frames");
}

#[test]
fn the_traps_of_the_readme_cost_nothing() {
    // Each of these announces far more than it holds; decoding must fail
    // from the header, in no time and without reserving what is announced.
    let dir = fixtures().join("frames");
    for name in ["bad_length_wraps_u32", "bad_length_overflow", "bad_depth_huge", "bad_deflate_long", "bad_deflate_mask_long"] {
        let bytes = read(&dir.join(format!("{name}.bin")));
        let start = std::time::Instant::now();
        for _ in 0..200 {
            assert!(decode_frame(&bytes).is_err(), "{name}");
        }
        assert!(start.elapsed().as_secs_f64() < 2.0, "{name}: 200 refusals took {:?}", start.elapsed());
    }
    // 65535 x 65535 deflated pixels are refused by the header alone.
    let huge = read(&dir.join("bad_depth_huge.bin"));
    assert_eq!(FrameHeader::parse(&huge).unwrap_err().code, ErrorCode::BadDimensions);
    // The sum of four lengths is taken in 64 bits.
    let wraps = read(&dir.join("bad_length_wraps_u32.bin"));
    let header = FrameHeader::parse(&wraps).unwrap();
    assert!(header.total_length() > u32::MAX as u64);
    assert_eq!((header.total_length() as u32) as usize, wraps.len(), "the trap: in 32 bits the sum is the real size");
}

/// A decoded frame agrees with itself: plane sizes follow the depth size
/// and it survives a round trip.
fn assert_consistent(frame: &CaptureFrame, what: &str) {
    let n = frame.depth_width as usize * frame.depth_height as usize;
    assert!(n >= 1, "{what}");
    assert_eq!(frame.depth_mm.len(), n, "{what}");
    assert!(frame.confidence.as_ref().is_none_or(|c| c.len() == n), "{what}");
    assert!(frame.mask.as_ref().is_none_or(|m| m.len() == n), "{what}");
    let again = decode_frame(&encode_frame(frame).unwrap()).unwrap();
    // Compared through the bytes: a corrupted float may be NaN.
    assert_eq!(encode_frame(&again).unwrap().len(), encode_frame(frame).unwrap().len(), "{what}");
    assert_eq!(again.depth_mm, frame.depth_mm, "{what}");
}

#[test]
fn truncated_and_corrupted_frames_decode_consistently_or_fail() {
    let dir = fixtures().join("frames");
    let hello = Hello::offline(CaptureMode::Room, "t", "d", "a");
    for (name, flips) in [
        ("frame_plain", &[0x01u8, 0x80, 0xFF, 0x55][..]),
        ("frame_conf_mask", &[0x01, 0x80, 0xFF, 0x55]),
        ("frame_deflate_conf_mask", &[0x01, 0x80, 0xFF, 0x55]),
        ("frame_extreme", &[0x01, 0x80, 0xFF, 0x55]),
        ("frame_lidar", &[0xFF]),
    ] {
        let bytes = read(&dir.join(format!("{name}.bin")));
        // Every truncation is refused (the size is part of the format).
        for cut in 0..bytes.len() {
            let error = decode_frame(&bytes[..cut]).expect_err("a truncated frame decoded");
            if cut < 128 {
                assert_eq!(error.code, ErrorCode::TooShort, "{name} cut at {cut}");
            }
        }
        // And every extension.
        let mut longer = bytes.clone();
        longer.push(0);
        assert_eq!(decode_frame(&longer).unwrap_err().code, ErrorCode::BadLength, "{name}");

        // Single-byte corruption: an error or a frame that is self-consistent,
        // and a session that swallows it (the header of the lidar frame only:
        // its planes are large and their corruption is covered by the others).
        let mut session = CaptureSession::new(&hello, IngestParams::default());
        let (mut decoded, mut refused) = (0u32, 0u32);
        let positions = if name == "frame_lidar" { 0..128 } else { 0..bytes.len() };
        for at in positions {
            for &flip in flips {
                let mut bad = bytes.clone();
                bad[at] ^= flip;
                match decode_frame(&bad) {
                    Ok(frame) => {
                        decoded += 1;
                        assert_consistent(&frame, &format!("{name}: byte {at} ^ {flip:#x}"));
                        // Whatever the pose, the intrinsics or the JPEG have
                        // become: used or not, never a panic.
                        let reply = session.handle_frame(&bad).expect("a frame that decodes is not a protocol error");
                        assert_eq!(reply.to_phone.len(), 1);
                    }
                    Err(_) => refused += 1,
                }
            }
        }
        assert!(decoded > 0 && refused > 0, "{name}: {decoded} decoded, {refused} refused");
        // The map never holds anything that is not a number.
        assert!(session.static_splats().iter().all(|s| s.pos.iter().chain(&s.scale).chain(&s.rot).all(|v| v.is_finite())));
    }
}

#[test]
fn corrupted_deflate_streams_of_a_full_size_frame_never_escape_their_plane() {
    // The lidar frame: 256x192 planes in about 3 kB of deflate. Corrupt the
    // payload at a spread of positions: a wrong stream must fail or give
    // planes of exactly the right size.
    let bytes = read(&fixtures().join("frames/frame_lidar.bin"));
    let (mut decoded, mut refused) = (0, 0);
    for at in (128..bytes.len()).step_by(7) {
        for flip in [0x01u8, 0x10, 0xFF] {
            let mut bad = bytes.clone();
            bad[at] ^= flip;
            match decode_frame(&bad) {
                Ok(frame) => {
                    decoded += 1;
                    assert_eq!(frame.depth_mm.len(), 256 * 192);
                    assert!(frame.confidence.as_ref().is_none_or(|c| c.len() == 256 * 192));
                    assert!(frame.mask.as_ref().is_none_or(|m| m.len() == 256 * 192));
                }
                Err(e) => {
                    refused += 1;
                    assert_eq!(e.code, ErrorCode::BadDeflate, "byte {at}: {e}");
                }
            }
        }
    }
    assert!(refused > 100, "{decoded} decoded, {refused} refused");
}

#[test]
fn depth_conversion_matches_the_phone() {
    let cases = json(&fixtures().join("depth_conversion.json"));
    let cases = cases.as_array().unwrap();
    assert!(cases.len() >= 20);
    for case in cases {
        let bits = u32::from_str_radix(case["bits"].as_str().unwrap(), 16).unwrap();
        let mm = depth_meters_to_mm(f32::from_bits(bits));
        assert_eq!(mm as u64, case["mm"].as_u64().unwrap(), "{} m ({bits:08x})", case["meters"]);
    }
}

fn header_matches(header: &GscapHeader, expected: &Value) {
    let text = |key: &str| expected.get(key).and_then(Value::as_str).unwrap_or_default();
    assert_eq!(header.device, text("device"));
    assert_eq!(header.app, text("app"));
    assert_eq!(header.mode, text("mode"));
    assert_eq!(header.name, text("name"));
    assert_eq!(header.created, text("created"));
    assert_eq!(header.protocol.map(u64::from), expected["protocol"].as_u64());
    match expected.get("region") {
        Some(region) => {
            let center: Vec<f32> = region["center"].as_array().unwrap().iter().map(|v| v.as_f64().unwrap() as f32).collect();
            let radius = region["radius"].as_f64().unwrap() as f32;
            assert_eq!(header.region, Some(CaptureRegion { center: [center[0], center[1], center[2]], radius }));
        }
        None => assert_eq!(header.region, None),
    }
    assert_eq!(header.floor, expected.get("floor").map(|f| f["y"].as_f64().unwrap() as f32));
}

#[test]
fn gscap_fixtures_read_whole_and_in_pieces() {
    for name in ["three_frames", "padded_header", "truncated", "truncated_length", "empty"] {
        let bytes = read(&fixtures().join(format!("{name}.gscap")));
        let side = json(&fixtures().join(format!("{name}.json")));
        assert_eq!(bytes.len() as u64, side["file_length"].as_u64().unwrap(), "{name}");
        let expected_frames = side["frames"].as_array().unwrap();
        let truncated = side["truncated"].as_bool().unwrap();

        let whole = split_gscap(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
        header_matches(&whole.header, &side["header"]);
        assert_eq!(whole.header_json_length as u64, side["header_json_length"].as_u64().unwrap(), "{name}");
        assert_eq!(whole.truncated, truncated, "{name}");
        assert_eq!(whole.frames.len(), expected_frames.len(), "{name}");
        for (frame, expected) in whole.frames.iter().zip(expected_frames) {
            assert_eq!(frame.len() as u64, expected["length"].as_u64().unwrap(), "{name}");
            assert_eq!(sha256(frame), expected["sha256"].as_str().unwrap(), "{name}");
            assert_eq!(decode_frame(frame).unwrap().index as u64, expected["index"].as_u64().unwrap(), "{name}");
        }

        // The push parser, fed byte by byte and in odd pieces.
        for chunk in [1, 5, 100, 4096] {
            let mut parser = GscapParser::new();
            let mut frames: Vec<Vec<u8>> = Vec::new();
            let mut header = None;
            for piece in bytes.chunks(chunk) {
                parser.push(piece);
                while let Some(item) = parser.next_item().unwrap() {
                    match item {
                        ParsedItem::Header(h) => header = Some(h),
                        ParsedItem::Frame(frame) => frames.push(frame.to_vec()),
                    }
                }
            }
            let end = parser.finish().unwrap();
            assert_eq!(header.as_ref(), Some(&whole.header), "{name}");
            assert_eq!((end.frames, end.truncated, end.trailing_bytes), (frames.len() as u64, truncated, whole.trailing_bytes), "{name}");
            assert_eq!(frames.iter().map(Vec::as_slice).collect::<Vec<_>>(), whole.frames, "{name}, pieces of {chunk}");
            assert_eq!(parser.header_json_length(), whole.header_json_length);
        }

        // Imported as a capture: the header's mode, name, region and floor
        // arrive in the session.
        let (session, summary) = import_gscap(&bytes, IngestParams::default()).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!((summary.frames, summary.truncated), (expected_frames.len() as u64, truncated), "{name}");
        assert_eq!(summary.name, side["header"]["name"].as_str().unwrap());
        assert_eq!(summary.bytes, bytes.len() as u64);
        assert_eq!(session.mode().name(), side["header"]["mode"].as_str().unwrap());
        assert!(session.is_finished());
        assert_eq!(session.frames_received() as usize, expected_frames.len());
        if let Some(floor) = whole.header.floor {
            assert_eq!(session.floor(), (floor, true));
        }
        if session.mode().uses_region() {
            assert_eq!(session.region(), whole.header.region);
        }
    }
    // The header of `three_frames` written by this implementation is the
    // reference's, byte for byte.
    let bytes = read(&fixtures().join("three_frames.gscap"));
    let whole = split_gscap(&bytes).unwrap();
    assert_eq!(encode_file_header(&whole.header, 0).unwrap(), bytes[..12 + whole.header_json_length]);
    let padded = read(&fixtures().join("padded_header.gscap"));
    let whole = split_gscap(&padded).unwrap();
    assert_eq!(whole.header_json_length, HEADER_PAD);
    assert_eq!(encode_file_header(&whole.header, HEADER_PAD).unwrap(), padded[..12 + HEADER_PAD]);
    // And so is the whole file.
    let mut writer = GscapWriter::new(Vec::new(), &whole.header, HEADER_PAD).unwrap();
    for frame in &whole.frames {
        writer.append(frame).unwrap();
    }
    assert_eq!(writer.into_inner(), padded);
}

#[test]
fn invalid_gscap_fixtures_fail_with_their_code() {
    let list = json(&fixtures().join("invalid_files.json"));
    let list = list.as_array().unwrap();
    assert!(list.len() >= 9);
    for case in list {
        let name = case["file"].as_str().unwrap();
        let expected = case["error"].as_str().unwrap();
        let bytes = read(&fixtures().join(name));
        assert_eq!(split_gscap(&bytes).unwrap_err().code.as_str(), expected, "{name}");
        assert_eq!(import_gscap(&bytes, IngestParams::default()).unwrap_err().code.as_str(), expected, "{name}");
        for chunk in [1, 3, 64, 4096] {
            let mut parser = GscapParser::new();
            let mut outcome = Ok(());
            for piece in bytes.chunks(chunk) {
                parser.push(piece);
                loop {
                    match parser.next_item() {
                        Ok(Some(_)) => {}
                        Ok(None) => break,
                        Err(e) => {
                            outcome = Err(e);
                            break;
                        }
                    }
                }
                assert!(parser.buffered() <= bytes.len(), "{name}: the parser buffers more than the file holds");
            }
            let error = outcome.and_then(|()| parser.finish().map(|_| ())).unwrap_err();
            assert_eq!(error.code.as_str(), expected, "{name}, pieces of {chunk}");
        }
    }
    // A file whose records are not frames is damaged, not cut short.
    let mut bytes = read(&fixtures().join("empty.gscap"));
    bytes.extend_from_slice(&200u32.to_le_bytes());
    bytes.extend_from_slice(&[0u8; 200]);
    assert!(split_gscap(&bytes).is_ok(), "the container itself is fine");
    assert_eq!(import_gscap(&bytes, IngestParams::default()).unwrap_err().code, ErrorCode::BadMagic);
    // A header that names no mode cannot be imported.
    let nameless = encode_file_header(&GscapHeader { mode: "fisheye".into(), ..GscapHeader::default() }, 0).unwrap();
    assert_eq!(import_gscap(&nameless, IngestParams::default()).unwrap_err().code, ErrorCode::BadHeader);
    assert_eq!(import_gscap(&[], IngestParams::default()).unwrap_err().code, ErrorCode::BadFileMagic);
}

#[test]
fn messages_match_the_fixtures_in_both_directions() {
    let messages = json(&fixtures().join("messages.json"));
    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    // What the phone sends: built here it is the same JSON, and the server
    // side reads it.
    let client = messages["client"].as_array().unwrap();
    assert!(client.len() >= 8);
    for case in client {
        let (build, args, expected) = (case["build"].as_str().unwrap(), &case["args"], &case["json"]);
        let text = serde_json::to_string(expected).unwrap();
        let number = |v: &Value| v.as_f64().unwrap() as f32;
        let (built, parsed) = match build {
            "hello" => {
                let mode = CaptureMode::from_name(args["mode"].as_str().unwrap()).unwrap();
                let hello = Hello {
                    token: args["token"].as_str().unwrap().into(),
                    ..Hello::offline(mode, args["name"].as_str().unwrap(), args["device"].as_str().unwrap(), args["app"].as_str().unwrap())
                };
                let parsed = Hello::parse(&text, TOKEN).unwrap();
                assert_eq!(Hello::parse(&text, "ffffffffffffffffffffffffffffffff").unwrap_err().code, IngestErrorCode::BadToken);
                (ClientMessage::Hello(hello), ClientMessage::Hello(parsed))
            }
            "region" => {
                let c = args["center"].as_array().unwrap();
                let region = CaptureRegion { center: [number(&c[0]), number(&c[1]), number(&c[2])], radius: number(&args["radius"]) };
                (ClientMessage::Region(region), ClientMessage::parse(&text).unwrap())
            }
            "floor" => (ClientMessage::Floor { y: number(&args["y"]) }, ClientMessage::parse(&text).unwrap()),
            "pause" => (ClientMessage::Pause, ClientMessage::parse(&text).unwrap()),
            "resume" => (ClientMessage::Resume, ClientMessage::parse(&text).unwrap()),
            "reset" => (ClientMessage::Reset, ClientMessage::parse(&text).unwrap()),
            "finish" => (ClientMessage::Finish, ClientMessage::parse(&text).unwrap()),
            other => panic!("unknown client message {other}"),
        };
        assert_eq!(built, parsed, "{build}");
        assert_eq!(&serde_json::from_str::<Value>(&built.to_json()).unwrap(), expected, "{build}");
    }

    // What the server sends, as the phone understands it.
    let server = messages["server"].as_array().unwrap();
    assert!(server.len() >= 26);
    for case in server {
        let text = case["text"].as_str().unwrap();
        let expect = &case["expect"];
        let parsed = ServerMessage::parse(text);
        if expect.is_null() {
            assert_eq!(parsed, None, "{text}");
            continue;
        }
        let u = |key: &str| expect[key].as_u64().unwrap();
        let expected = match expect["kind"].as_str().unwrap() {
            "welcome" => Ok(ServerMessage::Welcome { protocol: u("protocol") as u32, max_inflight: u("max_inflight") as u32 }),
            "ack" => Ok(ServerMessage::Ack { frame: u("frame") as u32, used: expect["used"].as_bool().unwrap() }),
            "progress" => Ok(ServerMessage::Progress { frames: u("frames"), splats: u("splats") }),
            "error" => Ok(ServerMessage::Error {
                code: expect["code"].as_str().unwrap().into(),
                message: expect["message"].as_str().unwrap().into(),
            }),
            "unknown" => Err(expect["type"].as_str().unwrap().to_string()),
            other => panic!("unknown kind {other}"),
        };
        assert_eq!(parsed, Some(expected.clone()), "{text}");
        // What this server writes reads back the same.
        if let Ok(message) = expected {
            assert_eq!(ServerMessage::parse(&message.to_json()), Some(Ok(message)));
        }
    }
    // The canonical texts of the specification, byte for byte.
    assert_eq!(ServerMessage::welcome(4).to_json(), server[0]["text"].as_str().unwrap());
    let texts: Vec<&str> = server.iter().map(|c| c["text"].as_str().unwrap()).collect();
    for message in [
        ServerMessage::Ack { frame: 0, used: true },
        ServerMessage::Ack { frame: 17, used: false },
        ServerMessage::Ack { frame: u32::MAX, used: true },
        ServerMessage::Progress { frames: 120, splats: 654_321 },
        ServerMessage::Error { code: "bad_token".into(), message: "wrong pairing token".into() },
    ] {
        assert!(texts.contains(&message.to_json().as_str()), "{} is not written as in the fixtures", message.to_json());
    }
}

#[test]
fn pairing_uris_match_the_fixtures() {
    let cases = json(&fixtures().join("pairing_uris.json"));
    let cases = cases.as_array().unwrap();
    assert!(cases.len() >= 54);
    for case in cases {
        let uri = case["uri"].as_str().unwrap();
        let parsed = parse_pairing_uri(uri);
        if case["ok"] == true {
            let pairing = parsed.unwrap_or_else(|e| panic!("{uri}: {e}"));
            assert_eq!(pairing.host, case["host"].as_str().unwrap(), "{uri}");
            assert_eq!(pairing.port as u64, case["port"].as_u64().unwrap(), "{uri}");
            assert_eq!(pairing.fingerprint, case["fp"].as_str().unwrap(), "{uri}");
            assert_eq!(pairing.token, case["token"].as_str().unwrap(), "{uri}");
            assert_eq!(pairing.ingest_url(), case["ingest_url"].as_str().unwrap(), "{uri}");
            assert_eq!(pairing.captures_url(), case["captures_url"].as_str().unwrap(), "{uri}");
            // The canonical form parses to the same pairing.
            assert_eq!(parse_pairing_uri(&pairing.uri()).unwrap(), pairing, "{uri}");
        } else {
            assert_eq!(parsed.unwrap_err().as_str(), case["error"].as_str().unwrap(), "{uri} ({})", case["note"]);
        }
    }
    // The first case is the canonical form, as /api/pair answers it.
    let first = &cases[0];
    let built = gs_core::ingest::pairing_uri(
        first["host"].as_str().unwrap(),
        first["port"].as_u64().unwrap() as u16,
        first["fp"].as_str().unwrap(),
        first["token"].as_str().unwrap(),
    );
    assert_eq!(built.unwrap(), first["uri"].as_str().unwrap());

    // The fingerprint is the SHA-256 of the certificate's DER bytes.
    let fp = json(&fixtures().join("fingerprint.json"));
    let hex = fp["der_hex"].as_str().unwrap();
    let der: Vec<u8> = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect();
    assert_eq!(sha256(&der), fp["sha256"].as_str().unwrap());
    assert_eq!(normalise_fingerprint(&sha256(&der).to_uppercase()).unwrap(), fp["sha256"].as_str().unwrap());
}

// ---------------------------------------------------------------------------
// Cross-checks with the Python reference
// ---------------------------------------------------------------------------

/// Runs `python3 tools/gscap.py ARGS`; `None` when there is no Python on
/// this machine (the cross-check is then skipped, loudly).
fn gscap_py(args: &[&str]) -> Option<(bool, String)> {
    let script = repo().join("tools/gscap.py");
    let output = Command::new("python3").arg("-B").arg(&script).args(args).output().ok()?;
    let text = format!("{}{}", String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    Some((output.status.success(), text))
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gs-core-ingest-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

#[test]
fn frames_and_files_written_here_verify_with_the_python_reference() {
    // A frame with every plane, deflated by miniz_oxide, and a real JPEG.
    let (w, h) = (64u16, 48u16);
    let n = w as usize * h as usize;
    let mut rgb = Vec::with_capacity(160 * 120 * 3);
    for y in 0..120u32 {
        for x in 0..160u32 {
            rgb.extend_from_slice(&[(x * 255 / 159) as u8, (y * 255 / 119) as u8, 128]);
        }
    }
    let mut jpeg = Vec::new();
    jpeg_encoder::Encoder::new(&mut jpeg, 85).encode(&rgb, 160, 120, jpeg_encoder::ColorType::Rgb).unwrap();
    let frame_at = |index: u32, compressed: bool| CaptureFrame {
        index,
        timestamp: 100.0 + index as f64 * 0.1,
        world_from_cam: gs_core::glam::Mat4::from_translation(gs_core::glam::Vec3::new(0.1 * index as f32, 0.0, 0.0)).to_cols_array(),
        intrinsics: [140.0, 140.0, 80.0, 60.0],
        tracking: Tracking::Normal,
        orientation: Orientation::Portrait,
        color_width: 160,
        color_height: 120,
        color: jpeg.clone(),
        depth_width: w,
        depth_height: h,
        depth_mm: (0..n).map(|i| 1500 + (i % 64) as u16 * 10).collect(),
        confidence: Some((0..n).map(|i| (i % 3) as u8).collect()),
        mask: Some((0..n).map(|i| if i % 5 == 0 { 255 } else { 0 }).collect()),
        compressed,
        reserved: 0,
    };
    let header = GscapHeader {
        protocol: Some(1),
        device: "rust".into(),
        app: "0.1.0".into(),
        mode: "person".into(),
        name: "Écrit par Rust".into(),
        created: "2026-11-14T10:00:00Z".into(),
        region: Some(CaptureRegion { center: [0.0, 0.0, -1.5], radius: 1.0 }),
        floor: Some(-1.3),
    };
    let mut writer = GscapWriter::new(Vec::new(), &header, HEADER_PAD).unwrap();
    for index in 0..5 {
        writer.append(&encode_frame(&frame_at(index, index % 2 == 0)).unwrap()).unwrap();
    }
    let file = scratch("rust.gscap");
    std::fs::write(&file, writer.into_inner()).unwrap();
    let single = scratch("rust_frame.bin");
    std::fs::write(&single, encode_frame(&frame_at(9, true)).unwrap()).unwrap();

    let Some((ok, text)) = gscap_py(&["verify", file.to_str().unwrap()]) else {
        eprintln!("python3 not found: the cross-check with tools/gscap.py is SKIPPED");
        return;
    };
    assert!(ok, "gscap.py verify refused a file written by Rust:\n{text}");
    assert!(text.contains("5 frames") && text.trim_end().ends_with("OK"), "{text}");
    let (ok, text) = gscap_py(&["verify", single.to_str().unwrap()]).unwrap();
    assert!(ok && text.contains("1 frames"), "{text}");
    let (ok, text) = gscap_py(&["info", file.to_str().unwrap(), "--frames"]).unwrap();
    assert!(ok, "{text}");
    assert!(text.contains("\"name\": \"Écrit par Rust\"") && text.contains("\"radius\": 1.0"), "{text}");
    assert!(text.contains("colour 160x120") && text.contains("depth 64x48") && text.contains("confidence, mask, deflate"), "{text}");
    // A file cut short is reported as such by both.
    let bytes = std::fs::read(&file).unwrap();
    let cut = scratch("rust_cut.gscap");
    std::fs::write(&cut, &bytes[..bytes.len() - 100]).unwrap();
    let (ok, text) = gscap_py(&["verify", cut.to_str().unwrap()]).unwrap();
    assert!(ok && text.contains("4 complete frames"), "{text}");
    let whole = split_gscap(&bytes[..bytes.len() - 100]).unwrap();
    assert!(whole.truncated && whole.frames.len() == 4);
    for path in [file, single, cut] {
        let _ = std::fs::remove_file(path);
    }
}

#[test]
fn the_synthetic_walk_of_the_python_reference_comes_out_as_its_box_room() {
    // `gscap.py synth`: a camera turning once inside a box room of
    // 4 x 2.6 x 5 m, eye at the origin 1.4 m above the floor, every wall
    // its own colour. 64x48 depth, 160x120 JPEG, deflated, with confidence.
    let file = scratch("synth.gscap");
    let Some((ok, text)) = gscap_py(&["synth", file.to_str().unwrap(), "--frames", "48"]) else {
        eprintln!("python3 not found: the synthetic walk of tools/gscap.py is SKIPPED");
        return;
    };
    assert!(ok, "{text}");
    let bytes = std::fs::read(&file).unwrap();
    let _ = std::fs::remove_file(&file);
    let whole = split_gscap(&bytes).unwrap();
    assert_eq!((whole.frames.len(), whole.header.mode.as_str(), whole.header.floor), (48, "room", Some(-1.4)));
    for frame in &whole.frames {
        let frame = decode_frame(frame).unwrap();
        assert_eq!((frame.depth_width, frame.depth_height, frame.color_width, frame.compressed), (64, 48, 160, true));
    }

    let (mut session, summary) = import_gscap(&bytes, IngestParams::default()).unwrap();
    assert_eq!((summary.frames, summary.frames_used, summary.truncated), (48, 48, false));
    assert_eq!(summary.name, "Synthetic room");
    let splats = session.static_splats();
    assert!(splats.len() > 20_000, "{} splats", splats.len());

    // The room, floor at y = 0: x in [-2, 2], y in [0, 2.6], z in [-2.5, 2.5].
    let (mut lo, mut hi) = ([f32::INFINITY; 3], [f32::NEG_INFINITY; 3]);
    for s in &splats {
        for k in 0..3 {
            lo[k] = lo[k].min(s.pos[k]);
            hi[k] = hi[k].max(s.pos[k]);
        }
    }
    // A depth pixel of this small synthetic sensor is 4.5 cm wide on the
    // far walls, and the fusion fills the cells under the whole pixel: at
    // the creases the faces overshoot by up to two cells of 2 cm.
    let expected = ([-2.0, 0.0, -2.5], [2.0, 2.6, 2.5]);
    for k in 0..3 {
        assert!((lo[k] - expected.0[k]).abs() < 0.045 && (hi[k] - expected.1[k]).abs() < 0.045, "axis {k}: {} to {}", lo[k], hi[k]);
    }
    // Every splat lies on one of the six faces (within a cell).
    let off_wall = |s: &gs_core::Splat| (0..3).map(|k| (s.pos[k] - expected.0[k]).abs().min((s.pos[k] - expected.1[k]).abs())).fold(f32::INFINITY, f32::min);
    // (At the creases the coarse pixels chamfer the corner by a cell or two.)
    let stray = splats.iter().filter(|s| off_wall(s) > 0.05).count();
    assert!(stray * 200 < splats.len(), "{stray} of {} splats float inside the room", splats.len());

    // The walls keep their colours: +X is (60, 160, 200), -X (200, 80, 60),
    // the floor (120, 120, 120), the ceiling (230, 220, 200), each with a
    // darker checker (-40), so the mean lies 20 below.
    let mean_color = |pick: &dyn Fn(&gs_core::Splat) -> bool| {
        let picked: Vec<_> = splats.iter().filter(|s| pick(s)).collect();
        assert!(picked.len() > 500, "{} splats on a face", picked.len());
        let mut sum = [0.0f32; 3];
        for s in &picked {
            for (total, &value) in sum.iter_mut().zip(&s.rgba) {
                *total += value as f32;
            }
        }
        sum.map(|v| v / picked.len() as f32)
    };
    let inner = |s: &gs_core::Splat, a: usize, b: usize| {
        (s.pos[a] - expected.0[a]).min(expected.1[a] - s.pos[a]) > 0.3 && (s.pos[b] - expected.0[b]).min(expected.1[b] - s.pos[b]) > 0.3
    };
    for (face, color, pick) in [
        ("+x", [60.0, 160.0, 200.0], &(|s: &gs_core::Splat| s.pos[0] > 1.97 && inner(s, 1, 2)) as &dyn Fn(&gs_core::Splat) -> bool),
        ("-x", [200.0, 80.0, 60.0], &|s: &gs_core::Splat| s.pos[0] < -1.97 && inner(s, 1, 2)),
        ("floor", [120.0, 120.0, 120.0], &|s: &gs_core::Splat| s.pos[1] < 0.03 && inner(s, 0, 2)),
        ("ceiling", [230.0, 220.0, 200.0], &|s: &gs_core::Splat| s.pos[1] > 2.57 && inner(s, 0, 2)),
    ] {
        let mean = mean_color(pick);
        for c in 0..3 {
            assert!((mean[c] - (color[c] - 20.0)).abs() < 14.0, "{face}: mean colour {mean:?}, wall {color:?}");
        }
    }

    // What a viewer is told: a capture, the room's height, the phone's path.
    let state = session.state();
    let capture = state.capture.as_ref().unwrap();
    assert_eq!((state.scene.as_str(), capture.mode, capture.floor_known, capture.imported), ("capture", CaptureMode::Room, true, true));
    let ceiling = state.ceiling_height.unwrap();
    assert!((ceiling - 2.6).abs() < 0.03, "ceiling at {ceiling}");
    let path = &state.scan.as_ref().unwrap().path;
    assert!(path.len() > 20 && path.iter().all(|p| (p[1] - 1.4).abs() < 1e-3), "the eye walks 1.4 m above the floor");
    assert!(state.bounds.min.y.abs() < 0.05 && (state.bounds.max.y - 2.6).abs() < 0.05, "{:?}", state.bounds);
    assert_eq!(session.snapshot().len(), 5, "State, two layers, the last preview, Stats");
}
