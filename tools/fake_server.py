#!/usr/bin/env python3
"""A stand-in for the capture side of gs-server (standard library only).

It implements the server half of "Real capture: the phone companion" in
docs/ARCHITECTURE.md over TLS with a self-signed certificate:

    GET  /ingest        WebSocket: hello -> welcome (or error + close), one
                        ack per frame, progress about once a second
    POST /api/captures  import of a .gscap, needs "Authorization: Bearer <token>"
    GET  /api/info      small JSON (used by the app to test reachability)
    GET  /api/pair      {"uri": "splatlive://pair?..."} (the real server only
                        answers this on its loopback listener)

It reconstructs nothing: it checks every frame with the reference decoder,
counts, and can save what it received as a .gscap (--save DIR) so that
`gscap.py verify` / `gscap.py extract` show what the phone really sent.

    python3 tools/fake_server.py --save out/captures

prints the pairing URI to enter (or scan) in the app. Run it on the Mac that
shares a Wi-Fi network with the phone.

Choices made where the specification is silent:
  * error codes: bad_hello, bad_protocol, bad_token, bad_message, bad_frame,
    flow_control (the last one only because this is a test tool: the phone
    had more than max_inflight frames unacknowledged);
  * `used` is true for frames with normal tracking received while not paused;
  * `progress.frames` counts the frames received since the hello or the last
    reset, `progress.splats` is a made-up number (valid depth pixels / 4).
"""

from __future__ import annotations

import argparse
import collections
import hmac
import json
import os
import secrets
import shutil
import socket
import ssl
import subprocess
import sys
import tempfile
import threading
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))

import gscap  # noqa: E402
import wsproto  # noqa: E402

HEADER_PAD = 1024


def lan_address() -> str:
    """The address this machine uses on the local network (no packet is sent)."""
    probe = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        probe.connect(("192.0.2.1", 9))
        return probe.getsockname()[0]
    except OSError:
        return "127.0.0.1"
    finally:
        probe.close()


def make_self_signed_cert(directory: str, addresses) -> tuple:
    """Create cert.pem / key.pem with the openssl command line tool.

    The certificate is reused when it already names the same addresses, so
    that its fingerprint (and therefore the pairing) survives a restart.
    """
    openssl = shutil.which("openssl")
    if openssl is None:
        raise RuntimeError("the openssl command line tool is needed to create a certificate")
    os.makedirs(directory, exist_ok=True)
    cert = os.path.join(directory, "cert.pem")
    key = os.path.join(directory, "key.pem")
    names = os.path.join(directory, "names.txt")
    wanted = ",".join(sorted(set(addresses)))
    if os.path.exists(cert) and os.path.exists(key) and os.path.exists(names):
        with open(names, "r", encoding="ascii") as f:
            if f.read().strip() == wanted:
                return cert, key
    alt = ["DNS:localhost"] + ["IP:%s" % a for a in sorted(set(addresses))]
    config = os.path.join(directory, "openssl.cnf")
    with open(config, "w", encoding="ascii") as f:
        f.write(
            "[req]\ndistinguished_name=dn\nx509_extensions=v3\nprompt=no\n"
            "[dn]\nCN=splat-live-fake\n"
            "[v3]\nsubjectAltName=%s\nbasicConstraints=critical,CA:FALSE\n"
            "keyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n" % ",".join(alt)
        )
    result = subprocess.run(
        [openssl, "req", "-x509", "-newkey", "rsa:2048", "-sha256", "-nodes", "-days", "365",
         "-keyout", key, "-out", cert, "-config", config],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE,
    )
    if result.returncode != 0:
        raise RuntimeError("openssl failed: %s" % result.stderr.decode("utf-8", "replace").strip())
    with open(names, "w", encoding="ascii") as f:
        f.write(wanted + "\n")
    return cert, key


def fingerprint_of_pem(cert_path: str) -> str:
    with open(cert_path, "r", encoding="ascii") as f:
        return gscap.certificate_fingerprint(ssl.PEM_cert_to_DER_cert(f.read()))


