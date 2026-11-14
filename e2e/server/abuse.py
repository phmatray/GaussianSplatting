#!/usr/bin/env python3
"""gs-server under abuse, seen from outside the process.

Starts the release binary and throws at it what the integration tests cannot
measure from inside: threads, file descriptors and resident memory before and
after connection churn, sockets that say nothing, malformed WebSocket frames,
dressed-up Origin and Host headers, a storm of LAN switches, and a shutdown
with everything still connected. Then a full-size house scan checks the
delivery contract byte for byte: a viewer that joins after the scan holds
exactly the layer of one that watched all of it.

    cargo build --release -p gs-server
    python3 e2e/server/abuse.py [port]      (default 7880; uses port .. port + 3)

    GS_SERVER_BIN=path   binary to test (default target/release/gs-server)

Standard library only. Takes about a minute and a half. Exit status 0 means
every check passed. Linux only (reads /proc).
"""
import hashlib
import json
import os
import signal
import socket
import ssl
import struct
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request

REPO = os.path.abspath(os.path.join(os.path.dirname(__file__), "../.."))
BIN = os.environ.get("GS_SERVER_BIN", os.path.join(REPO, "target/release/gs-server"))
PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 7880
LAN = PORT + 1
failed = 0


def check(name, ok, detail=""):
    global failed
    print(f"{'ok  ' if ok else 'FAIL'} {name}{f'  ({detail})' if detail else ''}", flush=True)
    failed += 0 if ok else 1


# -- process ---------------------------------------------------------------------

def stats(pid):
    status = open(f"/proc/{pid}/status").read()
    field = lambda key: int(next(line for line in status.splitlines() if line.startswith(key)).split()[1])
    return {"rss_kb": field("VmRSS:"), "threads": field("Threads:"), "fds": len(os.listdir(f"/proc/{pid}/fd"))}


def info(port=PORT):
    return json.loads(urllib.request.urlopen(f"http://127.0.0.1:{port}/api/info", timeout=5).read())


def start(args, log):
    process = subprocess.Popen([BIN] + args, stdout=open(log, "w"), stderr=subprocess.STDOUT)
    port = int(args[args.index("--port") + 1])
    for _ in range(100):
        try:
            assert info(port)["http_url"].endswith(f":{port}"), "the port was taken: another server is running"
            return process
        except (OSError, urllib.error.URLError):
            time.sleep(0.1)
    process.kill()
    raise RuntimeError(f"gs-server did not start: {open(log).read()}")


def stop(process, limit=10):
    """Ctrl-C; returns (exit code or None, seconds it took)."""
    began = time.time()
    process.send_signal(signal.SIGINT)
    try:
        return process.wait(limit), time.time() - began
    except subprocess.TimeoutExpired:
        process.kill()
        return None, time.time() - began


def post_lan(enabled):
    request = urllib.request.Request(
        f"http://127.0.0.1:{PORT}/api/lan", data=json.dumps({"enabled": enabled}).encode(),
        headers={"Content-Type": "application/json"})
    return json.loads(urllib.request.urlopen(request, timeout=20).read())


# -- sockets and a minimal WebSocket client ---------------------------------------

def tcp(port, rcvbuf=None, timeout=10):
    sock = socket.socket()
    if rcvbuf:
        sock.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, rcvbuf)
    sock.settimeout(timeout)
    sock.connect(("127.0.0.1", port))
    return sock


def tls(port, timeout=10):
    context = ssl.create_default_context()
    context.check_hostname = False
    context.verify_mode = ssl.CERT_NONE
    return context.wrap_socket(tcp(port, timeout=timeout))


def upgrade_request(host, origins=()):
    lines = [f"GET /ws HTTP/1.1", f"Host: {host}", "Upgrade: websocket", "Connection: Upgrade",
             "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==", "Sec-WebSocket-Version: 13"]
    lines += [f"Origin: {origin}" for origin in origins]
    return ("\r\n".join(lines) + "\r\n\r\n").encode()


def status_of(port, request, timeout=5):
    """Status code of the answer to a raw request (0 when there is none)."""
    sock = tcp(port, timeout=timeout)
    try:
        sock.sendall(request)
        head = sock.recv(12)
        return int(head[9:12]) if len(head) == 12 else 0
    except (OSError, ValueError):
        return 0
    finally:
        sock.close()


