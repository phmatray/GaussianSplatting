#!/usr/bin/env python3
"""Write the deterministic test fixtures of the capture format.

    python3 tools/make_fixtures.py            -> the Fixtures directory
    python3 tools/make_fixtures.py OUTDIR

The fixtures are shared by three implementations: tools/gscap.py (which
produces them), the Swift package SplatCaptureKit and the Rust server
(gs-core::ingest). See ios/SplatCapture/Fixtures/README.md.

The directory lives inside the Swift test target, because SwiftPM only
bundles resources found there:

    ios/SplatCapture/Packages/SplatCaptureKit/Tests/SplatCaptureKitTests/Fixtures

`ios/SplatCapture/Fixtures` is a symbolic link to it, for everybody else.

Nothing here depends on the clock or on random numbers. The only bytes that
may differ between machines are the deflate streams (they depend on the zlib
build); tests therefore compare compressed fixtures by their decoded content.
"""

from __future__ import annotations

import hashlib
import json
import math
import os
import struct
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import gscap  # noqa: E402
import tinyjpeg  # noqa: E402

DEFAULT_DIR = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))),
                           "ios", "SplatCapture", "Packages", "SplatCaptureKit", "Tests", "SplatCaptureKitTests",
                           "Fixtures")

FP = "3f5a9c0d7e2b41886a1f0c9d8e7b6a5f4e3d2c1b0a99887766554433221100ff"
TOKEN = "0123456789abcdef0123456789abcdef"

F32_MAX = 3.4028234663852886e38
F32_MIN_NORMAL = 1.1754943508222875e-38
F32_MIN_SUBNORMAL = 1.401298464324817e-45


def _write(path: str, data: bytes) -> None:
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as f:
        f.write(data)


def _write_json(path: str, obj) -> None:
    text = json.dumps(obj, indent=1, ensure_ascii=False, sort_keys=True) + "\n"
    _write(path, text.encode("utf-8"))


def _pose(yaw: float, position) -> tuple:
    c, s = math.cos(yaw), math.sin(yaw)
    x, y, z = position
    return (c, 0.0, -s, 0.0, 0.0, 1.0, 0.0, 0.0, s, 0.0, c, 0.0, x, y, z, 1.0)


def small_frame(flags: int) -> gscap.Frame:
    """4x3 depth, 16x12 colour; one frame for each of the 8 flag combinations."""
    depth_mm = [0, 500, 1000, 1500, 2000, 2500, 3000, 3500, 4000, 4999, 5000, 65535]
    confidence = bytes([0, 1, 2, 2, 1, 2, 2, 0, 2, 1, 0, 2])
    mask = bytes([0, 255, 255, 0, 0, 255, 255, 0, 0, 0, 255, 0])
    shade = 40 + 25 * flags
    return gscap.Frame(
        index=10 + flags,
        timestamp=1000.5 + 0.125 * flags,
        world_from_cam=_pose(0.1 * flags, (0.25 * flags, 1.5, -2.0)),
        intrinsics=(12.5, 12.25, 8.0, 6.0),
        tracking=(2, 1, 2, 0, 2, 2, 1, 2)[flags],
        orientation=flags % 4,
        color_width=16,
        color_height=12,
        color=tinyjpeg.encode_flat(16, 12, (shade, 255 - shade, 128)),
        depth_width=4,
        depth_height=3,
        depth=struct.pack("<12H", *depth_mm),
        confidence=confidence if flags & gscap.FLAG_CONFIDENCE else None,
        mask=mask if flags & gscap.FLAG_MASK else None,
        compressed=bool(flags & gscap.FLAG_DEFLATE),
    )


