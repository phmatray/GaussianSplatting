"""Tests of the capture tools: python3 -m unittest discover -s tools

Standard library only. The network tests need the openssl command line tool
to create a self-signed certificate and are skipped without it.
"""

from __future__ import annotations

import contextlib
import filecmp
import glob
import hashlib
import io
import json
import math
import os
import shutil
import socket
import struct
import sys
import tempfile
import threading
import time
import unittest
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import fake_phone  # noqa: E402
import fake_server  # noqa: E402
import gscap  # noqa: E402
import make_fixtures  # noqa: E402
import tinyjpeg  # noqa: E402
import wsproto  # noqa: E402

FIXTURES = make_fixtures.DEFAULT_DIR


def fixture(*parts: str) -> str:
    return os.path.join(FIXTURES, *parts)


def load_json(*parts: str):
    with open(fixture(*parts), "r", encoding="utf-8") as f:
        return json.load(f)


class TinyJpegTests(unittest.TestCase):
    def test_structure(self):
        data = tinyjpeg.encode_flat(16, 12, (10, 200, 30))
        info = tinyjpeg.inspect(data)
        self.assertEqual((info["width"], info["height"], info["components"]), (16, 12, 3))
        self.assertEqual(info["markers"], ["SOI", "E0", "DB", "C0", "C4", "C4", "DA", "EOI"])
        self.assertEqual(data[:2], b"\xff\xd8")
        self.assertEqual(data[-2:], b"\xff\xd9")
        self.assertLess(len(data), 400)

    def test_sizes_that_are_not_multiples_of_eight(self):
        info = tinyjpeg.inspect(tinyjpeg.encode_blocks(21, 9, lambda bx, by: (bx * 80, by * 100, 0)))
        self.assertEqual((info["width"], info["height"]), (21, 9))
        # 3 x 2 blocks, 3 components, at least 6 bits each.
        self.assertGreaterEqual(info["scan_bytes"], 3 * 2 * 3 * 6 // 8)

    def test_no_marker_inside_the_scan(self):
        # Extreme colours give long codes; any 0xFF in the scan must be stuffed.
        data = tinyjpeg.encode_blocks(64, 64, lambda bx, by: ((bx * 97) % 256, (by * 61) % 256, (bx * by * 13) % 256))
        info = tinyjpeg.inspect(data)
        scan = data[len(data) - 2 - info["scan_bytes"] : -2]
        for i, byte in enumerate(scan[:-1]):
            if byte == 0xFF:
                self.assertEqual(scan[i + 1], 0x00)

    def test_inspect_rejects_garbage(self):
        for bad in (b"", b"\xff\xd8", b"GSCF" * 10, tinyjpeg.encode_flat(8, 8)[:-2]):
            with self.assertRaises(ValueError):
                tinyjpeg.inspect(bad)


class FrameTests(unittest.TestCase):
    def frame(self, **overrides) -> gscap.Frame:
        values = dict(
            index=7, timestamp=12.5, world_from_cam=tuple(float(i) for i in range(16)),
            intrinsics=(500.0, 501.0, 320.0, 240.0), tracking=1, orientation=2,
            color_width=640, color_height=480, color=b"\xff\xd8JPEG\xff\xd9",
            depth_width=3, depth_height=2, depth=struct.pack("<6H", 0, 1, 2, 1000, 5000, 65535),
            confidence=bytes([0, 1, 2, 2, 1, 0]), mask=bytes([0, 255, 0, 255, 0, 255]),
        )
        values.update(overrides)
        return gscap.Frame(**values)

    def test_header_layout_matches_the_specification_table(self):
        data = gscap.encode_frame(self.frame())
        self.assertEqual(data[0:4], b"GSCF")
        self.assertEqual(data[4], 1)                                   # version
        self.assertEqual(data[5], 0b101)                               # confidence + mask, not compressed
        self.assertEqual(data[6], 1)                                   # tracking
        self.assertEqual(data[7], 2)                                   # orientation
        self.assertEqual(struct.unpack_from("<I", data, 8)[0], 7)      # frame index
        self.assertEqual(struct.unpack_from("<I", data, 12)[0], 0)     # reserved
        self.assertEqual(struct.unpack_from("<d", data, 16)[0], 12.5)  # timestamp
        self.assertEqual(struct.unpack_from("<16f", data, 24), tuple(float(i) for i in range(16)))
        self.assertEqual(struct.unpack_from("<4f", data, 88), (500.0, 501.0, 320.0, 240.0))
        self.assertEqual(struct.unpack_from("<HH", data, 104), (640, 480))
        self.assertEqual(struct.unpack_from("<I", data, 108)[0], 8)
        self.assertEqual(struct.unpack_from("<HH", data, 112), (3, 2))
        self.assertEqual(struct.unpack_from("<I", data, 116)[0], 12)
        self.assertEqual(struct.unpack_from("<I", data, 120)[0], 6)
        self.assertEqual(struct.unpack_from("<I", data, 124)[0], 6)
        self.assertEqual(data[128:136], b"\xff\xd8JPEG\xff\xd9")       # colour first
        self.assertEqual(data[136:148], struct.pack("<6H", 0, 1, 2, 1000, 5000, 65535))  # then depth
        self.assertEqual(data[148:154], bytes([0, 1, 2, 2, 1, 0]))     # then confidence
        self.assertEqual(data[154:160], bytes([0, 255, 0, 255, 0, 255]))  # then mask
        self.assertEqual(len(data), 160)

    def test_round_trip_of_every_flag_combination(self):
        for flags in range(8):
            frame = self.frame(
                confidence=bytes(6) if flags & 1 else None,
                mask=bytes([255] * 6) if flags & 4 else None,
                compressed=bool(flags & 2),
            )
            data = gscap.encode_frame(frame)
            self.assertEqual(data[5], flags)
            back = gscap.decode_frame(data)
            for name in ("index", "timestamp", "world_from_cam", "intrinsics", "tracking", "orientation",
                         "color_width", "color_height", "color", "depth_width", "depth_height", "depth",
                         "confidence", "mask", "compressed"):
                self.assertEqual(getattr(back, name), getattr(frame, name), "%s with flags %d" % (name, flags))
            self.assertEqual(back.flags, flags)

    def test_compressed_planes_are_independent_raw_deflate_streams(self):
        frame = self.frame(depth_width=64, depth_height=48, depth=struct.pack("<3072H", *([1500] * 3072)),
                           confidence=bytes([2]) * 3072, mask=bytes(3072), compressed=True)
        data = gscap.encode_frame(frame)
        color_len, = struct.unpack_from("<I", data, 108)
        depth_len, conf_len, mask_len = struct.unpack_from("<III", data, 116)
        self.assertLess(depth_len, 6144)
        pos = 128 + color_len
        # wbits = -15: no zlib header, no checksum.
        self.assertEqual(zlib.decompress(data[pos : pos + depth_len], -15), frame.depth)
        pos += depth_len
        self.assertEqual(zlib.decompress(data[pos : pos + conf_len], -15), frame.confidence)
        pos += conf_len
        self.assertEqual(zlib.decompress(data[pos : pos + mask_len], -15), frame.mask)
        self.assertEqual(pos + mask_len, len(data))
        # The colour image is stored as is.
        self.assertEqual(data[128 : 128 + color_len], frame.color)

    def test_encoder_refuses_inconsistent_frames(self):
        for overrides in (dict(depth=b"\x00"), dict(confidence=bytes(5)), dict(mask=bytes(7)),
                          dict(depth_width=0, depth=b""), dict(tracking=3), dict(orientation=4),
                          dict(index=1 << 32), dict(color_width=70000)):
            with self.assertRaises(gscap.FormatError):
                gscap.encode_frame(self.frame(**overrides))

    def test_inflate_limits(self):
        stream = gscap.deflate_raw(bytes(100))
        self.assertEqual(gscap.inflate_raw(stream, 100), bytes(100))
        for data, size in ((stream, 99), (stream, 101), (stream[:-1], 100), (stream + b"x", 100), (b"", 100)):
            with self.assertRaises(gscap.FormatError) as caught:
                gscap.inflate_raw(data, size)
            self.assertEqual(caught.exception.code, "bad_deflate")

    def test_depth_conversion(self):
        cases = {1.0: 1000, 0.0: 0, -1.0: 0, 2.5: 2500, 0.0004: 0, 0.0006: 1, 5.0: 5000, 65.535: 65535,
                 65.536: 0, 1e9: 0, float("inf"): 0, float("-inf"): 0}
        for meters, mm in cases.items():
            self.assertEqual(gscap.depth_meters_to_mm(meters), mm, meters)
        self.assertEqual(gscap.depth_meters_to_mm(float("nan")), 0)
        self.assertEqual(gscap.depth_plane_from_meters([1.0, 0.0, 2.0]), struct.pack("<3H", 1000, 0, 2000))


class FixtureTests(unittest.TestCase):
    def test_fixtures_exist(self):
        self.assertTrue(os.path.isdir(FIXTURES), "run tools/make_fixtures.py")
        self.assertTrue(os.path.exists(fixture("README.md")))

    def test_every_frame_fixture_matches_its_sidecar(self):
        paths = sorted(glob.glob(fixture("frames", "*.bin")))
        self.assertGreaterEqual(len(paths), 30)
        seen_flags = set()
        for path in paths:
            name = os.path.basename(path)
            with open(path, "rb") as f:
                data = f.read()
            with open(path[:-4] + ".json", "r", encoding="utf-8") as f:
                expected = json.load(f)
            self.assertEqual(len(data), expected["length"], name)
            if not expected["valid"]:
                self.assertTrue(name.startswith("bad_"), name)
                with self.assertRaises(gscap.FormatError, msg=name) as caught:
                    gscap.decode_frame(data)
                self.assertEqual(caught.exception.code, expected["error"], name)
                continue
            frame = gscap.decode_frame(data)
            described = frame.describe()
            for key, value in described.items():
                if key in ("timestamp", "world_from_cam", "intrinsics"):
                    continue  # compared exactly through their bit patterns
                self.assertEqual(expected[key], value, "%s: %s" % (name, key))
            self.assertEqual(data[5], expected["flags"], name)
            seen_flags.add(expected["flags"])
            self.assertEqual(struct.unpack_from("<I", data, 12)[0], expected["reserved"], name)
            lengths = expected["stored_lengths"]
            self.assertEqual(len(data), 128 + sum(lengths.values()), name)
            if expected["reencode_identical"]:
                self.assertEqual(gscap.encode_frame(frame), data, name)
            else:
                again = gscap.decode_frame(gscap.encode_frame(frame))
                self.assertEqual(again.describe(), described, name)
            if frame.color:
                info = tinyjpeg.inspect(frame.color)
                self.assertEqual((info["width"], info["height"]), (frame.color_width, frame.color_height), name)
        self.assertEqual(seen_flags, set(range(8)))

    def test_extreme_frame(self):
        with open(fixture("frames", "frame_extreme.bin"), "rb") as f:
            data = f.read()
        frame = gscap.decode_frame(data)
        self.assertEqual(frame.index, 2 ** 32 - 1)
        self.assertEqual(data[8:12], b"\xff\xff\xff\xff")
        self.assertEqual((frame.depth_width, frame.depth_height, frame.depth_mm()), (1, 1, [65535]))
        self.assertEqual(frame.color, b"")
        self.assertEqual(len(data), 130)
        self.assertTrue(all(math.isfinite(v) for v in frame.world_from_cam + frame.intrinsics + (frame.timestamp,)))
        self.assertEqual(math.copysign(1.0, frame.world_from_cam[4]), -1.0)  # -0.0 survives
        self.assertEqual(frame.world_from_cam[3], 2.0 ** -149)                # smallest subnormal f32

    def test_fixtures_are_what_the_generator_writes(self):
        """Guards against editing gscap.py or make_fixtures.py without regenerating."""
        with tempfile.TemporaryDirectory() as tmp:
            make_fixtures.generate(tmp)
            fresh = sorted(os.path.relpath(os.path.join(root, name), tmp)
                           for root, _, files in os.walk(tmp) for name in files)
            on_disk = sorted(os.path.relpath(os.path.join(root, name), FIXTURES)
                             for root, _, files in os.walk(FIXTURES) for name in files)
            self.assertEqual(fresh, [p for p in on_disk if p != "README.md"])
            for rel in fresh:
                if filecmp.cmp(os.path.join(tmp, rel), fixture(rel), shallow=False):
                    continue
                # Deflate streams depend on the zlib build; everything else must be identical.
                base = os.path.basename(rel)
                zlib_dependent = ("deflate" in base or base.startswith(("frame_lidar", "three_frames", "truncated")))
                self.assertTrue(zlib_dependent, "%s differs from what make_fixtures.py writes" % rel)
                if rel.endswith(".bin") and os.path.basename(rel).startswith("frame_"):
                    with open(os.path.join(tmp, rel), "rb") as a, open(fixture(rel), "rb") as b:
                        self.assertEqual(gscap.decode_frame(a.read()).describe(),
                                         gscap.decode_frame(b.read()).describe(), rel)

    def test_depth_conversion_fixture(self):
        cases = load_json("depth_conversion.json")
        self.assertGreater(len(cases), 30)
        for case in cases:
            value = struct.unpack("<f", struct.pack("<I", int(case["bits"], 16)))[0]
            self.assertEqual(gscap.depth_meters_to_mm(value), case["mm"], case)
        by_bits = {case["bits"]: case["mm"] for case in cases}
        self.assertEqual(by_bits["3f800000"], 1000)   # 1.0 m
        self.assertEqual(by_bits["40a00000"], 5000)   # 5.0 m
        self.assertEqual(by_bits["7fc00000"], 0)      # NaN
        self.assertEqual(by_bits["7f800000"], 0)      # +inf

    def test_fingerprint_fixture(self):
        case = load_json("fingerprint.json")
        der = bytes.fromhex(case["der_hex"])
        self.assertEqual(gscap.certificate_fingerprint(der), case["sha256"])
        self.assertEqual(hashlib.sha256(der).hexdigest(), case["sha256"])
        self.assertEqual(len(case["sha256"]), 64)


class AdversarialFrameTests(unittest.TestCase):
    """Hostile or damaged frames: the reference refuses them with a FormatError
    and a code, never with another exception, and never by allocating what a
    header merely claims."""

    def read(self, name: str) -> bytes:
        with open(fixture("frames", name + ".bin"), "rb") as f:
            return f.read()

    def code_of(self, data: bytes) -> str:
        with self.assertRaises(gscap.FormatError) as caught:
            gscap.decode_frame(data)
        return caught.exception.code

    def test_every_truncation_is_refused(self):
        for name in ("frame_plain", "frame_conf_mask", "frame_deflate_conf_mask", "frame_extreme"):
            data = self.read(name)
            for size in range(len(data)):
                code = self.code_of(data[:size])
                self.assertEqual(code, "too_short" if size < 128 else "bad_length", "%s cut at %d" % (name, size))
            self.assertEqual(self.code_of(data + b"\x00"), "bad_length", name)

    def test_single_byte_damage_never_escapes_as_another_exception(self):
        for name in ("frame_plain", "frame_conf_mask", "frame_deflate_conf_mask", "frame_deflate"):
            data = self.read(name)
            for position in range(len(data)):
                for value in (0x00, 0x01, 0x7F, 0x80, 0xFF, data[position] ^ 0x01, data[position] ^ 0x80):
                    damaged = bytearray(data)
                    damaged[position] = value
                    try:
                        frame = gscap.decode_frame(bytes(damaged))
                    except gscap.FormatError as exc:
                        self.assertTrue(exc.code, "%s byte %d" % (name, position))
                        continue
                    # Accepted: it must be self-consistent and encode again.
                    self.assertEqual(len(frame.depth), 2 * frame.depth_width * frame.depth_height)
                    gscap.decode_frame(gscap.encode_frame(frame))

    def test_wrong_magic(self):
        plain = self.read("frame_plain")
        for magic in (b"GSCG", b"gscf", b"GSCA", b"FCSG", b"\x00\x00\x00\x00", b"GSC\x00", b"RIFF"):
            self.assertEqual(self.code_of(magic + plain[4:]), "bad_magic", magic)
        # A .gscap is not a frame, and a frame is not a .gscap.
        with open(fixture("three_frames.gscap"), "rb") as f:
            self.assertEqual(self.code_of(f.read()), "bad_magic")
        with self.assertRaises(gscap.FormatError) as caught:
            gscap.GscapReader(io.BytesIO(plain))
        self.assertEqual(caught.exception.code, "bad_file_magic")

    def test_every_version_but_one(self):
        plain = bytearray(self.read("frame_plain"))
        for version in range(256):
            plain[4] = version
            if version == 1:
                gscap.decode_frame(bytes(plain))
            else:
                self.assertEqual(self.code_of(bytes(plain)), "bad_version", version)

    def test_every_flag_byte(self):
        """Bits 3 to 7 are refused whatever the other bits say, and before any length is looked at."""
        frames = {flags: gscap.encode_frame(make_fixtures.small_frame(flags)) for flags in range(8)}
        for value in range(256):
            data = bytearray(frames[value & 7])
            data[5] = value
            if value < 8:
                self.assertEqual(gscap.decode_frame(bytes(data)).flags, value)
            else:
                self.assertEqual(self.code_of(bytes(data)), "bad_flags", "flags 0x%02x" % value)
                self.assertEqual(self.code_of(bytes(data[:128])), "bad_flags", "flags 0x%02x, header only" % value)

    def test_tracking_and_orientation_ranges(self):
        plain = bytearray(self.read("frame_plain"))
        for value in range(256):
            data = bytearray(plain)
            data[6] = value
            if value > 2:
                self.assertEqual(self.code_of(bytes(data)), "bad_tracking", value)
            data = bytearray(plain)
            data[7] = value
            if value > 3:
                self.assertEqual(self.code_of(bytes(data)), "bad_orientation", value)

    def test_lengths_that_overflow(self):
        for name in ("frame_plain", "frame_conf_mask", "frame_deflate_conf_mask"):
            data = self.read(name)
            for offset in (108, 116, 120, 124):
                for value in (0xFFFFFFFF, 0xFFFFFF00, 0x80000000, 0x7FFFFFFF, 0x10000):
                    damaged = bytearray(data)
                    struct.pack_into("<I", damaged, offset, value)
                    self.assertEqual(self.code_of(bytes(damaged)), "bad_length", "%s offset %d = %#x" % (name, offset, value))
        # All four at once: the sum is far above 2^32 and must not wrap.
        damaged = bytearray(self.read("frame_deflate_conf_mask"))
        for offset in (108, 116, 120, 124):
            struct.pack_into("<I", damaged, offset, 0xFFFFFFFF)
        self.assertEqual(self.code_of(bytes(damaged)), "bad_length")
        self.assertEqual(gscap.parse_header(bytes(damaged))["total_length"], 128 + 4 * 0xFFFFFFFF)

    def test_lengths_whose_32_bit_sum_wraps_to_the_real_size(self):
        data = self.read("bad_length_wraps_u32")
        depth_length, confidence_length, mask_length = struct.unpack_from("<III", data, 116)
        color_length, = struct.unpack_from("<I", data, 108)
        announced = 128 + color_length + depth_length + confidence_length + mask_length
        self.assertEqual(announced & 0xFFFFFFFF, len(data))   # what a wrapping u32 sum would see
        self.assertGreater(announced, 1 << 32)
        self.assertEqual(self.code_of(data), "bad_length")

    def test_depth_size_limit(self):
        self.assertEqual(self.code_of(self.read("bad_depth_huge")), "bad_dimensions")
        base = make_fixtures.small_frame(0)
        # Exactly at the limit: accepted (2048 x 2048, 8 MiB of depth from a few kB).
        side = 2048
        self.assertEqual(side * side, gscap.MAX_DEPTH_PIXELS)
        big = gscap.Frame(depth_width=side, depth_height=side, depth=bytes(2 * side * side), compressed=True)
        encoded = gscap.encode_frame(big)
        self.assertLess(len(encoded), 20000)
        self.assertEqual(len(gscap.decode_frame(encoded).depth), 2 * side * side)
        # One column more: refused from the header alone, compressed or not.
        over = bytearray(encoded)
        struct.pack_into("<HH", over, 112, side + 1, side)
        self.assertEqual(self.code_of(bytes(over)), "bad_dimensions")
        plain = bytearray(gscap.encode_frame(base))
        struct.pack_into("<HH", plain, 112, 65535, 65535)
        self.assertEqual(self.code_of(bytes(plain)), "bad_dimensions")
        with self.assertRaises(gscap.FormatError):
            gscap.encode_frame(gscap.Frame(depth_width=side + 1, depth_height=side, depth=bytes(2 * side * (side + 1))))

    def test_deflate_bomb_stops_at_the_expected_size(self):
        """A plane that inflates to far more than its size is cut off after size + 1 bytes."""
        import tracemalloc
        base = make_fixtures.small_frame(0)
        bomb = gscap.deflate_raw(bytes(64 << 20), 9)   # 64 MiB of zeros in about 64 kB
        self.assertLess(len(bomb), 100000)
        for flags, planes in ((gscap.FLAG_DEFLATE, (bomb,)),
                              (gscap.FLAG_DEFLATE | gscap.FLAG_CONFIDENCE, (gscap.deflate_raw(base.depth), bomb)),
                              (gscap.FLAG_DEFLATE | gscap.FLAG_MASK, (gscap.deflate_raw(base.depth), b"", bomb))):
            data = make_fixtures._with_planes(base, flags, *planes)
            tracemalloc.start()
            try:
                self.assertEqual(self.code_of(data), "bad_deflate")
                peak = tracemalloc.get_traced_memory()[1]
            finally:
                tracemalloc.stop()
            self.assertLess(peak, 4 << 20, "inflated %d bytes for a 12 pixel plane" % peak)

    def test_bad_streams_in_the_second_and_third_plane(self):
        self.assertEqual(self.code_of(self.read("bad_deflate_conf_short")), "bad_deflate")
        self.assertEqual(self.code_of(self.read("bad_deflate_mask_long")), "bad_deflate")
        base = make_fixtures.small_frame(7)
        depth = gscap.deflate_raw(base.depth)
        confidence = gscap.deflate_raw(base.confidence)
        mask = gscap.deflate_raw(base.mask)
        flags = gscap.FLAG_DEFLATE | gscap.FLAG_CONFIDENCE | gscap.FLAG_MASK
        gscap.decode_frame(make_fixtures._with_planes(base, flags, depth, confidence, mask))
        for planes in ((depth, confidence[:-1], mask), (depth, confidence, mask[:-1]), (depth, confidence + b"\x00", mask),
                       (depth, b"\x07", mask), (depth, mask, depth), (confidence, depth, mask),
                       # the three streams glued into the first plane are not three planes
                       (depth + confidence + mask, b"\x03\x00", b"\x03\x00")):
            self.assertEqual(self.code_of(make_fixtures._with_planes(base, flags, *planes)), "bad_deflate")
        # zlib and gzip wrappers are not raw deflate.
        for wrapped in (zlib.compress(base.depth), zlib.compress(base.depth, wbits=31)):
            self.assertEqual(self.code_of(make_fixtures._with_planes(base, gscap.FLAG_DEFLATE, wrapped)), "bad_deflate")

    def test_flags_and_lengths_must_agree(self):
        base = make_fixtures.small_frame(0)
        pixels = bytes(12)
        cases = (
            (gscap.FLAG_CONFIDENCE, (base.depth,)),                    # flag without plane
            (gscap.FLAG_MASK, (base.depth,)),
            (0, (base.depth, pixels)),                                 # plane without flag
            (0, (base.depth, b"", pixels)),
            (gscap.FLAG_MASK, (base.depth, pixels)),                   # confidence bytes under the mask flag
            (gscap.FLAG_CONFIDENCE, (base.depth, b"", pixels)),
            (gscap.FLAG_DEFLATE | gscap.FLAG_CONFIDENCE, (gscap.deflate_raw(base.depth),)),
            (gscap.FLAG_DEFLATE, (gscap.deflate_raw(base.depth), gscap.deflate_raw(pixels))),
            (0, (gscap.deflate_raw(base.depth),)),                     # deflated without the flag
        )
        for flags, planes in cases:
            self.assertEqual(self.code_of(make_fixtures._with_planes(base, flags, *planes)), "bad_length", (flags, planes))
        # Uncompressed planes under the deflate flag are not deflate streams.
        self.assertEqual(self.code_of(make_fixtures._with_planes(base, gscap.FLAG_DEFLATE, base.depth)), "bad_deflate")

    def test_command_line_reports_instead_of_crashing(self):
        out, err = io.StringIO(), io.StringIO()
        paths = sorted(glob.glob(fixture("frames", "bad_*.bin"))) + sorted(glob.glob(fixture("bad_*.gscap")))
        self.assertGreaterEqual(len(paths), 35)
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            for path in paths:
                for command in ("verify", "info"):
                    self.assertEqual(gscap.main([command, path]), 1, "%s %s" % (command, path))
                with tempfile.TemporaryDirectory() as tmp:
                    self.assertEqual(gscap.main(["extract", path, tmp]), 1, path)
        self.assertNotIn("Traceback", out.getvalue() + err.getvalue())


class AdversarialFileTests(unittest.TestCase):
    def test_cut_at_every_byte(self):
        """A .gscap cut anywhere gives its complete frames; only a cut inside the header is an error."""
        with open(fixture("three_frames.gscap"), "rb") as f:
            whole = f.read()
        header_end = 12 + struct.unpack_from("<I", whole, 8)[0]
        boundaries = [header_end]
        while boundaries[-1] < len(whole):
            boundaries.append(boundaries[-1] + 4 + struct.unpack_from("<I", whole, boundaries[-1])[0])
        self.assertEqual(boundaries[-1], len(whole))
        self.assertEqual(len(boundaries), 4)
        for size in range(len(whole) + 1):
            source = io.BytesIO(whole[:size])
            if size < header_end:
                with self.assertRaises(gscap.FormatError) as caught:
                    gscap.GscapReader(source)
                self.assertEqual(caught.exception.code, "bad_file_magic" if size < 8 else "bad_header", size)
                continue
            reader = gscap.GscapReader(source)
            frames = list(reader)
            complete = sum(1 for b in boundaries[1:] if b <= size)
            self.assertEqual(len(frames), complete, size)
            self.assertEqual(reader.truncated, size not in boundaries, size)
            for data in frames:
                gscap.decode_frame(data)
            # The same through the command line: readable, exit code 0, a warning when cut.
            if size in boundaries or size in (boundaries[2] + 2, boundaries[2] + 4, boundaries[2] + 130, len(whole) - 1):
                with tempfile.TemporaryDirectory() as tmp:
                    path = os.path.join(tmp, "cut.gscap")
                    with open(path, "wb") as f:
                        f.write(whole[:size])
                    out = io.StringIO()
                    with contextlib.redirect_stdout(out):
                        self.assertEqual(gscap.main(["verify", path]), 0, size)
                    self.assertEqual("recording cut short" in out.getvalue(), size not in boundaries, size)
                    self.assertIn("%d frames" % complete, out.getvalue())

    def test_truncated_length_fixture(self):
        with gscap.GscapReader(fixture("truncated_length.gscap")) as reader:
            frames = list(reader)
            self.assertEqual((len(frames), reader.truncated, reader.trailing_bytes), (2, True, 2))
        with gscap.GscapReader(fixture("truncated.gscap")) as reader:
            frames = list(reader)
            self.assertEqual((len(frames), reader.truncated), (2, True))
            self.assertGreater(reader.trailing_bytes, 4 + 128)

    def test_record_lengths(self):
        head = gscap.encode_file_header(gscap.make_header(created="2026-01-01T00:00:00Z"))
        frame = gscap.encode_frame(make_fixtures.small_frame(0))
        good = head + struct.pack("<I", len(frame)) + frame
        for length in (0, 1, 127, gscap.MAX_FRAME_BYTES + 1, 0x7FFFFFFF, 0x80000000, 0xFFFFFFFF):
            with self.assertRaises(gscap.FormatError, msg=length) as caught:
                list(gscap.GscapReader(io.BytesIO(good + struct.pack("<I", length) + bytes(300))))
            self.assertEqual(caught.exception.code, "bad_length", length)
            # The frames before the bad record were already delivered.
            reader = gscap.GscapReader(io.BytesIO(good + struct.pack("<I", length) + bytes(300)))
            seen = []
            with self.assertRaises(gscap.FormatError):
                for data in reader:
                    seen.append(data)
            self.assertEqual(seen, [frame])
        # A plausible length with less data behind it is a recording cut short;
        # nothing of the announced size is allocated.
        for length in (128, 129, 5000, gscap.MAX_FRAME_BYTES):
            reader = gscap.GscapReader(io.BytesIO(good + struct.pack("<I", length) + bytes(100)))
            self.assertEqual(list(reader), [frame])
            self.assertTrue(reader.truncated)
        # A record of the right length that is not a frame: the reader hands it
        # over, the decoder refuses it, `verify` reports it.
        junk = good + struct.pack("<I", 200) + bytes(200)
        reader = gscap.GscapReader(io.BytesIO(junk))
        self.assertEqual(len(list(reader)), 2)
        with self.assertRaises(gscap.FormatError) as caught:
            list(gscap.GscapReader(io.BytesIO(junk)).frames())
        self.assertEqual(caught.exception.code, "bad_magic")
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "junk.gscap")
            with open(path, "wb") as f:
                f.write(junk)
            with contextlib.redirect_stdout(io.StringIO()):
                self.assertEqual(gscap.main(["verify", path]), 1)

    def test_header_lengths_and_contents(self):
        def refuse(data: bytes, code: str = "bad_header") -> None:
            with self.assertRaises(gscap.FormatError, msg=data[:40]) as caught:
                gscap.GscapReader(io.BytesIO(data))
            self.assertEqual(caught.exception.code, code, data[:40])

        magic = gscap.FILE_MAGIC
        for length in (0xFFFFFFFF, 0x80000000, gscap.MAX_HEADER_JSON + 1):
            refuse(magic + struct.pack("<I", length) + b"{}")
        refuse(magic)                                   # no length at all
        refuse(magic + b"\x02\x00")                     # half a length
        refuse(magic + struct.pack("<I", 0))            # empty JSON
        refuse(magic + struct.pack("<I", 2) + b"{")     # shorter than announced
        refuse(magic + struct.pack("<I", 4) + b"null")
        refuse(magic + struct.pack("<I", 2) + b"[]")
        refuse(magic + struct.pack("<I", 3) + b"\"a\"")
        refuse(magic + struct.pack("<I", 13) + b'{"name":"\xff\xfe"}')
        refuse(magic + struct.pack("<I", 13) + b'{"floor":NaN}')
        refuse(magic + struct.pack("<I", 14) + b'{"a":Infinity}')
        refuse(magic + struct.pack(">I", 2) + b"{}")    # a big-endian length
        deep = b"[" * 200000
        refuse(magic + struct.pack("<I", len(deep)) + deep)
        for wrong in (b"GSCAP2\x00\x00", b"GSCAP1\x00\x01", b"gscap1\x00\x00", b"GSCAP1  ", b"GSREC1\x00\x00", b"GSCF\x01\x00\x00\x00"):
            refuse(wrong + struct.pack("<I", 2) + b"{}", "bad_file_magic")
        # Unknown keys and odd values are not the reader's business.
        ok = b'{"protocol":1,"extra":[1,2,{"x":null}],"region":"none","floor":[]}'
        reader = gscap.GscapReader(io.BytesIO(magic + struct.pack("<I", len(ok)) + ok))
        self.assertEqual(list(reader), [])
        self.assertIsNone(gscap.header_region(reader.header))
        self.assertIsNone(gscap.header_floor_y(reader.header))

    def test_fixture_link(self):
        """ios/SplatCapture/Fixtures is the documented path; it leads to the Swift test resources."""
        documented = os.path.join(os.path.dirname(HERE), "ios", "SplatCapture", "Fixtures")
        self.assertTrue(os.path.isdir(documented))
        self.assertEqual(os.path.realpath(documented), os.path.realpath(FIXTURES))