def ws(port=PORT, secure=False, **options):
    sock = tls(port, **options) if secure else tcp(port, **options)
    sock.sendall(upgrade_request(f"127.0.0.1:{port}"))
    head = b""
    while b"\r\n\r\n" not in head:
        chunk = sock.recv(1)
        if not chunk:
            break
        head += chunk
    assert head.startswith(b"HTTP/1.1 101"), head
    return sock


def frame(opcode, payload=b"", fin=True, mask=True, declared=None):
    length = len(payload) if declared is None else declared
    first = (0x80 if fin else 0) | opcode
    masked = 0x80 if mask else 0
    if length < 126:
        head = struct.pack("!BB", first, masked | length)
    elif length < 65536:
        head = struct.pack("!BBH", first, masked | 126, length)
    else:
        head = struct.pack("!BBQ", first, masked | 127, length)
    if not mask:
        return head + payload
    key = b"\x01\x02\x03\x04"
    return head + key + bytes(byte ^ key[i % 4] for i, byte in enumerate(payload))


def recv_exact(sock, count):
    data = b""
    while len(data) < count:
        chunk = sock.recv(min(1 << 16, count - len(data)))
        if not chunk:
            raise EOFError
        data += chunk
    return data


def read_frame(sock):
    first, second = recv_exact(sock, 2)
    length = second & 0x7F
    if length == 126:
        length = struct.unpack("!H", recv_exact(sock, 2))[0]
    elif length == 127:
        length = struct.unpack("!Q", recv_exact(sock, 8))[0]
    return first & 0x0F, recv_exact(sock, length)


def how_it_ends(sock, limit=8):
    """Reads to the end: the close code, 'eof', or 'open' if it is still there."""
    end = time.time() + limit
    try:
        while time.time() < end:
            opcode, payload = read_frame(sock)
            if opcode == 8:
                return struct.unpack("!H", payload[:2])[0] if len(payload) >= 2 else "close"
    except EOFError:
        return "eof"
    except (socket.timeout, TimeoutError):
        return "open"
    except OSError:
        return "eof"
    return "open"


def is_open(sock):
    """Whether the server still holds its end of a socket that was sent nothing."""
    sock.setblocking(False)
    try:
        return (sock.recv(1) if isinstance(sock, ssl.SSLSocket) else sock.recv(1, socket.MSG_PEEK)) != b""
    except (BlockingIOError, ssl.SSLWantReadError):
        return True
    except OSError:
        return False
    finally:
        sock.setblocking(True)


def churn(cycles):
    for i in range(cycles):
        sock = ws(LAN, secure=True) if i % 2 else ws()
        for _ in range(4):
            read_frame(sock)
        if i % 4 < 2:
            try:
                sock.sendall(frame(8, struct.pack("!H", 1000)))
            except OSError:
                pass
        sock.close()


# -- part 1: the stage under abuse -------------------------------------------------