class IngestSession:
    """What one /ingest connection did (inspected by the tests)."""

    def __init__(self) -> None:
        self.hello = None
        self.messages = []          # client JSON messages after the hello
        self.frame_indices = []     # index of every frame received
        self.frames_used = 0
        self.max_unacked = 0
        self.flow_violation = False
        self.region = None
        self.floor = None
        self.paused = False
        self.resets = 0
        self.finished = False
        self.error = None           # error code sent to the client
        self.close_code = None
        self.saved_path = None
        self.done = threading.Event()


class FakeServer:
    def __init__(self, host: str = "127.0.0.1", port: int = 0, token=None, certfile=None, keyfile=None,
                 max_inflight: int = 4, ack_delay: float = 0.0, progress_interval: float = 1.0,
                 save_dir=None, strict_flow: bool = True, hello_timeout: float = 10.0, log=None) -> None:
        if certfile is None or keyfile is None:
            raise ValueError("certfile and keyfile are required (see make_self_signed_cert)")
        self.token = token or secrets.token_hex(16)
        self.max_inflight = max_inflight
        self.ack_delay = ack_delay
        self.progress_interval = progress_interval
        self.save_dir = save_dir
        self.strict_flow = strict_flow
        self.hello_timeout = hello_timeout
        self.log = log or (lambda text: None)
        self.fingerprint = fingerprint_of_pem(certfile)
        self.sessions = []
        self.uploads = []
        self._lock = threading.Lock()
        self._context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        self._context.minimum_version = ssl.TLSVersion.TLSv1_2
        self._context.load_cert_chain(certfile, keyfile)
        self._listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self._listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self._listener.bind((host, port))
        self._listener.listen(16)
        self.host = host
        self.port = self._listener.getsockname()[1]
        self._stop = threading.Event()
        self._thread = None

    # -- life cycle ---------------------------------------------------------

    def pairing_uri(self, host=None) -> str:
        return gscap.Pairing(host or self.host, self.port, self.fingerprint, self.token).uri

    def start(self) -> "FakeServer":
        self._thread = threading.Thread(target=self.serve_forever, name="fake-server", daemon=True)
        self._thread.start()
        return self

    def serve_forever(self) -> None:
        self._listener.settimeout(0.2)
        while not self._stop.is_set():
            try:
                conn, addr = self._listener.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            threading.Thread(target=self._handle, args=(conn, addr), daemon=True).start()

    def stop(self) -> None:
        self._stop.set()
        try:
            self._listener.close()
        except OSError:
            pass
        if self._thread is not None:
            self._thread.join(timeout=2.0)

    def __enter__(self) -> "FakeServer":
        return self.start()

    def __exit__(self, *exc) -> None:
        self.stop()

    # -- connections --------------------------------------------------------

    def _handle(self, conn, addr) -> None:
        conn.settimeout(15.0)
        try:
            tls = self._context.wrap_socket(conn, server_side=True)
        except (ssl.SSLError, OSError) as exc:
            # A phone that pins another certificate gives up here.
            self.log("%s: TLS handshake failed (%s)" % (addr[0], exc))
            conn.close()
            return
        tls.settimeout(None)
        try:
            request, headers, leftover = wsproto.read_http_head(tls, 15.0)
            parts = request.split(" ")
            if len(parts) < 2:
                raise wsproto.WsError("bad request line")
            method, path = parts[0], parts[1].split("?")[0]
            if method == "GET" and path == "/ingest":
                if not wsproto.is_websocket_upgrade(headers):
                    tls.sendall(wsproto.http_response(400, "Bad Request", b'{"error":"WebSocket upgrade expected"}'))
                else:
                    self._ingest(wsproto.server_handshake(tls, headers, leftover), addr[0])
            elif method == "GET" and path == "/api/info":
                body = {"name": "splat-live-fake", "version": "0.1.0", "protocol": gscap.PROTOCOL,
                        "ws_path": "/ws", "lan": {"enabled": True, "fingerprint": self.fingerprint},
                        "clients": 0}
                tls.sendall(wsproto.http_response(200, "OK", json.dumps(body).encode("utf-8")))
            elif method == "GET" and path == "/api/pair":
                host = headers.get("host", self.host).rsplit(":", 1)[0]
                body = {"uri": self.pairing_uri(host)}
                tls.sendall(wsproto.http_response(200, "OK", json.dumps(body).encode("utf-8")))
            elif method == "POST" and path == "/api/captures":
                self._import_capture(tls, headers, leftover, addr[0])
            else:
                tls.sendall(wsproto.http_response(404, "Not Found", b'{"error":"not found"}'))
        except (wsproto.WsError, OSError, ValueError) as exc:
            self.log("%s: %s" % (addr[0], exc))
        finally:
            try:
                tls.close()
            except OSError:
                pass

    # -- POST /api/captures ---------------------------------------------------

    def _import_capture(self, tls, headers, leftover, peer) -> None:
        if headers.get("expect", "").lower() == "100-continue":
            tls.sendall(b"HTTP/1.1 100 Continue\r\n\r\n")
        record = {"peer": peer, "bytes": 0, "frames": 0, "header": None, "error": None, "saved_path": None}
        with tempfile.TemporaryFile() as body:
            record["bytes"] = wsproto.read_body(tls, headers, leftover, body)
            authorization = headers.get("authorization", "")
            if not hmac.compare_digest(authorization.encode("utf-8"), ("Bearer " + self.token).encode("utf-8")):
                record["error"] = "unauthorized"
                answer = (401, "Unauthorized", {"ok": False, "error": "missing or wrong pairing token"})
            else:
                body.seek(0)
                try:
                    reader = gscap.GscapReader(body)
                    count, problems, _ = gscap.check_frames(reader)
                    record["header"] = reader.header
                    record["frames"] = count
                    if problems:
                        raise gscap.FormatError("bad_frame", problems[0])
                    answer = (200, "OK", {"ok": True, "frames": count, "truncated": reader.truncated,
                                          "name": reader.header.get("name", "")})
                    if self.save_dir:
                        os.makedirs(self.save_dir, exist_ok=True)
                        path = os.path.join(self.save_dir, "upload-%d.gscap" % (len(self.uploads) + 1))
                        body.seek(0)
                        with open(path, "wb") as out:
                            shutil.copyfileobj(body, out)
                        record["saved_path"] = path
                except gscap.FormatError as exc:
                    record["error"] = exc.code
                    answer = (400, "Bad Request", {"ok": False, "error": str(exc)})
        with self._lock:
            self.uploads.append(record)
        self.log("%s: POST /api/captures, %d bytes, %d frames -> %d %s" % (
            peer, record["bytes"], record["frames"], answer[0], record["error"] or "ok"))
        tls.sendall(wsproto.http_response(answer[0], answer[1], json.dumps(answer[2]).encode("utf-8")))

    # -- GET /ingest ------------------------------------------------------------

    def _ingest(self, ws, peer) -> None:
        session = IngestSession()
        with self._lock:
            self.sessions.append(session)
        writer = None
        file_header = None
        try:
            hello = self._read_hello(ws, session)
            if hello is None:
                return
            session.hello = hello
            self.log("%s: hello mode=%s name=%r device=%s app=%s" % (
                peer, hello["mode"], hello.get("name", ""), hello.get("device", "?"), hello.get("app", "?")))
            ws.send_text(gscap.welcome_message(self.max_inflight))

            if self.save_dir:
                os.makedirs(self.save_dir, exist_ok=True)
                file_header = {k: v for k, v in hello.items() if k not in ("type", "token")}
                file_header["created"] = gscap.now_rfc3339()
                with self._lock:
                    number = len(self.sessions)
                session.saved_path = os.path.join(self.save_dir, "ingest-%d.gscap" % number)
                writer = gscap.GscapWriter(session.saved_path, file_header, pad_to=HEADER_PAD)

            pending = collections.deque()  # (time the ack is due, frame index, used)
            frames = 0
            splats = 0
            next_progress = time.monotonic() + self.progress_interval
            while True:
                now = time.monotonic()
                while pending and (pending[0][0] <= now or session.finished):
                    _, index, used = pending.popleft()
                    ws.send_text(gscap.ack_message(index, used))
                if now >= next_progress:
                    ws.send_text(gscap.progress_message(frames, splats))
                    next_progress = now + self.progress_interval
                wait = next_progress - now
                if pending:
                    wait = min(wait, pending[0][0] - now)
                message = ws.recv(max(0.0, wait))
                if message is None:
                    continue
                opcode, payload = message
                if opcode == wsproto.OP_BINARY:
                    try:
                        frame = gscap.decode_frame(payload)
                    except gscap.FormatError as exc:
                        self._fail(ws, session, "bad_frame", str(exc), 1007)
                        return
                    session.frame_indices.append(frame.index)
                    unacked = len(pending) + 1
                    session.max_unacked = max(session.max_unacked, unacked)
                    if unacked > self.max_inflight:
                        session.flow_violation = True
                        if self.strict_flow:
                            self._fail(ws, session, "flow_control",
                                       "%d frames unacknowledged, max_inflight is %d" % (unacked, self.max_inflight), 1008)
                            return
                    used = frame.tracking == gscap.TRACKING_NORMAL and not session.paused
                    frames += 1
                    if used:
                        session.frames_used += 1
                        splats += sum(1 for v in frame.depth_mm() if v) // 4
                    pending.append((time.monotonic() + self.ack_delay, frame.index, used))
                    if writer is not None:
                        writer.append(payload)
                    if frames % 20 == 1:
                        self.log("%s: frame #%d, %d received, colour %dx%d, depth %dx%d, %d bytes" % (
                            peer, frame.index, frames, frame.color_width, frame.color_height,
                            frame.depth_width, frame.depth_height, len(payload)))
                else:
                    try:
                        obj = gscap.loads_strict(payload.decode("utf-8"))
                        kind = obj["type"]
                    except (ValueError, KeyError, TypeError):
                        self._fail(ws, session, "bad_message", "not a JSON message with a type", 1007)
                        return
                    session.messages.append(obj)
                    self.log("%s: %s" % (peer, json.dumps(obj)))
                    if kind == "region":
                        session.region = obj
                    elif kind == "floor":
                        session.floor = obj
                    elif kind == "pause":
                        session.paused = True
                    elif kind == "resume":
                        session.paused = False
                    elif kind == "reset":
                        session.resets += 1
                        frames = 0
                        splats = 0
                        session.frames_used = 0
                    elif kind == "finish":
                        session.finished = True
                        while pending:
                            _, index, used = pending.popleft()
                            ws.send_text(gscap.ack_message(index, used))
                        ws.send_text(gscap.progress_message(frames, splats))
                    else:
                        self._fail(ws, session, "bad_message", "unknown message type %r" % (kind,), 1008)
                        return
        except wsproto.WsClosed as exc:
            session.close_code = exc.code
        except wsproto.WsError as exc:
            session.error = session.error or "protocol"
            self.log("%s: %s" % (peer, exc))
        finally:
            if writer is not None:
                if session.region is not None:
                    file_header["region"] = {"center": session.region.get("center"),
                                             "radius": session.region.get("radius")}
                if session.floor is not None:
                    file_header["floor"] = {"y": session.floor.get("y")}
                try:
                    writer.rewrite_header(file_header)
                except gscap.FormatError:
                    pass
                writer.close()
            ws.shutdown()
            self.log("%s: ingest closed, %d frames (%d used)%s%s" % (
                peer, len(session.frame_indices), session.frames_used,
                ", finished" if session.finished else "",
                ", saved to %s" % session.saved_path if session.saved_path else ""))
            session.done.set()

    def _read_hello(self, ws, session):
        message = ws.recv(self.hello_timeout)
        if message is None:
            self._fail(ws, session, "bad_hello", "no hello within %.0f s" % self.hello_timeout, 1008)
            return None
        opcode, payload = message
        try:
            hello = gscap.loads_strict(payload.decode("utf-8")) if opcode == wsproto.OP_TEXT else None
        except ValueError:
            hello = None
        if not isinstance(hello, dict) or hello.get("type") != "hello":
            self._fail(ws, session, "bad_hello", "the first message must be a hello", 1008)
            return None
        if hello.get("protocol") != gscap.PROTOCOL:
            self._fail(ws, session, "bad_protocol", "this server speaks protocol %d" % gscap.PROTOCOL, 1008)
            return None
        token = hello.get("token")
        if not isinstance(token, str) or not hmac.compare_digest(token.encode("utf-8"), self.token.encode("utf-8")):
            self._fail(ws, session, "bad_token", "wrong pairing token, scan the QR code again", 1008)
            return None
        if hello.get("mode") not in gscap.MODES:
            self._fail(ws, session, "bad_hello", "unknown mode %r" % (hello.get("mode"),), 1008)
            return None
        return hello

    def _fail(self, ws, session, code: str, message: str, close_code: int) -> None:
        session.error = code
        self.log("error sent: %s (%s)" % (code, message))
        try:
            ws.send_text(gscap.error_message(code, message))
            ws.close(close_code, code)
            # Give the client a moment to read the error and answer the close.
            deadline = time.monotonic() + 1.0
            while time.monotonic() < deadline:
                if ws.recv(deadline - time.monotonic()) is None:
                    break
        except wsproto.WsError:
            pass