class AdversarialTextTests(unittest.TestCase):
    def test_server_messages(self):
        for text in ("[" * 200000, "{" * 200000, '{"type":"ack","frame":NaN}', '{"type":"welcome","protocol":Infinity}',
                     '{"type":"ack","frame":1e999,"used":true}', '{"type":"ack","frame":true}', '{"type":["ack"]}',
                     '{"type":"ack","frame":[1]}', "\x00", "{\"type\":\"ack\",\"frame\":1", "null", "1", "true"):
            self.assertIsNone(gscap.parse_server_message(text), text[:40])
        self.assertEqual(gscap.parse_server_message('{"type":"welcome","protocol":1,"max_inflight":1e400}'),
                         {"kind": "welcome", "protocol": 1, "max_inflight": 1})
        self.assertEqual(gscap.parse_server_message('{"type":"welcome","protocol":"1","max_inflight":-3}'),
                         {"kind": "welcome", "protocol": 0, "max_inflight": 1})
        self.assertEqual(gscap.parse_server_message('{"type":"progress","frames":-1,"splats":"many"}'),
                         {"kind": "progress", "frames": 0, "splats": 0})
        self.assertEqual(gscap.parse_server_message('{"type":"error","code":7,"message":null}'),
                         {"kind": "error", "code": "", "message": ""})

    def test_pairing_uris(self):
        fp, token = "a" * 64, "b" * 32
        cases = {
            "splatlive://pair?host=1.2.3.4&port=" + "9" * 5000 + "&fp=%s&token=%s" % (fp, token): "bad_port",
            "splatlive://pair?" + "&" * 100000: "missing_host",
            "splatlive://pair?host=%00&port=1&fp=" + fp + "&token=" + token: "bad_host",
            "splatlive://pair?host=\u00e9&port=1&fp=" + fp + "&token=" + token: "bad_host",
            "splatlive://pair?host=1.2.3.4&port=\u0661&fp=" + fp + "&token=" + token: "bad_port",
            "splatlive://pair?host=1.2.3.4&port=1&fp=" + "\uff41" * 64 + "&token=" + token: "bad_fp",
            "splatlive://pair?host=1.2.3.4&port=1&fp=" + fp + "&token=" + "\uff42" * 32: "bad_token",
            "splatlive://pair?host=a\ud800&port=1&fp=" + fp + "&token=" + token: "bad_encoding",
            "splatlive://pair?host=" + "a" * 254 + "&port=1&fp=" + fp + "&token=" + token: "bad_host",
            "splatlive://pair?host=1.2.3.4&port=1&fp=" + fp + "&token=" + token + "#x": "bad_token",
            "splatlive://pair?host=1.2.3.4:7879&port=1&fp=" + fp + "&token=" + token: "bad_host",
            "splatlive://pair?host=1.2.3.4&port=+1&fp=" + fp + "&token=" + token: "bad_port",
            "splatlive://pair?host=1.2.3.4&port=1&fp=" + fp + "&token=" + token + "&host=1.2.3.4": "duplicate_parameter",
            "splatlive://pair?host=1.2.3.4&port=1&fp=" + fp + "&token=" + token + "&%68ost=x": "duplicate_parameter",
            "splatlive://pair#?host=1.2.3.4&port=1&fp=" + fp + "&token=" + token: "bad_scheme",
            "\x00splatlive://pair?host=1.2.3.4&port=1&fp=" + fp + "&token=" + token: "bad_scheme",
        }
        for uri, code in cases.items():
            with self.assertRaises(gscap.FormatError, msg=uri[:60]) as caught:
                gscap.parse_pairing_uri(uri)
            self.assertEqual(caught.exception.code, code, uri[:60])
        self.assertEqual(gscap.parse_pairing_uri("splatlive://pair?host=" + "a" * 253 + "&port=1&fp=%s&token=%s" % (fp, token)).host,
                         "a" * 253)