def abuse(tmp):
    args = ["--port", str(PORT), "--lan", "--lan-port", str(LAN), "--cert-dir", os.path.join(tmp, "certs"), "--light"]
    server = start(args, os.path.join(tmp, "stage.log"))
    try:
        time.sleep(0.5)
        idle = stats(server.pid)

        # Connect/disconnect cycles, plain and TLS, polite and abrupt. The
        # first round warms up lazily created state; the second must cost
        # nothing more.
        churn(200)
        time.sleep(1.5)
        warm = stats(server.pid)
        churn(200)
        time.sleep(1.5)
        after = stats(server.pid)
        check("400 connect/disconnect cycles: no viewer, thread or descriptor left",
              info()["clients"] == 0 and after["threads"] <= idle["threads"] and after["fds"] <= idle["fds"] + 1,
              f"idle {idle}, after {after}")
        check("the second 200 cycles cost no memory", after["rss_kb"] - warm["rss_kb"] < 3000,
              f"{warm['rss_kb']} kB -> {after['rss_kb']} kB")

        # A storm of contradictory LAN switches while viewers come and go over TLS.
        stop_viewers, sessions, closes = threading.Event(), [0], {}

        def tls_viewer():
            while not stop_viewers.is_set():
                try:
                    sock = ws(LAN, secure=True, timeout=5)
                except (OSError, AssertionError):
                    time.sleep(0.01)
                    continue
                sessions[0] += 1
                end = how_it_ends(sock, 30)
                closes[end] = closes.get(end, 0) + 1
                sock.close()

        viewers = [threading.Thread(target=tls_viewer) for _ in range(6)]
        for viewer in viewers:
            viewer.start()
        local = ws()
        fingerprints, ports = set(), set()

        def switcher(offset):
            for i in range(60):
                lan = post_lan((i + offset) % 2 == 0)["lan"]
                if lan["enabled"]:
                    fingerprints.add(lan["fingerprint"])
                    ports.update(url.rsplit(":", 1)[1] for url in lan["urls"])
                time.sleep(0.004)

        switchers = [threading.Thread(target=switcher, args=(k,)) for k in range(3)]
        began = time.time()
        for thread in switchers:
            thread.start()
        frames = 0
        while any(thread.is_alive() for thread in switchers):
            if read_frame(local)[0] == 2:
                frames += 1
        took = time.time() - began
        post_lan(True)
        time.sleep(1)
        stop_viewers.set()
        post_lan(False)
        for viewer in viewers:
            viewer.join(40)
        local.close()
        time.sleep(1.5)
        calm = stats(server.pid)
        check("180 LAN switches: one certificate, one port", len(fingerprints) == 1 and ports <= {str(LAN)},
              f"{len(fingerprints)} fingerprint(s), ports {sorted(ports)}")
        check("viewers over TLS are told to go (1001) every time", sessions[0] >= 6 and set(closes) <= {1001, "eof"}
              and closes.get(1001, 0) * 2 >= sessions[0], f"{sessions[0]} sessions, closes {closes}")
        check("a loopback viewer streams through the storm", frames / took >= 30, f"{frames / took:.0f} frames/s")
        check("after the storm: LAN off, nobody left, nothing leaked",
              not info()["lan"]["enabled"] and info()["clients"] == 0 and calm["fds"] < idle["fds"]
              and calm["threads"] <= idle["threads"], f"{calm}")
        post_lan(True)

        # Sockets that ask for nothing. They are closed after the request
        # timeout (15 s by default) instead of being kept for ever.
        silent = [tcp(PORT) for _ in range(100)]
        trickle = [tcp(PORT) for _ in range(100)]
        for sock in trickle:
            sock.sendall(f"GET /api/info HTTP/1.1\r\nHost: 127.0.0.1:{PORT}\r\nX-Slow: ".encode())
        unshaken = [tcp(LAN) for _ in range(50)]
        shaken = [tls(LAN) for _ in range(50)]
        held = stats(server.pid)["fds"]
        began = time.time()
        while time.time() - began < 30:
            time.sleep(2)
            for sock in trickle:
                try:
                    sock.sendall(b"a")
                except OSError:
                    pass
            left = sum(is_open(sock) for group in (silent, trickle, shaken) for sock in group)
            if left == 0 and stats(server.pid)["fds"] <= idle["fds"] + 1:
                break
        waited = time.time() - began
        now = stats(server.pid)
        check("300 silent, trickling and half-open sockets are closed by the server",
              now["fds"] <= idle["fds"] + 1 and held >= idle["fds"] + 250,
              f"{held} descriptors while they were held, {now['fds']} after {waited:.0f} s")
        check("and the server answered all along", info()["name"] == "splat-live")
        for group in (silent, trickle, unshaken, shaken):
            for sock in group:
                sock.close()

        # Frames no browser would send. Each gets a close code or a closed
        # connection, at once, without the server reading megabytes first.
        huge = b'{"type":"restart"}' + b" " * (1 << 20)
        cases = [
            ("a text frame of 1 MiB", [frame(1, huge)], {1009}),
            ("a header announcing 2^62 bytes", [frame(1, b"x" * 64, declared=1 << 62)], {1009}),
            ("a binary header announcing 4 GiB", [frame(2, b"x" * 64, declared=1 << 32)], {1009}),
            ("a message of 200 fragments of 1 KiB", [frame(1, b" " * 1024, fin=False)] + [frame(0, b" " * 1024, fin=False)] * 199, {1009, "eof"}),
            ("an unmasked frame", [frame(1, b'{"type":"restart"}', mask=False)], {1002, "eof"}),
            ("a reserved opcode", [frame(3, b"abc")], {1002, "eof"}),
            ("a continuation of nothing", [frame(0, b"abc")], {1002, "eof"}),
            ("text that is not UTF-8", [frame(1, b"\xff\xfe\xfd")], {1007, "eof"}),
            ("a ping of 200 bytes", [frame(9, b"p" * 200)], {1002, "eof"}),
            ("reserved bits", [bytes([0xF1, 0x80, 1, 2, 3, 4])], {1002, "eof"}),
        ]
        memory = stats(server.pid)["rss_kb"]
        for name, payloads, expected in cases:
            sock = ws(timeout=6)
            try:
                for payload in payloads:
                    sock.sendall(payload)
            except OSError:
                pass  # the server hung up in the middle of it: what it said before is still to be read
            end = how_it_ends(sock, 6)
            sock.close()
            check(f"malformed: {name}", end in expected, f"ended with {end}")
        # Floods of well-formed frames leave the connection open and the server sane.
        sock = ws(timeout=6)
        for payload in [frame(9, b"")] * 5000 + [frame(1, b'{"type":"nope"}')] * 2000 + [frame(1, b"[" * 65536)] * 20:
            sock.sendall(payload)
        check("floods of pings and of garbage are swallowed", how_it_ends(sock, 3) == "open")
        sock.close()
        time.sleep(1)
        check("none of it cost memory or a session", stats(server.pid)["rss_kb"] - memory < 4000 and info()["clients"] == 0,
              f"{memory} kB -> {stats(server.pid)['rss_kb']} kB")

        # Origin and Host dressed up to look like the server's own.
        own, good = f"127.0.0.1:{PORT}", f"http://127.0.0.1:{PORT}"
        body = b'{"enabled":false}'

        def post(host, origins):
            lines = ["POST /api/lan HTTP/1.1", f"Host: {host}"] + [f"Origin: {origin}" for origin in origins]
            lines += ["Content-Type: application/json", f"Content-Length: {len(body)}", "Connection: close"]
            return ("\r\n".join(lines) + "\r\n\r\n").encode() + body

        tricks = [
            ("another site", own, ["http://evil.example"]),
            ("Origin: null", own, ["null"]),
            ("two origins, ours first", own, [good, "http://evil.example"]),
            ("two origins, ours last", own, ["http://evil.example", good]),
            ("DNS rebinding (foreign Host, same-host origin)", f"evil.example:{PORT}", [f"http://evil.example:{PORT}"]),
            ("userinfo in the host", f"{own}@evil.example", [f"http://{own}@evil.example"]),
            ("our host as a prefix", f"127.0.0.1.evil.example:{PORT}", [f"http://127.0.0.1.evil.example:{PORT}"]),
        ]
        for name, host, origins in tricks:
            codes = (status_of(PORT, post(host, origins)), status_of(PORT, upgrade_request(host, origins)))
            check(f"origin: {name} is refused on POST and WebSocket", codes == (403, 403), f"{codes}")
        check("origin: none of them switched LAN off", info()["lan"]["enabled"])
        check("origin: the page's own origin passes", status_of(PORT, post(own, [good])) == 200 and not info()["lan"]["enabled"])
        post_lan(True)
        escapes = ["/../../etc/passwd", "/%2e%2e/%2e%2e/etc/passwd", "/..%2f..%2fetc/passwd", "/a/..%5c..%5cetc/passwd", "/%00"]
        codes = [status_of(PORT, f"GET {path} HTTP/1.1\r\nHost: {own}\r\nConnection: close\r\n\r\n".encode()) for path in escapes]
        check("paths that climb out of the web build are refused", all(code == 400 for code in codes), f"{codes}")

        # Shutdown with everything connected: viewers that read, viewers
        # that do not, silent sockets, a trickling request.
        reading = [ws() for _ in range(10)] + [ws(LAN, secure=True) for _ in range(10)]
        stuck = [ws(rcvbuf=4096) for _ in range(10)]
        silent = [tcp(PORT) for _ in range(40)] + [tcp(LAN) for _ in range(20)]
        slow = tcp(PORT)
        slow.sendall(b"GET / HTTP/1.1\r\nHost: x\r\nX: ")
        time.sleep(2)
        loaded = stats(server.pid)
        ends = []
        readers = [threading.Thread(target=lambda sock=sock: ends.append(how_it_ends(sock, 15))) for sock in reading]
        for reader in readers:
            reader.start()
        time.sleep(0.5)
        code, took = stop(server)
        for reader in readers:
            reader.join(20)
        check("shutdown under load: clean exit within the grace period", code == 0 and took < 4,
              f"exit {code} in {took:.2f} s with {info_clients_text(loaded)}")
        check("shutdown under load: every viewer that reads is told (1001)", ends.count(1001) == len(reading), f"{ends}")
        for sock in reading + stuck + silent + [slow]:
            sock.close()
        server = None
    finally:
        if server is not None:
            stop(server)