def _print_qr(uri: str) -> None:
    qrencode = shutil.which("qrencode")
    if qrencode is None:
        print("(install qrencode, `brew install qrencode`, to get a QR code here;")
        print(" otherwise paste the URI in the app: Réglages > Saisie manuelle)")
        return
    subprocess.run([qrencode, "-t", "ANSIUTF8", "-m", "2", uri])


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description="Fake Splat Live capture server (TLS + WebSocket /ingest)")
    parser.add_argument("--bind", default="0.0.0.0", help="address to listen on (default: all)")
    parser.add_argument("--port", type=int, default=7879)
    parser.add_argument("--advertise", default=None, help="address written in the pairing URI (default: LAN address)")
    parser.add_argument("--token", default=None, help="32 hex digits (default: random at every start)")
    parser.add_argument("--max-inflight", type=int, default=4)
    parser.add_argument("--ack-delay", type=float, default=0.0, help="seconds before each ack (exercises flow control)")
    parser.add_argument("--save", default=None, metavar="DIR", help="save received frames and uploads as .gscap")
    parser.add_argument("--cert-dir", default=os.path.join(os.path.dirname(os.path.abspath(__file__)), ".fake_server"),
                        help="where the self-signed certificate is kept")
    parser.add_argument("--lenient-flow", action="store_true", help="do not close on a flow control violation")
    args = parser.parse_args(argv)

    address = args.advertise or lan_address()
    try:
        cert, key = make_self_signed_cert(args.cert_dir, [address, "127.0.0.1"])
    except RuntimeError as exc:
        print("error: %s" % exc, file=sys.stderr)
        return 1
    server = FakeServer(host=args.bind, port=args.port, token=args.token, certfile=cert, keyfile=key,
                        max_inflight=args.max_inflight, ack_delay=args.ack_delay, save_dir=args.save,
                        strict_flow=not args.lenient_flow,
                        log=lambda text: print(time.strftime("%H:%M:%S ") + text, flush=True))
    uri = server.pairing_uri(address)
    print("Fake Splat Live server on https://%s:%d (certificate SHA-256 %s)" % (address, server.port, server.fingerprint))
    print("Pairing URI:\n\n  %s\n" % uri)
    _print_qr(uri)
    print("Waiting for the phone. Ctrl-C to stop.", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.stop()
    return 0


if __name__ == "__main__":
    sys.exit(main())