class GscapFileTests(unittest.TestCase):
    def check_file(self, stem: str):
        expected = load_json(stem + ".json")
        path = fixture(stem + ".gscap")
        self.assertEqual(os.path.getsize(path), expected["file_length"])
        with gscap.GscapReader(path) as reader:
            self.assertEqual(reader.header, expected["header"])
            self.assertEqual(reader.frames_offset, 12 + expected["header_json_length"])
            frames = list(reader)
            self.assertEqual(reader.truncated, expected["truncated"])
        self.assertEqual(len(frames), len(expected["frames"]))
        for data, want in zip(frames, expected["frames"]):
            self.assertEqual(len(data), want["length"])
            self.assertEqual(hashlib.sha256(data).hexdigest(), want["sha256"])
            self.assertEqual(gscap.decode_frame(data).index, want["index"])
        return expected, frames

    def test_three_frames(self):
        expected, frames = self.check_file("three_frames")
        self.assertEqual([f["index"] for f in expected["frames"]], [10, 11, 17])
        with open(fixture("three_frames.gscap"), "rb") as f:
            raw = f.read()
        self.assertEqual(raw[:8], b"GSCAP1\x00\x00")
        length, = struct.unpack_from("<I", raw, 8)
        header = json.loads(raw[12 : 12 + length].decode("utf-8"))
        self.assertEqual(header["protocol"], 1)
        self.assertEqual(header["mode"], "object")
        self.assertEqual(header["created"], "2026-11-14T09:30:00Z")
        self.assertEqual(header["region"], {"center": [0.5, -0.25, -1.5], "radius": 0.75})
        self.assertEqual(header["floor"], {"y": -1.25})
        self.assertNotIn("token", header)
        # Records: u32 length, then the frame exactly as on the wire.
        pos = 12 + length
        for data in frames:
            self.assertEqual(struct.unpack_from("<I", raw, pos)[0], len(data))
            self.assertEqual(raw[pos + 4 : pos + 4 + len(data)], data)
            pos += 4 + len(data)
        self.assertEqual(pos, len(raw))

    def test_padded_header(self):
        expected, _ = self.check_file("padded_header")
        with open(fixture("padded_header.gscap"), "rb") as f:
            raw = f.read()
        self.assertEqual(struct.unpack_from("<I", raw, 8)[0], 1024)
        body = raw[12 : 12 + 1024]
        self.assertTrue(body.endswith(b" " * 16))
        self.assertTrue(body.rstrip(b" ").endswith(b"}"))
        self.assertEqual(json.loads(body.decode("utf-8")), expected["header"])

    def test_truncated_and_empty(self):
        expected, frames = self.check_file("truncated")
        self.assertTrue(expected["truncated"])
        self.assertEqual(len(frames), 2)
        self.check_file("empty")

    def test_invalid_files(self):
        listing = load_json("invalid_files.json")
        self.assertGreaterEqual(len(listing), 6)
        for case in listing:
            with self.assertRaises(gscap.FormatError, msg=case["file"]) as caught:
                with gscap.GscapReader(fixture(case["file"])) as reader:
                    list(reader)
            self.assertEqual(caught.exception.code, case["error"], case["file"])

    def test_writer_and_header_rewrite(self):
        frames = [gscap.encode_frame(make_fixtures.small_frame(f)) for f in (0, 3, 5)]
        with tempfile.TemporaryDirectory() as tmp:
            path = os.path.join(tmp, "x.gscap")
            header = gscap.make_header(mode="person", name="Test", created="2026-01-01T00:00:00Z")
            with gscap.GscapWriter(path, header, pad_to=1024) as writer:
                for data in frames:
                    writer.append(data)
                header = dict(header, region={"center": [1.0, 2.0, 3.0], "radius": 0.5}, floor={"y": -1.0})
                writer.rewrite_header(header)
                writer.append(frames[0])
                with self.assertRaises(gscap.FormatError):
                    writer.rewrite_header(dict(header, name="x" * 2000))
            got_header, got_frames, truncated = gscap.read_gscap(path)
            self.assertEqual(got_header, header)
            self.assertEqual(got_frames, frames + [frames[0]])
            self.assertFalse(truncated)
            self.assertEqual(gscap.header_region(got_header), ((1.0, 2.0, 3.0), 0.5))
            self.assertEqual(gscap.header_floor_y(got_header), -1.0)
        self.assertEqual(gscap.header_floor_y({"floor": -0.5}), -0.5)
        self.assertIsNone(gscap.header_floor_y({"floor": "low"}))
        self.assertIsNone(gscap.header_region({"region": {"center": [1, 2], "radius": 1}}))

    def test_command_line(self):
        with tempfile.TemporaryDirectory() as tmp:
            synth = os.path.join(tmp, "synth.gscap")
            out = io.StringIO()
            with contextlib.redirect_stdout(out):
                self.assertEqual(gscap.main(["synth", synth, "--frames", "12", "--depth-width", "32", "--color-width", "64"]), 0)
                self.assertEqual(gscap.main(["verify", synth]), 0)
                self.assertEqual(gscap.main(["info", "--frames", synth]), 0)
                self.assertEqual(gscap.main(["extract", synth, os.path.join(tmp, "out"), "--every", "5"]), 0)
                self.assertEqual(gscap.main(["verify", fixture("frames", "frame_lidar.bin")]), 0)
                self.assertEqual(gscap.main(["verify", fixture("truncated.gscap")]), 0)
            self.assertIn("12 frames", out.getvalue())
            self.assertIn("frames: 12", out.getvalue())
            names = sorted(os.listdir(os.path.join(tmp, "out")))
            self.assertIn("frame_000000.jpg", names)
            self.assertIn("frame_000005_depth.png", names)
            self.assertIn("frame_000005_depth_view.png", names)
            self.assertIn("frame_000010_conf.png", names)
            self.assertIn("header.json", names)
            with open(os.path.join(tmp, "out", "frame_000000_depth.png"), "rb") as f:
                png = f.read()
            self.assertEqual(png[:8], b"\x89PNG\r\n\x1a\n")
            self.assertEqual(struct.unpack(">IIBB", png[16:26]), (32, 24, 16, 0))  # 16-bit grayscale
            first = next(gscap.synth_frames(count=12, depth_size=(32, 24), color_size=(64, 48)))
            idat = png[png.index(b"IDAT") + 4 : png.index(b"IEND") - 8]
            rows = zlib.decompress(idat)
            self.assertEqual(len(rows), 24 * (1 + 32 * 2))
            self.assertEqual(struct.unpack(">32H", rows[1:65]), tuple(first.depth_mm()[:32]))
            err = io.StringIO()
            with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
                self.assertEqual(gscap.main(["verify", fixture("bad_header_json.gscap")]), 1)
                self.assertEqual(gscap.main(["info", fixture("frames", "bad_tracking.bin")]), 1)

    def test_synthetic_capture_is_geometrically_consistent(self):
        frames = list(gscap.synth_frames(count=8, depth_size=(32, 24), color_size=(64, 48)))
        self.assertEqual([f.index for f in frames], list(range(8)))
        for frame in frames:
            m = frame.world_from_cam
            # Unproject the centre pixels and check that they land on a wall of the room.
            fx, fy, cx, cy = (v * frame.depth_width / frame.color_width for v in frame.intrinsics)
            mm = frame.depth_mm()
            for row, col in ((12, 16), (3, 5), (20, 30)):
                z = mm[row * frame.depth_width + col] / 1000.0
                self.assertGreater(z, 0)
                xc = (col + 0.5 - cx) / fx * z
                yc = -(row + 0.5 - cy) / fy * z
                zc = -z
                world = [m[0 + k] * xc + m[4 + k] * yc + m[8 + k] * zc + m[12 + k] for k in range(3)]
                on_wall = (abs(abs(world[0]) - 2.0) < 0.01 or abs(abs(world[2]) - 2.5) < 0.01
                           or abs(world[1] + 1.4) < 0.01 or abs(world[1] - 1.2) < 0.01)
                self.assertTrue(on_wall, world)


