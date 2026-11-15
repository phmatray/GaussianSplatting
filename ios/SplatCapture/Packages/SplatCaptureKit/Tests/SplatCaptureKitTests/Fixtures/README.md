# Capture format fixtures

Small, deterministic test data for the capture frame and the `.gscap` file of
`docs/ARCHITECTURE.md`, section "Real capture: the phone companion". Three
implementations are tested against these files:

- `tools/gscap.py`, the Python reference (it produces them);
- `ios/SplatCapture/Packages/SplatCaptureKit`, the Swift code of the phone app;
- `gs-core::ingest` and `gs-server`, the Rust side.

The directory lives inside the Swift test target
(`ios/SplatCapture/Packages/SplatCaptureKit/Tests/SplatCaptureKitTests/Fixtures`)
because SwiftPM only bundles resources found there; `ios/SplatCapture/Fixtures`
is a symbolic link to it and is the path to use everywhere else.

Everything except this README is written by `python3 tools/make_fixtures.py`.
Do not edit the files by hand: `tools/test_tools.py` regenerates them into a
temporary directory and compares. The only bytes that may differ from one
machine to another are deflate streams (they depend on the zlib build), which
is why tests compare compressed frames by their decoded content.

## `frames/`: single capture frames

Every `NAME.bin` is one frame exactly as it travels in a WebSocket binary
message. `NAME.json` is its sidecar.

Valid frames (`"valid": true`):

| File | Flags | Contents |
|---|---|---|
| `frame_plain` | 0 | depth only |
| `frame_conf` | 1 | depth + confidence |
| `frame_deflate` | 2 | depth, deflated |
| `frame_deflate_conf` | 3 | depth + confidence, deflated |
| `frame_mask` | 4 | depth + mask |
| `frame_conf_mask` | 5 | depth + confidence + mask |
| `frame_deflate_mask` | 6 | depth + mask, deflated |
| `frame_deflate_conf_mask` | 7 | everything, deflated |
| `frame_extreme` | 0 | frame index 2^32-1, depth 1x1 of 65535 mm, colour length 0 with colour size 65535x65535, timestamp = largest double, float edge cases in pose and intrinsics (largest and smallest float, subnormal, -0.0; no NaN, no infinity) |
| `frame_lidar` | 7 | 256x192 planes, the size ARKit delivers, deflated (about 3 kB) |
| `frame_reserved_ignored` | 1 | bytes 12..15 are `0xDEADBEEF`: readers ignore the reserved field |

The eight flag frames have a 4x3 depth map and a 16x12 colour JPEG, and
differ in index (10 + flags), timestamp, pose, tracking and orientation.

Sidecar fields of a valid frame:

- `flags`, `compressed`, `tracking`, `orientation`, `index`, `reserved`,
  `length` (of the `.bin`);
- `timestamp` and `timestamp_bits` (the f64 as 16 hex digits);
- `world_from_cam` (16 numbers, column-major) and `world_from_cam_bits` (each
  f32 as 8 hex digits); `intrinsics` (`fx, fy, cx, cy`) and `intrinsics_bits`.
  Compare the bit patterns: they are exact, including -0.0;
- `color`: `width`, `height`, `length`, `sha256` of the JPEG bytes;
- `depth`, `confidence`, `mask` (the last two may be `null`): `width`,
  `height`, `sha256` of the decoded (inflated) plane, and `values` when the
  plane has at most 64 pixels (depth in millimeters, row-major from the top
  left);
- `stored_lengths`: the four byte lengths of the header (compressed sizes
  when the frame is deflated);
- `reencode_identical`: true when encoding the decoded fields again must give
  the same bytes (false for deflated frames and for the reserved-field one).

Invalid frames (`"valid": false`, file names start with `bad_`) carry one
fault each. The sidecar names the expected error code:

| Code | Meaning |
|---|---|
| `too_short` | fewer than 128 bytes |
| `bad_magic` | bytes 0..3 are not `GSCF` |
| `bad_version` | version is not 1 |
| `bad_flags` | a flag bit above bit 2 is set |
| `bad_tracking` | tracking above 2 |
| `bad_orientation` | orientation above 3 |
| `bad_dimensions` | depth width or height is 0, or the depth map has more than 2^22 pixels |
| `bad_length` | a length disagrees with the flags, with the plane size, or with the size of the frame |
| `bad_deflate` | a plane does not inflate to exactly its size |

Checks are made in that order (header fields first, then the total length,
then inflation). The reference also rejects bytes that follow the end of a
deflate stream inside a plane; no fixture relies on it because the Apple
Compression framework cannot detect it.

Some of the invalid frames are traps for a careless decoder rather than
plain mistakes:

| File | Trap |
|---|---|
| `bad_length_wraps_u32` | depth and mask lengths are each above 2^31 and their sum, taken in 32 bits, wraps around to the real size of the payload: add the four lengths in 64 bits (or with overflow checks) before comparing with the frame size, and never slice by a length before that comparison |
| `bad_length_overflow` | colour length 2^32-1 in a frame of 504 bytes: nothing of that size may be allocated |
| `bad_depth_huge` | a deflated depth map of 65535 x 65535 pixels (8.6 GB once inflated) in a frame of a few hundred bytes: the size must be refused from the header, before any buffer is reserved |
| `bad_deflate_long`, `bad_deflate_mask_long` | a stream that inflates to more than the plane: inflation must stop one byte after the expected size (a real attack inflates to gigabytes; `tools/test_tools.py` tests 64 MiB) |
| `bad_deflate_conf_short`, `bad_deflate_mask_long` | the depth stream is fine, the fault is in the second or third plane |
| `bad_header_only` | exactly 128 bytes, a valid header and no payload |
| `bad_flags_high`, `bad_flags_all` | unknown flag bits together with known ones |

The JPEGs are real baseline JPEGs (three components, flat 8x8 blocks) written
by `tools/tinyjpeg.py`; any decoder opens them.

## Decisions the specification leaves open

These are the choices of the reference implementation; the fixtures encode
them and the phone app follows them.

1. With flag bit 1, depth, confidence and mask are three independent raw
   deflate streams. The header lengths are the stored (compressed) sizes; the
   inflated sizes follow from the depth size (2 bytes per pixel for depth, 1
   for confidence and mask). The colour JPEG is never deflated.
2. Flag bit 0 (bit 2) is set exactly when the confidence (mask) length is not
   0. An absent plane has length 0 whether or not the frame is deflated.
3. Colour length 0 means there is no colour image. Colour width and height
   then still give the pixel grid the intrinsics refer to.
4. Depth is at least 1x1 and at most 2^22 pixels (4 194 304, for example
   2048 x 2048). This upper bound is an implementation limit of the
   reference and of the phone, not a statement of the specification: a
   decoder may choose another one, but it needs one (see `bad_depth_huge`).
   ARKit delivers 256 x 192. In a `.gscap`, a frame record is at most
   64 MiB; the phone's frames are a few hundred kB.
5. Depth in millimeters is `floor(meters * 1000 + 0.5)` in double precision;
   values that are not positive, not finite, or above 65535 mm become 0
   ("no value"). See `depth_conversion.json`.
6. The frame index starts at 0 and grows by one for every frame the phone
   sends or records. It is not reset by `pause`, `resume`, `reset` or a
   reconnection, so an `ack` can never be attributed to the wrong frame.
7. The JSON header of a `.gscap` holds the `hello` fields without `type` and
   `token` (`protocol`, `device`, `app`, `mode`, `name`), plus `created` and
   optionally `region` (`{"center":[x,y,z],"radius":r}`) and `floor`
   (`{"y":h}`). Readers ignore unknown keys. The JSON may be followed by
   ASCII spaces inside its declared length: the phone reserves 1024 bytes so
   that it can rewrite the header in place (region and floor are often known
   only after the first frames).
8. A `.gscap` that ends inside a frame record is a recording cut short, not
   an error: readers keep the complete frames.

## `.gscap` files

| File | Contents |
|---|---|
| `three_frames.gscap` | compact header with `region` and `floor`, frames `frame_plain`, `frame_conf`, `frame_deflate_conf_mask` |
| `padded_header.gscap` | header padded with spaces to 1024 bytes, two frames |
| `truncated.gscap` | `three_frames.gscap` cut in the middle of its third frame: two frames, `truncated` |
| `truncated_length.gscap` | `three_frames.gscap` cut inside the `u32` length of its third record (2 of 4 bytes): two frames, `truncated` |
| `empty.gscap` | a header and no frame |

`NAME.json` holds the expected `header`, `header_json_length`, `file_length`,
`truncated` and, per frame, `index`, `length` and `sha256` of the record.

`invalid_files.json` lists files a reader must refuse, with the error code:
`bad_file_magic` (wrong or incomplete magic), `bad_header` (the JSON length
is beyond the end of the file or above 1 MiB, the JSON is not UTF-8, not
JSON, or not an object) and `bad_length` (a frame record shorter than 128
bytes or longer than 64 MiB, `bad_frame_record_huge.gscap` announces
2^32-1). A reader must not allocate what a length field announces before
it knows the file holds that many bytes.

The reference reads the header as strict JSON: `NaN`, `Infinity` and
nesting deep enough to exhaust a recursive parser are refused
(`bad_header`); the same holds for the text frames of the stream.

## Other files

- `pairing_uris.json`: pairing URIs with the expected parse (`host`, `port`,
  `fp`, `token`, the `wss://` and `https://` addresses derived from them) or
  the expected error code. The exact rules are in the docstring of
  `parse_pairing_uri` in `tools/gscap.py`. A host with a colon must be an
  IPv6 literal with at least two colons, so that `192.168.1.20:7879` typed
  as a host is refused instead of being taken for one.
- `depth_conversion.json`: float32 bit patterns (meters) and the expected
  `u16` millimeters.
- `messages.json`: `client` lists the JSON text frames the phone sends
  (`hello`, `region`, `floor`, `pause`, `resume`, `reset`, `finish`) with the
  arguments they are built from; `server` lists server text frames with the
  way the phone understands them (`expect` is `null` when it drops the
  message).
- `fingerprint.json`: bytes and their SHA-256, the form of `fp`.
