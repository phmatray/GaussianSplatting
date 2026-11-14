#!/usr/bin/env python3
"""A simulated phone: replays a .gscap to a Splat Live server (standard library only).

    python3 tools/fake_phone.py stream CAPTURE.gscap "splatlive://pair?host=..&port=..&fp=..&token=.."
    python3 tools/fake_phone.py upload CAPTURE.gscap "splatlive://pair?..."

`stream` follows the /ingest protocol of docs/ARCHITECTURE.md the way the
iOS app does: TLS with the certificate pinned to the `fp` of the pairing URI
(checked before anything secret is sent), hello with the token, wait for
welcome, region and floor messages if the file has them, then the frames with
never more than max_inflight unacknowledged, finish, close.

By default every frame of the file is sent (the sender waits for acks).
With --realtime the frames are paced by their timestamps and, like the app,
a frame that cannot be sent in time is skipped instead of queued.

`upload` posts the file to /api/captures with the pairing token.
"""

from __future__ import annotations

import argparse
import json
import os
import socket
import ssl
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import gscap  # noqa: E402
import wsproto  # noqa: E402


class PhoneError(Exception):
    """The session failed; `code` is bad_fingerprint, connect, protocol or a server error code."""

    def __init__(self, code: str, message: str) -> None:
        super().__init__("%s: %s" % (code, message))
        self.code = code


def connect_pinned(pairing: gscap.Pairing, timeout: float = 10.0):
    """Open a TLS connection and accept exactly the certificate named by the pairing.

    The certificate is self-signed, so the usual chain and host name checks
    are replaced by the comparison of its SHA-256 with `fp`.
    """
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    context.check_hostname = False
    context.verify_mode = ssl.CERT_NONE
    try:
        raw = socket.create_connection((pairing.host, pairing.port), timeout=timeout)
    except OSError as exc:
        raise PhoneError("connect", "%s:%d: %s" % (pairing.host, pairing.port, exc))
    try:
        tls = context.wrap_socket(raw, server_hostname=None)
    except (ssl.SSLError, OSError) as exc:
        raw.close()
        raise PhoneError("connect", "TLS handshake failed: %s" % exc)
    der = tls.getpeercert(binary_form=True)
    seen = gscap.certificate_fingerprint(der) if der else ""
    if seen != pairing.fingerprint:
        tls.close()
        raise PhoneError("bad_fingerprint", "the server certificate is %s, the pairing expects %s" % (seen, pairing.fingerprint))
    tls.settimeout(None)
    return tls


class _Stream:
    """State of one /ingest session on the phone side."""

    def __init__(self, ws, log) -> None:
        self.ws = ws
        self.log = log
        self.max_inflight = 1
        self.unacked = set()
        self.stats = {
            "frames_in_file": 0, "frames_sent": 0, "frames_skipped": 0, "acked": 0, "used": 0,
            "max_unacked": 0, "max_inflight": None, "progress": None, "progress_count": 0,
        }

    def handle(self, message) -> None:
        opcode, payload = message
        if opcode != wsproto.OP_TEXT:
            raise PhoneError("protocol", "unexpected binary frame from the server")
        try:
            obj = gscap.loads_strict(payload.decode("utf-8"))
            kind = obj["type"]
        except (ValueError, KeyError, TypeError):
            raise PhoneError("protocol", "the server sent something that is not a JSON message")
        if not isinstance(obj, dict):
            raise PhoneError("protocol", "the server sent something that is not a JSON message")
        if kind == "ack":
            index = obj.get("frame")
            if isinstance(index, bool) or not isinstance(index, int) or index not in self.unacked:
                raise PhoneError("protocol", "ack for frame %r, which is not outstanding" % (index,))
            self.unacked.discard(index)
            self.stats["acked"] += 1
            if obj.get("used"):
                self.stats["used"] += 1
        elif kind == "progress":
            self.stats["progress"] = {"frames": obj.get("frames"), "splats": obj.get("splats")}
            self.stats["progress_count"] += 1
        elif kind == "error":
            raise PhoneError(str(obj.get("code", "error")), str(obj.get("message", "")))
        else:
            self.log("ignored server message: %s" % kind)

    def pump(self, timeout: float) -> bool:
        """Handle one server message if one arrives within `timeout`."""
        message = self.ws.recv(timeout)
        if message is None:
            return False
        self.handle(message)
        return True

    def drain(self) -> None:
        while self.pump(0.0):
            pass