class PairingTests(unittest.TestCase):
    def test_fixture_cases(self):
        cases = load_json("pairing_uris.json")
        self.assertGreater(sum(1 for c in cases if c["ok"]), 10)
        self.assertGreater(sum(1 for c in cases if not c["ok"]), 30)
        for case in cases:
            if case["ok"]:
                pairing = gscap.parse_pairing_uri(case["uri"])
                self.assertEqual((pairing.host, pairing.port, pairing.fingerprint, pairing.token),
                                 (case["host"], case["port"], case["fp"], case["token"]), case["uri"])
                self.assertEqual(pairing.ingest_url, case["ingest_url"])
                self.assertEqual(pairing.captures_url, case["captures_url"])
                # The canonical URI parses to the same pairing.
                again = gscap.parse_pairing_uri(pairing.uri)
                self.assertEqual((again.host, again.port, again.fingerprint, again.token),
                                 (pairing.host, pairing.port, pairing.fingerprint, pairing.token))
            else:
                with self.assertRaises(gscap.FormatError, msg=case["uri"]) as caught:
                    gscap.parse_pairing_uri(case["uri"])
                self.assertEqual(caught.exception.code, case["error"], case["uri"])

    def test_documented_example(self):
        fp = "ab" * 32
        token = "0f" * 16
        pairing = gscap.parse_pairing_uri("splatlive://pair?host=192.168.1.20&port=7879&fp=%s&token=%s" % (fp, token))
        self.assertEqual(pairing.ingest_url, "wss://192.168.1.20:7879/ingest")
        self.assertEqual(pairing.captures_url, "https://192.168.1.20:7879/api/captures")


