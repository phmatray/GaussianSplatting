"""Tests of the recording tool: python3 -m unittest discover -s tools

Standard library only. The fixtures under crates/gs-core/tests/record_data
are shared with the Rust tests (crates/gs-core/tests/record_tools.rs).
"""

from __future__ import annotations

import contextlib
import io
import json
import math
import os
import struct
import sys
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)

import gsrec  # noqa: E402

DATA = os.path.join(HERE, "..", "crates", "gs-core", "tests", "record_data")
FIRST_RECORD = 12 + gsrec.HEADER_JSON_SIZE


def codes(report):
    return [code for _, code in report["problems"]]


def patched(data, offset, new):
    out = bytearray(data)
    out[offset:offset + len(new)] = new
    return bytes(out)


def records(data):
    return list(gsrec.iter_records(data))


class Synth(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.plain = gsrec.synth_recording(splats=60, compress=False)
        cls.packed = gsrec.synth_recording(splats=60, compress=True)

    def test_synth_is_valid_and_deterministic(self):
        for data in (self.plain, self.packed):
            report = gsrec.verify(data)
            self.assertTrue(gsrec.is_valid(report), report["problems"])
            self.assertEqual(report["info"]["scene"], "synth")
            self.assertEqual(report["info"]["duration_s"], report["duration_s"])
            self.assertEqual(report["duration_s"], 4.0)
            self.assertEqual(len(report["keyframes"]), 4)
            self.assertEqual(report["keyframes"][0], (0.0, FIRST_RECORD))
            self.assertEqual((report["stats"], report["previews"]), (40, 40))
            self.assertEqual(report["records"], 4 + report["states"] + report["layers"] + 80 + 1)
        self.assertEqual(self.plain, gsrec.synth_recording(splats=60, compress=False))
        self.assertNotEqual(self.plain, gsrec.synth_recording(splats=60, compress=False, seed=2))
        # Random splats do not deflate; the JSON and the thumbnails do not either.
        packed = gsrec.verify(self.packed)
        self.assertLessEqual(packed["stored_bytes"], packed["raw_bytes"])
        self.assertEqual(packed["raw_bytes"], gsrec.verify(self.plain)["raw_bytes"])

    def test_the_fixture_is_what_synth_writes(self):
        with open(os.path.join(DATA, "python_synth.gsrec"), "rb") as handle:
            self.assertEqual(handle.read(), self.plain)

    def test_the_rust_fixture_reads_as_rust_says(self):
        with open(os.path.join(DATA, "rust_tiny.json"), "r", encoding="utf-8") as handle:
            expected = json.load(handle)
        got = json.loads(gsrec.report_json(os.path.join(DATA, "rust_tiny.gsrec")))
        self.assertEqual(got, expected)
        self.assertTrue(gsrec.is_valid(got))
        self.assertGreater(got["raw_bytes"], got["stored_bytes"])

    def test_keyframes_are_snapshots_of_the_stream(self):
        # Replaying the message records up to a keyframe gives the keyframe.
        state = live = stats = None
        static, previews = [], {}
        seen = 0
        for offset, kind, flags, time, stored in records(self.packed):
            payload = gsrec.inflate_raw(stored) if flags & gsrec.FLAG_DEFLATE else stored
            if kind == gsrec.KIND_KEYFRAME:
                messages = gsrec.split_keyframe(payload)
                if seen:
                    self.assertEqual(messages[0], state)
                    self.assertEqual(messages[1], gsrec.merge_layers(0, static))
                    self.assertEqual(messages[2], live)
                    self.assertEqual(messages[3:], [stats] + [previews[k] for k in sorted(previews)])
                else:
                    state, live, stats = messages[0], messages[2], messages[3]
                    static = [messages[1]]
                seen += 1
            elif kind == gsrec.KIND_MESSAGE:
                what = gsrec.classify(payload)
                if what == ("layer", 0, True):
                    static = [payload]
                elif what == ("layer", 0, False):
                    static.append(payload)
                elif what == ("layer", 1, True):
                    live = payload
                elif what == ("stats",):
                    stats = payload
                elif what[0] == "preview":
                    previews[what[1]] = payload
                else:
                    self.fail(what)
        self.assertEqual(seen, 4)

    def test_cut_anywhere_it_is_a_recording_cut_short(self):
        ends = [offset + gsrec.RECORD_HEADER_SIZE + len(stored) for offset, _, _, _, stored in records(self.plain)]
        times = [time for _, _, _, time, _ in records(self.plain)]
        for cut in list(range(0, len(self.plain), 37)) + list(range(len(self.plain) - 60, len(self.plain))):
            part = self.plain[:cut]
            if cut < FIRST_RECORD:
                with self.assertRaises(gsrec.FormatError) as raised:
                    gsrec.verify(part)
                self.assertEqual(raised.exception.code, "too_short", cut)
                continue
            report = gsrec.verify(part)
            whole = [i for i, end in enumerate(ends) if end <= cut]
            self.assertTrue(report["truncated"] and not report["complete"], cut)
            self.assertEqual(report["records"], len(whole), cut)
            self.assertEqual(report["duration_s"], times[whole[-1]] if whole else 0.0, cut)
            self.assertEqual(codes(report), [] if whole else ["empty"], cut)
            self.assertEqual(len(records(part)), len(whole))


class Damage(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.data = gsrec.synth_recording(splats=60, compress=False)
        cls.records = records(cls.data)
        # A message record in the middle, not right behind a keyframe.
        cls.at = next(i for i in range(40, len(cls.records)) if cls.records[i][1] == 0 and cls.records[i - 1][1] == 0)
        cls.offset = cls.records[cls.at][0]

    def check(self, data, expected, complete=None):
        report = gsrec.verify(data)
        self.assertEqual(report["problems"], expected)
        if complete is not None:
            self.assertEqual(report["complete"], complete)
        return report

    def test_file_header(self):
        cases = [
            (b"", "too_short"), (b"GSREC1", "too_short"), (self.data[:11], "too_short"), (self.data[:500], "too_short"),
            (b"GSCAP1\0\0" + self.data[8:], "bad_magic"), (b"X", "bad_magic"), (b"GSREC2\0\0", "bad_magic"),
            (gsrec.MAGIC + struct.pack("<I", gsrec.MAX_HEADER_JSON + 1), "bad_header"),
        ]
        for text in ["[]", "7", "null", "{", '{"duration_s":NaN}', '{"duration_s":1e999}', '{"a":"\\ud800"}', "",
                     '{"a":' + "[" * 200 + "]" * 200 + "}", '{"a":' + "[" * 100000 + "]" * 100000 + "}"]:
            cases.append((gsrec.MAGIC + struct.pack("<I", len(text)) + text.encode(), "bad_header"))
        cases.append((gsrec.MAGIC + struct.pack("<I", 4) + b"{\xff} ", "bad_header"))
        for data, code in cases:
            with self.assertRaises(gsrec.FormatError) as raised:
                gsrec.verify(data)
            self.assertEqual(raised.exception.code, code, data[:40])

    def test_header_fields_are_lenient(self):
        def info(text):
            return gsrec.parse_header(gsrec.MAGIC + struct.pack("<I", len(text)) + text.encode())[0]

        neutral = {"name": "", "created": "", "scene": "", "protocol": 0, "duration_s": 0.0}
        self.assertEqual(info("{}"), neutral)
        self.assertEqual(info('{"name":7,"protocol":-1,"duration_s":"long","scene":null}'), neutral)
        self.assertEqual(info('{"protocol":true,"duration_s":true}'), neutral)
        self.assertEqual(info('{"protocol":1.0,"duration_s":-3}'), neutral)
        self.assertEqual(info('{"protocol":99999999999,"duration_s":7}')["protocol"], 0xFFFFFFFF)
        self.assertEqual(info('{"protocol":99999999999,"duration_s":7}')["duration_s"], 7.0)
        # Beyond 64 bits an integer is a double: no protocol number.
        self.assertEqual(info('{"protocol":18446744073709551616}')["protocol"], 0)
        self.assertEqual(info('  {"name":"é","unknown":[1,{"a":2}]}   ')["name"], "é")
        self.assertEqual(info('{"a":' + "[" * 100 + "]" * 100 + "}"), neutral)

    def test_framing_damage_ends_the_readable_part(self):
        before = self.records[self.at - 1]
        for name, offset, new, code in [
            ("short length", 0, struct.pack("<I", 11), "bad_length"),
            ("huge length", 0, struct.pack("<I", gsrec.MAX_RECORD_LEN + 1), "bad_length"),
            ("kind", 4, b"\x03", "bad_kind"),
            ("flags", 5, b"\x02", "bad_flags"),
            ("time", 8, struct.pack("<d", math.nan), "bad_time"),
            ("negative time", 8, struct.pack("<d", -1.0), "bad_time"),
            ("time backwards", 8, struct.pack("<d", 0.0), "bad_time"),
        ]:
            report = self.check(patched(self.data, self.offset + offset, new), [(self.offset, code)], complete=False)
            self.assertEqual((report["records"], report["duration_s"], report["truncated"]), (self.at, before[3], False), name)
        # Order of the checks, and the reserved field is ignored.
        self.check(patched(self.data, self.offset + 4, b"\x09\xff"), [(self.offset, "bad_kind")])
        self.check(patched(self.data, self.offset + 6, b"\xab\xcd"), [], complete=True)

    def test_payload_damage_is_reported_and_skipped(self):
        payload = self.offset + gsrec.RECORD_HEADER_SIZE
        report = self.check(patched(self.data, payload, b"XXXX"), [(self.offset, "bad_message")], complete=True)
        self.assertEqual(report["records"], len(self.records))
        # A flag that claims deflate over a plain payload.
        self.check(patched(self.data, self.offset + 5, b"\x01"), [(self.offset, "bad_deflate")], complete=True)
        # A keyframe that is not a snapshot is not a keyframe, but stays in the index.
        keyframe = next(offset for offset, kind, _, _, _ in self.records[1:] if kind == gsrec.KIND_KEYFRAME)
        report = self.check(patched(self.data, keyframe + 16, struct.pack("<I", 5)), [(keyframe, "bad_keyframe")], complete=True)
        self.assertEqual(len(report["keyframes"]), 3)
        # The first record must be a keyframe; without any there is nothing to show.
        report = self.check(patched(self.data, FIRST_RECORD + 4, b"\x00"),
                            [(FIRST_RECORD, "bad_keyframe"), (FIRST_RECORD, "bad_message"), (self.records[-1][0], "bad_index")])
        self.assertFalse(report["complete"])
        only_messages = bytearray(self.data)
        for offset, kind, _, _, _ in self.records:
            if kind == gsrec.KIND_KEYFRAME:
                only_messages[offset + 4] = 0
        self.assertEqual(codes(gsrec.verify(bytes(only_messages)))[-1], "empty")

    def test_index_and_trailer(self):
        index = self.records[-1][0]
        self.assertEqual(self.records[-1][1], gsrec.KIND_INDEX)
        end = len(self.data)
        self.check(patched(self.data, index + 16 + 8, struct.pack("<Q", 7)), [(index, "bad_index")], complete=False)
        self.check(patched(self.data, index + 16, struct.pack("<d", 0.5)), [(index, "bad_index")], complete=False)
        self.check(patched(self.data, end - 16, struct.pack("<Q", index + 1)), [(end - 16, "bad_index")], complete=False)
        self.check(patched(self.data, end - 8, b"GSRECENX"), [(end - 16, "bad_index")], complete=False)
        self.check(self.data + b"x", [(end, "trailing_data")], complete=False)
        report = self.check(self.data[:end - 1], [], complete=False)
        self.assertTrue(report["truncated"])
        with self.assertRaises(gsrec.FormatError):
            gsrec.parse_index(b"\0" * 17)
        self.assertEqual(gsrec.parse_index(b""), [])

    def test_deflate_limits(self):
        self.assertEqual(gsrec.inflate_raw(gsrec.deflate_raw(b"abc" * 100)), b"abc" * 100)
        for bad in [b"", b"\x07", gsrec.deflate_raw(b"abc" * 100)[:-3]]:
            with self.assertRaises(gsrec.FormatError) as raised:
                gsrec.inflate_raw(bad)
            self.assertEqual(raised.exception.code, "bad_deflate")
        # A megabyte that inflates beyond the limit is refused, not held.
        bomb = gsrec.deflate_raw(bytes(gsrec.MAX_INFLATED + 1), 1)
        self.assertLess(len(bomb), 1 << 20)
        with self.assertRaises(gsrec.FormatError):
            gsrec.inflate_raw(bomb)


class Wire(unittest.TestCase):
    def test_classify(self):
        rng = gsrec._Lcg(3)
        box = (-1.0, 0.0, -1.0, 1.0, 2.0, 1.0)
        layer = gsrec.encode_layer(1, 2, 0, True, box, gsrec.synth_splats(rng, 5))
        self.assertEqual(gsrec.classify(layer), ("layer", 0, True))
        self.assertEqual(gsrec.classify(gsrec.encode_layer(1, 2, 1, False, box, b"")), ("layer", 1, False))
        self.assertEqual(gsrec.classify(gsrec.encode_text(gsrec.WIRE_STATE, 0, 0, "{}")), ("state",))
        self.assertEqual(gsrec.classify(gsrec.encode_text(gsrec.WIRE_STATS, 0, 0, "é")), ("stats",))
        preview = gsrec.encode_preview(0, 0, 4, 2, 3, bytes(18), bytes(12))
        self.assertEqual(gsrec.classify(preview), ("preview", 4))
        for bad in [
            b"", b"GSL1", layer[:-1], layer + b"\0", layer[:40], preview[:-1], preview[:20],
            patched(layer, 0, b"GSL2"), patched(layer, 4, b"\x09"), patched(layer, 16, b"\x02"), patched(layer, 17, b"\x02"),
            patched(layer, 20, struct.pack("<I", 6)), patched(layer, 20, struct.pack("<I", 0xFFFFFFFF)),
            patched(layer, 24, struct.pack("<f", math.nan)), patched(layer, 24, struct.pack("<f", 9.0)),
            gsrec.encode_text(gsrec.WIRE_STATS, 0, 0, "{}") + b"\xff",
        ]:
            self.assertIsNone(gsrec.classify(bad), bad[:24])

    def test_merge_layers(self):
        rng = gsrec._Lcg(8)
        boxes = [(-1.0, 0.0, -1.0, 1.0, 2.0, 1.0), (3.0, 0.5, -4.0, 5.0, 2.5, 2.0), (0.0,) * 6, (-6.0, 0.0, 0.0, -5.0, 0.1, 9.0)]
        counts = [50, 1, 0, 30]
        chunks = [gsrec.encode_layer(i, i, 0, i == 0, box, gsrec.synth_splats(rng, n)) for i, (box, n) in enumerate(zip(boxes, counts))]
        merged = gsrec.merge_layers(0, chunks)
        self.assertEqual(gsrec.classify(merged), ("layer", 0, True))
        self.assertEqual(merged[:16], chunks[-1][:16])
        self.assertEqual(struct.unpack_from("<I", merged, 20)[0], sum(counts))
        union = struct.unpack_from("<6f", merged, 24)
        self.assertEqual(union, (-6.0, 0.0, -4.0, 5.0, 2.5, 9.0))

        def positions(message):
            box = struct.unpack_from("<6f", message, 24)
            out = []
            for at in range(48, len(message), 16):
                u = struct.unpack_from("<3H", message, at)
                out.append(tuple(box[k] + (box[k + 3] - box[k]) * u[k] / 65535.0 for k in range(3)))
            return out

        before = [p for chunk in chunks for p in positions(chunk)]
        after = positions(merged)
        self.assertEqual(len(before), len(after))
        for k in range(3):
            step = (union[k + 3] - union[k]) / 65535.0
            self.assertLessEqual(max(abs(a[k] - b[k]) for a, b in zip(before, after)), step * 0.51 + 1e-6)
        tails = b"".join(chunk[at + 6:at + 16] for chunk in chunks for at in range(48, len(chunk), 16))
        self.assertEqual(tails, b"".join(merged[at + 6:at + 16] for at in range(48, len(merged), 16)))
        # One chunk is kept as it is; an append onto nothing is a replace.
        self.assertEqual(gsrec.merge_layers(0, chunks[:1]), chunks[0])
        self.assertEqual(gsrec.classify(gsrec.merge_layers(0, chunks[1:2])), ("layer", 0, True))
        self.assertEqual(gsrec.classify(gsrec.merge_layers(1, [])), ("layer", 1, True))
        self.assertEqual(gsrec.merge_layers(0, [merged, chunks[2]])[16:], merged[16:])

    def test_strict_json(self):
        self.assertEqual(gsrec.loads_strict(' {"a":[1,2.5,"x"]} '), {"a": [1, 2.5, "x"]})
        self.assertEqual(gsrec.loads_strict('"]]]]"'), "]]]]")
        for bad in ["NaN", "[Infinity]", "-Infinity", "1e999", '"\\udc00"', "[" * 128 + "]" * 128, "{", "01"]:
            with self.assertRaises(ValueError, msg=bad):
                gsrec.loads_strict(bad)
        self.assertEqual(gsrec.loads_strict("[" * 127 + "]" * 127)[0][0][0][0], gsrec.loads_strict("[" * 123 + "]" * 123))


class CommandLine(unittest.TestCase):
    def run_tool(self, *args):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            status = gsrec.main(list(args))
        return status, out.getvalue()

    def test_synth_info_verify(self):
        with tempfile.TemporaryDirectory() as directory:
            good = os.path.join(directory, "good.gsrec")
            status, text = self.run_tool("synth", good, "--seconds", "2", "--splats", "40", "--name", "Salon")
            self.assertEqual(status, 0)
            status, text = self.run_tool("verify", good)
            self.assertEqual(status, 0)
            self.assertIn("complete", text)
            self.assertIn("ok", text)
            status, text = self.run_tool("info", good)
            self.assertEqual(status, 0)
            self.assertIn("'Salon'", text)
            self.assertIn("2 keyframes", text)

            with open(good, "rb") as handle:
                data = handle.read()
            cut = os.path.join(directory, "cut.gsrec")
            with open(cut, "wb") as handle:
                handle.write(data[:len(data) // 2])
            junk = os.path.join(directory, "junk.gsrec")
            with open(junk, "wb") as handle:
                handle.write(b"not a recording")
            status, text = self.run_tool("verify", good, cut, junk, os.path.join(directory, "missing.gsrec"))
            self.assertEqual(status, 1)
            self.assertIn("cut short", text)
            self.assertIn("bad_magic", text)
            status, text = self.run_tool("verify", "--json", good, cut, junk)
            self.assertEqual(status, 1)
            lines = [json.loads(line) for line in text.splitlines()]
            self.assertEqual(len(lines), 3)
            self.assertTrue(lines[0]["complete"] and lines[1]["truncated"])
            self.assertEqual(lines[2], {"error": "bad_magic"})
            self.assertEqual(self.run_tool("verify", "--json", good)[0], 0)
            self.assertEqual(self.run_tool("info", junk)[0], 1)


if __name__ == "__main__":
    unittest.main()
