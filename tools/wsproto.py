"""Minimal RFC 6455 WebSocket and HTTP/1.1 helpers over a blocking socket.

Standard library only. Just enough for tools/fake_phone.py and
tools/fake_server.py: opening handshake (client and server), masking, text and
binary messages (fragmented ones are reassembled), ping/pong and close. No
extensions (permessage-deflate is never negotiated).

A WebSocket object is used from one thread; `recv(timeout)` polls with
select() so that a single loop can both wait for messages and do timed work.
"""

from __future__ import annotations

import base64
import hashlib
import os
import select
import socket
import ssl
import struct
import time

GUID = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11"

OP_CONT = 0x0
OP_TEXT = 0x1
OP_BINARY = 0x2
OP_CLOSE = 0x8
OP_PING = 0x9
OP_PONG = 0xA

MAX_HTTP_HEAD = 64 * 1024


class WsError(Exception):
    """Protocol violation or broken connection."""


class WsClosed(WsError):
    """The peer closed the connection (code 1006 when it just vanished)."""

    def __init__(self, code: int = 1006, reason: str = "") -> None:
        super().__init__("closed (%d) %s" % (code, reason))
        self.code = code
        self.reason = reason


def accept_key(key: str) -> str:
    return base64.b64encode(hashlib.sha1(key.encode("ascii") + GUID).digest()).decode("ascii")