class MessageTests(unittest.TestCase):
    def test_client_messages_match_the_specification(self):
        hello = json.loads(gscap.hello_message("t" * 32, "room", "Salon", device="iPhone17,2", app="0.1.0"))
        self.assertEqual(hello, {"type": "hello", "protocol": 1, "token": "t" * 32, "device": "iPhone17,2",
                                 "app": "0.1.0", "mode": "room", "name": "Salon"})
        self.assertEqual(json.loads(gscap.region_message((1, 2, 3), 0.5)),
                         {"type": "region", "center": [1.0, 2.0, 3.0], "radius": 0.5})
        self.assertEqual(json.loads(gscap.floor_message(-1.5)), {"type": "floor", "y": -1.5})
        for kind in ("pause", "resume", "reset", "finish"):
            self.assertEqual(json.loads(gscap.simple_message(kind)), {"type": kind})
        self.assertEqual(json.loads(gscap.welcome_message(4)), {"type": "welcome", "protocol": 1, "max_inflight": 4})
        self.assertEqual(json.loads(gscap.ack_message(3, True)), {"type": "ack", "frame": 3, "used": True})
        self.assertEqual(json.loads(gscap.progress_message(5, 9)), {"type": "progress", "frames": 5, "splats": 9})

    def test_fixture(self):
        cases = load_json("messages.json")
        for case in cases["server"]:
            self.assertEqual(gscap.parse_server_message(case["text"]), case["expect"], case["text"])
        kinds = [c["expect"]["kind"] for c in cases["server"] if c["expect"]]
        self.assertEqual(set(kinds), {"welcome", "ack", "progress", "error", "unknown"})
        builders = {"hello": lambda a: gscap.hello_message(a["token"], a["mode"], a["name"], a["device"], a["app"]),
                    "region": lambda a: gscap.region_message(a["center"], a["radius"]),
                    "floor": lambda a: gscap.floor_message(a["y"])}
        for case in cases["client"]:
            build = builders.get(case["build"], lambda a, kind=case["build"]: gscap.simple_message(kind))
            self.assertEqual(json.loads(build(case["args"])), case["json"])