def info_clients_text(loaded):
    return f"{loaded['fds']} descriptors and {loaded['rss_kb']} kB resident"


# -- part 2: the house, byte for byte -----------------------------------------------

class Viewer:
    """Keeps what matters of the stream: layer 0 as it came off the wire."""

    def __init__(self, port):
        self.sock = ws(port, timeout=120)
        self.replace = None      # the latest layer-0 Replace, without the message header
        self.appended = 0        # Append messages since then
        self.reliable = []       # (seq, kind) of State and layer-0 messages
        self.stats = None

    def step(self):
        opcode, data = read_frame(self.sock)
        if opcode == 9:
            self.sock.sendall(frame(10, data))
        if opcode != 2:
            return
        assert data[:4] == b"GSL1"
        kind, seq = data[4], struct.unpack("<I", data[8:12])[0]
        if kind == 1:
            self.reliable.append((seq, "state"))
        elif kind == 4:
            self.stats = json.loads(data[16:])
        elif kind == 2 and data[16] == 0:
            replace = data[17] == 0
            self.reliable.append((seq, "replace" if replace else "append"))
            if replace:
                self.replace, self.appended = data[16:], 0
            else:
                self.appended += 1

    def until(self, done, limit=300):
        began = time.time()
        while not done(self):
            self.step()
            assert time.time() - began < limit, "the scan did not get there in time"

    def scan(self):
        return (self.stats or {}).get("scan") or {}