def mask_bytes(data: bytes, key: bytes) -> bytes:
    n = len(data)
    if n == 0:
        return b""
    repeated = (key * (n // 4 + 1))[:n]
    return (int.from_bytes(data, "little") ^ int.from_bytes(repeated, "little")).to_bytes(n, "little")


def encode_frame(opcode: int, payload: bytes, masked: bool, fin: bool = True) -> bytes:
    head = bytearray([(0x80 if fin else 0) | opcode])
    n = len(payload)
    mask_bit = 0x80 if masked else 0
    if n < 126:
        head.append(mask_bit | n)
    elif n < 65536:
        head.append(mask_bit | 126)
        head += struct.pack(">H", n)
    else:
        head.append(mask_bit | 127)
        head += struct.pack(">Q", n)
    if masked:
        key = os.urandom(4)
        return bytes(head) + key + mask_bytes(payload, key)
    return bytes(head) + payload


def _readable(sock, timeout) -> bool:
    pending = getattr(sock, "pending", None)
    if pending is not None and pending() > 0:
        return True
    ready, _, _ = select.select([sock], [], [], timeout)
    return bool(ready)


def recv_chunk(sock, timeout):
    """Up to 64 KiB from the socket; None if no data arrived within `timeout`
    seconds (None = wait forever); b"" when the peer closed.

    select() only says that bytes arrived on the TCP socket. Over TLS those
    can be part of a record or a record without data (a session ticket), so
    the read itself is bounded too and "nothing yet" is reported as None.
    """
    if not _readable(sock, timeout):
        return None
    sock.settimeout(0.25)
    try:
        return sock.recv(1 << 16)
    except (socket.timeout, ssl.SSLWantReadError):
        return None
    finally:
        sock.settimeout(None)


class WebSocket:
    """One end of an established WebSocket connection."""

    def __init__(self, sock, is_client: bool, initial: bytes = b"", max_message: int = 80 << 20) -> None:
        self.sock = sock
        self.is_client = is_client
        self.max_message = max_message
        self._buf = bytearray(initial)
        self._fragments = None  # (opcode, bytearray) while a fragmented message is open
        self.close_sent = False
        self.closed = False

    # -- sending ----------------------------------------------------------

    def _send(self, opcode: int, payload: bytes) -> None:
        if self.closed:
            raise WsClosed(1006, "connection is closed")
        try:
            self.sock.sendall(encode_frame(opcode, payload, masked=self.is_client))
        except OSError as exc:
            self.closed = True
            raise WsClosed(1006, str(exc))

    def send_text(self, text: str) -> None:
        self._send(OP_TEXT, text.encode("utf-8"))

    def send_binary(self, data: bytes) -> None:
        self._send(OP_BINARY, data)

    def ping(self, payload: bytes = b"") -> None:
        self._send(OP_PING, payload)

    def close(self, code: int = 1000, reason: str = "") -> None:
        """Send a close frame (once). The socket itself is closed by shutdown()."""
        if self.close_sent or self.closed:
            return
        self.close_sent = True
        try:
            self._send(OP_CLOSE, struct.pack(">H", code) + reason.encode("utf-8")[:120])
        except WsClosed:
            pass

    def shutdown(self) -> None:
        self.closed = True
        try:
            self.sock.close()
        except OSError:
            pass

    # -- receiving --------------------------------------------------------

    def _parse_frame(self):
        """Pop one frame from the buffer: (fin, opcode, payload) or None."""
        buf = self._buf
        if len(buf) < 2:
            return None
        b0, b1 = buf[0], buf[1]
        if b0 & 0x70:
            raise WsError("reserved bits set (an extension was not negotiated)")
        masked = bool(b1 & 0x80)
        n = b1 & 0x7F
        pos = 2
        if n == 126:
            if len(buf) < 4:
                return None
            (n,) = struct.unpack_from(">H", buf, 2)
            pos = 4
        elif n == 127:
            if len(buf) < 10:
                return None
            (n,) = struct.unpack_from(">Q", buf, 2)
            pos = 10
        if n > self.max_message:
            raise WsError("frame of %d bytes is too large" % n)
        if masked == self.is_client:
            # Clients must mask, servers must not (RFC 6455, section 5.1).
            raise WsError("frame masking does not match the peer's role")
        if masked:
            if len(buf) < pos + 4 + n:
                return None
            key = bytes(buf[pos : pos + 4])
            pos += 4
            payload = mask_bytes(bytes(buf[pos : pos + n]), key)
        else:
            if len(buf) < pos + n:
                return None
            payload = bytes(buf[pos : pos + n])
        del buf[: pos + n]
        return bool(b0 & 0x80), b0 & 0x0F, payload

    def recv(self, timeout=None):
        """Next data message as (opcode, payload), or None after `timeout` seconds.

        Pings are answered, pongs dropped. A close frame is answered and
        raises WsClosed, as does a lost connection.
        """
        deadline = None if timeout is None else time.monotonic() + timeout
        while True:
            frame = self._parse_frame()
            if frame is None:
                if self.closed:
                    raise WsClosed(1006, "connection is closed")
                remaining = None if deadline is None else max(0.0, deadline - time.monotonic())
                try:
                    chunk = recv_chunk(self.sock, remaining)
                except (OSError, ValueError) as exc:
                    self.closed = True
                    raise WsClosed(1006, str(exc))
                if chunk is None:
                    if deadline is not None and time.monotonic() >= deadline:
                        return None
                    continue
                if not chunk:
                    self.closed = True
                    raise WsClosed(1006, "connection lost")
                self._buf += chunk
                continue
            fin, opcode, payload = frame
            if opcode == OP_PING:
                self._send(OP_PONG, payload)
            elif opcode == OP_PONG:
                pass
            elif opcode == OP_CLOSE:
                code = struct.unpack(">H", payload[:2])[0] if len(payload) >= 2 else 1005
                reason = payload[2:].decode("utf-8", "replace")
                self.close(code if code != 1005 else 1000)
                self.closed = True
                raise WsClosed(code, reason)
            elif opcode in (OP_TEXT, OP_BINARY):
                if self._fragments is not None:
                    raise WsError("new message inside a fragmented message")
                if fin:
                    return opcode, payload
                self._fragments = (opcode, bytearray(payload))
            elif opcode == OP_CONT:
                if self._fragments is None:
                    raise WsError("continuation frame without a message")
                self._fragments[1].extend(payload)
                if len(self._fragments[1]) > self.max_message:
                    raise WsError("message is too large")
                if fin:
                    first_opcode, data = self._fragments
                    self._fragments = None
                    return first_opcode, bytes(data)
            else:
                raise WsError("unknown opcode %d" % opcode)


# --------------------------------------------------------------------------
# HTTP/1.1 helpers
# --------------------------------------------------------------------------


def read_http_head(sock, timeout: float = 15.0, initial: bytes = b""):
    """Read up to the blank line: (first line, headers dict lower-cased, leftover bytes)."""
    buf = bytearray(initial)
    deadline = time.monotonic() + timeout
    while b"\r\n\r\n" not in buf:
        if len(buf) > MAX_HTTP_HEAD:
            raise WsError("HTTP head is too large")
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise WsError("timed out waiting for the HTTP head")
        chunk = recv_chunk(sock, remaining)
        if chunk is None:
            continue
        if not chunk:
            raise WsError("connection closed inside the HTTP head")
        buf += chunk
    head, _, leftover = bytes(buf).partition(b"\r\n\r\n")
    lines = head.decode("iso-8859-1").split("\r\n")
    headers = {}
    for line in lines[1:]:
        name, sep, value = line.partition(":")
        if sep:
            headers[name.strip().lower()] = value.strip()
    return lines[0], headers, leftover


def client_handshake(sock, host: str, path: str, timeout: float = 15.0, extra_headers=None) -> WebSocket:
    """Upgrade an open (TLS) socket to a WebSocket, client side."""
    key = base64.b64encode(os.urandom(16)).decode("ascii")
    lines = [
        "GET %s HTTP/1.1" % path,
        "Host: %s" % host,
        "Upgrade: websocket",
        "Connection: Upgrade",
        "Sec-WebSocket-Key: %s" % key,
        "Sec-WebSocket-Version: 13",
    ]
    for name, value in (extra_headers or {}).items():
        lines.append("%s: %s" % (name, value))
    sock.sendall(("\r\n".join(lines) + "\r\n\r\n").encode("ascii"))
    status, headers, leftover = read_http_head(sock, timeout)
    parts = status.split(" ", 2)
    if len(parts) < 2 or parts[1] != "101":
        raise WsError("the server refused the WebSocket upgrade: %s" % status)
    if headers.get("sec-websocket-accept") != accept_key(key):
        raise WsError("bad Sec-WebSocket-Accept")
    return WebSocket(sock, is_client=True, initial=leftover)


def is_websocket_upgrade(headers: dict) -> bool:
    return (
        headers.get("upgrade", "").lower() == "websocket"
        and "upgrade" in headers.get("connection", "").lower()
        and "sec-websocket-key" in headers
    )


def server_handshake(sock, headers: dict, leftover: bytes = b"") -> WebSocket:
    """Answer a WebSocket upgrade request whose head was already read."""
    if headers.get("sec-websocket-version") != "13":
        raise WsError("unsupported WebSocket version")
    response = (
        "HTTP/1.1 101 Switching Protocols\r\n"
        "Upgrade: websocket\r\n"
        "Connection: Upgrade\r\n"
        "Sec-WebSocket-Accept: %s\r\n\r\n" % accept_key(headers["sec-websocket-key"])
    )
    sock.sendall(response.encode("ascii"))
    return WebSocket(sock, is_client=False, initial=leftover)


def http_response(status: int, reason: str, body: bytes, content_type: str = "application/json") -> bytes:
    head = (
        "HTTP/1.1 %d %s\r\n"
        "Content-Type: %s\r\n"
        "Content-Length: %d\r\n"
        "Connection: close\r\n\r\n" % (status, reason, content_type, len(body))
    )
    return head.encode("ascii") + body


def _recv_body_chunk(sock, timeout: float) -> bytes:
    deadline = time.monotonic() + timeout
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise WsError("timed out reading an HTTP body")
        chunk = recv_chunk(sock, remaining)
        if chunk is None:
            continue
        if not chunk:
            raise WsError("connection closed inside an HTTP body")
        return chunk


def read_exact(sock, count: int, initial: bytes = b"", timeout: float = 30.0):
    """Generator of chunks adding up to exactly `count` bytes, starting with
    `initial` (bytes already read past the HTTP head). `timeout` bounds the
    wait for each chunk."""
    taken = initial[:count]
    if taken:
        yield taken
    remaining = count - len(taken)
    while remaining > 0:
        chunk = _recv_body_chunk(sock, timeout)[:remaining]
        remaining -= len(chunk)
        yield chunk


def read_body(sock, headers: dict, leftover: bytes, sink, timeout: float = 30.0) -> int:
    """Read a request body (Content-Length or chunked) into sink.write(); returns its size."""
    total = 0
    if headers.get("transfer-encoding", "").lower() == "chunked":
        buf = bytearray(leftover)

        def fill() -> None:
            buf.extend(_recv_body_chunk(sock, timeout))

        while True:
            while b"\r\n" not in buf:
                fill()
            line, _, rest = bytes(buf).partition(b"\r\n")
            buf[:] = rest
            size = int(line.split(b";")[0].strip() or b"0", 16)
            while len(buf) < size + 2:
                fill()
            sink.write(bytes(buf[:size]))
            total += size
            del buf[: size + 2]
            if size == 0:
                return total
    length = int(headers.get("content-length", "0"))
    for chunk in read_exact(sock, length, leftover, timeout):
        sink.write(chunk)
        total += len(chunk)
    return total