class WebSocketCodecTests(unittest.TestCase):
    def pair(self):
        a, b = socket.socketpair()
        return wsproto.WebSocket(a, is_client=True), wsproto.WebSocket(b, is_client=False)

    def test_known_handshake_value(self):
        # The example of RFC 6455, section 1.3.
        self.assertEqual(wsproto.accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=")

    def test_masking(self):
        # The masked "Hello" of RFC 6455, section 5.7.
        self.assertEqual(wsproto.mask_bytes(b"Hello", bytes.fromhex("37fa213d")), bytes.fromhex("7f9f4d5158"))
        frame = wsproto.encode_frame(wsproto.OP_TEXT, b"Hello", masked=False)
        self.assertEqual(frame, bytes.fromhex("810548656c6c6f"))
        masked = wsproto.encode_frame(wsproto.OP_BINARY, b"abc", masked=True)
        self.assertEqual(masked[0], 0x82)
        self.assertEqual(masked[1], 0x80 | 3)
        self.assertEqual(wsproto.mask_bytes(masked[6:], masked[2:6]), b"abc")

    def test_messages_of_every_length_class(self):
        client, server = self.pair()
        try:
            for size in (0, 1, 125, 126, 65535, 65536, 300000):
                payload = bytes((i * 31) % 256 for i in range(size))
                sender = threading.Thread(target=client.send_binary, args=(payload,))
                sender.start()
                self.assertEqual(server.recv(5.0), (wsproto.OP_BINARY, payload))
                sender.join()
                server.send_text("ok %d" % size)
                self.assertEqual(client.recv(5.0), (wsproto.OP_TEXT, ("ok %d" % size).encode()))
            self.assertIsNone(server.recv(0.05))
        finally:
            client.shutdown()
            server.shutdown()

    def test_ping_fragments_and_close(self):
        client, server = self.pair()
        try:
            client.ping(b"abc")
            client.sock.sendall(wsproto.encode_frame(wsproto.OP_TEXT, b"frag", masked=True, fin=False))
            client.sock.sendall(wsproto.encode_frame(wsproto.OP_CONT, b"ment", masked=True, fin=False))
            client.sock.sendall(wsproto.encode_frame(wsproto.OP_CONT, b"ed", masked=True, fin=True))
            self.assertEqual(server.recv(2.0), (wsproto.OP_TEXT, b"fragmented"))
            # The ping was answered while waiting.
            raw = client.sock.recv(16)
            self.assertEqual(raw, wsproto.encode_frame(wsproto.OP_PONG, b"abc", masked=False))
            client.close(1000, "bye")
            with self.assertRaises(wsproto.WsClosed) as caught:
                server.recv(2.0)
            self.assertEqual((caught.exception.code, caught.exception.reason), (1000, "bye"))
            with self.assertRaises(wsproto.WsClosed):
                client.recv(2.0)  # the close frame echoed by the server
        finally:
            client.shutdown()
            server.shutdown()

    def test_unmasked_client_frame_is_refused(self):
        client, server = self.pair()
        try:
            client.sock.sendall(wsproto.encode_frame(wsproto.OP_TEXT, b"x", masked=False))
            with self.assertRaises(wsproto.WsError):
                server.recv(2.0)
        finally:
            client.shutdown()
            server.shutdown()


@unittest.skipUnless(shutil.which("openssl"), "the openssl command line tool is not installed")
class NetworkTests(unittest.TestCase):
    """fake_phone.py against fake_server.py over real TLS on the loopback interface."""

    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.mkdtemp(prefix="splat-tools-")
        cls.cert, cls.key = fake_server.make_self_signed_cert(os.path.join(cls.tmp, "cert"), ["127.0.0.1"])
        cls.synth = os.path.join(cls.tmp, "synth.gscap")
        gscap.write_gscap(cls.synth, gscap.make_header(mode="room", name="Synth", created="2026-11-14T10:00:00Z"),
                          gscap.synth_frames(count=40, depth_size=(32, 24), color_size=(64, 48)))

    @classmethod
    def tearDownClass(cls):
        shutil.rmtree(cls.tmp, ignore_errors=True)

    def server(self, **options):
        options.setdefault("progress_interval", 0.05)
        server = fake_server.FakeServer(certfile=self.cert, keyfile=self.key, **options).start()
        self.addCleanup(server.stop)
        return server

    def raw_client(self, server):
        """An /ingest WebSocket opened by hand (to misbehave on purpose)."""
        tls = fake_phone.connect_pinned(gscap.parse_pairing_uri(server.pairing_uri()))
        ws = wsproto.client_handshake(tls, "127.0.0.1:%d" % server.port, "/ingest")
        self.addCleanup(ws.shutdown)
        return ws

    def expect_error(self, ws, code):
        opcode, payload = ws.recv(5.0)
        message = json.loads(payload.decode("utf-8"))
        self.assertEqual((message["type"], message["code"]), ("error", code))
        self.assertTrue(message["message"])
        with self.assertRaises(wsproto.WsClosed):  # "... followed by a close"
            ws.recv(5.0)

    def test_certificate_reuse_and_fingerprint(self):
        again = fake_server.make_self_signed_cert(os.path.join(self.tmp, "cert"), ["127.0.0.1"])
        self.assertEqual(again, (self.cert, self.key))
        fp = fake_server.fingerprint_of_pem(self.cert)
        self.assertRegex(fp, "^[0-9a-f]{64}$")
        server = self.server()
        self.assertEqual(server.fingerprint, fp)
        pairing = gscap.parse_pairing_uri(server.pairing_uri())
        self.assertEqual((pairing.host, pairing.port, pairing.fingerprint, pairing.token),
                         ("127.0.0.1", server.port, fp, server.token))
        self.assertRegex(server.token, "^[0-9a-f]{32}$")

    def test_stream_fixture_file(self):
        server = self.server()
        stats = fake_phone.stream_capture(fixture("three_frames.gscap"), server.pairing_uri())
        self.assertEqual((stats["frames_sent"], stats["acked"], stats["used"]), (3, 3, 2))
        self.assertEqual(stats["max_inflight"], 4)
        session = server.sessions[0]
        self.assertTrue(session.done.wait(5.0))
        self.assertEqual(session.hello, {"type": "hello", "protocol": 1, "token": server.token,
                                         "device": "iPhone17,2", "app": "0.1.0", "mode": "object", "name": "Théière"})
        self.assertEqual(session.frame_indices, [10, 11, 17])
        self.assertEqual(session.region, {"type": "region", "center": [0.5, -0.25, -1.5], "radius": 0.75})
        self.assertEqual(session.floor, {"type": "floor", "y": -1.25})
        self.assertEqual([m["type"] for m in session.messages], ["region", "floor", "finish"])
        self.assertTrue(session.finished)
        self.assertIsNone(session.error)
        self.assertEqual(session.close_code, 1000)
        self.assertEqual(stats["progress"]["frames"], 3)

    def test_flow_control_with_slow_acks(self):
        server = self.server(max_inflight=2, ack_delay=0.01)
        stats = fake_phone.stream_capture(self.synth, server.pairing_uri())
        self.assertEqual((stats["frames_sent"], stats["acked"], stats["frames_skipped"]), (40, 40, 0))
        self.assertEqual(stats["max_inflight"], 2)
        self.assertLessEqual(stats["max_unacked"], 2)
        session = server.sessions[0]
        self.assertTrue(session.done.wait(5.0))
        self.assertEqual(session.frame_indices, list(range(40)))
        self.assertLessEqual(session.max_unacked, 2)
        self.assertFalse(session.flow_violation)
        self.assertGreater(stats["progress_count"], 1)
        self.assertGreater(stats["progress"]["splats"], 0)

    def test_realtime_replay_skips_instead_of_queueing(self):
        server = self.server(max_inflight=1, ack_delay=0.05)
        stats = fake_phone.stream_capture(self.synth, server.pairing_uri(), realtime=True, speed=20.0)
        self.assertGreater(stats["frames_skipped"], 0)
        self.assertEqual(stats["frames_sent"] + stats["frames_skipped"], 40)
        self.assertEqual(stats["acked"], stats["frames_sent"])
        self.assertEqual(stats["max_unacked"], 1)
        session = server.sessions[0]
        self.assertTrue(session.done.wait(5.0))
        self.assertFalse(session.flow_violation)
        self.assertEqual(session.frame_indices, sorted(session.frame_indices))

    def test_saved_stream_is_a_valid_capture_file(self):
        save = os.path.join(self.tmp, "saved")
        server = self.server(save_dir=save)
        fake_phone.stream_capture(fixture("three_frames.gscap"), server.pairing_uri())
        session = server.sessions[0]
        self.assertTrue(session.done.wait(5.0))
        header, frames, truncated = gscap.read_gscap(session.saved_path)
        original_header, original_frames, _ = gscap.read_gscap(fixture("three_frames.gscap"))
        self.assertEqual(frames, original_frames)
        self.assertFalse(truncated)
        self.assertNotIn("token", header)
        self.assertNotIn("type", header)
        for key in ("protocol", "device", "app", "mode", "name", "region", "floor"):
            self.assertEqual(header[key], original_header[key], key)
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(gscap.main(["verify", session.saved_path]), 0)

    def test_wrong_token(self):
        server = self.server()
        pairing = gscap.parse_pairing_uri(server.pairing_uri())
        wrong = gscap.Pairing(pairing.host, pairing.port, pairing.fingerprint, "f" * 32).uri
        with self.assertRaises(fake_phone.PhoneError) as caught:
            fake_phone.stream_capture(fixture("three_frames.gscap"), wrong)
        self.assertEqual(caught.exception.code, "bad_token")
        session = server.sessions[0]
        self.assertTrue(session.done.wait(5.0))
        self.assertEqual(session.error, "bad_token")
        self.assertEqual(session.frame_indices, [])

    def test_wrong_fingerprint_sends_nothing(self):
        server = self.server()
        pairing = gscap.parse_pairing_uri(server.pairing_uri())
        wrong = gscap.Pairing(pairing.host, pairing.port, "0" * 64, pairing.token).uri
        with self.assertRaises(fake_phone.PhoneError) as caught:
            fake_phone.stream_capture(fixture("three_frames.gscap"), wrong)
        self.assertEqual(caught.exception.code, "bad_fingerprint")
        with self.assertRaises(fake_phone.PhoneError) as caught:
            fake_phone.upload_capture(fixture("three_frames.gscap"), wrong)
        self.assertEqual(caught.exception.code, "bad_fingerprint")
        time.sleep(0.2)
        self.assertEqual(server.sessions, [])
        self.assertEqual(server.uploads, [])

    def test_unreachable_server(self):
        probe = socket.socket()
        probe.bind(("127.0.0.1", 0))
        port = probe.getsockname()[1]
        probe.close()
        uri = gscap.Pairing("127.0.0.1", port, "0" * 64, "0" * 32).uri
        with self.assertRaises(fake_phone.PhoneError) as caught:
            fake_phone.stream_capture(fixture("three_frames.gscap"), uri, timeout=2.0)
        self.assertEqual(caught.exception.code, "connect")

    def test_bad_hello_variants(self):
        server = self.server()
        ws = self.raw_client(server)
        ws.send_text(json.dumps({"type": "hello", "protocol": 2, "token": server.token, "mode": "room"}))
        self.expect_error(ws, "bad_protocol")
        ws = self.raw_client(server)
        ws.send_binary(gscap.encode_frame(make_fixtures.small_frame(0)))
        self.expect_error(ws, "bad_hello")
        ws = self.raw_client(server)
        ws.send_text(json.dumps({"type": "hello", "protocol": 1, "mode": "room"}))
        self.expect_error(ws, "bad_token")
        ws = self.raw_client(server)
        ws.send_text(json.dumps({"type": "hello", "protocol": 1, "token": server.token, "mode": "panorama"}))
        self.expect_error(ws, "bad_hello")

    def test_hello_timeout(self):
        server = self.server(hello_timeout=0.2)
        ws = self.raw_client(server)
        self.expect_error(ws, "bad_hello")

    def welcome(self, server, mode="room"):
        ws = self.raw_client(server)
        ws.send_text(gscap.hello_message(server.token, mode, "raw"))
        opcode, payload = ws.recv(5.0)
        self.assertEqual(json.loads(payload.decode("utf-8")),
                         {"type": "welcome", "protocol": 1, "max_inflight": server.max_inflight})
        return ws

    def next_of_type(self, ws, kind, timeout=5.0):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            message = ws.recv(deadline - time.monotonic())
            if message is None:
                break
            obj = json.loads(message[1].decode("utf-8"))
            if obj["type"] == kind:
                return obj
        self.fail("no %s message" % kind)

    def test_ack_for_every_frame_and_control_messages(self):
        server = self.server(progress_interval=30.0)
        ws = self.welcome(server, "live")
        normal = gscap.encode_frame(make_fixtures.small_frame(0))    # index 10, tracking normal
        limited = gscap.encode_frame(make_fixtures.small_frame(1))   # index 11, tracking limited
        ws.send_binary(normal)
        self.assertEqual(self.next_of_type(ws, "ack"), {"type": "ack", "frame": 10, "used": True})
        ws.send_binary(limited)
        self.assertEqual(self.next_of_type(ws, "ack"), {"type": "ack", "frame": 11, "used": False})
        ws.send_text(gscap.simple_message("pause"))
        ws.send_binary(normal)
        self.assertEqual(self.next_of_type(ws, "ack"), {"type": "ack", "frame": 10, "used": False})
        ws.send_text(gscap.simple_message("resume"))
        ws.send_text(gscap.simple_message("reset"))
        ws.send_binary(normal)
        self.assertEqual(self.next_of_type(ws, "ack"), {"type": "ack", "frame": 10, "used": True})
        ws.send_text(gscap.simple_message("finish"))
        self.assertEqual(self.next_of_type(ws, "progress")["frames"], 1)
        ws.close(1000)
        session = server.sessions[0]
        self.assertTrue(session.done.wait(5.0))
        self.assertEqual([m["type"] for m in session.messages], ["pause", "resume", "reset", "finish"])
        self.assertEqual((session.resets, session.finished, session.hello["mode"]), (1, True, "live"))

    def test_every_invalid_fixture_frame_is_refused_by_the_server(self):
        """error + close for each of them, and the server keeps serving afterwards."""
        server = self.server()
        paths = sorted(glob.glob(fixture("frames", "bad_*.bin")))
        self.assertGreaterEqual(len(paths), 29)
        for path in paths:
            ws = self.welcome(server)
            with open(path, "rb") as f:
                ws.send_binary(f.read())
            self.expect_error(ws, "bad_frame")
        for session in server.sessions:
            self.assertTrue(session.done.wait(5.0))
            self.assertEqual((session.error, session.frame_indices), ("bad_frame", []))
        stats = fake_phone.stream_capture(fixture("three_frames.gscap"), server.pairing_uri())
        self.assertEqual((stats["frames_sent"], stats["acked"]), (3, 3))

    def test_hostile_text_messages(self):
        server = self.server()
        for text in ("[" * 100000, '{"type":NaN}', "[]", "null", '"pause"', '{"type":["pause"]}', '{"type":{"a":1}}',
                     '{"Type":"pause"}', ""):
            ws = self.welcome(server)
            ws.send_text(text)
            self.expect_error(ws, "bad_message")
        for hello in ("[" * 100000, "[]", '"hello"', '{"type":"hello","protocol":NaN}'):
            ws = self.raw_client(server)
            ws.send_text(hello)
            self.expect_error(ws, "bad_hello")
        stats = fake_phone.stream_capture(fixture("three_frames.gscap"), server.pairing_uri())
        self.assertEqual(stats["acked"], 3)

    def test_upload_of_damaged_files(self):
        server = self.server()
        uri = server.pairing_uri()
        for name in sorted(os.path.basename(p) for p in glob.glob(fixture("bad_*.gscap"))):
            status, body = fake_phone.upload_capture(fixture(name), uri)
            self.assertEqual((status, body["ok"]), (400, False), name)
        status, body = fake_phone.upload_capture(fixture("truncated_length.gscap"), uri)
        self.assertEqual((status, body["frames"], body["truncated"]), (200, 2, True))
        status, body = fake_phone.upload_capture(fixture("frames", "frame_plain.bin"), uri)   # a frame is not a file
        self.assertEqual((status, body["ok"]), (400, False))

    def test_bad_frame_and_bad_message(self):
        server = self.server()
        ws = self.welcome(server)
        with open(fixture("frames", "bad_deflate_garbage.bin"), "rb") as f:
            ws.send_binary(f.read())
        self.expect_error(ws, "bad_frame")
        ws = self.welcome(server)
        ws.send_text("{\"type\":\"dance\"}")
        self.expect_error(ws, "bad_message")
        ws = self.welcome(server)
        ws.send_text("hello?")
        self.expect_error(ws, "bad_message")

    def test_flow_control_violation_is_detected(self):
        server = self.server(max_inflight=2, ack_delay=5.0, progress_interval=30.0)
        ws = self.welcome(server)
        frame = gscap.encode_frame(make_fixtures.small_frame(0))
        for _ in range(3):
            ws.send_binary(frame)
        self.expect_error(ws, "flow_control")
        self.assertTrue(server.sessions[0].flow_violation)

    def test_upload(self):
        save = os.path.join(self.tmp, "uploads")
        server = self.server(save_dir=save)
        uri = server.pairing_uri()
        status, body = fake_phone.upload_capture(self.synth, uri)
        self.assertEqual((status, body["ok"], body["frames"], body["name"]), (200, True, 40, "Synth"))
        self.assertTrue(filecmp.cmp(server.uploads[0]["saved_path"], self.synth, shallow=False))
        status, body = fake_phone.upload_capture(fixture("truncated.gscap"), uri)
        self.assertEqual((status, body["frames"], body["truncated"]), (200, 2, True))
        status, body = fake_phone.upload_capture(self.synth, uri, token="f" * 32)
        self.assertEqual((status, body["ok"]), (401, False))
        status, body = fake_phone.upload_capture(fixture("bad_header_json.gscap"), uri)
        self.assertEqual((status, body["ok"]), (400, False))
        self.assertEqual([u["error"] for u in server.uploads], [None, None, "unauthorized", "bad_header"])

    def test_plain_http_routes(self):
        server = self.server()
        pairing = gscap.parse_pairing_uri(server.pairing_uri())

        def get(path):
            tls = fake_phone.connect_pinned(pairing)
            try:
                tls.sendall(("GET %s HTTP/1.1\r\nHost: %s\r\nConnection: close\r\n\r\n" % (path, pairing.authority)).encode())
                status, headers, leftover = wsproto.read_http_head(tls, 5.0)
                body = b"".join(wsproto.read_exact(tls, int(headers["content-length"]), leftover, 5.0))
                return int(status.split(" ")[1]), json.loads(body.decode("utf-8"))
            finally:
                tls.close()

        status, info = get("/api/info")
        self.assertEqual((status, info["protocol"], info["lan"]["fingerprint"]), (200, 1, server.fingerprint))
        status, pair = get("/api/pair")
        self.assertEqual((status, pair["uri"]), (200, server.pairing_uri()))
        self.assertEqual(get("/nothing")[0], 404)
        self.assertEqual(get("/ingest")[0], 400)  # not a WebSocket upgrade


if __name__ == "__main__":
    unittest.main()
