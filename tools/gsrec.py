#!/usr/bin/env python3
"""Reference implementation of the Splat Live recording file, .gsrec.

Specification: docs/ARCHITECTURE.md, "4D recording and sharing". The Rust
implementation is crates/gs-core/src/record; this file is written
independently of it, from the specification and the decisions listed in
that module's documentation, so that the format has two implementations
that agree (crates/gs-core/tests/record_tools.rs compares them).
Standard library only; works with Python 3.8 and later.

File (little-endian):

    8      magic "GSREC1\\0\\0"
    4      u32 length of the JSON header
    ..     JSON object: name, created (RFC 3339), scene, protocol,
           duration_s (0 while unknown); may be padded with spaces inside
           its length; unknown keys are ignored
    ..     records
    8      u64 file offset of the index record
    8      "GSRECEND"

Record: u32 length of what follows, u8 kind, u8 flags (bit 0: the payload
is raw-deflate compressed, RFC 1951), u16 reserved, f64 time in seconds
since the start, payload.

    kind 0  message   one wire message exactly as sent to viewers
    kind 1  keyframe  a complete snapshot, each message prefixed by its u32
                      length: State, layer 0 Replace, layer 1 Replace, then
                      optionally Stats and Preview messages
    kind 2  index     (f64 time, u64 file offset) of every keyframe record;
                      the last record, its time is the duration

Decisions taken where the specification is silent:

  * Records are in time order. A keyframe holds the state reached by the
    records before it and adds nothing: forward playback skips keyframes,
    a seek starts at the last keyframe not after the wanted time.
  * The first record is a keyframe.
  * A record length is at least 12 and at most MAX_RECORD_LEN; a payload
    inflates to at most MAX_INFLATED bytes. Flag bits 1..7 must be 0, the
    reserved field is ignored, a time is finite and not negative.
  * A record is checked in this order: bad_length, bad_kind, bad_flags,
    bad_time (also: earlier than the record before), then its payload:
    bad_deflate, and bad_message / bad_keyframe / bad_index.
  * Damage to the framing ends the readable part of the file; a record
    whose payload is bad is reported and skipped.
  * A file that ends inside a record, or without index and trailer, is a
    recording cut short: it plays up to its last complete record.
  * The JSON header is at most 1 MiB, UTF-8, a JSON object; NaN, Infinity,
    numbers out of range and nesting deeper than 127 levels are invalid.
    Fields of the wrong type count as missing.

Wire messages (crates/gs-core/src/codec.rs): 16-byte header "GSL1", u8
kind (1 State, 2 Layer, 3 Preview, 4 Stats), u8 flags, u16 reserved, u32
seq, u32 time_ms. State / Stats: UTF-8 JSON. Layer: u8 layer id (0 or 1),
u8 mode (0 Replace, 1 Append), u16 reserved, u32 count, 6 x f32 bounding
box, count x 16 bytes. Preview: u8 camera id, u8 reserved, u16 width, u16
height, u16 reserved, width x height x 5 bytes.

Command line:

    gsrec.py info FILE...            header and summary
    gsrec.py verify [--json] FILE... check everything (exit code 1 unless
                                     every file is complete and flawless)
    gsrec.py synth OUT.gsrec         a small synthetic recording
"""

from __future__ import annotations

import argparse
import json
import math
import struct
import sys
import zlib

MAGIC = b"GSREC1\0\0"
END_MAGIC = b"GSRECEND"
TRAILER_SIZE = 16
RECORD_HEADER_SIZE = 16
MAX_HEADER_JSON = 1 << 20
HEADER_JSON_SIZE = 1012
MAX_RECORD_LEN = 128 << 20
MAX_INFLATED = 128 << 20
FLAG_DEFLATE = 1
KIND_MESSAGE, KIND_KEYFRAME, KIND_INDEX = 0, 1, 2
PROTOCOL = 1
MAX_JSON_DEPTH = 127

WIRE_MAGIC = b"GSL1"
WIRE_STATE, WIRE_LAYER, WIRE_PREVIEW, WIRE_STATS = 1, 2, 3, 4
SPLAT_SIZE = 16


class FormatError(ValueError):
    """A recording that cannot be read; `code` is the stable error name."""

    def __init__(self, code: str, message: str = "") -> None:
        super().__init__(message or code)
        self.code = code


