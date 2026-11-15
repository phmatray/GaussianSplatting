#!/usr/bin/env python3
"""Offline refinement and dataset export, end to end, against the real
`gs-server` binary (standard library only).

A small synthetic capture (`tools/gscap.py synth`: a walk in a box room) is
uploaded like a `.gscap` recorded on the phone; the server keeps it, and the
kept capture is then refined with the `rapide` preset cut down to a few
dozen iterations (`--refine-iterations`), cancelled once, exported as COLMAP
and nerfstudio datasets, and its result is listed, downloaded by ranges,
packed into a share archive and deleted. A WebSocket viewer is timed the
whole while: the server must stay responsive during a job.

    cargo build --release -p gs-server
    python3 e2e/server/refine.py [--port 7960] [--out DIR]

Exit code 0 when every check passed.
"""

from __future__ import annotations

import argparse
import io
import json
import os
import shutil
import signal
import ssl
import struct
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
import zipfile

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
sys.path.insert(0, os.path.join(REPO, "tools"))

import gscap  # noqa: E402

FAILED = []
ITERATIONS = 40


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
        time.sleep(0.05)
    print("     timed out waiting for: %s" % what, flush=True)
    return False


class Server:
    def __init__(self, binary: str, port: int, data: str, dist: str, log_path: str) -> None:
        self.port = port
        self.data = data
        self.log = open(log_path, "wb")
        args = [binary, "--port", str(port), "--lan", "--lan-port", str(port + 1), "--light", "--dist", dist,
                "--cert-dir", os.path.join(data, "cert"), "--recordings-dir", os.path.join(data, "recordings"),
                "--refine-iterations", str(ITERATIONS), "--refine-threads", "2", "--verbose"]
        self.process = subprocess.Popen(args, stdout=self.log, stderr=subprocess.STDOUT)
        self.base = "http://127.0.0.1:%d" % port
        if not wait_for("the server to answer", lambda: self.get("/api/info")[0] == 200, 20.0):
            self.process.kill()
            raise RuntimeError("gs-server did not start; see %s" % log_path)

    def request(self, path: str, method: str = "GET", body=None, headers=None, timeout: float = 120.0, base=None):
        """(status, headers, body) of a request, to the loopback listener unless `base` says otherwise."""
        request = urllib.request.Request((base or self.base) + path, data=body, method=method, headers=headers or {})
        context = ssl._create_unverified_context() if base and base.startswith("https") else None
        # Header names in lower case: HTTP/1.1 and HTTP/2 servers differ.
        lower = lambda headers: {name.lower(): value for name, value in headers.items()}  # noqa: E731
        try:
            with urllib.request.urlopen(request, timeout=timeout, context=context) as answer:
                return answer.status, lower(answer.headers), answer.read()
        except urllib.error.HTTPError as error:
            return error.code, lower(error.headers), error.read()
        except OSError:
            return 0, {}, b""

    def get(self, path: str, **options):
        status, headers, body = self.request(path, **options)
        try:
            return status, json.loads(body)
        except ValueError:
            return status, None

    def post(self, path: str, value, **options) -> tuple:
        status, _, body = self.request(path, "POST", json.dumps(value).encode(), {"Content-Type": "application/json"}, **options)
        try:
            return status, json.loads(body)
        except ValueError:
            return status, None

    def job(self):
        return (self.get("/api/refine")[1] or {}).get("job")

    def last(self):
        return (self.get("/api/refine")[1] or {}).get("last")

    def wait_for_the_end(self, timeout: float = 240.0):
        """Polls until no job runs; returns (last, statuses seen, worst /api/info time in ms)."""
        seen, worst = [], 0.0
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            start = time.monotonic()
            self.get("/api/info")
            worst = max(worst, (time.monotonic() - start) * 1000.0)
            job = self.job()
            if job is None:
                return self.last(), seen, worst
            seen.append(job)
            time.sleep(0.1)
        return None, seen, worst

    def stop(self) -> int:
        self.process.send_signal(signal.SIGTERM)
        try:
            code = self.process.wait(timeout=30)
        except subprocess.TimeoutExpired:
            self.process.kill()
            code = -9
        self.log.close()
        return code


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--port", type=int, default=7960)
    parser.add_argument("--out", default=None, help="where the server log and the files stay (default: a temporary directory, removed)")
    parser.add_argument("--binary", default=os.path.join(REPO, "target", "release", "gs-server"))
    args = parser.parse_args()
    if not os.path.exists(args.binary):
        print("SKIPPED: %s does not exist (cargo build --release -p gs-server)" % args.binary)
        return 0
    out = args.out or tempfile.mkdtemp(prefix="splat-live-refine-")
    os.makedirs(out, exist_ok=True)
    data = os.path.join(out, "data")
    shutil.rmtree(data, ignore_errors=True)
    # A stand-in web build: the share archive needs one, the checks do not.
    dist = os.path.join(out, "dist")
    os.makedirs(dist, exist_ok=True)
    with open(os.path.join(dist, "index.html"), "w", encoding="utf-8") as f:
        f.write("<!doctype html><title>Splat Live</title>")

    # A walk in a box room, as the phone would have recorded it offline.
    walk = os.path.join(out, "walk.gscap")
    header = gscap.make_header(mode="room", name="Salle d'essai", device="synthetic", floor={"y": -1.4})
    gscap.write_gscap(walk, header, gscap.synth_frames(count=48, depth_size=(96, 72), color_size=(320, 240), grain=24))
    live = os.path.join(out, "live.gscap")
    gscap.write_gscap(live, gscap.make_header(mode="live", name="Direct", device="synthetic"),
                      gscap.synth_frames(count=4, depth_size=(32, 24), color_size=(64, 48)))

    server = Server(args.binary, args.port, data, dist, os.path.join(out, "server.log"))
    code = 1
    try:
        lan = "https://127.0.0.1:%d" % (args.port + 1)
        status, state = server.get("/api/refine")
        check("no job at start", status == 200 and state == {"job": None, "last": None}, json.dumps(state))
        status, listed = server.get("/api/refined")
        check("no result at start, and the directories are told at the machine", status == 200 and listed["results"] == []
              and listed["dir"] == os.path.join(data, "refined") and listed["datasets_dir"] == os.path.join(data, "datasets"), json.dumps(listed))

        # ---- the capture reaches the server and is kept
        with open(walk, "rb") as f:
            status, _, body = server.request("/api/captures", "POST", f.read(), {"Content-Type": "application/octet-stream"})
        answer = json.loads(body)
        check("the uploaded capture is ingested and kept", status == 200 and answer["ok"] and answer["frames"] == 48 and answer["file"], json.dumps(answer))
        kept = answer["file"]
        stem = kept[: -len(".gscap")]
        with open(live, "rb") as f:
            status, _, body = server.request("/api/captures", "POST", f.read(), {"Content-Type": "application/octet-stream"})
        live_kept = json.loads(body).get("file")
        status, captures = server.get("/api/captures")
        check("both are listed among the kept captures", status == 200 and {c["file"] for c in captures["captures"]} == {kept, live_kept}
              and isinstance(captures.get("free_bytes"), int), json.dumps([c["file"] for c in captures["captures"]]))
        status, record = server.get("/api/record")
        check("the state of the disk is told with the recorder's", status == 200 and record["storage"]["problem"] is None
              and isinstance(record["storage"]["free_bytes"], int) and record["storage"]["low"] == (record["storage"]["free_bytes"] < 2 << 30), json.dumps(record.get("storage")))

        # ---- refusals
        status, body = server.post("/api/refine", {"action": "start", "capture": "nothing.gscap"})
        check("no such capture", (status, body["code"]) == (404, "not_found"))
        status, body = server.post("/api/refine", {"action": "start", "capture": kept, "preset": "ultra"})
        check("unknown preset", (status, body["code"]) == (400, "bad_request"))
        status, body = server.post("/api/refine", {"action": "start", "capture": live_kept})
        check("a live capture cannot be refined", (status, body["code"]) == (400, "unsupported_mode"), json.dumps(body))
        status, body = server.post("/api/refine", {"action": "cancel"})
        check("nothing to cancel", (status, body["code"]) == (409, "no_job"))
        status, body = server.post("/api/refine", {"action": "start", "capture": kept}, base=lan)
        check("the LAN listener does not start jobs", (status, (body or {}).get("code")) == (403, "loopback_only"), "%s %s" % (status, body))
        status, _, body = server.request("/api/refine", "POST", json.dumps({"action": "start", "capture": kept}).encode(),
                                         {"Content-Type": "application/json", "Origin": "https://evil.example"})
        check("a page of another origin does not start jobs", status == 403 and json.loads(body)["code"] == "origin_not_allowed")
        check("a refused request started nothing", server.job() is None)

        # ---- a job cancelled while it optimises
        status, body = server.post("/api/refine", {"action": "start", "capture": kept, "preset": "fin"})
        check("a refinement starts", status == 200 and body["job"]["kind"] == "refine" and body["job"]["preset"] == "fin"
              and body["job"]["threads"] == 2, json.dumps(body))
        status, body = server.post("/api/refine", {"action": "export", "capture": kept, "format": "colmap"})
        check("one job at a time", (status, body["code"], body["job"]["id"]) == (409, "job_running", 1), json.dumps(body))
        wait_for("the optimisation", lambda: (server.job() or {}).get("stage") == "optimise" or server.job() is None, 120.0)
        asked = time.monotonic()
        status, body = server.post("/api/refine", {"action": "cancel"})
        last, _, _ = server.wait_for_the_end(60.0)
        took = time.monotonic() - asked
        check("a cancelled job stops within seconds", status == 200 and last is not None and last["outcome"] in ("cancelled", "done") and took < 30.0,
              "%.1f s, %s" % (took, json.dumps(last)[:200]))
        if last and last["outcome"] == "cancelled":
            check("a cancelled job keeps nothing", last["result"] is None and os.listdir(os.path.join(data, "refined")) == [],
                  ",".join(os.listdir(os.path.join(data, "refined"))))
        else:
            # The machine was fast enough to finish 40 iterations first.
            for name in os.listdir(os.path.join(data, "refined")):
                os.remove(os.path.join(data, "refined", name))

        # ---- the job to its end
        started = time.monotonic()
        status, body = server.post("/api/refine", {"action": "start", "capture": kept, "preset": "rapide", "beyond_sensor": False})
        check("the rapide preset starts", status == 200 and body["job"]["preset"] == "rapide", json.dumps(body))
        last, seen, worst = server.wait_for_the_end()
        took = time.monotonic() - started
        check("the job ends with a result", last is not None and last["outcome"] == "done" and last["error"] is None
              and last["iterations_done"] == ITERATIONS, json.dumps(last)[:300])
        stages = [job["stage"] for job in seen]
        check("its progress went through the optimisation", "optimise" in stages and all(job["iterations"] in (0, ITERATIONS) for job in seen)
              and all(a["iteration"] <= b["iteration"] for a, b in zip(seen, seen[1:])), ",".join(sorted(set(stages))))
        check("the server answered /api/info within 250 ms during the job", worst < 250.0, "worst %.0f ms over %d polls, job of %.1f s" % (worst, len(seen), took))
        result = last["result"]
        name = "%s-rapide" % stem
        check("the result is named after its capture and preset", result["file"] == name + ".ply" and result["capture"] == kept
              and result["preset"] == "rapide" and result["mode"] == "room" and result["title"] == "Salle d'essai"
              and result["beyond_sensor"] is False and result["gaussians"] > 500, json.dumps(result))
        status, listed = server.get("/api/refined")
        check("it is listed", status == 200 and listed["results"] == [result])
        status, listed = server.get("/api/refined", base=lan)
        check("it is listed on the LAN too, without the directory", status == 200 and listed["results"][0]["file"] == result["file"] and "dir" not in listed)

        # ---- served whole and by ranges
        status, headers, ply = server.request("/refined/%s.ply" % name)
        check("the .ply is served", status == 200 and len(ply) == result["size"] and ply.startswith(b"ply\n")
              and headers.get("accept-ranges") == "bytes", "%d bytes" % len(ply))
        vertex_line = [line for line in ply[:2000].split(b"\n") if line.startswith(b"element vertex")]
        check("it holds the Gaussians the listing counts", vertex_line == [b"element vertex %d" % result["gaussians"]], repr(vertex_line))
        status, headers, part = server.request("/refined/%s.ply" % name, headers={"Range": "bytes=100-199"})
        check("a range of it is served", status == 206 and part == ply[100:200] and headers.get("content-range") == "bytes 100-199/%d" % len(ply))
        status, headers, part = server.request("/refined/%s.ply" % name, headers={"Range": "bytes=-32"}, base=lan)
        check("and over the LAN listener", status == 206 and part == ply[-32:])
        with open(os.path.join(data, "refined", name + ".json"), "r", encoding="utf-8") as f:
            described = json.load(f)
        check("its description holds the optimiser's report", described["capture"] == kept and described["report"]["completed"] is True
              and described["report"]["config"]["train"]["photo_beyond_sensor"] is False and described["report"]["iterations_done"] == ITERATIONS)

        # ---- shared like a recording
        status, headers, archive = server.request("/api/share/refined/%s.zip" % name)
        names = zipfile.ZipFile(io.BytesIO(archive)).namelist() if status == 200 else []
        check("the share archive holds the web build and the result", status == 200 and set(names) == {"index.html", "share.json", "scene.ply"}, ",".join(names))
        if status == 200:
            unpacked = zipfile.ZipFile(io.BytesIO(archive))
            manifest = json.loads(unpacked.read("share.json"))
            check("the archive passes its checksums and names the result", unpacked.testzip() is None and unpacked.read("scene.ply") == ply
                  and manifest["file"] == "scene.ply" and manifest["name"] == "Salle d'essai", json.dumps(manifest))
            check("a refined room is shared for a walk, from where the phone started", manifest.get("nav") == "fly"
                  and manifest.get("view") == result["view"] and len(result["view"]) == 6, json.dumps(manifest))
        status, _, _ = server.request("/api/share/refined/nothing.zip")
        check("no archive of what does not exist", status == 404)

        # ---- datasets for other trainers
        for format_name, landmark in (("colmap", os.path.join("sparse", "0", "images.bin")), ("nerfstudio", "transforms.json")):
            status, body = server.post("/api/refine", {"action": "export", "capture": kept, "format": format_name})
            last, _, _ = server.wait_for_the_end(120.0)
            dataset = (last or {}).get("dataset") or {}
            directory = dataset.get("dir", "")
            check("the capture is exported as a %s dataset" % format_name, status == 200 and last is not None and last["outcome"] == "done"
                  and directory == os.path.join(data, "datasets", "%s-%s" % (stem, format_name)) and os.path.isfile(os.path.join(directory, landmark))
                  and len(os.listdir(os.path.join(directory, "images"))) == dataset["images"] > 10, json.dumps(dataset))
        with open(os.path.join(data, "datasets", stem + "-nerfstudio", "transforms.json"), "r", encoding="utf-8") as f:
            transforms = json.load(f)
        check("the nerfstudio dataset lists its frames", len(transforms["frames"]) > 10 and "transform_matrix" in transforms["frames"][0])
        with open(os.path.join(data, "datasets", stem + "-colmap", "sparse", "0", "cameras.bin"), "rb") as f:
            cameras = struct.unpack("<Q", f.read(8))[0]
        check("the COLMAP dataset has one camera per image", cameras == len(os.listdir(os.path.join(data, "datasets", stem + "-colmap", "images"))))

        # ---- deleting
        status, _, body = server.request("/refined/%s.ply" % name, "DELETE", base=lan)
        check("the LAN listener does not delete", status == 403)
        status, _, body = server.request("/refined/%s.ply" % name, "DELETE")
        check("the result is deleted with its description", status == 200 and json.loads(body) == {"ok": True}
              and os.listdir(os.path.join(data, "refined")) == [], ",".join(os.listdir(os.path.join(data, "refined"))))
        status, _, _ = server.request("/refined/%s.ply" % name)
        check("and is gone", status == 404)

        # ---- a server that stops takes its job with it
        server.post("/api/refine", {"action": "start", "capture": kept, "preset": "standard"})
        time.sleep(1.0)
        asked = time.monotonic()
        code = server.stop()
        check("the server stops cleanly with a job running", code == 0 and time.monotonic() - asked < 20.0, "exit %s in %.1f s" % (code, time.monotonic() - asked))
        check("no half-written result is left", [n for n in os.listdir(os.path.join(data, "refined")) if not n.startswith(".")] == [],
              ",".join(os.listdir(os.path.join(data, "refined"))))
    finally:
        if server.process.poll() is None:
            server.stop()
        if args.out is None:
            shutil.rmtree(out, ignore_errors=True)
    print("\n%d checks failed" % len(FAILED) if FAILED else "\nall checks passed")
    for name in FAILED:
        print("FAILED: %s" % name)
    return 1 if FAILED else 0


if __name__ == "__main__":
    sys.exit(main())
