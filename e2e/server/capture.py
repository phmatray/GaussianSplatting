#!/usr/bin/env python3
"""Real capture, end to end, with the reference tools (standard library only).

Starts the real `gs-server` binary and plays the phone's part with
`tools/fake_phone.py`: TLS with the certificate pinned to the fingerprint of
the pairing URI, the /ingest protocol with its flow control, and uploads to
/api/captures. A WebSocket viewer watches the whole time, the way a browser
does. What the server leaves on disk is checked with the reference readers
(`tools/gscap.py`, `tools/gsrec.py`).

    cargo build --release -p gs-server
    cargo build --release -p gs-core --features par --example gscap_write   # optional: simulated phone
    python3 e2e/server/capture.py [--port 7890] [--out DIR]

Covered: pairing (URI, fingerprint, token), a synthetic walk in every mode,
captures filmed by the simulated phone of gs-core in every mode (when the
example binary exists), wrong token, wrong fingerprint, a flow control
violation, a reconnection in the middle of a capture, reset, finish, a second
phone, uploads with and without the token, a 1920x1440 capture paced at ten
frames a second while the API and the viewer are timed, and a clean stop.

Exit code 0 when every check passed.
"""

from __future__ import annotations

import argparse
import json
import os
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.request

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.join(REPO, "tools"))

import fake_phone  # noqa: E402
import gscap  # noqa: E402
import gsrec  # noqa: E402
import wsproto  # noqa: E402

FAILED = []


def check(name: str, ok: bool, detail: str = "") -> bool:
    print("%s %s%s" % ("ok  " if ok else "FAIL", name, "  (%s)" % detail if detail else ""), flush=True)
    if not ok:
        FAILED.append(name)
    return ok


def wait_for(what: str, condition, timeout: float = 30.0) -> bool:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if condition():
            return True
        time.sleep(0.02)
    print("     timed out waiting for: %s" % what, flush=True)
    return False


# ---------------------------------------------------------------- the server

class Server:
    def __init__(self, binary: str, port: int, data: str, log_path: str) -> None:
        self.port = port
        self.data = data
        self.recordings = os.path.join(data, "recordings")
        self.captures = os.path.join(data, "captures")
        self.log = open(log_path, "wb")
        args = [binary, "--port", str(port), "--lan", "--lan-port", str(port + 1), "--light",
                "--cert-dir", os.path.join(data, "cert"), "--recordings-dir", self.recordings, "--verbose"]
        self.process = subprocess.Popen(args, stdout=self.log, stderr=subprocess.STDOUT)
        self.base = "http://127.0.0.1:%d" % port
        if not wait_for("the server to answer", lambda: self.get_json("/api/info") is not None, 20.0):
            self.process.kill()
            raise RuntimeError("gs-server did not start; see %s" % log_path)

    def request(self, path: str, method: str = "GET", body=None, headers=None, timeout: float = 120.0):
        """(status, headers, body) of a request to the loopback listener."""
        request = urllib.request.Request(self.base + path, data=body, method=method, headers=headers or {})
        try:
            with urllib.request.urlopen(request, timeout=timeout) as answer:
                return answer.status, dict(answer.headers), answer.read()
        except urllib.error.HTTPError as error:
            return error.code, dict(error.headers), error.read()

    def get_json(self, path: str):
        try:
            status, _, body = self.request(path, timeout=5.0)
            return json.loads(body) if status == 200 else None
        except (OSError, ValueError):
            return None

    def post_json(self, path: str, value) -> tuple:
        status, _, body = self.request(path, "POST", json.dumps(value).encode(), {"Content-Type": "application/json"})
        try:
            return status, json.loads(body)
        except ValueError:
            return status, None

    def stop(self) -> int:
        self.process.send_signal(signal.SIGTERM)
        try:
            code = self.process.wait(timeout=20)
        except subprocess.TimeoutExpired:
            self.process.kill()
            code = -9
        self.log.close()
        return code


# ---------------------------------------------------------------- the viewer