# --------------------------------------------------------------------- JSON

def _json_depth(text: str) -> int:
    """Deepest nesting of arrays and objects in a JSON text."""
    depth = deepest = 0
    in_string = escaped = False
    for ch in text:
        if in_string:
            if escaped:
                escaped = False
            elif ch == "\\":
                escaped = True
            elif ch == '"':
                in_string = False
        elif ch == '"':
            in_string = True
        elif ch in "[{":
            depth += 1
            deepest = max(deepest, depth)
        elif ch in "]}":
            depth -= 1
    return deepest


def _has_surrogate(value) -> bool:
    if isinstance(value, str):
        return any(0xD800 <= ord(ch) <= 0xDFFF for ch in value)
    if isinstance(value, list):
        return any(_has_surrogate(v) for v in value)
    if isinstance(value, dict):
        return any(_has_surrogate(k) or _has_surrogate(v) for k, v in value.items())
    return False


def loads_strict(text: str):
    """json.loads that refuses what a strict parser refuses: NaN and
    Infinity, numbers too large for a double, lone surrogates and very deep
    nesting. Raises ValueError."""

    def refuse(name):
        raise ValueError("invalid JSON constant " + name)

    def parse_float(literal):
        value = float(literal)
        if math.isinf(value):
            raise ValueError("number out of range")
        return value

    def parse_int(literal):
        value = int(literal)
        # Integers beyond 64 bits are read as doubles.
        return value if -(1 << 63) <= value < (1 << 64) else parse_float(literal)

    if _json_depth(text) > MAX_JSON_DEPTH:
        raise ValueError("JSON nested too deeply")
    try:
        value = json.loads(text, parse_constant=refuse, parse_float=parse_float, parse_int=parse_int)
    except RecursionError as error:
        raise ValueError("JSON nested too deeply") from error
    if _has_surrogate(value):
        raise ValueError("lone surrogate in a JSON string")
    return value


# --------------------------------------------------------------------- wire

def classify(message: bytes):
    """What a wire message is, or None for one a viewer would refuse.

    Returns ("state",), ("stats",), ("preview", camera) or
    ("layer", layer_id, replace).
    """
    if len(message) < 16 or message[:4] != WIRE_MAGIC:
        return None
    kind = message[4]
    if kind in (WIRE_STATE, WIRE_STATS):
        try:
            message[16:].decode("utf-8", "strict")
        except UnicodeDecodeError:
            return None
        return ("state",) if kind == WIRE_STATE else ("stats",)
    if kind == WIRE_PREVIEW:
        if len(message) < 24:
            return None
        width, height = struct.unpack_from("<HH", message, 18)
        if len(message) != 24 + width * height * 5:
            return None
        return ("preview", message[16])
    if kind == WIRE_LAYER:
        if len(message) < 48:
            return None
        layer, mode = message[16], message[17]
        if layer > 1 or mode > 1:
            return None
        (count,) = struct.unpack_from("<I", message, 20)
        if len(message) != 48 + count * SPLAT_SIZE:
            return None
        box = struct.unpack_from("<6f", message, 24)
        if not all(math.isfinite(v) for v in box) or any(box[k] > box[k + 3] for k in range(3)):
            return None
        return ("layer", layer, mode == 0)
    return None


def wire_header(kind: int, seq: int, time_ms: int) -> bytes:
    return WIRE_MAGIC + bytes([kind, 0, 0, 0]) + struct.pack("<II", seq & 0xFFFFFFFF, time_ms & 0xFFFFFFFF)


def encode_text(kind: int, seq: int, time_ms: int, text: str) -> bytes:
    """A State or Stats message."""
    return wire_header(kind, seq, time_ms) + text.encode("utf-8")