def house(tmp):
    port = PORT + 2
    server = start(["--port", str(port), "--lan-port", str(port + 1), "--scene", "house", "--cert-dir", os.path.join(tmp, "certs")],
                   os.path.join(tmp, "house.log"))
    try:
        early = Viewer(port)
        early.until(lambda v: v.scan().get("progress", 0) > 0.05)
        middle = Viewer(port)
        middle.until(lambda v: v.stats is not None)
        joined_after = middle.reliable[1][0]
        early.sock.sendall(frame(1, b'{"type":"scan_finish"}'))
        reader = threading.Thread(target=lambda: middle.until(lambda v: v.scan().get("state") == "done"))
        reader.start()
        began = time.time()
        early.until(lambda v: v.scan().get("state") == "done")
        reader.join()
        fused = time.time() - began
        began = time.time()
        late = Viewer(port)
        late.until(lambda v: v.stats is not None)
        synced = time.time() - began

        splats = struct.unpack("<I", early.replace[4:8])[0]
        digest = lambda viewer: hashlib.sha256(viewer.replace).hexdigest()[:16]
        check("house: the scan ends with layer 0 as a whole", early.appended == 0 and splats > 100000,
              f"{splats} splats, {len(early.replace) / 1e6:.2f} MB, rest of the walk fused in {fused:.1f} s")
        check("house: a late joiner holds the same layer 0, byte for byte", late.replace == early.replace,
              f"sha256 {digest(early)} / {digest(late)}, synchronised in {synced * 1000:.0f} ms")
        check("house: so does the viewer that joined in the middle", middle.replace == early.replace, digest(middle))
        for name, viewer in (("early", early), ("mid-scan", middle), ("late", late)):
            seqs = [seq for seq, _ in viewer.reliable]
            check(f"house: reliable messages of the {name} viewer are in order", seqs == sorted(set(seqs)), f"{len(seqs)} messages")
        tail = lambda viewer: [message for message in viewer.reliable if message[0] > joined_after]
        check("house: after its snapshot the mid-scan viewer got exactly what the early one got",
              tail(middle) == tail(early) and len(tail(middle)) > 5, f"{len(tail(middle))} reliable messages")
        for viewer in (early, middle, late):
            viewer.sock.close()
    finally:
        code, _ = stop(server)
        check("house: clean exit", code == 0, f"exit {code}")


def main():
    if not os.path.exists(BIN):
        print(f"SKIPPED: {BIN} does not exist (cargo build --release -p gs-server)")
        return 0
    with tempfile.TemporaryDirectory(prefix="gs-abuse-") as tmp:
        abuse(tmp)
        house(tmp)
    print("\nall checks passed" if failed == 0 else f"\n{failed} check(s) FAILED")
    return 0 if failed == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