class Viewer(threading.Thread):
    """A viewer on /ws that keeps what a real one keeps: the State, the
    size of both layers, the latest Stats."""

    def __init__(self, port: int) -> None:
        super().__init__(daemon=True)
        sock = socket.create_connection(("127.0.0.1", port), timeout=10)
        sock.settimeout(None)
        self.ws = wsproto.client_handshake(sock, "127.0.0.1:%d" % port, "/ws")
        self.lock = threading.Lock()
        self.state = {}
        self.stats = {}
        self.layers = [0, 0]
        self.history = []        # size of layer 0 after each of its messages
        self.appends = 0
        self.replaces = 0
        self.previews = 0
        self.live_frames = 0
        self.messages = 0
        self.arrivals = []       # time of every message
        self.error = None
        self.stopped = False
        self.start()

    def run(self) -> None:
        try:
            while not self.stopped:
                message = self.ws.recv(0.5)
                if message is None:
                    continue
                opcode, payload = message
                if opcode != wsproto.OP_BINARY:
                    raise ValueError("text frame from the server")
                what = gsrec.classify(payload)
                if what is None:
                    raise ValueError("a message a viewer cannot read")
                with self.lock:
                    self.messages += 1
                    self.arrivals.append(time.monotonic())
                    if what[0] == "state":
                        self.state = json.loads(payload[16:].decode("utf-8"))
                    elif what[0] == "stats":
                        self.stats = json.loads(payload[16:].decode("utf-8"))
                    elif what[0] == "preview":
                        self.previews += 1
                    else:
                        _, layer, replace = what
                        (count,) = struct.unpack_from("<I", payload, 20)
                        self.layers[layer] = count if replace else self.layers[layer] + count
                        if layer == 0:
                            self.history.append(self.layers[0])
                            if replace:
                                self.replaces += 1
                            elif count:
                                self.appends += 1
                        else:
                            self.live_frames += 1
        except wsproto.WsClosed:
            pass
        except Exception as error:  # noqa: BLE001 (reported by the main thread)
            self.error = error

    def snapshot(self) -> dict:
        with self.lock:
            return {"scene": self.state.get("scene"), "capture": self.state.get("capture") or {},
                    "stats": (self.stats.get("capture") or {}), "layers": list(self.layers),
                    "appends": self.appends, "replaces": self.replaces, "previews": self.previews,
                    "live_frames": self.live_frames, "history": list(self.history), "messages": self.messages}

    def mark(self) -> dict:
        """Counters now, to measure what a step added."""
        return self.snapshot()

    def send(self, value) -> None:
        self.ws.send_text(json.dumps(value))

    def close(self) -> None:
        self.stopped = True
        self.ws.close()
        self.join(timeout=3)
        self.ws.shutdown()


# ---------------------------------------------------------------- phones

def raw_phone(uri: str, mode: str = "room", name: str = "Essai", device: str = "iPhone17,2", token=None):
    """A hand-driven phone: TLS, WebSocket and hello; returns (ws, first server message)."""
    pairing = gscap.parse_pairing_uri(uri)
    tls = fake_phone.connect_pinned(pairing)
    ws = wsproto.client_handshake(tls, pairing.authority, "/ingest")
    ws.send_text(gscap.hello_message(pairing.token if token is None else token, mode, name, device=device))
    first = ws.recv(10.0)
    return ws, json.loads(first[1].decode("utf-8")) if first else None


def read_until_closed(ws, timeout: float = 10.0) -> tuple:
    """(messages, close code) of everything the server still says."""
    messages = []
    deadline = time.monotonic() + timeout
    try:
        while time.monotonic() < deadline:
            message = ws.recv(max(0.0, deadline - time.monotonic()))
            if message is not None:
                messages.append(json.loads(message[1].decode("utf-8")))
    except wsproto.WsClosed as closed:
        return messages, closed.code
    return messages, None


def stream(path: str, uri: str, **options):
    """`fake_phone.py stream`; returns (statistics, None) or (None, error code)."""
    try:
        return fake_phone.stream_capture(path, uri, **options), None
    except fake_phone.PhoneError as error:
        return None, error.code