def encode_layer(seq: int, time_ms: int, layer: int, replace: bool, box, splats: bytes) -> bytes:
    """A layer message from quantized splats (16 bytes each) and the box
    (min x, y, z, max x, y, z) their positions are quantized in."""
    assert len(splats) % SPLAT_SIZE == 0
    body = bytes([layer, 0 if replace else 1, 0, 0]) + struct.pack("<I", len(splats) // SPLAT_SIZE) + struct.pack("<6f", *box)
    return wire_header(WIRE_LAYER, seq, time_ms) + body + splats


def encode_preview(seq: int, time_ms: int, camera: int, width: int, height: int, rgb: bytes, depth_mm: bytes) -> bytes:
    assert len(rgb) == width * height * 3 and len(depth_mm) == width * height * 2
    return wire_header(WIRE_PREVIEW, seq, time_ms) + struct.pack("<BBHHH", camera, 0, width, height, 0) + rgb + depth_mm


def _f32(value: float) -> float:
    return struct.unpack("<f", struct.pack("<f", value))[0]


def merge_layers(layer: int, chunks) -> bytes:
    """One Replace message with the splats of `chunks` (layer messages, a
    Replace or the first Append followed by Appends). Several chunks
    quantize positions in different boxes, so positions are requantized
    into the union of the boxes; the other ten bytes of a splat are copied."""
    chunks = [c for c in chunks if classify(c)]
    if not chunks:
        return encode_layer(0, 0, layer, True, (0.0,) * 6, b"")
    if len(chunks) == 1:
        only = bytearray(chunks[0])
        only[17] = 0
        return bytes(only)
    boxes = [struct.unpack_from("<6f", c, 24) for c in chunks]
    filled = [b for b, c in zip(boxes, chunks) if len(c) > 48]
    if not filled:
        union = (0.0,) * 6
    else:
        union = tuple(min(b[k] for b in filled) for k in range(3)) + tuple(max(b[k] for b in filled) for k in range(3, 6))
    out = bytearray()
    for box, chunk in zip(boxes, chunks):
        splats = chunk[48:]
        if box == union:
            out += splats
            continue
        for at in range(0, len(splats), SPLAT_SIZE):
            pos = struct.unpack_from("<3H", splats, at)
            moved = []
            for k in range(3):
                t = _f32(pos[k] / 65535.0)
                p = _f32(_f32(box[k] * _f32(1.0 - t)) + _f32(box[k + 3] * t))
                extent = _f32(union[k + 3] - union[k])
                scale = _f32(65535.0 / extent) if extent > 0.0 else 0.0
                moved.append(max(0, min(65535, int(_f32(_f32(_f32(p - union[k]) * scale) + 0.5)))))
            out += struct.pack("<3H", *moved) + splats[at + 6:at + SPLAT_SIZE]
    header = chunks[-1][:16]
    body = bytes([layer, 0, 0, 0]) + struct.pack("<I", len(out) // SPLAT_SIZE) + struct.pack("<6f", *union)
    return header + body + bytes(out)


# --------------------------------------------------------------------- file

def deflate_raw(data: bytes, level: int = 1) -> bytes:
    compressor = zlib.compressobj(level, zlib.DEFLATED, -15)
    return compressor.compress(data) + compressor.flush()


def inflate_raw(data: bytes) -> bytes:
    """Inflates a raw deflate stream of at most MAX_INFLATED bytes."""
    inflater = zlib.decompressobj(-15)
    try:
        out = inflater.decompress(data, MAX_INFLATED + 1)
    except zlib.error as error:
        raise FormatError("bad_deflate", str(error)) from error
    if len(out) > MAX_INFLATED or not inflater.eof:
        raise FormatError("bad_deflate", "stream too long or cut short")
    return out


def parse_header(data: bytes):
    """The JSON header of a recording and the offset of its first record."""
    seen = min(len(data), len(MAGIC))
    if data[:seen] != MAGIC[:seen]:
        raise FormatError("bad_magic")
    if len(data) < 12:
        raise FormatError("too_short")
    (length,) = struct.unpack_from("<I", data, 8)
    if length > MAX_HEADER_JSON:
        raise FormatError("bad_header", "header of %d bytes" % length)
    if len(data) < 12 + length:
        raise FormatError("too_short")
    try:
        header = loads_strict(data[12:12 + length].decode("utf-8", "strict"))
    except (UnicodeDecodeError, ValueError) as error:
        raise FormatError("bad_header", str(error)) from error
    if not isinstance(header, dict):
        raise FormatError("bad_header", "not a JSON object")

    def text(key):
        value = header.get(key)
        return value if isinstance(value, str) else ""

    protocol = header.get("protocol")
    if isinstance(protocol, bool) or not isinstance(protocol, int) or protocol < 0:
        protocol = 0
    duration = header.get("duration_s")
    if isinstance(duration, bool) or not isinstance(duration, (int, float)):
        duration = 0.0
    duration = float(duration)
    if not math.isfinite(duration) or duration < 0.0:
        duration = 0.0
    info = {
        "name": text("name"),
        "created": text("created"),
        "scene": text("scene"),
        "protocol": min(protocol, 0xFFFFFFFF),
        "duration_s": duration,
    }
    return info, 12 + length


def encode_file_header(name: str, created: str, scene: str, duration_s: float, pad_to: int = HEADER_JSON_SIZE) -> bytes:
    header = {"name": name, "created": created, "scene": scene, "protocol": PROTOCOL, "duration_s": duration_s}
    text = json.dumps(header, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
    if len(text) > max(pad_to, 0) > 0:
        raise ValueError("header of %d bytes does not fit into %d" % (len(text), pad_to))
    text = text.ljust(pad_to, b" ")
    return MAGIC + struct.pack("<I", len(text)) + text


def encode_record(kind: int, time: float, payload: bytes, compress: bool = False) -> bytes:
    flags = 0
    if compress:
        packed = deflate_raw(payload)
        if len(packed) < len(payload):
            payload, flags = packed, FLAG_DEFLATE
    return struct.pack("<IBBHd", len(payload) + 12, kind, flags, 0, time) + payload


def encode_keyframe(messages) -> bytes:
    """The payload of a keyframe record."""
    return b"".join(struct.pack("<I", len(m)) + m for m in messages)


def split_keyframe(payload: bytes):
    """The messages of a keyframe payload; FormatError("bad_keyframe")
    unless it is State, layer 0 Replace, layer 1 Replace, then Stats and
    Preview messages only."""
    messages = []
    at = 0
    while at < len(payload):
        if len(payload) - at < 4:
            raise FormatError("bad_keyframe", "cut inside a length")
        (length,) = struct.unpack_from("<I", payload, at)
        body = payload[at + 4:at + 4 + length]
        if len(body) != length:
            raise FormatError("bad_keyframe", "cut inside a message")
        kind = classify(body)
        expected = {0: kind == ("state",), 1: kind == ("layer", 0, True), 2: kind == ("layer", 1, True)}.get(
            len(messages), kind is not None and kind[0] in ("stats", "preview"))
        if not expected:
            raise FormatError("bad_keyframe", "message %d is %r" % (len(messages), kind))
        messages.append(body)
        at += 4 + length
    if len(messages) < 3:
        raise FormatError("bad_keyframe", "fewer than three messages")
    return messages


def parse_index(payload: bytes):
    """The (time, offset) entries of an index payload."""
    if len(payload) % 16:
        raise FormatError("bad_index", "length %d" % len(payload))
    entries = []
    for at in range(0, len(payload), 16):
        time, offset = struct.unpack_from("<dQ", payload, at)
        ordered = not entries or (time >= entries[-1][0] and offset > entries[-1][1])
        if not math.isfinite(time) or time < 0.0 or not ordered:
            raise FormatError("bad_index", "entry %d" % (at // 16))
        entries.append((time, offset))
    return entries


def peek_record(data: bytes, pos: int):
    """The fixed fields of the record at `pos`: (kind, flags, time, payload
    length). None when fewer than 16 bytes are left; FormatError when the
    record breaks the rules (a bad length is reported from 4 bytes)."""
    if len(data) - pos < 4:
        return None
    (length,) = struct.unpack_from("<I", data, pos)
    if not 12 <= length <= MAX_RECORD_LEN:
        raise FormatError("bad_length", "record length %d" % length)
    if len(data) - pos < RECORD_HEADER_SIZE:
        return None
    kind, flags, _, time = struct.unpack_from("<BBHd", data, pos + 4)
    if kind > KIND_INDEX:
        raise FormatError("bad_kind", "record kind %d" % kind)
    if flags & ~FLAG_DEFLATE:
        raise FormatError("bad_flags", "record flags %#x" % flags)
    if not math.isfinite(time) or time < 0.0:
        raise FormatError("bad_time", "record time %r" % time)
    return kind, flags, time, length - 12


def iter_records(data: bytes):
    """Yields (offset, kind, flags, time, stored payload) for every
    complete record, in file order, until the index, the end of the data or
    damage to the framing. Payloads are not inflated."""
    _, pos = parse_header(data)
    last = 0.0
    while True:
        try:
            head = peek_record(data, pos)
        except FormatError:
            return
        if head is None:
            return
        kind, flags, time, length = head
        if len(data) - pos < RECORD_HEADER_SIZE + length or time < last:
            return
        yield pos, kind, flags, time, data[pos + RECORD_HEADER_SIZE:pos + RECORD_HEADER_SIZE + length]
        if kind == KIND_INDEX:
            return
        last = time
        pos += RECORD_HEADER_SIZE + length


def verify(data: bytes) -> dict:
    """Reads a whole recording and checks everything in it. Raises
    FormatError when the file header is unusable (too_short, bad_magic,
    bad_header); everything else is listed in "problems" as (offset, code),
    in file order."""
    info, first_record = parse_header(data)
    report = {
        "info": info, "bytes": len(data), "complete": False, "truncated": False, "records": 0,
        "states": 0, "layers": 0, "stats": 0, "previews": 0, "keyframes": [], "duration_s": 0.0,
        "stored_bytes": 0, "raw_bytes": 0, "problems": [],
    }
    problems = report["problems"]
    keyframe_records = []
    pos = first_record
    last = 0.0
    while True:
        try:
            head = peek_record(data, pos)
        except FormatError as error:
            problems.append((pos, error.code))
            break
        if head is None or len(data) - pos < RECORD_HEADER_SIZE + head[3]:
            report["truncated"] = True
            break
        kind, flags, time, length = head
        if time < last:
            problems.append((pos, "bad_time"))
            break
        last = time
        report["duration_s"] = time
        if report["records"] == 0 and kind != KIND_KEYFRAME:
            problems.append((pos, "bad_keyframe"))
        report["records"] += 1
        stored = data[pos + RECORD_HEADER_SIZE:pos + RECORD_HEADER_SIZE + length]
        report["stored_bytes"] += length
        if kind == KIND_KEYFRAME:
            keyframe_records.append((time, pos))
        try:
            payload = inflate_raw(stored) if flags & FLAG_DEFLATE else stored
            report["raw_bytes"] += len(payload)
            if kind == KIND_MESSAGE:
                what = classify(payload)
                if what is None:
                    raise FormatError("bad_message")
                report[{"state": "states", "layer": "layers", "stats": "stats", "preview": "previews"}[what[0]]] += 1
            elif kind == KIND_KEYFRAME:
                split_keyframe(payload)
                report["keyframes"].append((time, pos))
            else:
                listed = parse_index(payload)
                same = len(listed) == len(keyframe_records) and all(
                    struct.pack("<dQ", *a) == struct.pack("<dQ", *b) for a, b in zip(listed, keyframe_records))
                if not same:
                    raise FormatError("bad_index", "the index does not list the keyframe records")
        except FormatError as error:
            problems.append((pos, error.code))
        index_at = pos
        pos += RECORD_HEADER_SIZE + length
        if kind == KIND_INDEX:
            trailer = data[pos:]
            if len(trailer) < TRAILER_SIZE:
                report["truncated"] = True
            else:
                found = len(problems)
                if struct.unpack_from("<Q", trailer, 0)[0] != index_at or trailer[8:TRAILER_SIZE] != END_MAGIC:
                    problems.append((pos, "bad_index"))
                if len(trailer) > TRAILER_SIZE:
                    problems.append((pos + TRAILER_SIZE, "trailing_data"))
                index_ok = not any(offset == index_at for offset, _ in problems)
                report["complete"] = index_ok and len(problems) == found
            break
    if not report["keyframes"]:
        problems.append((first_record, "empty"))
    return report


def is_valid(report: dict) -> bool:
    return report["complete"] and not report["problems"]


def report_json(path_or_data) -> str:
    """One line of JSON describing a file: what `verify --json` prints."""
    data = path_or_data if isinstance(path_or_data, (bytes, bytearray)) else _read(path_or_data)
    try:
        report = verify(bytes(data))
    except FormatError as error:
        return json.dumps({"error": error.code})
    return json.dumps(report)


# -------------------------------------------------------------------- synth

class _Lcg:
    """A small deterministic generator (the same numbers on every Python)."""

    def __init__(self, seed: int) -> None:
        self.state = seed & 0xFFFFFFFFFFFFFFFF

    def next(self) -> int:
        self.state = (self.state * 6364136223846793005 + 1442695040888963407) & 0xFFFFFFFFFFFFFFFF
        return self.state >> 33

    def below(self, n: int) -> int:
        return self.next() % n


def synth_splats(rng: _Lcg, count: int) -> bytes:
    """`count` quantized splats with valid fields everywhere."""
    out = bytearray()
    for _ in range(count):
        rot = rng.below(161) + 161 * rng.below(161) + 161 * 161 * rng.below(161) + (rng.below(4) << 22)
        out += struct.pack("<3H", rng.below(65536), rng.below(65536), rng.below(65536))
        out += bytes([90 + rng.below(40), 90 + rng.below(40), 20 + rng.below(20)])
        out += bytes([rng.below(256), rng.below(256), rng.below(256), 216])
        out += rot.to_bytes(3, "little")
    return bytes(out)


def synth_recording(seconds: float = 4.0, hz: float = 10.0, splats: int = 200, seed: int = 1, compress: bool = True,
                    keyframe_every: float = 1.0, name: str = "synth", created: str = "2026-11-14T10:00:00Z") -> bytes:
    """A small recording of an invented session: a static layer that grows
    by appends and is replaced once, a live layer replaced every tick, two
    thumbnails, statistics; a keyframe every `keyframe_every` seconds."""
    rng = _Lcg(seed)
    seq = 0

    def stamp(time):
        nonlocal seq
        seq += 1
        return seq, int(time * 1000)

    def box(center):
        return (center - 2.0, 0.0, -3.0 - center, center + 2.0, 2.5, 3.0)

    state = encode_text(WIRE_STATE, *stamp(0.0), json.dumps({"version": PROTOCOL, "scene": "synth", "seed": seed}))
    static = [encode_layer(*stamp(0.0), 0, True, box(0.0), synth_splats(rng, splats))]
    live = encode_layer(*stamp(0.0), 1, True, box(0.5), synth_splats(rng, splats // 4))
    stats = encode_text(WIRE_STATS, *stamp(0.0), json.dumps({"tick": 0, "splats_static": splats}))
    previews = {}

    def keyframe(time):
        return encode_record(KIND_KEYFRAME, time, encode_keyframe(
            [state, merge_layers(0, static), live, stats] + [previews[k] for k in sorted(previews)]), compress)

    out = bytearray(encode_file_header(name, created, "synth", 0.0))
    index = [(0.0, len(out))]
    out += keyframe(0.0)
    last_keyframe = 0.0
    time = 0.0
    ticks = int(round(seconds * hz))
    for tick in range(1, ticks + 1):
        time = tick / hz
        if tick == ticks // 2:
            static = [encode_layer(*stamp(time), 0, True, box(0.25), synth_splats(rng, splats + 7))]
            out += encode_record(KIND_MESSAGE, time, static[0], compress)
        elif tick % 4 == 0:
            static.append(encode_layer(*stamp(time), 0, False, box(tick * 0.125), synth_splats(rng, splats // 10)))
            out += encode_record(KIND_MESSAGE, time, static[-1], compress)
        live = encode_layer(*stamp(time), 1, True, box(0.5), synth_splats(rng, splats // 4 + tick % 5))
        out += encode_record(KIND_MESSAGE, time, live, compress)
        camera = tick % 2
        pixels = bytes(rng.below(256) for _ in range(6 * 4 * 5))
        previews[camera] = encode_preview(*stamp(time), camera, 6, 4, pixels[:72], pixels[72:])
        out += encode_record(KIND_MESSAGE, time, previews[camera], compress)
        stats = encode_text(WIRE_STATS, *stamp(time), json.dumps({"tick": tick, "splats_static": sum(len(c) - 48 for c in static) // 16}))
        out += encode_record(KIND_MESSAGE, time, stats, compress)
        if time - last_keyframe >= keyframe_every and tick != ticks:
            index.append((time, len(out)))
            out += keyframe(time)
            last_keyframe = time
    index_at = len(out)
    out += encode_record(KIND_INDEX, time, b"".join(struct.pack("<dQ", t, o) for t, o in index))
    out += struct.pack("<Q", index_at) + END_MAGIC
    header = encode_file_header(name, created, "synth", time)
    out[:len(header)] = header
    return bytes(out)


# ---------------------------------------------------------------------- CLI

def _read(path: str) -> bytes:
    with open(path, "rb") as handle:
        return handle.read()


def _summary(path: str, report: dict) -> str:
    info = report["info"]
    state = "complete" if report["complete"] else ("cut short" if report["truncated"] else "damaged")
    ratio = report["stored_bytes"] / report["raw_bytes"] if report["raw_bytes"] else 1.0
    lines = [
        "%s: %s, %d bytes, %.3f s" % (path, state, report["bytes"], report["duration_s"]),
        "  name %r, scene %r, created %s, protocol %d, header duration %.3f s" % (
            info["name"], info["scene"], info["created"] or "?", info["protocol"], info["duration_s"]),
        "  %d records: %d keyframes, %d state, %d layer, %d stats, %d preview messages" % (
            report["records"], len(report["keyframes"]), report["states"], report["layers"], report["stats"], report["previews"]),
        "  payloads: %d bytes stored, %d inflated (%.0f %%)" % (report["stored_bytes"], report["raw_bytes"], ratio * 100.0),
    ]
    if report["keyframes"]:
        times = ", ".join("%.2f" % t for t, _ in report["keyframes"][:12])
        lines.append("  keyframes at %s%s s" % (times, ", ..." if len(report["keyframes"]) > 12 else ""))
    return "\n".join(lines)


def _cmd_info(args) -> int:
    status = 0
    for path in args.files:
        try:
            print(_summary(path, verify(_read(path))))
        except (FormatError, OSError) as error:
            print("%s: %s" % (path, getattr(error, "code", error)))
            status = 1
    return status


def _cmd_verify(args) -> int:
    status = 0
    for path in args.files:
        try:
            data = _read(path)
        except OSError as error:
            print(json.dumps({"error": "unreadable"}) if args.json else "%s: %s" % (path, error))
            status = 1
            continue
        if args.json:
            line = report_json(data)
            print(line)
            decoded = json.loads(line)
            status |= int("error" in decoded or not is_valid(decoded))
            continue
        try:
            report = verify(data)
        except FormatError as error:
            print("%s: %s" % (path, error.code))
            status = 1
            continue
        print(_summary(path, report))
        for offset, code in report["problems"]:
            print("  problem at byte %d: %s" % (offset, code))
        if not is_valid(report):
            status = 1
        else:
            print("  ok")
    return status


def _cmd_synth(args) -> int:
    data = synth_recording(seconds=args.seconds, hz=args.hz, splats=args.splats, seed=args.seed,
                           compress=not args.no_compress, keyframe_every=args.keyframe_every, name=args.name)
    with open(args.out, "wb") as handle:
        handle.write(data)
    print("%s: %d bytes" % (args.out, len(data)))
    return 0


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description="Reference tool for .gsrec recordings.")
    sub = parser.add_subparsers(dest="command", required=True)
    p = sub.add_parser("info", help="header and summary of recordings")
    p.add_argument("files", nargs="+")
    p.set_defaults(func=_cmd_info)
    p = sub.add_parser("verify", help="check every record (exit code 1 unless all files are complete and flawless)")
    p.add_argument("--json", action="store_true", help="one line of JSON per file")
    p.add_argument("files", nargs="+")
    p.set_defaults(func=_cmd_verify)
    p = sub.add_parser("synth", help="write a small synthetic recording")
    p.add_argument("out")
    p.add_argument("--seconds", type=float, default=4.0)
    p.add_argument("--hz", type=float, default=10.0)
    p.add_argument("--splats", type=int, default=200)
    p.add_argument("--seed", type=int, default=1)
    p.add_argument("--keyframe-every", type=float, default=1.0)
    p.add_argument("--name", default="synth")
    p.add_argument("--no-compress", action="store_true")
    p.set_defaults(func=_cmd_synth)
    args = parser.parse_args(argv)
    return args.func(args)


if __name__ == "__main__":
    sys.exit(main())