def stream_capture(path: str, uri: str, mode=None, name=None, realtime: bool = False, speed: float = 1.0,
                   limit: int = 0, finish: bool = True, timeout: float = 20.0, log=None) -> dict:
    """Replay a .gscap to the server named by the pairing URI; returns statistics."""
    log = log or (lambda text: None)
    pairing = gscap.parse_pairing_uri(uri)
    tls = connect_pinned(pairing, timeout)
    ws = None
    try:
        try:
            ws = wsproto.client_handshake(tls, pairing.authority, "/ingest", timeout)
        except wsproto.WsError as exc:
            raise PhoneError("connect", str(exc))
        with gscap.GscapReader(path) as reader:
            header = reader.header
            stream = _Stream(ws, log)
            ws.send_text(gscap.hello_message(
                pairing.token,
                mode or header.get("mode", "room"),
                name if name is not None else str(header.get("name", "")),
                device=str(header.get("device", "fake-phone")),
                app=str(header.get("app", "0.1.0")),
            ))
            # Nothing but the hello goes out before the welcome.
            first = ws.recv(timeout)
            if first is None:
                raise PhoneError("protocol", "no welcome within %.0f s" % timeout)
            try:
                welcome = gscap.loads_strict(first[1].decode("utf-8")) if first[0] == wsproto.OP_TEXT else {}
            except ValueError:
                welcome = {}
            if not isinstance(welcome, dict):
                welcome = {}
            if welcome.get("type") == "error":
                raise PhoneError(str(welcome.get("code", "error")), str(welcome.get("message", "")))
            if welcome.get("type") != "welcome" or welcome.get("protocol") != gscap.PROTOCOL:
                raise PhoneError("protocol", "expected a welcome, got %r" % (welcome,))
            inflight = welcome.get("max_inflight", 1)
            if isinstance(inflight, bool) or not isinstance(inflight, int):
                inflight = 1  # as the app: a welcome without a usable max_inflight means 1
            stream.max_inflight = max(1, inflight)
            stream.stats["max_inflight"] = stream.max_inflight
            log("welcome, max_inflight=%d" % stream.max_inflight)

            region = gscap.header_region(header)
            if region is not None:
                ws.send_text(gscap.region_message(region[0], region[1]))
            floor = gscap.header_floor_y(header)
            if floor is not None:
                ws.send_text(gscap.floor_message(floor))

            start_wall = time.monotonic()
            start_time = None
            for data in reader:
                stream.stats["frames_in_file"] += 1
                if limit and stream.stats["frames_sent"] >= limit:
                    continue
                parsed = gscap.parse_header(data)
                if realtime:
                    if start_time is None:
                        start_time = parsed["timestamp"]
                    due = start_wall + (parsed["timestamp"] - start_time) / max(speed, 1e-6)
                    while True:
                        remaining = due - time.monotonic()
                        if remaining <= 0:
                            break
                        stream.pump(remaining)
                    stream.drain()
                    if len(stream.unacked) >= stream.max_inflight:
                        stream.stats["frames_skipped"] += 1
                        continue
                else:
                    stream.drain()
                    deadline = time.monotonic() + timeout
                    while len(stream.unacked) >= stream.max_inflight:
                        if not stream.pump(max(0.0, deadline - time.monotonic())) and time.monotonic() >= deadline:
                            raise PhoneError("protocol", "no ack within %.0f s" % timeout)
                stream.unacked.add(parsed["index"])
                stream.stats["max_unacked"] = max(stream.stats["max_unacked"], len(stream.unacked))
                ws.send_binary(data)
                stream.stats["frames_sent"] += 1

            deadline = time.monotonic() + timeout
            while stream.unacked:
                if not stream.pump(max(0.0, deadline - time.monotonic())) and time.monotonic() >= deadline:
                    raise PhoneError("protocol", "%d frames were never acknowledged" % len(stream.unacked))
            if finish:
                ws.send_text(gscap.simple_message("finish"))
                # The server answers a finish with a last progress report.
                seen = stream.stats["progress_count"]
                deadline = time.monotonic() + 2.0
                while stream.stats["progress_count"] == seen and time.monotonic() < deadline:
                    stream.pump(deadline - time.monotonic())
            ws.close(1000, "done")
            try:
                deadline = time.monotonic() + 2.0
                while time.monotonic() < deadline:
                    if not stream.pump(deadline - time.monotonic()):
                        break
            except wsproto.WsClosed:
                pass
            return stream.stats
    except wsproto.WsClosed as exc:
        raise PhoneError("protocol", "the server closed the connection: %s" % exc)
    except wsproto.WsError as exc:
        raise PhoneError("protocol", str(exc))
    finally:
        if ws is not None:
            ws.shutdown()
        else:
            tls.close()