def synth(path: str, mode: str, name: str, frames: int, color_width: int = 160, depth_width: int = 64) -> None:
    header = gscap.make_header(mode=mode, name=name, device="synthetic", floor={"y": -1.4})
    gscap.write_gscap(path, header, gscap.synth_frames(
        count=frames, depth_size=(depth_width, depth_width * 3 // 4), color_size=(color_width, color_width * 3 // 4)))


def files(directory: str, extension: str) -> list:
    try:
        return sorted(name for name in os.listdir(directory) if name.endswith(extension))
    except OSError:
        return []


def newest_recording(server: Server, title: str):
    listing = server.get_json("/api/recordings") or {}
    for entry in listing.get("recordings", []):
        if entry["title"] == title:
            return entry
    return None


def verify_recording(server: Server, entry: dict) -> dict:
    status, headers, body = server.request("/" + entry["url"])
    report = gsrec.verify(body) if status == 200 else {"problems": ["HTTP %d" % status], "complete": False}
    report["http_length"] = len(body)
    return report


# ---------------------------------------------------------------- the checks

def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--bin", default=os.environ.get("GS_SERVER_BIN", os.path.join(REPO, "target/release/gs-server")))
    parser.add_argument("--simulator", default=os.environ.get(
        "GS_GSCAP_WRITE", os.path.join(REPO, "target/release/examples/gscap_write")))
    parser.add_argument("--port", type=int, default=7890)
    parser.add_argument("--out", default=None, help="keep the server's files and its log here")
    args = parser.parse_args(argv)
    if not os.path.exists(args.bin):
        print("SKIPPED: %s does not exist (cargo build --release -p gs-server)" % args.bin)
        return 0

    work = args.out or tempfile.mkdtemp(prefix="gs-capture-")
    os.makedirs(work, exist_ok=True)
    for stale in ("recordings", "captures"):
        shutil.rmtree(os.path.join(work, stale), ignore_errors=True)
    files_dir = os.path.join(work, "inputs")
    os.makedirs(files_dir, exist_ok=True)
    server = Server(args.bin, args.port, work, os.path.join(work, "server.log"))
    viewer = None
    try:
        # -- pairing --------------------------------------------------------
        info = server.get_json("/api/info")
        pair = server.get_json("/api/pair")
        if not check("GET /api/pair gives a pairing URI", bool(pair and pair.get("uri")), json.dumps(pair)):
            print("     this machine has no network address a phone could use: nothing more to test")
            return 1
        uri = pair["uri"]
        pairing = gscap.parse_pairing_uri(uri)
        check("the URI names the LAN listener", "https://%s:%d" % (pairing.host, pairing.port) in info["lan"]["urls"])
        check("its fingerprint is the certificate's",
              pairing.fingerprint == info["lan"]["fingerprint"].replace(":", "").lower())
        tls = fake_phone.connect_pinned(pairing)
        tls.close()
        check("the pinned TLS connection opens", True)
        status, headers, body = server.request("/api/pair.svg")
        check("GET /api/pair.svg is an SVG", status == 200 and b"<svg" in body, headers.get("Content-Type", ""))
        lan = ssl_get(pairing, "/api/pair")
        check("the pairing is not given over the network", lan[0] == 403, "HTTP %d" % lan[0])

        viewer = Viewer(server.port)
        wait_for("the viewer's first state", lambda: viewer.snapshot()["scene"] == "stage")

        # -- a synthetic walk, streamed ------------------------------------
        walk = os.path.join(files_dir, "walk.gscap")
        synth(walk, "room", "Marche", 60)
        before = viewer.mark()
        stats, error = stream(walk, uri)
        check("fake_phone streams the synthetic walk", error is None, str(error))
        if stats:
            check("every frame is acknowledged", stats["acked"] == stats["frames_sent"] == 60, json.dumps(stats))
            check("most frames are used", stats["used"] >= 50, "%d of 60" % stats["used"])
            check("never more than max_inflight unacknowledged",
                  stats["max_unacked"] <= stats["max_inflight"] == 4, json.dumps(stats))
            check("finish is answered with a progress report",
                  stats["progress"] is not None and stats["progress"]["frames"] == 60, json.dumps(stats["progress"]))
        wait_for("the finished capture", lambda: viewer.snapshot()["stats"].get("finished") is True)
        seen = viewer.snapshot()
        check("the viewer was switched to the capture", seen["scene"] == "capture" and seen["capture"].get("name") == "Marche",
              json.dumps(seen["capture"]))
        check("the viewer saw the cloud grow", seen["appends"] - before["appends"] >= 30,
              "%d appends, %d splats" % (seen["appends"] - before["appends"], seen["layers"][0]))
        check("then the final layer", seen["replaces"] > before["replaces"] and seen["layers"][0] > 1000,
              "%d splats" % seen["layers"][0])
        check("with the phone's preview", seen["previews"] > before["previews"])
        if stats and stats["progress"]:
            check("progress counts the viewer's splats", stats["progress"]["splats"] == seen["layers"][0],
                  "%s vs %d" % (stats["progress"]["splats"], seen["layers"][0]))

        # What it left on disk, read by the reference tools.
        wait_for("the recording", lambda: newest_recording(server, "Marche") is not None)
        entry = newest_recording(server, "Marche")
        check("the capture is listed as a recording", entry is not None and entry["scene"] == "capture", json.dumps(entry))
        if entry:
            report = verify_recording(server, entry)
            check("tools/gsrec.py verifies it", gsrec.is_valid(report), json.dumps(report.get("problems")))
            check("it is a still (one keyframe)", len(report.get("keyframes", [])) == 1)
            status, headers, part = server.request("/" + entry["url"], headers={"Range": "bytes=0-1023"})
            content_range = {name.lower(): value for name, value in headers.items()}.get("content-range", "")
            check("it is served by ranges", status == 206 and len(part) == 1024
                  and content_range.startswith("bytes 0-1023/"), "HTTP %d, %s" % (status, content_range))
            command = [sys.executable, os.path.join(REPO, "tools/gsrec.py"), "verify",
                       os.path.join(server.recordings, entry["file"])]
            check("`gsrec.py verify` on the file itself", subprocess.run(command, stdout=subprocess.DEVNULL).returncode == 0)
        kept = [name for name in files(server.captures, ".gscap") if name.startswith("Marche-")]
        check("the raw stream is kept as a .gscap", len(kept) == 1, str(kept))
        if kept:
            header, frames, truncated = gscap.read_gscap(os.path.join(server.captures, kept[0]))
            check("with every frame", len(frames) == 60 and not truncated, "%d frames" % len(frames))
            check("and the floor of the phone", abs(gscap.header_floor_y(header) + 1.4) < 1e-5 and header.get("mode") == "room",
                  json.dumps(header))
            command = [sys.executable, os.path.join(REPO, "tools/gscap.py"), "verify", os.path.join(server.captures, kept[0])]
            check("`gscap.py verify` on the kept file", subprocess.run(command, stdout=subprocess.DEVNULL).returncode == 0)

        # -- every mode ------------------------------------------------------
        for mode in ("object", "person", "live"):
            path = os.path.join(files_dir, "synth-%s.gscap" % mode)
            synth(path, mode, "Synth %s" % mode, 30)
            before = viewer.mark()
            stats, error = stream(path, uri)
            ok = error is None and stats["acked"] == 30
            check("synthetic %s capture" % mode, ok, str(error or stats))
            wait_for("the %s capture to finish" % mode, lambda: viewer.snapshot()["stats"].get("finished") is True
                     and viewer.snapshot()["capture"].get("mode") == mode)
            seen = viewer.snapshot()
            if mode == "live":
                check("live: every frame is used and replaces the live layer",
                      stats is not None and stats["used"] == 30 and seen["live_frames"] - before["live_frames"] >= 15,
                      "%d live layers" % (seen["live_frames"] - before["live_frames"]))
                wait_for("the live recording", lambda: newest_recording(server, "Synth live") is not None)
                entry = newest_recording(server, "Synth live")
                report = verify_recording(server, entry) if entry else {}
                check("live: recorded as a stream", bool(entry) and gsrec.is_valid(report) and report["layers"] >= 30
                      and report["duration_s"] > 0, "%s layers, %.2f s" % (report.get("layers"), report.get("duration_s", 0)))
            else:
                check("%s: the viewer holds the capture" % mode, seen["capture"].get("mode") == mode, json.dumps(seen["capture"]))

        simulated = {}
        if os.path.exists(args.simulator):
            for scenario, frames in (("bedroom", 60), ("object", 40), ("person", 40), ("live", 45)):
                path = os.path.join(files_dir, "sim-%s.gscap" % scenario)
                if not os.path.exists(path):
                    command = [args.simulator, scenario, path, "--frames", str(frames), "--color-width", "480"]
                    subprocess.run(command, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                simulated[scenario] = path
                header = gscap.GscapReader(path).header
                before = viewer.mark()
                stats, error = stream(path, uri)
                check("simulated phone, %s: streamed" % scenario, error is None and stats["acked"] == frames, str(error or stats))
                wait_for("the end of %s" % scenario, lambda: viewer.snapshot()["stats"].get("finished") is True
                         and viewer.snapshot()["capture"].get("name") == header["name"])
                seen = viewer.snapshot()
                if scenario == "live":
                    # Not in front of the scan that came before it: these
                    # frames start again at index 0, as after a relaunch.
                    check("simulated phone, live: a relief on the live layer, and nothing behind it",
                          seen["layers"][0] == 0 and seen["layers"][1] > 3000
                          and seen["live_frames"] - before["live_frames"] >= frames // 2, json.dumps(seen["layers"]))
                else:
                    check("simulated phone, %s: used frames and a cloud" % scenario,
                          stats is not None and stats["used"] >= frames * 3 // 4 and seen["layers"][0] > 3000,
                          "%s used, %d splats" % (stats and stats["used"], seen["layers"][0]))
                entry = newest_recording(server, header["name"])
                wait_for("its recording", lambda: newest_recording(server, header["name"]) is not None)
                entry = newest_recording(server, header["name"])
                check("simulated phone, %s: saved as a valid recording" % scenario,
                      bool(entry) and gsrec.is_valid(verify_recording(server, entry)))
        else:
            print("     no %s: the simulated phone's captures are skipped" % os.path.relpath(args.simulator, REPO))
            print("     (cargo build --release -p gs-core --features par --example gscap_write)")

        # -- what must be refused ----------------------------------------------
        wrong_token = uri.replace(pairing.token, "0" * 32)
        _, error = stream(walk, wrong_token)
        check("a wrong token is refused (bad_token)", error == "bad_token", str(error))
        flipped = ("0" if pairing.fingerprint[0] != "0" else "1") + pairing.fingerprint[1:]
        _, error = stream(walk, uri.replace(pairing.fingerprint, flipped))
        check("a wrong fingerprint stops the phone before the hello (bad_fingerprint)", error == "bad_fingerprint", str(error))
        status, body = fake_phone.upload_capture(walk, uri, token="0" * 32)
        check("an upload with a wrong token is refused (401)", status == 401, "HTTP %d" % status)
        status, body = fake_phone.upload_capture(walk, uri, token="")
        check("an upload without a token is refused (401)", status == 401, "HTTP %d" % status)

        frames = list(gscap.GscapReader(walk))
        ws, welcome = raw_phone(uri, name="Flot")
        check("a hand-driven phone is welcomed", bool(welcome) and welcome.get("type") == "welcome"
              and welcome.get("max_inflight") == 4, json.dumps(welcome))
        try:
            for frame in frames[:30]:
                ws.send_binary(frame)
        except wsproto.WsClosed:
            pass
        said, code = read_until_closed(ws)
        errors = [m for m in said if m.get("type") == "error"]
        check("thirty frames at once: flow_control, then closed",
              bool(errors) and errors[0]["code"] == "flow_control" and code == 1008, "%s, close %s" % (errors, code))
        ws.shutdown()
        check("the server did not fuse the flood",
              sum(1 for m in said if m.get("type") == "ack") <= 4, "%d acks" % sum(1 for m in said if m.get("type") == "ack"))

        # -- a second phone while one is capturing -----------------------------
        ws, welcome = raw_phone(uri, name="Premier", device="iPhone17,2")
        ws.send_binary(frames[0])
        other, refusal = raw_phone(uri, name="Second", device="iPad14,3")
        check("a second phone is refused while the first captures (busy)",
              bool(refusal) and refusal.get("type") == "error" and refusal.get("code") == "busy", json.dumps(refusal))
        other.shutdown()
        status, body = fake_phone.upload_capture(walk, uri)
        check("an upload waits for the capturing phone (409)", status == 409, "HTTP %d" % status)

        # -- reset ----------------------------------------------------------------
        for frame in frames[1:4]:
            ws.send_binary(frame)
            ws.recv(10.0)
        wait_for("the first phone's frames", lambda: viewer.snapshot()["stats"].get("frames_received") == 4)
        check("the viewer follows the first phone", viewer.snapshot()["capture"].get("name") == "Premier"
              and viewer.snapshot()["layers"][0] > 0)
        ws.send_text(gscap.simple_message("reset"))
        answer = None
        for _ in range(20):
            message = ws.recv(5.0)
            if message is None:
                break
            answer = json.loads(message[1].decode("utf-8"))
            if answer.get("type") == "progress" and answer.get("frames") == 0:
                break
        check("reset is answered with an empty progress", answer == {"type": "progress", "frames": 0, "splats": 0}, json.dumps(answer))
        check("and empties the viewer's scene", wait_for("the empty layer", lambda: viewer.snapshot()["layers"][0] == 0, 10.0))
        ws.close(1000, "done")
        read_until_closed(ws, 2.0)
        ws.shutdown()

        # -- a connection lost in the middle of a capture ---------------------
        first_half = os.path.join(files_dir, "walk-1.gscap")
        second_half = os.path.join(files_dir, "walk-2.gscap")
        header = gscap.GscapReader(walk).header
        header["name"] = "Coupure"
        gscap.write_gscap(first_half, header, frames[:25])
        gscap.write_gscap(second_half, header, frames[25:])
        stats, error = stream(first_half, uri, finish=False)
        check("first half, then the connection ends without finish", error is None and stats["acked"] == 25, str(error or stats))
        wait_for("the first half", lambda: viewer.snapshot()["stats"].get("frames_received") == 25)
        half = viewer.snapshot()
        check("the capture is still on show, unfinished", half["capture"].get("name") == "Coupure"
              and half["stats"].get("finished") is False and half["layers"][0] > 0, json.dumps(half["stats"]))
        stats, error = stream(second_half, uri)
        check("the phone returns with the same hello and goes on", error is None and stats["acked"] == 35, str(error or stats))
        check("the scan continued where it was",
              stats is not None and stats["progress"] is not None and stats["progress"]["frames"] == 60, json.dumps(stats and stats["progress"]))
        wait_for("the end of the capture", lambda: viewer.snapshot()["stats"].get("finished") is True)
        whole = viewer.snapshot()
        check("nothing was thrown away on the way", 0 not in whole["history"][len(half["history"]):]
              and whole["layers"][0] > half["layers"][0] // 2, "%d then %d splats" % (half["layers"][0], whole["layers"][0]))
        kept = [name for name in files(server.captures, ".gscap") if name.startswith("Coupure-")]
        if check("one file for the whole capture", len(kept) == 1, str(kept)):
            wait_for("the complete file", lambda: len(gscap.read_gscap(os.path.join(server.captures, kept[0]))[1]) == 60, 10.0)
            check("with the frames of both connections", len(gscap.read_gscap(os.path.join(server.captures, kept[0]))[1]) == 60)

        # -- uploads -------------------------------------------------------------
        viewer.send({"type": "set_scene", "scene": "stage"})
        check("a viewer returns to the simulation", wait_for("the stage", lambda: viewer.snapshot()["scene"] == "stage", 10.0))
        status, body = fake_phone.upload_capture(walk, uri)
        ok = status == 200 and isinstance(body, dict) and body.get("ok") is True
        check("fake_phone uploads the walk", ok and body.get("frames") == 60 and body.get("truncated") is False
              and body.get("name") == "Marche", "HTTP %d %s" % (status, json.dumps(body)))
        check("the viewer is shown the import", wait_for("the import", lambda: viewer.snapshot()["scene"] == "capture"
              and viewer.snapshot()["capture"].get("imported") is True, 10.0))
        if ok and body.get("recording"):
            status, _, data = server.request("/recordings/%s.gsrec" % urllib.request.quote(body["recording"]))
            check("the import was saved as a valid recording", status == 200 and gsrec.is_valid(gsrec.verify(data)))
        if "live" in simulated:
            status, body = fake_phone.upload_capture(simulated["live"], uri)
            ok = status == 200 and isinstance(body, dict) and bool(body.get("recording"))
            if check("the simulated live capture uploads", ok, "HTTP %d" % status):
                status, _, data = server.request("/recordings/%s.gsrec" % urllib.request.quote(body["recording"]))
                report = gsrec.verify(data)
                check("and becomes a recording with the capture's own time line (45 frames at 15 per second)",
                      gsrec.is_valid(report) and abs(report["duration_s"] - 44 / 15.0) < 0.1, "%.2f s" % report["duration_s"])
        broken = os.path.join(files_dir, "broken.gscap")
        with open(broken, "wb") as out:
            out.write(open(walk, "rb").read()[:3000] + b"\xff" * 2000)
        status, body = fake_phone.upload_capture(broken, uri)
        check("a damaged file is refused (400)", status == 400 and isinstance(body, dict) and body.get("ok") is False,
              "HTTP %d %s" % (status, json.dumps(body)))

        # -- a heavy capture in real time: nothing may stall -------------------
        heavy = os.path.join(files_dir, "heavy.gscap")
        if not os.path.exists(heavy):
            synth(heavy, "room", "Lourd", 50, color_width=1920, depth_width=256)
        size = os.path.getsize(heavy) / 50 / 1e3
        latencies, stop = [], threading.Event()

        def probe() -> None:
            while not stop.is_set():
                started = time.monotonic()
                server.get_json("/api/info")
                latencies.append(time.monotonic() - started)
                time.sleep(0.05)

        prober = threading.Thread(target=probe, daemon=True)
        prober.start()
        started = time.monotonic()
        stats, error = stream(heavy, uri, realtime=True)
        elapsed = time.monotonic() - started
        stop.set()
        prober.join(timeout=5)
        check("1920x1440 colour, 256x192 depth, ten frames a second (%.0f kB per frame)" % size,
              error is None and stats["acked"] == stats["frames_sent"], str(error or stats))
        if stats:
            check("the server keeps up: few frames skipped by the phone", stats["frames_skipped"] <= 10,
                  "%d of 50 skipped, %.1f s" % (stats["frames_skipped"], elapsed))
            check("about one progress report per second", 2 <= stats["progress_count"] <= elapsed + 3,
                  "%d in %.1f s" % (stats["progress_count"], elapsed))
        latencies.sort()
        worst, median = latencies[-1], latencies[len(latencies) // 2]
        check("the API answers while frames are fused", worst < 1.0 and median < 0.1,
              "median %.0f ms, worst %.0f ms over %d requests" % (median * 1e3, worst * 1e3, len(latencies)))

        check("the viewer never received anything it could not read", viewer.error is None, str(viewer.error))
        viewer.close()
        viewer = None
    finally:
        if viewer is not None:
            viewer.close()
        code = server.stop()
    check("the server stops cleanly", code == 0, "exit code %s" % code)
    log = open(os.path.join(work, "server.log"), "r", encoding="utf-8", errors="replace").read()
    token = gscap.parse_pairing_uri(uri).token if "uri" in dir() else None
    # The banner of the terminal shows the pairing code as a QR drawing,
    # never as text; the log lines must not hold the token either.
    check("the token is not in the server's output", token is None or token not in log)
    check("no internal error in the server's log", "erreur interne" not in log and "panicked" not in log)
    for name in files(server.recordings, ".gsrec"):
        report = gsrec.verify(open(os.path.join(server.recordings, name), "rb").read())
        if not gsrec.is_valid(report):
            check("recording %s is whole after the stop" % name, False, json.dumps(report["problems"]))
    leftovers = [n for n in os.listdir(server.captures) if not n.endswith(".gscap")] if os.path.isdir(server.captures) else []
    check("no half-received file is left behind", not leftovers, str(leftovers))
    if not args.out:
        shutil.rmtree(work, ignore_errors=True)
    print("%d check(s) failed: %s" % (len(FAILED), ", ".join(FAILED)) if FAILED else "all checks passed", flush=True)
    return 1 if FAILED else 0


def ssl_get(pairing, path: str) -> tuple:
    """(status, body) of a GET on the LAN listener through the pinned connection."""
    tls = fake_phone.connect_pinned(pairing)
    try:
        tls.sendall(("GET %s HTTP/1.1\r\nHost: %s\r\nConnection: close\r\n\r\n" % (path, pairing.authority)).encode())
        status_line, headers, leftover = wsproto.read_http_head(tls, 10.0)
        length = int(headers.get("content-length", "0"))
        body = b"".join(wsproto.read_exact(tls, length, leftover, 10.0))
        return int(status_line.split(" ")[1]), body
    finally:
        tls.close()


if __name__ == "__main__":
    sys.exit(main())
