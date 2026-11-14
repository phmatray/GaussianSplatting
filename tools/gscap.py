#!/usr/bin/env python3
"""Reference implementation of the Splat Live capture frame and .gscap file.

Specification: docs/ARCHITECTURE.md, "Real capture: the phone companion".
Standard library only; works with Python 3.8 and later.

Capture frame (one WebSocket binary frame, little-endian, 128-byte header):

    0   4  magic "GSCF"
    4   1  version, 1
    5   1  flags: bit 0 confidence present, bit 1 depth/confidence/mask are
           raw-deflate compressed (RFC 1951), bit 2 mask present
    6   1  tracking: 0 not available, 1 limited, 2 normal
    7   1  display orientation: 0 sensor native (landscape), 1 portrait,
           2 landscape flipped, 3 portrait upside down
    8   4  frame index u32
    12  4  reserved, 0
    16  8  timestamp f64, seconds
    24  64 world_from_cam, 16 x f32, column-major
    88  16 fx, fy, cx, cy as f32 (pixels of the colour image as sent)
    104 4  colour width, height as u16
    108 4  colour byte length u32 (a JPEG)
    112 4  depth width, height as u16
    116 4  depth byte length u32
    120 4  confidence byte length u32 (0 if absent)
    124 4  mask byte length u32 (0 if absent)
    128 .. colour, depth, confidence, mask

Decisions taken where the specification is silent (see also the README of
ios/SplatCapture/Fixtures):

  * With flag bit 1, depth, confidence and mask are three independent raw
    deflate streams; the byte lengths in the header are the stored
    (compressed) sizes. The colour JPEG is never deflated.
  * Flag bit 0 (bit 2) is set exactly when the confidence (mask) length is
    not zero. An absent plane has length 0, compressed or not.
  * Uncompressed sizes are implied: depth = 2 * w * h bytes (u16 LE),
    confidence and mask = w * h bytes. Depth width and height are >= 1.
  * A colour length of 0 means "no colour image"; the colour width and
    height still name the pixel grid the intrinsics refer to.
  * Flag bits 3..7 must be 0, tracking <= 2, orientation <= 3. The reserved
    field is written as 0 and ignored when reading.
  * A frame is exactly 128 + the four lengths bytes long (the sum is taken
    without wrapping: four u32 lengths can exceed 2^32).
  * Implementation limit: a depth map has at most MAX_DEPTH_PIXELS (2^22)
    pixels, so that a small deflated frame cannot ask a reader for gigabytes
    (65535 x 65535 would inflate to 8.6 GB). ARKit delivers 256 x 192.

.gscap file: 8 bytes "GSCAP1\\0\\0", u32 LE length + JSON header, then for
every frame a u32 LE length and the frame exactly as on the wire. The JSON
header may be followed by ASCII spaces inside its length (the phone reserves
room to rewrite it in place when the recording ends).

Command line:

    gscap.py info FILE            summary of a .gscap or of a single frame
    gscap.py verify FILE          decode everything, report problems
    gscap.py extract FILE OUTDIR  JPEG, PNG depth and planes, JSON per frame
    gscap.py synth OUT.gscap      a synthetic walk inside a box room
"""

from __future__ import annotations

import argparse
import datetime
import hashlib
import json
import math
import os
import struct
import sys
import zlib

FRAME_MAGIC = b"GSCF"
FRAME_VERSION = 1
HEADER_SIZE = 128
HEADER_STRUCT = struct.Struct("<4sBBBBIId16f4fHHIHHIII")
assert HEADER_STRUCT.size == HEADER_SIZE

FLAG_CONFIDENCE = 1
FLAG_DEFLATE = 2
FLAG_MASK = 4
KNOWN_FLAGS = FLAG_CONFIDENCE | FLAG_DEFLATE | FLAG_MASK

TRACKING_NOT_AVAILABLE = 0
TRACKING_LIMITED = 1
TRACKING_NORMAL = 2
TRACKING_NAMES = ("not_available", "limited", "normal")

ORIENTATION_NAMES = ("landscape_native", "portrait", "landscape_flipped", "portrait_upside_down")

FILE_MAGIC = b"GSCAP1\x00\x00"
MAX_HEADER_JSON = 1 << 20
MAX_FRAME_BYTES = 64 << 20
MAX_DEPTH_PIXELS = 1 << 22

PROTOCOL = 1
MODES = ("room", "object", "person", "live")

IDENTITY = (1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 0.0, 1.0)


class FormatError(ValueError):
    """A frame, file or pairing URI that does not follow the specification.

    `code` is a stable identifier shared with the Swift implementation and
    with the fixtures.
    """

    def __init__(self, code: str, message: str = "") -> None:
        super().__init__("%s: %s" % (code, message) if message else code)
        self.code = code


def loads_strict(text):
    """json.loads restricted to standard JSON, failing with ValueError only.

    Python's parser also accepts NaN, Infinity and -Infinity, which serde_json
    and JSONSerialization refuse, and raises RecursionError on deeply nested
    input; both become ValueError here so that hostile text is simply invalid.
    """
    def refuse(name):
        raise ValueError("%s is not JSON" % name)

    try:
        return json.loads(text, parse_constant=refuse)
    except RecursionError:
        raise ValueError("JSON nested too deeply")


# --------------------------------------------------------------------------
# Raw deflate
# --------------------------------------------------------------------------


def deflate_raw(data: bytes, level: int = 6) -> bytes:
    """RFC 1951 stream without zlib header or checksum."""
    comp = zlib.compressobj(level, zlib.DEFLATED, -15)
    return comp.compress(data) + comp.flush()


def inflate_raw(data: bytes, expected_size: int) -> bytes:
    """Inflate one raw deflate stream that must yield exactly expected_size bytes."""
    decomp = zlib.decompressobj(-15)
    try:
        out = decomp.decompress(data, expected_size + 1)
    except zlib.error as exc:
        raise FormatError("bad_deflate", str(exc))
    if len(out) != expected_size:
        raise FormatError("bad_deflate", "inflates to %s%d bytes, expected %d" % (
            "more than " if len(out) > expected_size else "", min(len(out), expected_size), expected_size))
    if not decomp.eof:
        raise FormatError("bad_deflate", "stream is not terminated")
    if decomp.unused_data:
        raise FormatError("bad_deflate", "bytes after the end of the stream")
    return out