def upload_capture(path: str, uri: str, timeout: float = 60.0, token=None) -> tuple:
    """POST a .gscap to /api/captures; returns (HTTP status, decoded JSON or text)."""
    pairing = gscap.parse_pairing_uri(uri)
    tls = connect_pinned(pairing, timeout)
    try:
        size = os.path.getsize(path)
        head = (
            "POST /api/captures HTTP/1.1\r\n"
            "Host: %s\r\n"
            "Authorization: Bearer %s\r\n"
            "Content-Type: application/octet-stream\r\n"
            "Content-Length: %d\r\n"
            "Connection: close\r\n\r\n" % (pairing.authority, pairing.token if token is None else token, size)
        )
        tls.sendall(head.encode("ascii"))
        with open(path, "rb") as f:
            while True:
                chunk = f.read(1 << 16)
                if not chunk:
                    break
                tls.sendall(chunk)
        status_line, headers, leftover = wsproto.read_http_head(tls, timeout)
        length = int(headers.get("content-length", "0"))
        body = b"".join(wsproto.read_exact(tls, length, leftover, timeout))
        status = int(status_line.split(" ")[1])
        try:
            return status, json.loads(body.decode("utf-8"))
        except ValueError:
            return status, body.decode("utf-8", "replace")
    except (wsproto.WsError, OSError, ValueError, IndexError) as exc:
        raise PhoneError("protocol", str(exc))
    finally:
        tls.close()


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description="Simulated Splat Live phone")
    sub = parser.add_subparsers(dest="command")
    p = sub.add_parser("stream", help="replay a .gscap over wss://host:port/ingest")
    p.add_argument("file")
    p.add_argument("uri", help="pairing URI, splatlive://pair?host=..&port=..&fp=..&token=..")
    p.add_argument("--mode", choices=gscap.MODES, default=None, help="override the mode of the file")
    p.add_argument("--name", default=None, help="override the name of the file")
    p.add_argument("--realtime", action="store_true", help="pace by timestamps and skip frames like the app")
    p.add_argument("--speed", type=float, default=1.0, help="time factor for --realtime")
    p.add_argument("--limit", type=int, default=0, help="send at most N frames")
    p.add_argument("--no-finish", action="store_true", help="close without sending finish")
    p = sub.add_parser("upload", help="POST a .gscap to https://host:port/api/captures")
    p.add_argument("file")
    p.add_argument("uri")
    args = parser.parse_args(argv)
    if not args.command:
        parser.print_help()
        return 2
    try:
        if args.command == "stream":
            stats = stream_capture(args.file, args.uri, mode=args.mode, name=args.name, realtime=args.realtime,
                                   speed=args.speed, limit=args.limit, finish=not args.no_finish,
                                   log=lambda text: print(text, flush=True))
            print(json.dumps(stats, indent=1))
        else:
            status, body = upload_capture(args.file, args.uri)
            print("HTTP %d: %s" % (status, json.dumps(body) if not isinstance(body, str) else body))
            return 0 if 200 <= status < 300 else 1
    except (PhoneError, gscap.FormatError, OSError) as exc:
        print("error: %s" % exc, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
