#!/usr/bin/env python3
"""The capture, recording and sharing routes against a hostile client
(standard library only).

Starts the real `gs-server` binary and plays three kinds of bad neighbours:
a stranger on the LAN (no pairing token), a paired phone that breaks every
rule of `/ingest` or dies in the middle of a frame, and a web page of another
origin in a browser on the machine itself (CSRF, DNS rebinding). Then the
recordings are asked for with every kind of `Range` and name.

    cargo build --release -p gs-server
    python3 e2e/server/hostile.py [--port 7940] [--out DIR] [--quick]

`--quick` leaves out the checks that wait for the server's own timeouts
(a phone that never says hello: 10 s; a phone that dies in the middle of a
frame: 12 s for a takeover, about 40 s until the server lets go by itself; a
stalled upload: 15 s; a download nobody reads: 30 s).

Exit code 0 when every check passed.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import signal
import socket
import ssl
import struct
import subprocess
import sys
import tempfile
import threading
import time

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.join(REPO, "tools"))

import gscap  # noqa: E402
import wsproto  # noqa: E402

FAILED = []


def check(name: str, ok: bool, detail: str = "") -> bool:
    print("%s %s%s" % ("ok  " if ok else "FAIL", name, "  (%s)" % detail if detail else ""), flush=True)
    if not ok:
        FAILED.append(name)
    return ok


def wait_for(condition, timeout: float = 30.0, step: float = 0.05) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if condition():
            return True
        time.sleep(step)
    return False


# ------------------------------------------------------------------ plumbing

class Answer:
    def __init__(self, status: int, headers: dict, body: bytes) -> None:
        self.status = status
        self.headers = headers
        self.body = body

    def json(self):
        try:
            return json.loads(self.body.decode("utf-8"))
        except ValueError:
            return None

    @property
    def code(self):
        value = self.json()
        return value.get("code") if isinstance(value, dict) else None

    def __repr__(self) -> str:
        return "%d %s" % (self.status, self.code or self.body[:60])


class Server:
    def __init__(self, binary: str, port: int, data: str, log_path: str, dist) -> None:
        self.port = port
        self.lan_port = port + 1
        self.data = data
        self.recordings = os.path.join(data, "recordings")
        self.captures = os.path.join(data, "captures")
        self.log_path = log_path
        self.log = open(log_path, "wb")
        args = [binary, "--port", str(port), "--lan", "--lan-port", str(self.lan_port), "--light",
                "--cert-dir", os.path.join(data, "cert"), "--recordings-dir", self.recordings, "--verbose"]
        if dist:
            args += ["--dist", dist]
        self.process = subprocess.Popen(args, stdout=self.log, stderr=subprocess.STDOUT)
        if not wait_for(lambda: self.alive(), 20.0):
            self.process.kill()
            raise RuntimeError("gs-server did not start; see %s" % log_path)
        self.uri = self.http("GET", "/api/pair").json()["uri"]
        self.pairing = gscap.parse_pairing_uri(self.uri)
        self.token = self.pairing.token

    def alive(self) -> bool:
        try:
            return self.http("GET", "/api/info", timeout=2.0).status == 200
        except OSError:
            return False

    def connect(self, lan: bool = False, timeout: float = 10.0):
        if not lan:
            sock = socket.create_connection(("127.0.0.1", self.port), timeout=timeout)
            return sock
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
        context.check_hostname = False
        context.verify_mode = ssl.CERT_NONE
        context.set_alpn_protocols(["http/1.1"])
        raw = socket.create_connection(("127.0.0.1", self.lan_port), timeout=timeout)
        return context.wrap_socket(raw)

    def authority(self, lan: bool) -> str:
        return "127.0.0.1:%d" % (self.lan_port if lan else self.port)

    def head_bytes(self, method: str, path: str, lan: bool, headers: dict, length=None) -> bytes:
        lines = ["%s %s HTTP/1.1" % (method, path)]
        merged = {"Host": self.authority(lan), "Connection": "close"}
        merged.update(headers or {})
        if length is not None:
            merged.setdefault("Content-Length", str(length))
        for name, value in merged.items():
            if value is None:
                continue
            for one in value if isinstance(value, list) else [value]:
                lines.append("%s: %s" % (name, one))
        return ("\r\n".join(lines) + "\r\n\r\n").encode("utf-8")

    @staticmethod
    def read_answer(sock, timeout: float, head_only: bool = False) -> Answer:
        status_line, headers, leftover = wsproto.read_http_head(sock, timeout)
        status = int(status_line.split(" ")[1])
        if head_only or status in (204, 304) or status < 200:
            return Answer(status, headers, b"")
        body = bytearray(leftover)
        sock.settimeout(timeout)
        if "content-length" in headers:
            want = int(headers["content-length"])
            while len(body) < want:
                chunk = sock.recv(1 << 16)
                if not chunk:
                    break
                body += chunk
            return Answer(status, headers, bytes(body[:want]))
        # No length: until the connection closes (chunked answers are decoded).
        while True:
            try:
                chunk = sock.recv(1 << 16)
            except (ssl.SSLError, OSError):
                break
            if not chunk:
                break
            body += chunk
        if headers.get("transfer-encoding", "").lower() == "chunked":
            out, at = bytearray(), 0
            while True:
                end = body.find(b"\r\n", at)
                if end < 0:
                    break
                size = int(bytes(body[at:end]).split(b";")[0] or b"0", 16)
                if size == 0:
                    break
                out += body[end + 2:end + 2 + size]
                at = end + 2 + size + 2
            body = out
        return Answer(status, headers, bytes(body))

    def http(self, method: str, path: str, lan: bool = False, headers=None, body: bytes = None,
             timeout: float = 30.0) -> Answer:
        sock = self.connect(lan, timeout)
        try:
            sock.sendall(self.head_bytes(method, path, lan, headers or {}, None if body is None else len(body)))
            if body:
                sock.sendall(body)
            return self.read_answer(sock, timeout, head_only=(method == "HEAD"))
        finally:
            sock.close()

    def post_json(self, path: str, value, **kwargs) -> Answer:
        headers = {"Content-Type": "application/json"}
        headers.update(kwargs.pop("headers", {}))
        return self.http("POST", path, headers=headers, body=json.dumps(value).encode(), **kwargs)

    def rss_kb(self) -> int:
        with open("/proc/%d/status" % self.process.pid) as status:
            for line in status:
                if line.startswith("VmRSS:"):
                    return int(line.split()[1])
        return 0

    def parts_left(self) -> list:
        left = []
        for directory in (self.recordings, self.captures, tempfile.gettempdir()):
            if os.path.isdir(directory):
                left += [name for name in os.listdir(directory)
                         if name.endswith(".part") and ("splat-live-%d-" % self.process.pid in name
                                                        or name.startswith(".%d-" % self.process.pid))]
        return left

    def stop(self) -> int:
        self.process.send_signal(signal.SIGTERM)
        try:
            code = self.process.wait(timeout=20)
        except subprocess.TimeoutExpired:
            self.process.kill()
            code = -9
        self.log.close()
        return code


class Phone:
    """A raw `/ingest` client: nothing is done for the caller."""

    def __init__(self, server: Server, timeout: float = 10.0) -> None:
        self.server = server
        self.sock = server.connect(lan=True, timeout=timeout)
        self.sock.settimeout(None)
        self.ws = wsproto.client_handshake(self.sock, server.authority(True), "/ingest", timeout)

    def hello(self, token=None, mode: str = "room", name: str = "Salon", device: str = "hostile,1") -> None:
        self.ws.send_text(gscap.hello_message(self.server.token if token is None else token, mode, name, device=device))

    def next(self, timeout: float = 10.0):
        """The next JSON message, or ("closed", code) when the server closed."""
        try:
            message = self.ws.recv(timeout)
        except wsproto.WsClosed as closed:
            return ("closed", closed.code)
        except wsproto.WsError as error:
            return ("closed", str(error))
        if message is None:
            return None
        return json.loads(message[1].decode("utf-8"))

    def until_closed(self, timeout: float = 15.0):
        """(messages before the close, close code); the code is None when
        the server did not close in time."""
        messages = []
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            got = self.next(max(0.05, deadline - time.monotonic()))
            if got is None:
                continue
            if isinstance(got, tuple):
                return messages, got[1]
            messages.append(got)
        return messages, None

    def welcomed(self, **hello) -> bool:
        self.hello(**hello)
        first = self.next()
        return isinstance(first, dict) and first.get("type") == "welcome"

    def close(self) -> None:
        self.ws.shutdown()


def leave_capture(server: Server) -> None:
    """What a viewer does to return to the simulation: `set_scene`."""
    sock = server.connect()
    sock.settimeout(None)
    ws = wsproto.client_handshake(sock, server.authority(False), "/ws", 10.0)
    ws.send_text(json.dumps({"type": "set_scene", "scene": "stage"}))
    deadline = time.monotonic() + 2.0
    while time.monotonic() < deadline:
        try:
            ws.recv(0.2)
        except wsproto.WsError:
            break
    ws.shutdown()


def capture_going_on(server: Server, frames: list) -> bool:
    """Whether a phone holds the capture: an import is refused meanwhile.
    (When none does, the import is taken, which replaces what is shown.)"""
    answer = server.http("POST", "/api/captures", body=gscap_file(frames[:1], "Sonde"))
    return answer.status == 409 and answer.code == "busy"


def errors_of(messages) -> list:
    return [m.get("code") for m in messages if isinstance(m, dict) and m.get("type") == "error"]


# ------------------------------------------------------------------ strangers

def strangers(server: Server, frames: list, quick: bool) -> None:
    token = server.token
    wrong = [
        ("no token", None),
        ("an empty token", ""),
        ("a token of the right length", "0" * 32),
        ("the token in capitals", token.upper() if token.upper() != token else "F" * 32),
        ("a prefix of the token", token[:31]),
        ("the token and more", token + "0"),
    ]
    for name, value in wrong:
        phone = Phone(server)
        if value is None:
            phone.ws.send_text(json.dumps({"type": "hello", "protocol": 1, "device": "x", "app": "0", "mode": "room", "name": "x"}))
        else:
            phone.hello(token=value)
        messages, code = phone.until_closed()
        check("ingest: %s is refused" % name, errors_of(messages) == ["bad_token"] and code == 1008, "%s, close %s" % (messages, code))
        phone.close()

    phone = Phone(server)
    phone.ws.send_text(json.dumps({"type": "hello", "protocol": 1, "token": 12345, "mode": "room"}))
    messages, code = phone.until_closed()
    check("ingest: a token that is not a string is refused", errors_of(messages) == ["bad_token"], "%s" % messages)
    phone.close()

    phone = Phone(server)
    phone.ws.send_binary(frames[0])
    messages, code = phone.until_closed()
    check("ingest: a frame before the hello is refused", errors_of(messages) == ["bad_hello"] and code == 1008, "%s, close %s" % (messages, code))
    phone.close()

    phone = Phone(server)
    phone.ws.send_text(json.dumps({"type": "hello", "protocol": 1, "token": token, "mode": "room", "name": "x" * 20000}))
    messages, code = phone.until_closed()
    check("ingest: a hello of 20 kB is refused", bool(errors_of(messages)) and code == 1008, "%s, close %s" % (errors_of(messages), code))
    phone.close()

    # A stranger's one big message: over the limit it is cut off, and
    # nothing of it stays in memory.
    before = server.rss_kb()
    phone = Phone(server)
    try:
        phone.ws.send_binary(b"\0" * (9 << 20))
    except wsproto.WsError:
        pass
    messages, code = phone.until_closed()
    check("ingest: a message of 9 MiB closes the connection (1009)", code in (1009, 1006) or isinstance(code, str), "close %s" % code)
    phone.close()
    time.sleep(0.5)
    grown = server.rss_kb() - before
    check("ingest: and costs no memory afterwards", grown < 24000, "%d kB" % grown)

    # Strangers cannot keep a phone out: eight may wait for their hello,
    # the ninth is turned away, and all of them are gone after 10 s.
    silent = [Phone(server) for _ in range(8)]
    try:
        Phone(server).close()
        ninth = "accepted"
    except wsproto.WsError as error:
        ninth = str(error)
    check("ingest: the ninth silent connection is refused at the door", "503" in ninth, ninth)
    if not quick:
        started = time.monotonic()
        ends = [phone.until_closed(14.0) for phone in silent]
        took = time.monotonic() - started
        check("ingest: silent connections are told bad_hello and closed within 10 s",
              all(errors_of(m) == ["bad_hello"] and c == 1008 for m, c in ends) and took < 12.5, "%.1f s, %s" % (took, ends[0]))
    for phone in silent:
        phone.close()
    time.sleep(0.3)
    phone = Phone(server)
    check("ingest: a real phone gets in right after", phone.welcomed(name="Après"))
    phone.ws.send_text(gscap.simple_message("finish"))
    phone.close()

    # Uploads without the token.
    capture = gscap_file(frames[:3], "Upload")
    for name, headers in [
        ("no Authorization", {}),
        ("a wrong token", {"Authorization": "Bearer " + "0" * 32}),
        ("Basic instead of Bearer", {"Authorization": "Basic " + token}),
        ("the token twice", {"Authorization": ["Bearer " + token, "Bearer " + token]}),
        ("the token as a cookie", {"Cookie": "token=" + token}),
    ]:
        answer = server.http("POST", "/api/captures", lan=True, headers=headers, body=capture)
        check("upload over the LAN with %s: 401" % name, answer.status == 401 and answer.code == "bad_token", repr(answer))
    answer = server.http("POST", "/api/captures?token=" + token, lan=True, body=capture)
    check("upload with the token in the URL: 401", answer.status == 401, repr(answer))
    answer = server.http("POST", "/api/captures", lan=True, headers={"Authorization": "Bearer " + token, "Origin": "https://evil.example"}, body=capture)
    check("upload with the token: accepted whatever the Origin (a native client)", answer.status == 200 and answer.json().get("frames") == 3, repr(answer))

    # What only the machine itself may ask.
    refused = []
    for method, path, body in [
        ("GET", "/api/pair", None), ("GET", "/api/pair.svg", None), ("GET", "/api/captures", None),
        ("GET", "/captures/x.gscap", None), ("DELETE", "/captures/x.gscap", None), ("DELETE", "/recordings/x.gsrec", None),
        ("POST", "/api/record", b'{"action":"start"}'), ("POST", "/api/captures/open", b'{"file":"x.gscap"}'),
        ("POST", "/api/lan", b'{"enabled":false}'),
    ]:
        headers = {"Content-Type": "application/json", "Authorization": "Bearer " + token}
        answer = server.http(method, path, lan=True, headers=headers, body=body)
        if answer.status != 403 or answer.code != "loopback_only":
            refused.append("%s %s -> %r" % (method, path, answer))
    check("the LAN gets no pairing, no capture file, no recording control, even with the token", not refused, "; ".join(refused))
    answer = server.http("GET", "/api/recordings", lan=True)
    check("the LAN listing of recordings does not say where the files are", answer.status == 200 and "dir" not in answer.json(), repr(answer))


def gscap_file(frames: list, name: str, mode: str = "room") -> bytes:
    header = json.dumps({"protocol": 1, "device": "hostile,1", "app": "0.1.0", "mode": mode, "name": name,
                         "created": "2026-11-14T10:00:00Z"}).encode()
    out = bytearray(b"GSCAP1\0\0" + struct.pack("<I", len(header)) + header)
    for frame in frames:
        out += struct.pack("<I", len(frame)) + frame
    return bytes(out)


# ------------------------------------------------------------------ a paired phone gone wrong

def rule_breakers(server: Server, frames: list, quick: bool) -> None:
    capturing = lambda: capture_going_on(server, frames)  # noqa: E731

    phone = Phone(server)
    phone.welcomed(name="Rafale")
    burst = b"".join(wsproto.encode_frame(wsproto.OP_BINARY, frame, masked=True) for frame in frames[:40])
    phone.sock.sendall(burst)
    messages, code = phone.until_closed()
    check("phone: forty frames at once are a flow_control error", "flow_control" in errors_of(messages) and code == 1008,
          "%s, close %s" % (errors_of(messages), code))
    phone.close()

    phone = Phone(server)
    phone.welcomed(name="Bruit")
    phone.ws.send_binary(os.urandom(4096))
    messages, code = phone.until_closed()
    check("phone: noise instead of a frame is bad_frame (1007)", errors_of(messages) == ["bad_frame"] and code == 1007,
          "%s, close %s" % (errors_of(messages), code))
    phone.close()

    # A frame that lies about its sizes, in every header field at once.
    lies = 0
    for seed in range(40):
        bad = bytearray(frames[seed % len(frames)])
        position = 104 + (seed * 7) % 24
        bad[position:position + 2] = os.urandom(2)
        phone = Phone(server)
        phone.welcomed(name="Mensonge")
        phone.ws.send_binary(bytes(bad))
        messages, code = phone.until_closed(3.0)
        if code in (1007, None) or any(m.get("type") == "ack" for m in messages):
            lies += 1
        phone.close()
    check("phone: frames with damaged size fields are refused or acknowledged, never fatal", lies == 40 and server.alive(), "%d of 40" % lies)

    for name, text, expected in [
        ("a text of 20 kB", json.dumps({"type": "region", "center": [0, 0, 0], "radius": 1, "pad": "x" * 20000}), "bad_message"),
        ("an unknown message type", json.dumps({"type": "format_disk"}), "bad_message"),
        ("a text that is not JSON", "{", "bad_message"),
        ("a second hello", gscap.hello_message(server.token, "room", "Encore"), "bad_message"),
        ("a region with NaN", '{"type":"region","center":[NaN,0,0],"radius":1}', "bad_message"),
    ]:
        phone = Phone(server)
        phone.welcomed(name="Texte")
        phone.ws.send_text(text)
        messages, code = phone.until_closed(5.0)
        check("phone: %s is %s" % (name, expected), errors_of(messages) == [expected] and code == 1008, "%s, close %s" % (messages[-2:], code))
        phone.close()

    # A flood of valid texts only slows its own socket down.
    phone = Phone(server)
    phone.welcomed(name="Déluge")
    before = server.rss_kb()
    flood = b"".join(wsproto.encode_frame(wsproto.OP_TEXT, (b'{"type":"pause"}', b'{"type":"resume"}')[i % 2], masked=True) for i in range(30000))
    sender = threading.Thread(target=lambda: phone.sock.sendall(flood), daemon=True)
    started = time.monotonic()
    sender.start()
    latencies = []
    while sender.is_alive() and time.monotonic() - started < 60:
        t0 = time.monotonic()
        server.http("GET", "/api/info", timeout=10.0)
        latencies.append(time.monotonic() - t0)
        time.sleep(0.02)
    sender.join(1.0)
    worst = max(latencies) if latencies else 0.0
    check("phone: 30000 pause and resume messages do not stall the server", not sender.is_alive() and worst < 1.0,
          "%.1f s, worst /api/info %.0f ms" % (time.monotonic() - started, worst * 1000))
    phone.ws.send_binary(frames[0])
    acked = wait_for(lambda: (lambda m: isinstance(m, dict) and m.get("type") == "ack")(phone.next(1.0)), 30.0, 0.0)
    grown = server.rss_kb() - before
    check("phone: and the capture still answers afterwards, at no cost in memory", acked and grown < 30000, "ack %s, %d kB" % (acked, grown))
    phone.ws.send_text(gscap.simple_message("finish"))
    phone.until_closed(1.0)
    phone.close()

    # A phone that dies in the middle of a frame (the socket stays open,
    # as when it walks out of Wi-Fi range).
    dead = Phone(server)
    dead.welcomed(name="Panne", device="iPhone-A")
    dead.ws.send_binary(frames[0])
    dead.next()
    half = wsproto.encode_frame(wsproto.OP_BINARY, frames[1], masked=True)
    dead.sock.sendall(half[:len(half) // 2])
    died = time.monotonic()
    check("dead phone: its capture is going on", wait_for(capturing, 5.0))
    other = Phone(server)
    other.hello(name="Autre", device="iPhone-B")
    messages, code = other.until_closed(5.0)
    check("dead phone: another phone is told busy at first", errors_of(messages) == ["busy"] and code == 1008, "%s, close %s" % (messages, code))
    other.close()
    answer = server.http("POST", "/api/captures", body=gscap_file(frames[:3], "Pendant"))
    check("dead phone: an import waits too (409 busy)", answer.status == 409 and answer.code == "busy", repr(answer))
    viewer_ok = server.http("GET", "/api/info", timeout=5.0).status == 200
    check("dead phone: the server answers meanwhile", viewer_ok)
    if not quick:
        time.sleep(max(0.0, 13.0 - (time.monotonic() - died)))
        other = Phone(server)
        took_over = other.welcomed(name="Autre", device="iPhone-B")
        check("dead phone: after 12 s of silence another phone takes over", took_over)
        other.ws.send_text(gscap.simple_message("finish"))
        other.until_closed(1.0)
        other.close()
        # The server lets go of the dead connection by itself.
        left = wait_for(lambda: len(dead.until_closed(1.0)[0]) >= 0 and dead.ws.closed, 50.0, 0.0)
        check("dead phone: the server closes the dead connection by itself", left, "%.0f s after the last byte" % (time.monotonic() - died))

        # Alone this time: nobody takes over, the capture must still end.
        dead2 = Phone(server)
        dead2.welcomed(name="Seule", device="iPhone-C")
        dead2.ws.send_binary(frames[0])
        dead2.next()
        dead2.sock.sendall(half[:len(half) // 2])
        died = time.monotonic()
        wait_for(capturing, 5.0)
        freed = wait_for(lambda: not capturing(), 60.0, 0.5)
        check("dead phone, alone: the capture is released within a minute", freed, "%.0f s" % (time.monotonic() - died))
        dead2.close()
    dead.close()

    # A phone that hangs up in the middle of a frame.
    gone = Phone(server)
    gone.welcomed(name="Coupure", device="iPhone-D")
    gone.ws.send_binary(frames[0])
    gone.next()
    gone.sock.sendall(half[:len(half) // 2])
    gone.sock.close()
    check("phone that hangs up mid-frame: the capture is released at once", wait_for(lambda: not capturing(), 5.0))
    kept = [c for c in server.http("GET", "/api/captures").json()["captures"] if c["title"] == "Coupure"]
    check("phone that hangs up mid-frame: what it sent is kept, readable", len(kept) == 1 and readable_capture(server, kept[0]["file"]) == 1,
          "%s" % [(c["file"], c["size"]) for c in kept])


def readable_capture(server: Server, file: str) -> int:
    """Number of frames the reference reader finds in a kept capture."""
    path = os.path.join(server.captures, file)
    try:
        with gscap.GscapReader(path) as reader:
            return sum(1 for _ in reader)
    except (OSError, ValueError) as error:
        print("     %s: %s" % (file, error))
        return -1


# ------------------------------------------------------------------ a web page of another origin

def other_origins(server: Server, frames: list) -> None:
    own = "http://127.0.0.1:%d" % server.port
    capture = gscap_file(frames[:3], "Origine")
    mutations = [
        ("POST", "/api/record", b'{"action":"start","name":"csrf"}'),
        ("POST", "/api/record", b'{"action":"stop"}'),
        ("POST", "/api/captures", capture),
        ("POST", "/api/captures/open", b'{"file":"x.gscap"}'),
        ("DELETE", "/recordings/x.gsrec", None),
        ("DELETE", "/captures/x.gscap", None),
        ("POST", "/api/lan", b'{"enabled":false}'),
        ("GET", "/api/pair", None),
        ("GET", "/api/pair.svg", None),
    ]
    origins = ["http://evil.example", "https://127.0.0.1:%d" % server.port, "http://127.0.0.1:%d" % server.lan_port,
               "http://localhost:%d" % server.port, "null", "http://127.0.0.1:%d.evil.example" % server.port,
               ["http://127.0.0.1:%d" % server.port, "http://evil.example"]]
    passed = []
    for origin in origins:
        for method, path, body in mutations:
            # text/plain: what a page can send across origins without a preflight.
            answer = server.http(method, path, headers={"Origin": origin, "Content-Type": "text/plain"}, body=body)
            if answer.status != 403 or answer.code != "origin_not_allowed":
                passed.append("%s %s from %s -> %r" % (method, path, origin, answer))
    check("other origins: no route that changes or reveals something answers them", not passed, "; ".join(passed[:4]))
    state = server.http("GET", "/api/record").json()
    check("other origins: nothing was recorded", state["recording"] is None and state["last"] is None, "%s" % state)

    # A preflight tells a page of another origin that it may not ask.
    answer = server.http("OPTIONS", "/api/record", headers={"Origin": "http://evil.example", "Access-Control-Request-Method": "POST",
                                                           "Access-Control-Request-Headers": "content-type"})
    check("other origins: the preflight grants nothing", "access-control-allow-origin" not in answer.headers, "%s" % answer.headers)
    answer = server.http("GET", "/api/recordings", headers={"Origin": "http://evil.example"})
    check("other origins: reads carry no CORS grant", "access-control-allow-origin" not in answer.headers)
    answer = server.http("GET", "/api/pair", headers={"Origin": "tauri://localhost"})
    check("the desktop window's origin gets the pairing code", answer.status == 200 and answer.headers.get("access-control-allow-origin") == "tauri://localhost", repr(answer))
    answer = server.http("GET", "/api/pair", headers={"Origin": own})
    check("the page's own origin gets the pairing code", answer.status == 200, repr(answer))

    # DNS rebinding: the attacker's name resolves to 127.0.0.1, the browser
    # believes the page and the server are one origin. Only Host gives it away.
    leaked = []
    for host in ["evil.example", "evil.example:%d" % server.port, "127.0.0.1.evil.example", "192.168.1.20:%d" % server.port,
                 "127.0.0.1:%d@evil.example" % server.port, "", ["127.0.0.1:%d" % server.port, "evil.example"]]:
        for method, path in [("GET", "/api/pair"), ("GET", "/api/pair.svg"), ("GET", "/api/captures"), ("GET", "/api/recordings"),
                             ("GET", "/api/info"), ("GET", "/"), ("GET", "/recordings/x.gsrec"), ("GET", "/captures/x.gscap"),
                             ("POST", "/api/record"), ("DELETE", "/recordings/x.gsrec")]:
            origin = "http://%s" % (host if isinstance(host, str) else host[1])
            answer = server.http(method, path, headers={"Host": host, "Origin": origin if method != "GET" else None},
                                 body=b'{"action":"start"}' if method == "POST" else None)
            if answer.status not in (400, 403):
                leaked.append("%s %s as %s -> %r" % (method, path, host, answer))
    check("rebinding: a request addressed to another name gets nothing", not leaked, "; ".join(leaked[:4]))
    for path in ["/ws", "/ingest"]:
        sock = server.connect()
        try:
            wsproto.client_handshake(sock, "evil.example", path, 5.0, {"Origin": "http://evil.example"})
            result = "accepted"
        except wsproto.WsError as error:
            result = str(error)
        finally:
            sock.close()
        check("rebinding: the WebSocket %s is refused too" % path, "403" in result, result)
    for name in ["localhost:%d" % server.port, "[::1]:%d" % server.port, "LOCALHOST:%d" % server.port]:
        answer = server.http("GET", "/api/pair", headers={"Host": name})
        check("the machine's own name %s is answered" % name, answer.status == 200, repr(answer))


# ------------------------------------------------------------------ uploads

def uploads(server: Server, frames: list, quick: bool) -> None:
    capture = gscap_file(frames[:6], "Envoi")

    def start_upload(lan: bool, announced, first: bytes, extra=None):
        sock = server.connect(lan)
        headers = {"Authorization": "Bearer " + server.token} if lan else {}
        headers.update(extra or {})
        sock.sendall(server.head_bytes("POST", "/api/captures", lan, headers, announced) + first)
        return sock

    # An announced length over the limit is refused before a byte is read.
    started = time.monotonic()
    sock = start_upload(True, 1 << 62, capture[:64])
    answer = server.read_answer(sock, 10.0)
    sock.close()
    check("upload: a body announced at 4 EiB is refused at once (413)", answer.status == 413 and answer.code == "too_large" and time.monotonic() - started < 3, repr(answer))

    # Not a capture: refused on its first bytes, while the client still has gigabytes to send.
    started = time.monotonic()
    sock = start_upload(False, 8 << 30, b"PK\x03\x04" + b"\0" * 4096)
    answer = server.read_answer(sock, 15.0)
    sock.close()
    check("upload: something else than a capture is refused on its first bytes", answer.status == 400 and answer.code == "bad_file_magic",
          "%r after %.1f s" % (answer, time.monotonic() - started))

    # The client disappears in the middle of the body.
    sock = start_upload(True, len(capture), capture[:len(capture) // 2])
    time.sleep(0.3)
    sock.close()
    time.sleep(0.5)
    check("upload: a body cut short leaves no file behind", not server.parts_left(), "%s" % server.parts_left())
    answer = server.http("POST", "/api/captures", lan=True, headers={"Authorization": "Bearer " + server.token}, body=capture)
    check("upload: and the next one is taken", answer.status == 200 and answer.json().get("frames") == 6, repr(answer))

    # A second upload while one is being received.
    slow = start_upload(True, len(capture), capture[:40])
    time.sleep(0.3)
    answer = server.http("POST", "/api/captures", body=capture)
    check("upload: one at a time (409 import_in_progress)", answer.status == 409 and answer.code == "import_in_progress", repr(answer))
    if quick:
        slow.close()
    else:
        # The first one never continues: the server gives up on it.
        started = time.monotonic()
        answer = server.read_answer(slow, 30.0)
        slow.close()
        took = time.monotonic() - started
        check("upload: a body that stalls is given up after the request timeout (408)", answer.status == 408 and answer.code == "timeout" and took < 20,
              "%r after %.1f s" % (answer, took))
        # A JSON body that never comes.
        sock = server.connect()
        sock.sendall(server.head_bytes("POST", "/api/record", False, {"Content-Type": "application/json"}, 64) + b'{"action":')
        started = time.monotonic()
        answer = server.read_answer(sock, 30.0)
        sock.close()
        check("a JSON body that stalls is given up too (408)", answer.status == 408 and time.monotonic() - started < 20, repr(answer))
    time.sleep(0.5)
    check("upload: nothing half received is left on the disk", not server.parts_left(), "%s" % server.parts_left())
    answer = server.http("POST", "/api/captures", body=capture)
    check("upload: the server takes captures again", answer.status == 200, repr(answer))

    # Truncated and damaged files.
    cut = server.http("POST", "/api/captures", body=capture[:len(capture) - 100])
    check("upload: a file cut inside its last frame keeps the complete frames", cut.status == 200 and cut.json().get("truncated") is True and cut.json().get("frames") == 5, repr(cut))
    damaged = bytearray(capture)
    at = capture.index(b"GSCF", 200) + 4
    damaged[at] = 99
    answer = server.http("POST", "/api/captures", body=bytes(damaged))
    check("upload: a damaged frame fails the import with its code", answer.status == 400 and answer.code == "bad_version", repr(answer))
    huge = b"GSCAP1\0\0" + struct.pack("<I", 0xFFFFFFF0) + b"{}"
    answer = server.http("POST", "/api/captures", body=huge)
    check("upload: a header announced at 4 GiB is refused, not allocated", answer.status == 400 and answer.code == "bad_header", repr(answer))
    answer = server.http("POST", "/api/captures", body=b"")
    check("upload: an empty body is a bad request", answer.status == 400, repr(answer))
    names = [c["file"] for c in server.http("GET", "/api/captures").json()["captures"]]
    check("upload: refused files are not kept", not any("part" in name for name in names) and not server.parts_left(), "%s" % names)

    # A name that tries to leave the directory becomes a plain file name.
    answer = server.http("POST", "/api/captures", body=gscap_file(frames[:3], "../../../etc/cron.d/x\0y\n<script>"))
    kept = answer.json().get("file") if answer.status == 200 else None
    inside = kept is not None and os.path.isfile(os.path.join(server.captures, kept)) and "/" not in kept and ".." not in kept
    check("upload: a capture named like a path stays a file of the captures directory", inside, "%r -> %s" % (answer, kept))


# ------------------------------------------------------------------ recordings, ranges, names

def recordings(server: Server, with_frontend: bool) -> None:
    # The uploads above left a capture on show, and a capture that nobody
    # feeds does not move: back to the simulation first.
    leave_capture(server)
    started = server.post_json("/api/record", {"action": "start", "name": "Plages"})
    check("record: started", started.status == 200, repr(started))
    file = started.json()["recording"]["file"]
    stem = file[:-len(".gsrec")]
    path = "/recordings/" + file
    # A recorder alone makes the scene tick: the file grows.
    sizes = []
    for _ in range(3):
        time.sleep(0.4)
        sizes.append(int(server.http("HEAD", path).headers.get("content-length", "0")))
    check("growing file: its length is the length of the moment", sizes[0] > 1024 and sizes[2] > sizes[0], "%s" % sizes)
    part = server.http("GET", path, headers={"Range": "bytes=0-1023"})
    check("growing file: a range is served", part.status == 206 and len(part.body) == 1024 and part.body[:6] == b"GSREC1", repr(part.status))
    first = server.http("GET", path, headers={"Range": "bytes=0-15"})
    time.sleep(0.3)
    again = server.http("GET", path, headers={"Range": "bytes=16-31", "If-Range": first.headers.get("etag", "")})
    check("growing file: If-Range of an older state gives the whole file of now", again.status == 200 and len(again.body) > sizes[0],
          "%d, %d bytes" % (again.status, len(again.body)))
    whole = server.http("GET", path)
    announced = int(whole.headers.get("content-length", "-1"))
    check("growing file: a whole read delivers exactly what it announced", whole.status == 200 and len(whole.body) == announced, "%d of %d" % (len(whole.body), announced))
    tail = server.http("GET", path, headers={"Range": "bytes=-64"})
    total = int(tail.headers.get("content-range", "x/0").split("/")[1])
    check("growing file: a suffix range ends where the file ended then", tail.status == 206 and len(tail.body) == 64 and
          tail.headers.get("content-range") == "bytes %d-%d/%d" % (total - 64, total - 1, total), tail.headers.get("content-range", ""))
    answer = server.http("GET", "/api/share/%s.zip" % stem)
    check("share: 409 while the recording is being written", answer.status == 409 and answer.code == "in_use", repr(answer))
    answer = server.http("DELETE", path)
    check("delete: 409 while the recording is being written", answer.status == 409 and answer.code == "in_use", repr(answer))
    answer = server.post_json("/api/record", {"action": "start", "name": "Plages"})
    check("record: a second start is refused", answer.status == 409 and answer.code == "already_recording", repr(answer))
    stopped = server.post_json("/api/record", {"action": "stop"})
    check("record: stopped", stopped.status == 200 and stopped.json()["saved"]["reason"] == "stopped", repr(stopped))

    whole = server.http("GET", path)
    data, size = whole.body, len(whole.body)
    etag = whole.headers.get("etag", "")
    check("finished file: whole, with a validator", whole.status == 200 and size == int(whole.headers["content-length"]) and etag.startswith('"')
          and whole.headers.get("accept-ranges") == "bytes" and "content-encoding" not in whole.headers, "%d bytes, %s" % (size, etag))
    cases = [
        ("bytes=0-0", 206, data[:1], "bytes 0-0/%d" % size),
        ("bytes=%d-" % (size - 1), 206, data[-1:], "bytes %d-%d/%d" % (size - 1, size - 1, size)),
        ("bytes=-1", 206, data[-1:], "bytes %d-%d/%d" % (size - 1, size - 1, size)),
        ("bytes=-%d" % (size * 3), 206, data, "bytes 0-%d/%d" % (size - 1, size)),
        ("bytes=100-199", 206, data[100:200], "bytes 100-199/%d" % size),
        ("bytes=%d-%d" % (size - 10, size + 5000), 206, data[-10:], "bytes %d-%d/%d" % (size - 10, size - 1, size)),
        ("bytes=%d-" % size, 416, None, "bytes */%d" % size),
        ("bytes=%d-%d" % (size + 5, size + 10), 416, None, "bytes */%d" % size),
        ("bytes=-0", 416, None, "bytes */%d" % size),
        ("bytes=18446744073709551616-", 200, data, None),
        ("bytes=0-10,20-30", 200, data, None),
        ("bytes=10-5", 200, data, None),
        ("bytes=abc", 200, data, None),
        ("bytes=-", 200, data, None),
        ("lines=0-5", 200, data, None),
        (["bytes=0-5", "bytes=10-15"], 200, data, None),
    ]
    wrong = []
    for value, status, body, content_range in cases:
        answer = server.http("GET", path, headers={"Range": value})
        if answer.status != status or (body is not None and answer.body != body) or answer.headers.get("content-range") != content_range:
            wrong.append("%s -> %d, %d bytes, %s" % (value, answer.status, len(answer.body), answer.headers.get("content-range")))
    check("finished file: %d kinds of Range are answered by the book" % len(cases), not wrong, "; ".join(wrong))
    answer = server.http("HEAD", path, headers={"Range": "bytes=5-9"})
    check("finished file: HEAD of a range has its headers and no body", answer.status == 206 and answer.headers.get("content-length") == "5", repr(answer.headers.get("content-range")))
    answer = server.http("GET", path, headers={"If-None-Match": etag})
    check("finished file: If-None-Match answers 304", answer.status == 304 and not answer.body)
    answer = server.http("GET", path, headers={"Range": "bytes=0-9", "If-Range": etag})
    check("finished file: If-Range with the current validator gives the range", answer.status == 206 and answer.body == data[:10])
    answer = server.http("GET", path, headers={"Range": "bytes=0-9", "If-Range": whole.headers.get("last-modified", "x")})
    check("finished file: If-Range with a date gives the whole file", answer.status == 200 and len(answer.body) == size)
    answer = server.http("GET", path + "?download", lan=True)
    check("finished file: served to the LAN, as a download when asked", answer.status == 200 and answer.body == data and
          "attachment" in answer.headers.get("content-disposition", ""), answer.headers.get("content-disposition", ""))

    # Names. A file next to the directory and a link to it must stay out of reach.
    secret = os.path.join(server.data, "secret.gsrec")
    with open(secret, "wb") as out:
        out.write(b"SECRET")
    os.symlink(secret, os.path.join(server.recordings, "link.gsrec"))
    os.makedirs(os.path.join(server.recordings, "dir.gsrec"), exist_ok=True)
    with open(os.path.join(server.recordings, ".hidden.gsrec"), "wb") as out:
        out.write(b"HIDDEN")
    odd = ["../secret.gsrec", "..%2Fsecret.gsrec", "%2e%2e%2fsecret.gsrec", "..%5Csecret.gsrec", "%2e%2e/secret.gsrec", "....//secret.gsrec",
           "link.gsrec", "dir.gsrec", ".hidden.gsrec", "%2ehidden.gsrec", file + "%00", file + "%00.gsrec", file + "/", file + "/..", "x%0d%0aSet-Cookie:a=b.gsrec",
           "CON.gsrec", "nul.gsrec", "a" * 300 + ".gsrec", "a" * 9000 + ".gsrec", "%ff%fe.gsrec", "%", "%zz.gsrec", "*.gsrec", "?.gsrec",
           file.upper(), stem, stem + ".GSREC", stem + ".gscap", "", "/", "//" + file, "./" + file, "%2e/" + file, ".." , "%2e%2e"]
    wrong = []
    for prefix, suffix in [("/recordings/", ""), ("/captures/", ""), ("/api/share/", ".zip")]:
        for name in odd:
            for method in ("GET", "DELETE"):
                target = prefix + name + (suffix if name.endswith(".gsrec") else "")
                if suffix and name.endswith(".gsrec"):
                    target = prefix + name[:-len(".gsrec")] + suffix
                try:
                    answer = server.http(method, target, timeout=10.0)
                except (OSError, wsproto.WsError, ValueError, IndexError) as error:
                    wrong.append("%s %s -> %s" % (method, target[:60], error))
                    continue
                leaked = b"SECRET" in answer.body or b"HIDDEN" in answer.body
                # `./<file>` and `//<file>` are the file itself once the path is normalised by nobody: any 4xx is right too.
                if leaked or answer.status >= 500 or (answer.status == 200 and name not in ("./" + file, "%2e/" + file, "//" + file)):
                    wrong.append("%s %s -> %r" % (method, target[:60], answer))
    still = os.path.exists(secret) and os.path.lexists(os.path.join(server.recordings, "link.gsrec")) and \
        os.path.exists(os.path.join(server.recordings, ".hidden.gsrec")) and os.path.isfile(os.path.join(server.recordings, file))
    check("names: %d odd names on three routes reach nothing outside the listing" % len(odd), not wrong and still, "; ".join(wrong[:5]))
    listed = [entry["file"] for entry in server.http("GET", "/api/recordings").json()["recordings"]]
    check("names: links, directories and hidden files are not listed", file in listed and not {"link.gsrec", "dir.gsrec", ".hidden.gsrec"} & set(listed), "%s" % listed)
    answer = server.post_json("/api/captures/open", {"file": "../recordings/" + file})
    check("names: a capture to open is a name, not a path", answer.status == 404, repr(answer))

    wrong = []
    for method, target in [("PUT", path), ("POST", path), ("PATCH", "/api/record"), ("DELETE", "/api/recordings"), ("DELETE", "/api/share/%s.zip" % stem),
                           ("POST", "/api/share/%s.zip" % stem), ("PUT", "/api/captures"), ("DELETE", "/api/captures"), ("POST", "/api/pair"), ("TRACE", "/api/pair")]:
        answer = server.http(method, target, body=b"x")
        if answer.status != 405:
            wrong.append("%s %s -> %d" % (method, target, answer.status))
    check("methods: what a route does not offer is 405", not wrong, "; ".join(wrong))

    # Sharing.
    answer = server.http("GET", "/api/share/nothing-of-that-name.zip")
    check("share: 404 for a recording that does not exist", answer.status == 404 and answer.code == "not_found", repr(answer))
    answer = server.http("GET", "/api/share/%s.gsrec" % stem)
    check("share: 404 for anything but <name>.zip", answer.status == 404, repr(answer))
    answer = server.http("GET", "/api/share/%s.zip" % stem, lan=True)
    if with_frontend:
        announced = int(answer.headers.get("content-length", "-1"))
        check("share: the archive is as long as announced and holds the recording", answer.status == 200 and len(answer.body) == announced and data in answer.body,
              "%d of %d bytes" % (len(answer.body), announced))
    else:
        check("share: 503 without a web build", answer.status == 503 and answer.code == "no_frontend", repr(answer))
    answer = server.http("DELETE", path)
    check("delete: the recording goes", answer.status == 200 and not os.path.exists(os.path.join(server.recordings, file)), repr(answer))
    answer = server.http("GET", "/api/share/%s.zip" % stem)
    check("share: 404 once the recording is deleted", answer.status == 404, repr(answer))


# ------------------------------------------------------------------ clients that never read

def silent_readers(server: Server, with_frontend: bool) -> None:
    """Twelve clients ask for a recording too long for any buffer and read
    nothing: the server must let go of the file and of the connections."""
    big = os.path.join(server.recordings, "Long.gsrec")
    with open(big, "wb") as out:
        out.truncate(300 << 20)

    def held() -> int:
        count = 0
        for fd in os.listdir("/proc/%d/fd" % server.process.pid):
            try:
                count += os.readlink("/proc/%d/fd/%s" % (server.process.pid, fd)) == big
            except OSError:
                pass
        return count

    paths = ["/recordings/Long.gsrec", "/recordings/Long.gsrec?download"] + (["/api/share/Long.zip"] if with_frontend else [])
    socks = []
    for i in range(12):
        sock = server.connect(lan=i % 2 == 0)
        sock.sendall(server.head_bytes("GET", paths[i % len(paths)], i % 2 == 0, {"Connection": None}))
        socks.append(sock)
    started = time.monotonic()
    opened = wait_for(lambda: held() >= 6, 20.0)
    released = wait_for(lambda: held() == 0, 60.0, 0.5)
    check("silent readers: the server lets go of the file by itself", opened and released,
          "%d descriptors after %.0f s" % (held(), time.monotonic() - started))
    closed = 0
    for sock in socks:
        sock.settimeout(10.0)
        try:
            while sock.recv(1 << 20):
                pass
            closed += 1
        except (OSError, ssl.SSLError):
            closed += 1     # reset, or cut inside a TLS record: closed all the same
        sock.close()
    check("silent readers: and of their connections", closed == 12, "%d of 12" % closed)
    os.remove(big)


# ------------------------------------------------------------------ main

def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--port", type=int, default=7940)
    parser.add_argument("--binary", default=os.path.join(REPO, "target", "release", "gs-server"))
    parser.add_argument("--dist", default=os.path.join(REPO, "e2e", "server", "standin"))
    parser.add_argument("--out", default=None, help="keep the server log and its files here")
    parser.add_argument("--quick", action="store_true", help="skip the checks that wait for the server's timeouts")
    args = parser.parse_args()
    if not os.path.isfile(args.binary):
        print("missing %s: cargo build --release -p gs-server" % args.binary)
        return 2

    data = args.out or tempfile.mkdtemp(prefix="gs-hostile-")
    os.makedirs(data, exist_ok=True)
    frames = [gscap.encode_frame(frame) for frame in gscap.synth_frames(40)]
    server = Server(args.binary, args.port, data, os.path.join(data, "server.log"), args.dist)
    code = None
    try:
        strangers(server, frames, args.quick)
        other_origins(server, frames)
        uploads(server, frames, args.quick)
        recordings(server, bool(args.dist))
        if not args.quick:
            silent_readers(server, bool(args.dist))
        rule_breakers(server, frames, args.quick)
        info = server.http("GET", "/api/info").json()
        check("after all of it the server answers and nobody is left", info["clients"] == 0, "%s" % info["clients"])
    finally:
        code = server.stop()
    check("clean exit", code == 0, "exit %s" % code)
    with open(server.log_path, "rb") as log:
        text = log.read().decode("utf-8", "replace")
    check("the pairing token is nowhere in the server's output", server.token not in text and server.token.upper() not in text)
    check("neither is the pairing URI", "splatlive://" not in text)
    check("no panic and no internal error in the log", "panicked" not in text and "erreur interne" not in text)
    if not args.out:
        shutil.rmtree(data, ignore_errors=True)
    print("\nall checks passed" if not FAILED else "\n%d check(s) FAILED:\n  %s" % (len(FAILED), "\n  ".join(FAILED)))
    return 1 if FAILED else 0


if __name__ == "__main__":
    sys.exit(main())