# --------------------------------------------------------------------------
# Depth conversion
# --------------------------------------------------------------------------


def depth_meters_to_mm(value: float) -> int:
    """One depth sample, float32 meters to u16 millimeters.

    Not positive, not finite or beyond 65.535 m gives 0 ("no value").
    Rounding is floor(m * 1000 + 0.5), computed in double precision.
    """
    if not (value > 0.0) or math.isinf(value):
        return 0
    mm = math.floor(value * 1000.0 + 0.5)
    if mm > 65535:
        return 0
    return int(mm)


def depth_plane_from_meters(values) -> bytes:
    """Sequence of meters (row-major) to the u16 LE plane of a frame."""
    mm = [depth_meters_to_mm(v) for v in values]
    return struct.pack("<%dH" % len(mm), *mm)


def depth_plane_to_mm(plane: bytes) -> list:
    return list(struct.unpack("<%dH" % (len(plane) // 2), plane))


# --------------------------------------------------------------------------
# Frame
# --------------------------------------------------------------------------


class Frame:
    """A decoded capture frame. Planes are held uncompressed."""

    __slots__ = (
        "index", "timestamp", "world_from_cam", "intrinsics", "tracking", "orientation",
        "color_width", "color_height", "color", "depth_width", "depth_height", "depth",
        "confidence", "mask", "compressed", "reserved",
    )

    def __init__(
        self,
        index: int = 0,
        timestamp: float = 0.0,
        world_from_cam=IDENTITY,
        intrinsics=(1.0, 1.0, 0.0, 0.0),
        tracking: int = TRACKING_NORMAL,
        orientation: int = 0,
        color_width: int = 0,
        color_height: int = 0,
        color: bytes = b"",
        depth_width: int = 1,
        depth_height: int = 1,
        depth: bytes = b"\x00\x00",
        confidence=None,
        mask=None,
        compressed: bool = False,
        reserved: int = 0,
    ) -> None:
        self.index = index
        self.timestamp = timestamp
        self.world_from_cam = tuple(world_from_cam)
        self.intrinsics = tuple(intrinsics)
        self.tracking = tracking
        self.orientation = orientation
        self.color_width = color_width
        self.color_height = color_height
        self.color = bytes(color)
        self.depth_width = depth_width
        self.depth_height = depth_height
        self.depth = bytes(depth)
        self.confidence = None if confidence is None else bytes(confidence)
        self.mask = None if mask is None else bytes(mask)
        self.compressed = compressed
        self.reserved = reserved

    @property
    def flags(self) -> int:
        flags = 0
        if self.confidence is not None:
            flags |= FLAG_CONFIDENCE
        if self.compressed:
            flags |= FLAG_DEFLATE
        if self.mask is not None:
            flags |= FLAG_MASK
        return flags

    def depth_mm(self) -> list:
        return depth_plane_to_mm(self.depth)

    def position(self) -> tuple:
        m = self.world_from_cam
        return (m[12], m[13], m[14])

    def describe(self) -> dict:
        """JSON-friendly description with exact bit patterns (used by fixtures)."""
        return {
            "flags": self.flags,
            "compressed": self.compressed,
            "tracking": self.tracking,
            "orientation": self.orientation,
            "index": self.index,
            "timestamp": self.timestamp,
            "timestamp_bits": "%016x" % struct.unpack("<Q", struct.pack("<d", self.timestamp))[0],
            "world_from_cam": list(self.world_from_cam),
            "world_from_cam_bits": [_f32_bits(v) for v in self.world_from_cam],
            "intrinsics": list(self.intrinsics),
            "intrinsics_bits": [_f32_bits(v) for v in self.intrinsics],
            "color": {
                "width": self.color_width,
                "height": self.color_height,
                "length": len(self.color),
                "sha256": hashlib.sha256(self.color).hexdigest(),
            },
            "depth": _describe_plane(self.depth, self.depth_width, self.depth_height, True),
            "confidence": None if self.confidence is None
            else _describe_plane(self.confidence, self.depth_width, self.depth_height, False),
            "mask": None if self.mask is None
            else _describe_plane(self.mask, self.depth_width, self.depth_height, False),
        }


def _f32_bits(value: float) -> str:
    return "%08x" % struct.unpack("<I", struct.pack("<f", value))[0]


def _describe_plane(plane: bytes, width: int, height: int, is_depth: bool) -> dict:
    out = {"width": width, "height": height, "sha256": hashlib.sha256(plane).hexdigest()}
    if width * height <= 64:
        out["values"] = depth_plane_to_mm(plane) if is_depth else list(plane)
    return out


def _check_u(name: str, value: int, bits: int) -> int:
    if not isinstance(value, int) or not (0 <= value < (1 << bits)):
        raise FormatError("bad_value", "%s does not fit in %d bits: %r" % (name, bits, value))
    return value


def encode_frame(frame: Frame, compress=None, level: int = 6) -> bytes:
    """Serialise a frame. `compress` defaults to frame.compressed."""
    if compress is None:
        compress = frame.compressed
    pixels = frame.depth_width * frame.depth_height
    if frame.depth_width < 1 or frame.depth_height < 1:
        raise FormatError("bad_dimensions", "depth must be at least 1x1")
    if pixels > MAX_DEPTH_PIXELS:
        raise FormatError("bad_dimensions", "depth of more than %d pixels" % MAX_DEPTH_PIXELS)
    if len(frame.depth) != 2 * pixels:
        raise FormatError("bad_length", "depth plane must be %d bytes" % (2 * pixels))
    if frame.confidence is not None and len(frame.confidence) != pixels:
        raise FormatError("bad_length", "confidence plane must be %d bytes" % pixels)
    if frame.mask is not None and len(frame.mask) != pixels:
        raise FormatError("bad_length", "mask plane must be %d bytes" % pixels)
    if len(frame.world_from_cam) != 16 or len(frame.intrinsics) != 4:
        raise FormatError("bad_value", "world_from_cam needs 16 values, intrinsics 4")
    if frame.tracking > 2:
        raise FormatError("bad_tracking")
    if frame.orientation > 3:
        raise FormatError("bad_orientation")

    flags = 0
    depth = frame.depth
    confidence = b""
    mask = b""
    if frame.confidence is not None:
        flags |= FLAG_CONFIDENCE
        confidence = frame.confidence
    if frame.mask is not None:
        flags |= FLAG_MASK
        mask = frame.mask
    if compress:
        flags |= FLAG_DEFLATE
        depth = deflate_raw(depth, level)
        confidence = deflate_raw(confidence, level) if confidence else b""
        mask = deflate_raw(mask, level) if mask else b""

    header = HEADER_STRUCT.pack(
        FRAME_MAGIC,
        FRAME_VERSION,
        flags,
        _check_u("tracking", frame.tracking, 8),
        _check_u("orientation", frame.orientation, 8),
        _check_u("index", frame.index, 32),
        _check_u("reserved", frame.reserved, 32),
        frame.timestamp,
        *frame.world_from_cam,
        *frame.intrinsics,
        _check_u("color_width", frame.color_width, 16),
        _check_u("color_height", frame.color_height, 16),
        _check_u("color length", len(frame.color), 32),
        _check_u("depth_width", frame.depth_width, 16),
        _check_u("depth_height", frame.depth_height, 16),
        len(depth),
        len(confidence),
        len(mask),
    )
    return header + frame.color + depth + confidence + mask


def parse_header(data: bytes) -> dict:
    """Decode and validate the 128-byte header only (no payload access)."""
    if len(data) < HEADER_SIZE:
        raise FormatError("too_short", "%d bytes, a header is %d" % (len(data), HEADER_SIZE))
    fields = HEADER_STRUCT.unpack_from(data, 0)
    magic, version, flags, tracking, orientation, index, reserved, timestamp = fields[:8]
    if magic != FRAME_MAGIC:
        raise FormatError("bad_magic", repr(magic))
    if version != FRAME_VERSION:
        raise FormatError("bad_version", str(version))
    if flags & ~KNOWN_FLAGS:
        raise FormatError("bad_flags", "0x%02x" % flags)
    if tracking > 2:
        raise FormatError("bad_tracking", str(tracking))
    if orientation > 3:
        raise FormatError("bad_orientation", str(orientation))
    header = {
        "flags": flags,
        "tracking": tracking,
        "orientation": orientation,
        "index": index,
        "reserved": reserved,
        "timestamp": timestamp,
        "world_from_cam": fields[8:24],
        "intrinsics": fields[24:28],
        "color_width": fields[28],
        "color_height": fields[29],
        "color_length": fields[30],
        "depth_width": fields[31],
        "depth_height": fields[32],
        "depth_length": fields[33],
        "confidence_length": fields[34],
        "mask_length": fields[35],
    }
    if header["depth_width"] < 1 or header["depth_height"] < 1:
        raise FormatError("bad_dimensions", "depth %dx%d" % (header["depth_width"], header["depth_height"]))
    if header["depth_width"] * header["depth_height"] > MAX_DEPTH_PIXELS:
        raise FormatError("bad_dimensions", "depth %dx%d is more than %d pixels" % (
            header["depth_width"], header["depth_height"], MAX_DEPTH_PIXELS))
    has_conf = bool(flags & FLAG_CONFIDENCE)
    has_mask = bool(flags & FLAG_MASK)
    if has_conf != (header["confidence_length"] != 0):
        raise FormatError("bad_length", "confidence flag and length disagree")
    if has_mask != (header["mask_length"] != 0):
        raise FormatError("bad_length", "mask flag and length disagree")
    pixels = header["depth_width"] * header["depth_height"]
    if not flags & FLAG_DEFLATE:
        if header["depth_length"] != 2 * pixels:
            raise FormatError("bad_length", "depth is %d bytes, expected %d" % (header["depth_length"], 2 * pixels))
        if has_conf and header["confidence_length"] != pixels:
            raise FormatError("bad_length", "confidence is %d bytes, expected %d" % (header["confidence_length"], pixels))
        if has_mask and header["mask_length"] != pixels:
            raise FormatError("bad_length", "mask is %d bytes, expected %d" % (header["mask_length"], pixels))
    elif header["depth_length"] == 0:
        raise FormatError("bad_length", "compressed depth is empty")
    header["total_length"] = (
        HEADER_SIZE + header["color_length"] + header["depth_length"]
        + header["confidence_length"] + header["mask_length"]
    )
    return header


def decode_frame(data: bytes) -> Frame:
    """Parse one capture frame; raises FormatError (with a code) if invalid."""
    data = bytes(data)
    h = parse_header(data)
    if len(data) != h["total_length"]:
        raise FormatError("bad_length", "frame is %d bytes, header announces %d" % (len(data), h["total_length"]))
    pos = HEADER_SIZE
    color = data[pos : pos + h["color_length"]]
    pos += h["color_length"]
    depth = data[pos : pos + h["depth_length"]]
    pos += h["depth_length"]
    confidence = data[pos : pos + h["confidence_length"]]
    pos += h["confidence_length"]
    mask = data[pos : pos + h["mask_length"]]
    pixels = h["depth_width"] * h["depth_height"]
    compressed = bool(h["flags"] & FLAG_DEFLATE)
    if compressed:
        depth = inflate_raw(depth, 2 * pixels)
        if confidence:
            confidence = inflate_raw(confidence, pixels)
        if mask:
            mask = inflate_raw(mask, pixels)
    return Frame(
        index=h["index"],
        timestamp=h["timestamp"],
        world_from_cam=h["world_from_cam"],
        intrinsics=h["intrinsics"],
        tracking=h["tracking"],
        orientation=h["orientation"],
        color_width=h["color_width"],
        color_height=h["color_height"],
        color=color,
        depth_width=h["depth_width"],
        depth_height=h["depth_height"],
        depth=depth,
        confidence=confidence if h["flags"] & FLAG_CONFIDENCE else None,
        mask=mask if h["flags"] & FLAG_MASK else None,
        compressed=compressed,
        reserved=h["reserved"],
    )


# --------------------------------------------------------------------------
# .gscap file
# --------------------------------------------------------------------------


def now_rfc3339() -> str:
    return datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def make_header(mode: str = "room", name: str = "", device: str = "python", app: str = "0.1.0",
                created=None, region=None, floor=None) -> dict:
    """The JSON header of a .gscap: the hello fields (without `type` and
    `token`, which only make sense on a live connection) plus `created`."""
    header = {
        "protocol": PROTOCOL,
        "device": device,
        "app": app,
        "mode": mode,
        "name": name,
        "created": created or now_rfc3339(),
    }
    if region is not None:
        header["region"] = {"center": [float(v) for v in region["center"]], "radius": float(region["radius"])}
    if floor is not None:
        header["floor"] = {"y": float(floor["y"] if isinstance(floor, dict) else floor)}
    return header


def header_floor_y(header: dict):
    """Floor height from a header; accepts {"y": h} and a bare number."""
    floor = header.get("floor")
    if isinstance(floor, dict):
        floor = floor.get("y")
    if isinstance(floor, bool) or not isinstance(floor, (int, float)):
        return None
    return float(floor)


def header_region(header: dict):
    """(center tuple, radius) from a header, or None."""
    region = header.get("region")
    if not isinstance(region, dict):
        return None
    center = region.get("center")
    radius = region.get("radius")
    if (not isinstance(center, list) or len(center) != 3
            or not all(isinstance(v, (int, float)) and not isinstance(v, bool) for v in center)
            or isinstance(radius, bool) or not isinstance(radius, (int, float))):
        return None
    return (tuple(float(v) for v in center), float(radius))


def encode_file_header(header: dict, pad_to: int = 0) -> bytes:
    """File magic + length + JSON, optionally padded with spaces to pad_to bytes of JSON."""
    body = json.dumps(header, ensure_ascii=False, separators=(",", ":"), sort_keys=True).encode("utf-8")
    if len(body) < pad_to:
        body += b" " * (pad_to - len(body))
    if len(body) > MAX_HEADER_JSON:
        raise FormatError("bad_header", "header JSON is too large")
    return FILE_MAGIC + struct.pack("<I", len(body)) + body


class GscapWriter:
    """Writes a .gscap. Use as a context manager."""

    def __init__(self, path: str, header: dict, pad_to: int = 0) -> None:
        self._file = open(path, "wb")
        self._pad_to = pad_to
        self._file.write(encode_file_header(header, pad_to))
        self.frames = 0

    def append(self, frame_bytes: bytes) -> None:
        if len(frame_bytes) < HEADER_SIZE or len(frame_bytes) > MAX_FRAME_BYTES:
            raise FormatError("bad_length", "a frame is between 128 bytes and 64 MiB")
        self._file.write(struct.pack("<I", len(frame_bytes)))
        self._file.write(frame_bytes)
        self.frames += 1

    def rewrite_header(self, header: dict) -> None:
        """Replace the header in place; needs a writer created with pad_to."""
        data = encode_file_header(header, self._pad_to)
        if len(data) != len(FILE_MAGIC) + 4 + self._pad_to:
            raise FormatError("bad_header", "the new header does not fit in the reserved %d bytes" % self._pad_to)
        position = self._file.tell()
        self._file.seek(0)
        self._file.write(data)
        self._file.seek(position)

    def close(self) -> None:
        self._file.close()

    def __enter__(self) -> "GscapWriter":
        return self

    def __exit__(self, *exc) -> None:
        self.close()


class GscapReader:
    """Reads a .gscap from a path or a binary file object.

    Iterating yields the raw bytes of every complete frame. A file that ends
    in the middle of a frame (a recording cut short) is not an error:
    iteration stops and `truncated` becomes True.
    """

    def __init__(self, source) -> None:
        self._own = isinstance(source, (str, bytes, os.PathLike))
        self._file = open(source, "rb") if self._own else source
        try:
            magic = self._file.read(len(FILE_MAGIC))
            if magic != FILE_MAGIC:
                raise FormatError("bad_file_magic", repr(magic))
            raw = self._file.read(4)
            if len(raw) != 4:
                raise FormatError("bad_header", "file ends inside the header length")
            (length,) = struct.unpack("<I", raw)
            if length > MAX_HEADER_JSON:
                raise FormatError("bad_header", "header JSON of %d bytes" % length)
            body = self._file.read(length)
            if len(body) != length:
                raise FormatError("bad_header", "file ends inside the header")
            try:
                header = loads_strict(body.decode("utf-8"))
            except (UnicodeDecodeError, ValueError) as exc:
                raise FormatError("bad_header", str(exc))
            if not isinstance(header, dict):
                raise FormatError("bad_header", "the header is not a JSON object")
        except Exception:
            self.close()
            raise
        self.header = header
        self.frames_offset = len(FILE_MAGIC) + 4 + length
        self.truncated = False
        self.trailing_bytes = 0

    def __iter__(self):
        self._file.seek(self.frames_offset)
        self.truncated = False
        self.trailing_bytes = 0
        while True:
            raw = self._file.read(4)
            if not raw:
                return
            if len(raw) < 4:
                self.truncated = True
                self.trailing_bytes = len(raw)
                return
            (length,) = struct.unpack("<I", raw)
            if length < HEADER_SIZE or length > MAX_FRAME_BYTES:
                raise FormatError("bad_length", "frame record of %d bytes" % length)
            data = self._file.read(length)
            if len(data) < length:
                self.truncated = True
                self.trailing_bytes = 4 + len(data)
                return
            yield data

    def frames(self):
        """Decoded frames."""
        for data in self:
            yield decode_frame(data)

    def close(self) -> None:
        if self._own:
            self._file.close()

    def __enter__(self) -> "GscapReader":
        return self

    def __exit__(self, *exc) -> None:
        self.close()


def write_gscap(path: str, header: dict, frames, pad_to: int = 0) -> int:
    """Write a whole file. `frames` yields Frame objects or encoded bytes."""
    with GscapWriter(path, header, pad_to) as writer:
        for frame in frames:
            writer.append(encode_frame(frame) if isinstance(frame, Frame) else frame)
        return writer.frames


def read_gscap(path: str):
    """(header, list of raw frame bytes, truncated)."""
    with GscapReader(path) as reader:
        frames = list(reader)
        return reader.header, frames, reader.truncated


# --------------------------------------------------------------------------
# Pairing URI
# --------------------------------------------------------------------------

PAIRING_PREFIX = "splatlive://pair"
_HEX_CHARS = set("0123456789abcdefABCDEF")
_HOST_CHARS = set("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789.-")
_IPV6_CHARS = _HEX_CHARS | set(":.")
_ASCII_LOWER = {c: c + 32 for c in range(ord("A"), ord("Z") + 1)}


def _ascii_lower(text: str) -> str:
    """Lowercase A-Z only, so that both implementations agree on every input."""
    return text.translate(_ASCII_LOWER)


class Pairing:
    __slots__ = ("host", "port", "fingerprint", "token")

    def __init__(self, host: str, port: int, fingerprint: str, token: str) -> None:
        self.host = host
        self.port = port
        self.fingerprint = fingerprint
        self.token = token

    @property
    def authority(self) -> str:
        host = "[%s]" % self.host if ":" in self.host else self.host
        return "%s:%d" % (host, self.port)

    @property
    def ingest_url(self) -> str:
        return "wss://%s/ingest" % self.authority

    @property
    def captures_url(self) -> str:
        return "https://%s/api/captures" % self.authority

    @property
    def uri(self) -> str:
        return "%s?host=%s&port=%d&fp=%s&token=%s" % (
            PAIRING_PREFIX, self.host, self.port, self.fingerprint, self.token)

    def __repr__(self) -> str:
        return "Pairing(%r, %d, fp=%s..., token=...)" % (self.host, self.port, self.fingerprint[:8])


def _percent_decode(text: str) -> str:
    out = bytearray()
    try:
        raw = text.encode("utf-8")
    except UnicodeEncodeError:  # a lone surrogate: not text at all
        raise FormatError("bad_encoding", repr(text))
    i = 0
    while i < len(raw):
        if raw[i] == 0x25:
            pair = raw[i + 1 : i + 3]
            if len(pair) != 2 or not all(chr(c) in "0123456789abcdefABCDEF" for c in pair):
                raise FormatError("bad_encoding", text)
            out.append(int(pair.decode("ascii"), 16))
            i += 3
        else:
            out.append(raw[i])
            i += 1
    try:
        return out.decode("utf-8")
    except UnicodeDecodeError:
        raise FormatError("bad_encoding", text)


def normalise_fingerprint(text: str) -> str:
    """64 lowercase hex digits from hex with optional colons, or raise bad_fp."""
    fp = text.replace(":", "")
    if len(fp) != 64 or not set(fp) <= _HEX_CHARS:
        raise FormatError("bad_fp", text)
    return _ascii_lower(fp)


def parse_pairing_uri(text: str) -> Pairing:
    """Parse splatlive://pair?host=..&port=..&fp=..&token=..

    Rules (identical in the Swift implementation):
      1. Leading and trailing spaces, tabs, CR and LF are ignored.
      2. The text is split at the first "?". The part before it must be
         "splatlive://pair", letters A-Z compared without case: bad_scheme.
      3. The query is split on "&"; empty items are skipped; each item is
         split at its first "=" and both halves are percent-decoded (a "%"
         not followed by two hex digits, or bytes that are not UTF-8:
         bad_encoding). "+" is not a space.
      4. host, port, fp and token may appear only once (duplicate_parameter)
         and are all required (missing_host, missing_port, missing_fp,
         missing_token, checked in that order). Other keys are ignored.
      5. host: optional surrounding brackets are removed; 1 to 253 characters;
         either letters, digits, "." and "-" (not at either end), or, if it
         contains ":", hex digits, ":" and "." with at least two ":" (an
         IPv6 literal, no zone). Lowercased. bad_host.
      6. port: 1 to 5 ASCII digits, value 1 to 65535. bad_port.
      7. fp: colons removed, then exactly 64 hex digits, lowercased. bad_fp.
      8. token: exactly 32 hex digits, lowercased. bad_token.
    """
    text = text.strip(" \t\r\n")
    prefix, sep, query = text.partition("?")
    if not sep or _ascii_lower(prefix) != PAIRING_PREFIX:
        raise FormatError("bad_scheme", text[:40])
    params = {}
    for item in query.split("&"):
        if not item:
            continue
        key, _, value = item.partition("=")
        key = _percent_decode(key)
        value = _percent_decode(value)
        if key in ("host", "port", "fp", "token"):
            if key in params:
                raise FormatError("duplicate_parameter", key)
            params[key] = value
    for key in ("host", "port", "fp", "token"):
        if key not in params:
            raise FormatError("missing_" + key)

    host = params["host"]
    if len(host) >= 2 and host.startswith("[") and host.endswith("]"):
        host = host[1:-1]
    if not host or len(host) > 253:
        raise FormatError("bad_host", params["host"])
    if ":" in host:
        # An IPv6 literal has at least two colons; "1.2.3.4:7879" is a mistake.
        if not set(host) <= _IPV6_CHARS or host.count(":") < 2:
            raise FormatError("bad_host", params["host"])
    elif not set(host) <= _HOST_CHARS or host[0] in ".-" or host[-1] in ".-":
        raise FormatError("bad_host", params["host"])
    host = _ascii_lower(host)

    port_text = params["port"]
    if not (1 <= len(port_text) <= 5) or not all(c in "0123456789" for c in port_text):
        raise FormatError("bad_port", port_text)
    port = int(port_text)
    if not (1 <= port <= 65535):
        raise FormatError("bad_port", port_text)

    fingerprint = normalise_fingerprint(params["fp"])

    token = params["token"]
    if len(token) != 32 or not set(token) <= _HEX_CHARS:
        raise FormatError("bad_token")
    return Pairing(host, port, fingerprint, _ascii_lower(token))


def certificate_fingerprint(der: bytes) -> str:
    """SHA-256 of a DER certificate as 64 lowercase hex digits."""
    return hashlib.sha256(der).hexdigest()


# --------------------------------------------------------------------------
# Stream messages (JSON text frames)
# --------------------------------------------------------------------------


def _dumps(obj: dict) -> str:
    return json.dumps(obj, ensure_ascii=False, separators=(",", ":"))


def hello_message(token: str, mode: str, name: str = "", device: str = "python", app: str = "0.1.0") -> str:
    return _dumps({"type": "hello", "protocol": PROTOCOL, "token": token, "device": device,
                   "app": app, "mode": mode, "name": name})


def region_message(center, radius: float) -> str:
    return _dumps({"type": "region", "center": [float(v) for v in center], "radius": float(radius)})


def floor_message(y: float) -> str:
    return _dumps({"type": "floor", "y": float(y)})


def simple_message(kind: str) -> str:
    if kind not in ("pause", "resume", "reset", "finish"):
        raise ValueError(kind)
    return _dumps({"type": kind})


def welcome_message(max_inflight: int = 4) -> str:
    return _dumps({"type": "welcome", "protocol": PROTOCOL, "max_inflight": max_inflight})


def ack_message(frame: int, used: bool) -> str:
    return _dumps({"type": "ack", "frame": frame, "used": bool(used)})


def progress_message(frames: int, splats: int) -> str:
    return _dumps({"type": "progress", "frames": frames, "splats": splats})


def error_message(code: str, message: str) -> str:
    return _dumps({"type": "error", "code": code, "message": message})


def _json_uint(value, limit: int):
    """A JSON number that is a whole value in [0, limit], else None."""
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    if isinstance(value, float):
        if not math.isfinite(value) or value != math.floor(value):
            return None
        value = int(value)
    return value if 0 <= value <= limit else None


def parse_server_message(text: str):
    """Interpret a server text frame the way the phone does.

    Returns None for anything that is not a JSON object with a string
    `type` (or an ack without a usable frame number), otherwise a dict with
    `kind`: welcome (protocol, max_inflight clamped to at least 1, 1 if
    absent), ack (frame, used; used is false if absent), progress (frames,
    splats; 0 if absent), error (code, message; "" if absent) or unknown
    (type), which the phone ignores.
    """
    try:
        obj = loads_strict(text)
    except ValueError:
        return None
    if not isinstance(obj, dict) or not isinstance(obj.get("type"), str):
        return None
    kind = obj["type"]
    if kind == "welcome":
        protocol = _json_uint(obj.get("protocol"), 0xFFFFFFFF)
        inflight = _json_uint(obj.get("max_inflight"), 0xFFFFFFFF)
        return {"kind": "welcome", "protocol": 0 if protocol is None else protocol,
                "max_inflight": max(1, 1 if inflight is None else inflight)}
    if kind == "ack":
        frame = _json_uint(obj.get("frame"), 0xFFFFFFFF)
        if frame is None:
            return None
        return {"kind": "ack", "frame": frame, "used": obj.get("used") is True}
    if kind == "progress":
        frames = _json_uint(obj.get("frames"), (1 << 53))
        splats = _json_uint(obj.get("splats"), (1 << 53))
        return {"kind": "progress", "frames": frames or 0, "splats": splats or 0}
    if kind == "error":
        code = obj.get("code")
        message = obj.get("message")
        return {"kind": "error", "code": code if isinstance(code, str) else "",
                "message": message if isinstance(message, str) else ""}
    return {"kind": "unknown", "type": kind}


# --------------------------------------------------------------------------
# Synthetic capture (a camera turning and walking inside a box room)
# --------------------------------------------------------------------------


def _pose_yaw(position, yaw: float) -> tuple:
    """world_from_cam (column-major) of a level camera turned by yaw around +Y.

    yaw = 0 looks along -Z; a positive yaw turns to the left (towards -X).
    """
    c, s = math.cos(yaw), math.sin(yaw)
    x, y, z = position
    return (c, 0.0, -s, 0.0, 0.0, 1.0, 0.0, 0.0, s, 0.0, c, 0.0, x, y, z, 1.0)


def synth_frames(count: int = 60, depth_size=(64, 48), color_size=(160, 120), fps: float = 10.0,
                 room=(4.0, 2.6, 5.0), with_confidence: bool = True, compress: bool = True):
    """Yield Frame objects of a walk in a box room (floor y = -1.4, eye at 0).

    The room spans x in [-w/2, w/2], y in [-1.4, h - 1.4], z in [-d/2, d/2];
    as in ARKit the origin is where the session started, not the floor.
    Geometry is exact (depth along the camera axis), colours are flat per
    8x8 block and differ per wall, so that a reconstruction can be checked.
    """
    import tinyjpeg  # local module, only needed for synthetic data

    dw, dh = depth_size
    cw, ch = color_size
    half_w, height, half_d = room[0] / 2.0, room[1], room[2] / 2.0
    lo = (-half_w, -1.4, -half_d)
    hi = (half_w, height - 1.4, half_d)
    fov_x = math.radians(60.0)
    fx_d = (dw / 2.0) / math.tan(fov_x / 2.0)
    fx_c = (cw / 2.0) / math.tan(fov_x / 2.0)
    wall_colors = [(200, 80, 60), (60, 160, 200), (120, 120, 120), (230, 220, 200), (90, 170, 90), (220, 190, 80)]

    def cast(origin, direction):
        """Distance to the inside of the box and the index of the wall hit."""
        best_t, best_wall = float("inf"), 0
        for axis in range(3):
            d = direction[axis]
            if abs(d) < 1e-9:
                continue
            bound = hi[axis] if d > 0 else lo[axis]
            t = (bound - origin[axis]) / d
            if 0 < t < best_t:
                best_t, best_wall = t, axis * 2 + (1 if d > 0 else 0)
        return best_t, best_wall

    for i in range(count):
        phase = i / max(1, count - 1)
        yaw = phase * 2.0 * math.pi
        position = (0.6 * math.sin(phase * 2.0 * math.pi), 0.0, 0.8 * math.cos(phase * 2.0 * math.pi) - 0.3)
        pose = _pose_yaw(position, yaw)
        right = pose[0:3]
        up = pose[4:7]
        back = pose[8:11]

        def ray(u, v, fx, cx, cy):
            # Camera space: x right, y up, -z forward; v grows downward.
            xc = (u - cx) / fx
            yc = -(v - cy) / fx
            return tuple(right[k] * xc + up[k] * yc - back[k] for k in range(3))

        depth = []
        for row in range(dh):
            for col in range(dw):
                direction = ray(col + 0.5, row + 0.5, fx_d, dw / 2.0, dh / 2.0)
                t, _ = cast(position, direction)
                # The direction has a forward component of exactly 1, so the
                # ray parameter is the depth along the camera axis.
                depth.append(t if t <= 5.0 else 0.0)
        confidence = None
        if with_confidence:
            confidence = bytes(2 if 0.0 < d <= 3.0 else (1 if d > 0.0 else 0) for d in depth)

        def block(bx, by):
            direction = ray(bx * 8 + 4.0, by * 8 + 4.0, fx_c, cw / 2.0, ch / 2.0)
            t, wall = cast(position, direction)
            hit = tuple(position[k] + direction[k] * t for k in range(3))
            checker = (int(math.floor(hit[0] * 2)) + int(math.floor(hit[1] * 2)) + int(math.floor(hit[2] * 2))) & 1
            base = wall_colors[wall]
            return tuple(max(0, c - 40 * checker) for c in base)

        yield Frame(
            index=i,
            timestamp=100.0 + i / fps,
            world_from_cam=pose,
            intrinsics=(fx_c, fx_c, cw / 2.0, ch / 2.0),
            tracking=TRACKING_NORMAL,
            orientation=0,
            color_width=cw,
            color_height=ch,
            color=tinyjpeg.encode_blocks(cw, ch, block),
            depth_width=dw,
            depth_height=dh,
            depth=depth_plane_from_meters(depth),
            confidence=confidence,
            mask=None,
            compressed=compress,
        )


# --------------------------------------------------------------------------
# Command line
# --------------------------------------------------------------------------


def _is_single_frame(path: str) -> bool:
    with open(path, "rb") as f:
        return f.read(4) == FRAME_MAGIC


def _read_file(path: str) -> bytes:
    with open(path, "rb") as f:
        return f.read()


def _frame_line(frame: Frame, size: int) -> str:
    x, y, z = frame.position()
    valid = sum(1 for v in frame.depth_mm() if v)
    return ("#%-10d t=%-12.3f %-13s pos=(%.2f, %.2f, %.2f) colour %dx%d %d B, depth %dx%d %d%% valid%s%s%s, %d B" % (
        frame.index, frame.timestamp, TRACKING_NAMES[frame.tracking], x, y, z,
        frame.color_width, frame.color_height, len(frame.color),
        frame.depth_width, frame.depth_height,
        100 * valid // (frame.depth_width * frame.depth_height),
        ", confidence" if frame.confidence is not None else "",
        ", mask" if frame.mask is not None else "",
        ", deflate" if frame.compressed else "",
        size))


def check_frames(frames_bytes) -> tuple:
    """Decode every frame; returns (count, problems, stats)."""
    problems = []
    stats = {"bytes": 0, "first_t": None, "last_t": None, "used_tracking": [0, 0, 0], "path": 0.0}
    last_index = None
    last_pos = None
    count = 0
    for n, data in enumerate(frames_bytes):
        count += 1
        stats["bytes"] += len(data)
        try:
            frame = decode_frame(data)
        except FormatError as exc:
            problems.append("frame %d: %s" % (n, exc))
            continue
        if last_index is not None and frame.index <= last_index:
            problems.append("frame %d: index %d does not increase (previous %d)" % (n, frame.index, last_index))
        last_index = frame.index
        if stats["last_t"] is not None and frame.timestamp < stats["last_t"]:
            problems.append("frame %d: timestamp goes backwards" % n)
        values = list(frame.world_from_cam) + list(frame.intrinsics) + [frame.timestamp]
        if not all(math.isfinite(v) for v in values):
            problems.append("frame %d: non-finite pose, intrinsics or timestamp" % n)
        if frame.color and frame.color[:2] != b"\xFF\xD8":
            problems.append("frame %d: colour does not start with a JPEG SOI marker" % n)
        if frame.confidence is not None and max(frame.confidence) > 2:
            problems.append("frame %d: confidence value above 2" % n)
        if frame.mask is not None and set(frame.mask) - {0, 255}:
            problems.append("frame %d: mask value other than 0 or 255" % n)
        if stats["first_t"] is None:
            stats["first_t"] = frame.timestamp
        stats["last_t"] = frame.timestamp
        stats["used_tracking"][frame.tracking] += 1
        pos = frame.position()
        if last_pos is not None and all(math.isfinite(v) for v in pos + last_pos):
            stats["path"] += math.dist(pos, last_pos)
        last_pos = pos
    return count, problems, stats


def _cmd_info(args) -> int:
    if _is_single_frame(args.file):
        data = _read_file(args.file)
        frame = decode_frame(data)
        print(_frame_line(frame, len(data)))
        return 0
    with GscapReader(args.file) as reader:
        print("header: " + json.dumps(reader.header, ensure_ascii=False, sort_keys=True))
        count = 0
        total = 0
        first = last = None
        for data in reader:
            frame = decode_frame(data)
            if args.frames:
                print(_frame_line(frame, len(data)))
            first = frame.timestamp if first is None else first
            last = frame.timestamp
            count += 1
            total += len(data)
        duration = (last - first) if count else 0.0
        print("frames: %d, %.1f s, %.1f MB%s" % (
            count, duration, total / 1e6, ", TRUNCATED (last frame incomplete)" if reader.truncated else ""))
    return 0


def _cmd_verify(args) -> int:
    if _is_single_frame(args.file):
        count, problems, stats = check_frames([_read_file(args.file)])
        truncated = False
    else:
        with GscapReader(args.file) as reader:
            try:
                count, problems, stats = check_frames(reader)
            except FormatError as exc:
                print("FAILED: %s" % exc)
                return 1
            truncated = reader.truncated
            if reader.header.get("protocol") != PROTOCOL:
                problems.append("header: protocol is %r, expected %d" % (reader.header.get("protocol"), PROTOCOL))
            if reader.header.get("mode") not in MODES:
                problems.append("header: mode is %r" % (reader.header.get("mode"),))
            if not isinstance(reader.header.get("created"), str):
                problems.append("header: no `created` time")
    for problem in problems:
        print("problem: " + problem)
    if truncated:
        print("warning: the file ends inside a frame (recording cut short); %d complete frames" % count)
    duration = (stats["last_t"] - stats["first_t"]) if stats["first_t"] is not None else 0.0
    print("%d frames, %.1f s, %.1f MB, camera path %.2f m, tracking normal/limited/none %d/%d/%d" % (
        count, duration, stats["bytes"] / 1e6, stats["path"],
        stats["used_tracking"][2], stats["used_tracking"][1], stats["used_tracking"][0]))
    print("OK" if not problems else "FAILED: %d problem(s)" % len(problems))
    return 0 if not problems else 1


def png_gray(width: int, height: int, samples, bits: int = 8) -> bytes:
    """A grayscale PNG (8 or 16 bits per sample) from row-major samples."""
    def chunk(kind: bytes, body: bytes) -> bytes:
        return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", zlib.crc32(kind + body) & 0xFFFFFFFF)

    if bits == 16:
        raw = struct.pack(">%dH" % (width * height), *samples)
        stride = width * 2
    else:
        raw = bytes(samples)
        stride = width
    rows = b"".join(b"\x00" + raw[y * stride : (y + 1) * stride] for y in range(height))
    return (b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, bits, 0, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(rows, 6))
            + chunk(b"IEND", b""))


def extract_frame(frame: Frame, outdir: str, stem: str) -> None:
    """Write what a frame holds as files any image viewer opens.

    STEM.jpg             the colour image, as received
    STEM_depth.png       16-bit grayscale, the millimeters themselves
    STEM_depth_view.png  8-bit view of the depth: near is bright, black is "no value"
    STEM_conf.png        confidence: black low, gray medium, white high
    STEM_mask.png        person mask
    STEM.json            pose, intrinsics, sizes
    """
    def write(name: str, data: bytes) -> None:
        with open(os.path.join(outdir, name), "wb") as f:
            f.write(data)

    w, h = frame.depth_width, frame.depth_height
    if frame.color:
        write(stem + ".jpg", frame.color)
    mm = frame.depth_mm()
    write(stem + "_depth.png", png_gray(w, h, mm, 16))
    write(stem + "_depth_view.png", png_gray(w, h, [0 if v == 0 else 255 - min(235, v * 235 // 6000) for v in mm]))
    if frame.confidence is not None:
        write(stem + "_conf.png", png_gray(w, h, [min(v, 2) * 127 for v in frame.confidence]))
    if frame.mask is not None:
        write(stem + "_mask.png", png_gray(w, h, frame.mask))
    meta = frame.describe()
    for key in ("timestamp_bits", "world_from_cam_bits", "intrinsics_bits"):
        meta.pop(key)
    with open(os.path.join(outdir, stem + ".json"), "w", encoding="utf-8") as f:
        json.dump(meta, f, indent=1)


def _cmd_extract(args) -> int:
    os.makedirs(args.outdir, exist_ok=True)
    if _is_single_frame(args.file):
        extract_frame(decode_frame(_read_file(args.file)), args.outdir, "frame_000000")
        print("1 frame written to %s" % args.outdir)
        return 0
    written = 0
    with GscapReader(args.file) as reader:
        with open(os.path.join(args.outdir, "header.json"), "w", encoding="utf-8") as f:
            json.dump(reader.header, f, indent=1, ensure_ascii=False)
        for n, data in enumerate(reader):
            if n < args.first or (args.every > 1 and (n - args.first) % args.every):
                continue
            if args.count and written >= args.count:
                break
            extract_frame(decode_frame(data), args.outdir, "frame_%06d" % n)
            written += 1
    print("%d frame(s) written to %s" % (written, args.outdir))
    return 0


def _cmd_synth(args) -> int:
    header = make_header(mode=args.mode, name=args.name, device="synthetic", floor={"y": -1.4})
    frames = synth_frames(count=args.frames, depth_size=(args.depth_width, args.depth_width * 3 // 4),
                          color_size=(args.color_width, args.color_width * 3 // 4), fps=args.fps)
    count = write_gscap(args.out, header, frames)
    print("%s: %d frames, %.1f kB" % (args.out, count, os.path.getsize(args.out) / 1e3))
    return 0


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description="Splat Live capture files (.gscap) and frames")
    sub = parser.add_subparsers(dest="command")
    p = sub.add_parser("info", help="summary of a .gscap file or of a single frame")
    p.add_argument("file")
    p.add_argument("--frames", action="store_true", help="one line per frame")
    p.set_defaults(func=_cmd_info)
    p = sub.add_parser("verify", help="decode every frame and report problems (exit code 1 if any)")
    p.add_argument("file")
    p.set_defaults(func=_cmd_verify)
    p = sub.add_parser("extract", help="write JPEG, PNG and JSON files for the frames")
    p.add_argument("file")
    p.add_argument("outdir")
    p.add_argument("--first", type=int, default=0, help="first frame (position in the file)")
    p.add_argument("--count", type=int, default=0, help="number of frames, 0 = all")
    p.add_argument("--every", type=int, default=1, help="keep one frame out of N")
    p.set_defaults(func=_cmd_extract)
    p = sub.add_parser("synth", help="write a synthetic capture (walk in a box room)")
    p.add_argument("out")
    p.add_argument("--frames", type=int, default=60)
    p.add_argument("--fps", type=float, default=10.0)
    p.add_argument("--mode", choices=MODES, default="room")
    p.add_argument("--name", default="Synthetic room")
    p.add_argument("--depth-width", type=int, default=64)
    p.add_argument("--color-width", type=int, default=160)
    p.set_defaults(func=_cmd_synth)
    args = parser.parse_args(argv)
    if not args.command:
        parser.print_help()
        return 2
    try:
        return args.func(args)
    except FormatError as exc:
        print("error: %s" % exc, file=sys.stderr)
        return 1
    except OSError as exc:
        print("error: %s" % exc, file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
