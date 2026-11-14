"""A very small baseline JPEG writer (standard library only).

It exists so that fixtures and synthetic captures can carry real, decodable
JPEGs without any imaging dependency. Every 8x8 block is one flat colour (only
the DC coefficient is coded), which is enough for tests: the capture format
treats the colour image as opaque bytes anyway.

    encode_blocks(width, height, block_rgb) -> bytes
    encode_flat(width, height, rgb) -> bytes
    inspect(data) -> dict   (marker walk: SOI, SOF0 size, SOS, EOI)
"""

from __future__ import annotations

import struct

# Annex K standard Huffman tables (luminance DC and AC).
_DC_BITS = bytes([0, 1, 5, 1, 1, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0])
_DC_VALS = bytes(range(12))
_AC_BITS = bytes([0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 0x7D])
_AC_VALS = bytes.fromhex(
    "01020300041105122131410613516107"
    "227114328191a1082342b1c11552d1f0"
    "2433627282090a161718191a25262728"
    "292a3435363738393a43444546474849"
    "4a535455565758595a63646566676869"
    "6a737475767778797a83848586878889"
    "8a92939495969798999aa2a3a4a5a6a7"
    "a8a9aab2b3b4b5b6b7b8b9bac2c3c4c5"
    "c6c7c8c9cad2d3d4d5d6d7d8d9dae1e2"
    "e3e4e5e6e7e8e9eaf1f2f3f4f5f6f7f8"
    "f9fa"
)

# Quantisation table: 8 for the DC term makes the coded DC value equal to
# (block mean - 128); the AC terms are never used.
_QUANT = bytes([8] + [16] * 63)


def _huffman_codes(bits: bytes, vals: bytes) -> dict:
    """Canonical Huffman codes: symbol -> (code, length)."""
    codes = {}
    code = 0
    k = 0
    for length in range(1, 17):
        for _ in range(bits[length - 1]):
            codes[vals[k]] = (code, length)
            code += 1
            k += 1
        code <<= 1
    return codes


_DC_CODES = _huffman_codes(_DC_BITS, _DC_VALS)
_AC_CODES = _huffman_codes(_AC_BITS, _AC_VALS)
_EOB = _AC_CODES[0x00]


class _BitWriter:
    def __init__(self) -> None:
        self.out = bytearray()
        self.acc = 0
        self.n = 0

    def put(self, code: int, length: int) -> None:
        self.acc = (self.acc << length) | (code & ((1 << length) - 1))
        self.n += length
        while self.n >= 8:
            self.n -= 8
            byte = (self.acc >> self.n) & 0xFF
            self.out.append(byte)
            if byte == 0xFF:
                self.out.append(0x00)  # byte stuffing
        self.acc &= (1 << self.n) - 1

    def finish(self) -> bytes:
        if self.n:
            pad = 8 - self.n
            self.put((1 << pad) - 1, pad)
        return bytes(self.out)


def _segment(marker: int, payload: bytes) -> bytes:
    return struct.pack(">BBH", 0xFF, marker, len(payload) + 2) + payload


def _rgb_to_ycbcr(rgb) -> tuple:
    r, g, b = rgb
    y = 0.299 * r + 0.587 * g + 0.114 * b
    cb = 128 - 0.168736 * r - 0.331264 * g + 0.5 * b
    cr = 128 + 0.5 * r - 0.418688 * g - 0.081312 * b
    return tuple(max(0, min(255, int(round(v)))) for v in (y, cb, cr))


def encode_blocks(width: int, height: int, block_rgb) -> bytes:
    """Encode an image whose 8x8 blocks are flat colours.

    `block_rgb(bx, by)` returns the (r, g, b) of block column bx, row by.
    """
    if not (1 <= width <= 65535 and 1 <= height <= 65535):
        raise ValueError("JPEG size out of range")
    bw = (width + 7) // 8
    bh = (height + 7) // 8

    out = bytearray(b"\xFF\xD8")
    out += _segment(0xE0, b"JFIF\x00\x01\x01\x00\x00\x01\x00\x01\x00\x00")
    out += _segment(0xDB, b"\x00" + _QUANT)
    # SOF0: 8 bit, three components, no subsampling, all on quant table 0.
    out += _segment(
        0xC0,
        struct.pack(">BHHB", 8, height, width, 3) + bytes([1, 0x11, 0, 2, 0x11, 0, 3, 0x11, 0]),
    )
    out += _segment(0xC4, b"\x00" + _DC_BITS + _DC_VALS)
    out += _segment(0xC4, b"\x10" + _AC_BITS + _AC_VALS)
    out += _segment(0xDA, bytes([3, 1, 0x00, 2, 0x00, 3, 0x00, 0, 63, 0]))

    bits = _BitWriter()
    previous = [0, 0, 0]
    for by in range(bh):
        for bx in range(bw):
            ycc = _rgb_to_ycbcr(block_rgb(bx, by))
            for c in range(3):
                dc = ycc[c] - 128
                diff = dc - previous[c]
                previous[c] = dc
                category = abs(diff).bit_length()
                code, length = _DC_CODES[category]
                bits.put(code, length)
                if category:
                    bits.put(diff if diff > 0 else diff + (1 << category) - 1, category)
                bits.put(*_EOB)
    out += bits.finish()
    out += b"\xFF\xD9"
    return bytes(out)


def encode_flat(width: int, height: int, rgb=(128, 128, 128)) -> bytes:
    return encode_blocks(width, height, lambda bx, by: rgb)


def inspect(data: bytes) -> dict:
    """Walk the marker segments of a JPEG. Raises ValueError if malformed."""
    if len(data) < 4 or data[:2] != b"\xFF\xD8":
        raise ValueError("no SOI marker")
    pos = 2
    info = {"markers": ["SOI"], "width": None, "height": None, "components": None}
    while True:
        if pos + 4 > len(data) or data[pos] != 0xFF:
            raise ValueError("bad marker at offset %d" % pos)
        marker = data[pos + 1]
        (length,) = struct.unpack(">H", data[pos + 2 : pos + 4])
        if length < 2 or pos + 2 + length > len(data):
            raise ValueError("bad segment length at offset %d" % pos)
        body = data[pos + 4 : pos + 2 + length]
        pos += 2 + length
        info["markers"].append("%02X" % marker)
        if marker in (0xC0, 0xC1, 0xC2):
            precision, height, width, components = struct.unpack(">BHHB", body[:6])
            info.update(width=width, height=height, components=components, precision=precision)
        if marker == 0xDA:
            break
    if info["width"] is None:
        raise ValueError("no SOF segment before SOS")
    if data[-2:] != b"\xFF\xD9":
        raise ValueError("no EOI marker")
    info["markers"].append("EOI")
    info["scan_bytes"] = len(data) - pos - 2
    return info