def extreme_frame() -> gscap.Frame:
    """Largest frame index, 1x1 depth, no colour image, float edge cases (no NaN)."""
    return gscap.Frame(
        index=0xFFFFFFFF,
        timestamp=1.7976931348623157e308,
        world_from_cam=(
            F32_MAX, -F32_MAX, F32_MIN_NORMAL, F32_MIN_SUBNORMAL,
            -0.0, 0.0, 1.0, -1.0,
            16777216.0, 0.1, -0.1, 1e-10,
            123456.7890625, -F32_MIN_SUBNORMAL, 2.5, 1.0,
        ),
        intrinsics=(F32_MIN_SUBNORMAL, F32_MAX, -0.0, 65535.5),
        tracking=gscap.TRACKING_NOT_AVAILABLE,
        orientation=3,
        color_width=65535,
        color_height=65535,
        color=b"",
        depth_width=1,
        depth_height=1,
        depth=struct.pack("<H", 65535),
        confidence=None,
        mask=None,
        compressed=False,
    )


def lidar_frame() -> gscap.Frame:
    """The plane sizes ARKit delivers (256x192), compressed, with confidence and mask."""
    w, h = 256, 192
    depth = []
    confidence = bytearray()
    mask = bytearray()
    for row in range(h):
        for col in range(w):
            border = min(col, row, w - 1 - col, h - 1 - row)
            inside = (col - 128) ** 2 + (row - 110) ** 2 < 50 ** 2
            if border < 2:
                mm = 0
            elif inside:
                mm = 1200 + (col - 128) // 8
            else:
                mm = 2400 + 4 * (col // 4) + 6 * (row // 4)
            depth.append(mm)
            confidence.append(0 if border < 2 else (1 if border < 12 else 2))
            mask.append(255 if inside else 0)
    palette = [(180, 60, 50), (60, 150, 190), (200, 190, 120), (90, 160, 90)]
    return gscap.Frame(
        index=1234,
        timestamp=86400.25,
        world_from_cam=_pose(0.75, (1.25, 0.125, -3.5)),
        intrinsics=(48.0, 48.0, 32.0, 24.0),
        tracking=gscap.TRACKING_NORMAL,
        orientation=1,
        color_width=64,
        color_height=48,
        color=tinyjpeg.encode_blocks(64, 48, lambda bx, by: palette[(bx + 2 * by) % 4]),
        depth_width=w,
        depth_height=h,
        depth=struct.pack("<%dH" % (w * h), *depth),
        confidence=bytes(confidence),
        mask=bytes(mask),
        compressed=True,
    )


def valid_frames() -> list:
    names = {
        0: "frame_plain",
        1: "frame_conf",
        2: "frame_deflate",
        3: "frame_deflate_conf",
        4: "frame_mask",
        5: "frame_conf_mask",
        6: "frame_deflate_mask",
        7: "frame_deflate_conf_mask",
    }
    out = [(names[flags], gscap.encode_frame(small_frame(flags)), "flag combination %d" % flags) for flags in range(8)]
    out.append(("frame_extreme", gscap.encode_frame(extreme_frame()),
                "frame index 2^32-1, 1x1 depth of 65535 mm, colour length 0, float edge cases"))
    out.append(("frame_lidar", gscap.encode_frame(lidar_frame()), "256x192 planes as ARKit delivers them, compressed"))
    reserved = bytearray(gscap.encode_frame(small_frame(1)))
    struct.pack_into("<I", reserved, 12, 0xDEADBEEF)
    out.append(("frame_reserved_ignored", bytes(reserved), "the reserved field is not 0: readers ignore it"))
    return out


def _patched(base: bytes, offset: int, fmt: str, *values) -> bytes:
    data = bytearray(base)
    struct.pack_into(fmt, data, offset, *values)
    return bytes(data)


def _with_planes(base_frame: gscap.Frame, flags: int, depth: bytes, confidence: bytes = b"", mask: bytes = b"") -> bytes:
    """Assemble a frame by hand so that inconsistent ones can be written."""
    header = bytearray(gscap.encode_frame(base_frame, compress=False)[: gscap.HEADER_SIZE])
    header[5] = flags
    struct.pack_into("<I", header, 116, len(depth))
    struct.pack_into("<I", header, 120, len(confidence))
    struct.pack_into("<I", header, 124, len(mask))
    return bytes(header) + base_frame.color + depth + confidence + mask


def invalid_frames() -> list:
    """(name, bytes, error code, explanation); each has exactly one fault."""
    plain = gscap.encode_frame(small_frame(0))
    base = small_frame(0)
    pixels = base.depth_width * base.depth_height
    good_stream = gscap.deflate_raw(base.depth)
    # A deflate stream written by hand (one stored block), so that the two
    # frames built on it do not depend on the zlib build: 4 zero bytes.
    stored_block = b"\x01\x04\x00\xfb\xff\x00\x00\x00\x00"
    # Depth and mask lengths that each exceed 2^31 and whose sum, taken in 32
    # bits, wraps around to the true size of the payload.
    wraps = bytearray(_with_planes(base, gscap.FLAG_DEFLATE | gscap.FLAG_MASK, stored_block))
    struct.pack_into("<I", wraps, 116, 0x80000000 + len(stored_block))
    struct.pack_into("<I", wraps, 124, 0x80000000)
    huge = bytearray(_with_planes(base, gscap.FLAG_DEFLATE, stored_block))
    struct.pack_into("<HH", huge, 112, 65535, 65535)
    return [
        ("bad_version_zero", _patched(plain, 4, "<B", 0), "bad_version", "version 0"),
        ("bad_flags_high", _patched(plain, 5, "<B", 0x80), "bad_flags", "flag bit 7 is set"),
        ("bad_flags_all", _patched(plain, 5, "<B", 0xFF), "bad_flags", "all eight flag bits are set"),
        ("bad_header_only", plain[:128], "bad_length", "128 bytes: a valid header whose payload is missing"),
        ("bad_length_overflow", _patched(plain, 108, "<I", 0xFFFFFFFF), "bad_length",
         "colour length 2^32-1 in a frame of 504 bytes"),
        ("bad_length_wraps_u32", bytes(wraps), "bad_length",
         "depth and mask lengths above 2^31 whose 32-bit sum wraps around to the real payload size"),
        ("bad_depth_huge", bytes(huge), "bad_dimensions",
         "deflated depth of 65535 x 65535 pixels (8.6 GB once inflated) in a frame of %d bytes" % len(huge)),
        ("bad_deflate_conf_short",
         _with_planes(base, gscap.FLAG_DEFLATE | gscap.FLAG_CONFIDENCE, good_stream, gscap.deflate_raw(bytes(pixels - 1))),
         "bad_deflate", "depth is fine, the confidence stream inflates to 11 bytes instead of 12"),
        ("bad_deflate_mask_long",
         _with_planes(base, gscap.FLAG_DEFLATE | gscap.FLAG_MASK, good_stream, b"", gscap.deflate_raw(bytes(pixels + 1))),
         "bad_deflate", "depth is fine, the mask stream inflates to 13 bytes instead of 12"),
        ("bad_empty", b"", "too_short", "no bytes at all"),
        ("bad_too_short", plain[:127], "too_short", "127 bytes, one less than a header"),
        ("bad_magic", b"GSCG" + plain[4:], "bad_magic", "magic is GSCG"),
        ("bad_version", _patched(plain, 4, "<B", 2), "bad_version", "version 2"),
        ("bad_flags", _patched(plain, 5, "<B", 0x08), "bad_flags", "flag bit 3 is set"),
        ("bad_tracking", _patched(plain, 6, "<B", 3), "bad_tracking", "tracking 3"),
        ("bad_orientation", _patched(plain, 7, "<B", 4), "bad_orientation", "orientation 4"),
        ("bad_depth_zero", _patched(plain, 112, "<H", 0), "bad_dimensions", "depth width 0"),
        ("bad_conf_flag_without_plane", _patched(plain, 5, "<B", gscap.FLAG_CONFIDENCE), "bad_length",
         "confidence flag set, confidence length 0"),
        ("bad_conf_plane_without_flag", _with_planes(base, 0, base.depth, bytes(pixels)), "bad_length",
         "confidence bytes present, flag clear"),
        ("bad_mask_plane_without_flag", _with_planes(base, 0, base.depth, b"", bytes(pixels)), "bad_length",
         "mask bytes present, flag clear"),
        ("bad_depth_length", _with_planes(base, 0, base.depth[:-2]), "bad_length",
         "uncompressed depth of 22 bytes for 4x3 pixels"),
        ("bad_conf_length", _with_planes(base, gscap.FLAG_CONFIDENCE, base.depth, bytes(pixels + 1)), "bad_length",
         "uncompressed confidence of 13 bytes for 4x3 pixels"),
        ("bad_truncated_payload", plain[:-1], "bad_length", "last byte missing"),
        ("bad_trailing_byte", plain + b"\x00", "bad_length", "one byte after the frame"),
        ("bad_deflate_garbage", _with_planes(base, gscap.FLAG_DEFLATE, b"\x07\xff\xff\xff"), "bad_deflate",
         "depth stream starts with the reserved block type"),
        ("bad_deflate_short", _with_planes(base, gscap.FLAG_DEFLATE, gscap.deflate_raw(base.depth[:-2])), "bad_deflate",
         "depth stream inflates to 22 bytes instead of 24"),
        ("bad_deflate_long", _with_planes(base, gscap.FLAG_DEFLATE, gscap.deflate_raw(base.depth + b"\x01\x02")),
         "bad_deflate", "depth stream inflates to 26 bytes instead of 24"),
        ("bad_deflate_truncated", _with_planes(base, gscap.FLAG_DEFLATE, good_stream[: len(good_stream) // 2]),
         "bad_deflate", "depth stream cut in half"),
        ("bad_deflate_empty", _with_planes(base, gscap.FLAG_DEFLATE, b""), "bad_length",
         "compressed depth of 0 bytes"),
    ]


def pairing_cases() -> list:
    ok = "splatlive://pair?host=192.168.1.20&port=7879&fp=%s&token=%s" % (FP, TOKEN)
    colon_fp = ":".join(FP[i : i + 2] for i in range(0, 64, 2)).upper()

    def good(uri, host="192.168.1.20", port=7879, note=""):
        authority = ("[%s]" % host if ":" in host else host) + ":%d" % port
        return {"uri": uri, "ok": True, "host": host, "port": port, "fp": FP, "token": TOKEN,
                "ingest_url": "wss://%s/ingest" % authority,
                "captures_url": "https://%s/api/captures" % authority, "note": note}

    def bad(uri, error, note=""):
        return {"uri": uri, "ok": False, "error": error, "note": note}

    q = "host=192.168.1.20&port=7879&fp=%s&token=%s" % (FP, TOKEN)
    return [
        good(ok, note="canonical form, as in /api/pair"),
        good("  \t" + ok + "\r\n", note="surrounding whitespace"),
        good("splatlive://pair?token=%s&v=2&fp=%s&port=7879&extra&host=192.168.1.20" % (TOKEN, FP),
             note="any order, unknown parameters ignored"),
        good("SPLATLIVE://Pair?" + q, note="scheme and host of the URI in any case"),
        good("splatlive://pair?host=192.168.1.20&port=7879&fp=%s&token=%s" % (colon_fp, TOKEN.upper()),
             note="fingerprint in upper case with colons (as /api/info shows it), token in upper case"),
        good("splatlive://pair?host=192.168.1.20&port=7879&fp=%s&token=%s" % (colon_fp.replace(":", "%3A"), TOKEN),
             note="percent-encoded colons"),
        good("splatlive://pair?host=MacBook-Pro.local&port=7879&fp=%s&token=%s" % (FP, TOKEN),
             host="macbook-pro.local", note="host name, lowercased"),
        good("splatlive://pair?host=%%5Bfe80%%3A%%3A1%%5D&port=7879&fp=%s&token=%s" % (FP, TOKEN),
             host="fe80::1", note="IPv6 literal in brackets"),
        good("splatlive://pair?host=fd00::12:34&port=7879&fp=%s&token=%s" % (FP, TOKEN),
             host="fd00::12:34", note="IPv6 literal without brackets"),
        good("splatlive://pair?host=10.0.0.2&port=1&fp=%s&token=%s" % (FP, TOKEN), host="10.0.0.2", port=1),
        good("splatlive://pair?host=10.0.0.2&port=65535&fp=%s&token=%s" % (FP, TOKEN), host="10.0.0.2", port=65535),
        good("splatlive://pair?host=10.0.0.2&port=07879&fp=%s&token=%s" % (FP, TOKEN), host="10.0.0.2",
             note="leading zero in the port"),
        good("splatlive://pair?&&" + q + "&", note="empty items between ampersands"),
        bad("", "bad_scheme", "empty"),
        bad("https://192.168.1.20:7879/", "bad_scheme", "a web address"),
        bad("splatlive://pair", "bad_scheme", "no query"),
        bad("splatlive://pairing?" + q, "bad_scheme"),
        bad("splatlive://pair/?" + q, "bad_scheme", "path before the query"),
        bad("splatlive:pair?" + q, "bad_scheme"),
        bad("splat://pair?" + q, "bad_scheme"),
        bad("splatlive://pair?port=7879&fp=%s&token=%s" % (FP, TOKEN), "missing_host"),
        bad("splatlive://pair?host=192.168.1.20&fp=%s&token=%s" % (FP, TOKEN), "missing_port"),
        bad("splatlive://pair?host=192.168.1.20&port=7879&token=%s" % TOKEN, "missing_fp"),
        bad("splatlive://pair?host=192.168.1.20&port=7879&fp=%s" % FP, "missing_token"),
        bad("splatlive://pair?", "missing_host", "empty query"),
        bad("splatlive://pair?" + q + "&host=10.0.0.9", "duplicate_parameter", "a second host"),
        bad("splatlive://pair?" + q + "&token=" + TOKEN, "duplicate_parameter", "a second token, even if equal"),
        bad("splatlive://pair?host=&port=7879&fp=%s&token=%s" % (FP, TOKEN), "bad_host", "empty host"),
        bad("splatlive://pair?host=192.168.1.20%%2Fx&port=7879&fp=%s&token=%s" % (FP, TOKEN), "bad_host", "slash"),
        bad("splatlive://pair?host=my%%20mac&port=7879&fp=%s&token=%s" % (FP, TOKEN), "bad_host", "space"),
        bad("splatlive://pair?host=.local&port=7879&fp=%s&token=%s" % (FP, TOKEN), "bad_host", "leading dot"),
        bad("splatlive://pair?host=user@10.0.0.2&port=7879&fp=%s&token=%s" % (FP, TOKEN), "bad_host", "user info"),
        bad("splatlive://pair?host=fe80::1%%25en0&port=7879&fp=%s&token=%s" % (FP, TOKEN), "bad_host", "IPv6 zone"),
        bad("splatlive://pair?host=192.168.1.20:7879&port=7879&fp=%s&token=%s" % (FP, TOKEN), "bad_host",
            "port glued to the host (one colon is not an IPv6 literal)"),
        bad("splatlive://pair?host=a:b&port=7879&fp=%s&token=%s" % (FP, TOKEN), "bad_host", "one colon"),
        bad("splatlive://pair?host=10.0.0.2&port=0&fp=%s&token=%s" % (FP, TOKEN), "bad_port"),
        bad("splatlive://pair?host=10.0.0.2&port=65536&fp=%s&token=%s" % (FP, TOKEN), "bad_port"),
        bad("splatlive://pair?host=10.0.0.2&port=123456&fp=%s&token=%s" % (FP, TOKEN), "bad_port"),
        bad("splatlive://pair?host=10.0.0.2&port=&fp=%s&token=%s" % (FP, TOKEN), "bad_port", "empty"),
        bad("splatlive://pair?host=10.0.0.2&port=-1&fp=%s&token=%s" % (FP, TOKEN), "bad_port"),
        bad("splatlive://pair?host=10.0.0.2&port=78.9&fp=%s&token=%s" % (FP, TOKEN), "bad_port"),
        bad("splatlive://pair?host=10.0.0.2&port=https&fp=%s&token=%s" % (FP, TOKEN), "bad_port"),
        bad("splatlive://pair?host=10.0.0.2&port=%%EF%%BC%%97879&fp=%s&token=%s" % (FP, TOKEN), "bad_port",
            "a full-width digit"),
        bad("splatlive://pair?host=10.0.0.2&port=7879&fp=%s&token=%s" % (FP[:-1], TOKEN), "bad_fp", "63 digits"),
        bad("splatlive://pair?host=10.0.0.2&port=7879&fp=%s&token=%s" % (FP + "0", TOKEN), "bad_fp", "65 digits"),
        bad("splatlive://pair?host=10.0.0.2&port=7879&fp=%s&token=%s" % ("g" + FP[1:], TOKEN), "bad_fp", "not hex"),
        bad("splatlive://pair?host=10.0.0.2&port=7879&fp=&token=%s" % TOKEN, "bad_fp", "empty"),
        bad("splatlive://pair?host=10.0.0.2&port=7879&fp=%s&token=%s" % (FP, TOKEN[:-1]), "bad_token", "31 digits"),
        bad("splatlive://pair?host=10.0.0.2&port=7879&fp=%s&token=%s" % (FP, TOKEN + "0"), "bad_token", "33 digits"),
        bad("splatlive://pair?host=10.0.0.2&port=7879&fp=%s&token=%s" % (FP, "z" + TOKEN[1:]), "bad_token", "not hex"),
        bad("splatlive://pair?host=10.0.0.2&port=7879&fp=%s&token=%s" % (FP, TOKEN[:16] + ":" + TOKEN[16:-1]),
            "bad_token", "colons are not accepted in the token"),
        bad("splatlive://pair?host=10.0.0.2%%2&port=7879&fp=%s&token=%s" % (FP, TOKEN), "bad_encoding",
            "percent sign with one digit"),
        bad("splatlive://pair?host=10.0.0.2&port=7879&fp=%s&token=%s&x=%%zz" % (FP, TOKEN), "bad_encoding",
            "bad escape in an ignored parameter"),
        bad("splatlive://pair?host=%%ff&port=7879&fp=%s&token=%s" % (FP, TOKEN), "bad_encoding", "not UTF-8"),
    ]


def depth_cases() -> list:
    def f32(value: float) -> int:
        return struct.unpack("<I", struct.pack("<f", value))[0]

    bits = [f32(v) for v in (
        0.0, -0.0, 1.0, 0.0004, 0.00049, 0.0005, 0.00051, 0.001, 0.0015, 0.0025, 0.25, 0.3333, 2.5,
        4.9994, 4.9995, 4.9996, 5.0, 12.3456, 65.0, 65.5349, 65.535, 65.5354, 65.5356, 65.536, 66.0, 100.0,
        -1.0, -0.001, F32_MAX, -F32_MAX, F32_MIN_NORMAL, F32_MIN_SUBNORMAL,
    )]
    bits += [0x7F800000, 0xFF800000, 0x7FC00000, 0xFFC00000, 0x7F800001, 0x7FFFFFFF]  # +inf, -inf, NaNs
    cases = []
    for b in bits:
        value = struct.unpack("<f", struct.pack("<I", b))[0]
        cases.append({"bits": "%08x" % b, "meters": repr(value), "mm": gscap.depth_meters_to_mm(value)})
    return cases


def message_cases() -> dict:
    client = [
        {"build": "hello", "args": {"token": TOKEN, "device": "iPhone17,2", "app": "0.1.0", "mode": "room", "name": "Salon"},
         "json": json.loads(gscap.hello_message(TOKEN, "room", "Salon", device="iPhone17,2", app="0.1.0"))},
        {"build": "hello", "args": {"token": TOKEN, "device": "iPad13,8", "app": "0.1.0", "mode": "person",
                                    "name": "Zoé \"debout\" \\ essai 2"},
         "json": json.loads(gscap.hello_message(TOKEN, "person", "Zoé \"debout\" \\ essai 2", device="iPad13,8"))},
        {"build": "region", "args": {"center": [0.5, -0.25, -1.5], "radius": 0.75},
         "json": json.loads(gscap.region_message([0.5, -0.25, -1.5], 0.75))},
        {"build": "floor", "args": {"y": -1.25}, "json": json.loads(gscap.floor_message(-1.25))},
    ]
    for kind in ("pause", "resume", "reset", "finish"):
        client.append({"build": kind, "args": {}, "json": json.loads(gscap.simple_message(kind))})
    server_texts = [
        '{"type":"welcome","protocol":1,"max_inflight":4}',
        '{"max_inflight":16,"protocol":1,"type":"welcome","extra":true}',
        '{"type":"welcome","protocol":1}',
        '{"type":"welcome","protocol":1,"max_inflight":0}',
        '{"type":"welcome","protocol":2,"max_inflight":4}',
        '{"type":"ack","frame":0,"used":true}',
        '{"type":"ack","frame":17,"used":false}',
        '{"type":"ack","frame":4294967295,"used":true}',
        '{"type":"ack","frame":5}',
        '{"type":"ack","used":true}',
        '{"type":"ack","frame":-1,"used":true}',
        '{"type":"ack","frame":4294967296,"used":true}',
        '{"type":"ack","frame":1.5,"used":true}',
        '{"type":"ack","frame":"3","used":true}',
        '{"type":"progress","frames":120,"splats":654321}',
        '{"type":"progress","frames":3,"splats":5000000000}',
        '{"type":"progress"}',
        '{"type":"error","code":"bad_token","message":"wrong pairing token"}',
        '{"type":"error"}',
        '{"type":"stats","fps":12}',
        '{"type":5}',
        '{"frame":1}',
        '[]',
        '"welcome"',
        'not json',
        '',
    ]
    server = [{"text": text, "expect": gscap.parse_server_message(text)} for text in server_texts]
    return {"client": client, "server": server}


def gscap_fixtures(outdir: str) -> None:
    frames = [gscap.encode_frame(small_frame(flags)) for flags in (0, 1, 7)]

    def describe(path: str, header: dict, frame_list, truncated: bool, header_json_length: int) -> dict:
        return {
            "header": header,
            "header_json_length": header_json_length,
            "file_length": os.path.getsize(path),
            "truncated": truncated,
            "frames": [{"index": gscap.parse_header(f)["index"], "length": len(f),
                        "sha256": hashlib.sha256(f).hexdigest()} for f in frame_list],
        }

    # 1. Compact header with region and floor.
    header = gscap.make_header(mode="object", name="Théière", device="iPhone17,2", app="0.1.0",
                               created="2026-11-14T09:30:00Z",
                               region={"center": [0.5, -0.25, -1.5], "radius": 0.75}, floor={"y": -1.25})
    path = os.path.join(outdir, "three_frames.gscap")
    gscap.write_gscap(path, header, frames)
    with gscap.GscapReader(path) as reader:
        json_length = reader.frames_offset - 12
    _write_json(os.path.join(outdir, "three_frames.json"), describe(path, header, frames, False, json_length))

    # 2. Header padded with spaces to 1024 bytes, as the phone writes it.
    header2 = gscap.make_header(mode="room", name="Salon", device="iPhone17,2", app="0.1.0",
                                created="2026-11-14T09:45:00Z")
    path = os.path.join(outdir, "padded_header.gscap")
    gscap.write_gscap(path, header2, frames[:2], pad_to=1024)
    _write_json(os.path.join(outdir, "padded_header.json"), describe(path, header2, frames[:2], False, 1024))

    # 3. A recording cut short: the third frame is incomplete.
    with open(os.path.join(outdir, "three_frames.gscap"), "rb") as f:
        whole = f.read()
    path = os.path.join(outdir, "truncated.gscap")
    _write(path, whole[: len(whole) - len(frames[2]) // 2])
    _write_json(os.path.join(outdir, "truncated.json"), describe(path, header, frames[:2], True, json_length))

    # 3b. Cut inside the u32 length of the third record (2 of its 4 bytes).
    path = os.path.join(outdir, "truncated_length.gscap")
    _write(path, whole[: len(whole) - len(frames[2]) - 2])
    _write_json(os.path.join(outdir, "truncated_length.json"), describe(path, header, frames[:2], True, json_length))

    # 4. No frames at all.
    path = os.path.join(outdir, "empty.gscap")
    gscap.write_gscap(path, header2, [])
    with gscap.GscapReader(path) as reader:
        json_length2 = reader.frames_offset - 12
    _write_json(os.path.join(outdir, "empty.json"), describe(path, header2, [], False, json_length2))

    # Files a reader must refuse.
    good_head = gscap.encode_file_header(header2)
    bad = [
        ("bad_file_magic.gscap", b"GSCAP2\x00\x00" + good_head[8:], "bad_file_magic", "magic is GSCAP2"),
        ("bad_file_short.gscap", b"GSCAP1", "bad_file_magic", "6 bytes"),
        ("bad_header_array.gscap", gscap.FILE_MAGIC + struct.pack("<I", 2) + b"[]", "bad_header",
         "the header is a JSON array"),
        ("bad_header_json.gscap", gscap.FILE_MAGIC + struct.pack("<I", 5) + b"{nope", "bad_header", "not JSON"),
        ("bad_header_length.gscap", gscap.FILE_MAGIC + struct.pack("<I", 4096) + b"{}", "bad_header",
         "header length beyond the end of the file"),
        ("bad_header_length_huge.gscap", gscap.FILE_MAGIC + struct.pack("<I", 0xFFFFFFFF) + b"{}", "bad_header",
         "header length 2^32-1"),
        ("bad_header_utf8.gscap", gscap.FILE_MAGIC + struct.pack("<I", 13) + b'{"name":"\xff\xfe"}', "bad_header",
         "the header is not UTF-8"),
        ("bad_frame_record.gscap", good_head + struct.pack("<I", 64) + bytes(64), "bad_length",
         "a frame record shorter than a frame header"),
        ("bad_frame_record_huge.gscap", good_head + struct.pack("<I", 0xFFFFFFFF) + bytes(200), "bad_length",
         "a frame record of 2^32-1 bytes (above the 64 MiB limit)"),
    ]
    listing = []
    for name, data, error, note in bad:
        _write(os.path.join(outdir, name), data)
        listing.append({"file": name, "error": error, "note": note})
    _write_json(os.path.join(outdir, "invalid_files.json"), listing)


def generate(outdir: str) -> None:
    frames_dir = os.path.join(outdir, "frames")
    for name, data, note in valid_frames():
        _write(os.path.join(frames_dir, name + ".bin"), data)
        frame = gscap.decode_frame(data)
        sidecar = frame.describe()
        raw = struct.unpack_from("<I", data, 12)[0]
        sidecar.update({
            "valid": True,
            "note": note,
            "length": len(data),
            "reserved": raw,
            # True when encoding the decoded fields again must give the same bytes
            # (not for deflate streams, which depend on the compressor).
            "reencode_identical": not frame.compressed and raw == 0,
            "stored_lengths": {k: gscap.parse_header(data)[k] for k in
                               ("color_length", "depth_length", "confidence_length", "mask_length")},
        })
        _write_json(os.path.join(frames_dir, name + ".json"), sidecar)
    for name, data, error, note in invalid_frames():
        _write(os.path.join(frames_dir, name + ".bin"), data)
        _write_json(os.path.join(frames_dir, name + ".json"),
                    {"valid": False, "error": error, "note": note, "length": len(data)})
    gscap_fixtures(outdir)
    _write_json(os.path.join(outdir, "pairing_uris.json"), pairing_cases())
    _write_json(os.path.join(outdir, "depth_conversion.json"), depth_cases())
    _write_json(os.path.join(outdir, "messages.json"), message_cases())
    der = bytes((i * 37 + 11) % 256 for i in range(300))
    _write_json(os.path.join(outdir, "fingerprint.json"),
                {"der_hex": der.hex(), "sha256": gscap.certificate_fingerprint(der),
                 "note": "any bytes do: the fingerprint is the SHA-256 of the certificate's DER encoding"})


def main(argv=None) -> int:
    outdir = (argv or sys.argv[1:] or [DEFAULT_DIR])[0]
    generate(outdir)
    count = sum(len(files) for _, _, files in os.walk(outdir))
    print("fixtures written to %s (%d files)" % (outdir, count))
    return 0


if __name__ == "__main__":
    sys.exit(main())
