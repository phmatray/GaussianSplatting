#!/usr/bin/env bash
# The desktop application with the REAL interface, under Xvfb.
#
# Builds src-tauri against a copy of the web build (crates/gs-app/dist, or
# the directory given with GS_DIST) with the `probe` feature, runs the
# binary under Xvfb and checks, from the log and from a screenshot, that the
# Leptos interface is shown in the window, that it found the embedded
# server through the `server_info` command ("Serveur natif"), and that the
# stream arrives over the WebSocket and is drawn with WebGL2.
#
#   e2e/server/tauri-real.sh [--debug] [output-dir]
#
# Needs xvfb-run and ImageMagick (`import`). Exit status 0 means success;
# 77 means skipped (no web build to embed).
set -u
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
PROFILE=release
CARGO_FLAGS=(--release)
if [ "${1:-}" = "--debug" ]; then PROFILE=debug; CARGO_FLAGS=(); shift; fi
OUT=${1:-$ROOT/target/tauri-real}
SRC=${GS_DIST:-$ROOT/crates/gs-app/dist}
mkdir -p "$OUT"
OUT=$(cd "$OUT" && pwd)

if [ ! -f "$SRC/index.html" ]; then
  echo "SKIPPED: $SRC has no index.html (cd crates/gs-app && trunk build --release)"
  exit 77
fi
# A copy: Trunk empties the original while it rebuilds.
DIST=$OUT/dist
rm -rf "$DIST" && cp -r "$SRC" "$DIST" && rm -f "$DIST/.trunk-lock"

echo "== build ($PROFILE, real frontend from $SRC, probe)"
( cd "$ROOT" && TAURI_CONFIG="{\"build\":{\"frontendDist\":\"$DIST\"}}" GS_DIST_DIR="$DIST" \
    cargo build -p splat-live --features custom-protocol,probe "${CARGO_FLAGS[@]}" ) || exit 1
# Kept aside: another build of the package would replace the binary.
BIN=$OUT/splat-live-$PROFILE
cp "$ROOT/target/$PROFILE/splat-live" "$BIN" || exit 1
LOG=$OUT/tauri-$PROFILE.log
PNG=$OUT/tauri-$PROFILE.png

echo "== run under Xvfb"
export BIN LOG PNG
# The application data (certificate) goes to a scratch home.
export XDG_DATA_HOME=$OUT/data SPLAT_LIVE_VERBOSE=1
xvfb-run -a -s "-screen 0 1280x800x24" bash -c '
  "$BIN" > "$LOG" 2>&1 &
  PID=$!
  # Wait until the interface reports rendered frames of a connected stream.
  for i in $(seq 1 180); do
    grep -Eq "probe: .*\"link\":\"connected\".*\"frames\":[0-9]{2,}" "$LOG" 2>/dev/null && break
    kill -0 $PID 2>/dev/null || { echo "the application exited early"; break; }
    sleep 0.5
  done
  sleep 4
  import -window root "$PNG" 2>/dev/null || echo "screenshot failed"
  # A termination signal must end the application cleanly.
  kill -TERM $PID 2>/dev/null
  for i in $(seq 1 40); do kill -0 $PID 2>/dev/null || break; sleep 0.25; done
  if kill -0 $PID 2>/dev/null; then echo "the application ignored SIGTERM"; kill -KILL $PID; fi
  wait $PID 2>/dev/null; echo "exit status: $?"
'

echo "== checks"
FAIL=0
check() { if grep -Eq "$2" "$LOG"; then echo "ok   $1"; else echo "FAIL $1"; FAIL=1; fi; }
FIRST=$(grep -o "probe: .*" "$LOG" | grep '"link":"connected"' | head -1 | cut -c8-)
LAST=$(grep -o "probe: .*" "$LOG" | tail -1 | cut -c8-)
echo "last report from the window: $LAST"
check "server started in-process"                    "server ready on http://127.0.0.1:[0-9]+"
check "window loaded the bundled interface"          "probe: .*\"origin\":\"(tauri://localhost|http://tauri.localhost)\".*\"title\":\"Splat Live\""
check "the interface started (window.__splatLive)"   "probe: .*\"hook\":true"
check "server_info command was called"               "server_info command called by the window"
check "source is the native server"                  "probe: .*\"source\":\"server\""
check "badge says « Serveur natif »"                 "probe: .*\"badge\":\"Serveur natif\""
check "GET /api/info from the window (CORS)"         "GET /api/info \(origin (tauri://localhost|http://tauri.localhost)\) -> 200"
check "WebSocket accepted from the window"           "GET /ws \(origin (tauri://localhost|http://tauri.localhost)\) -> 101"
check "stream connected"                             "probe: .*\"link\":\"connected\""
check "WebGL2 available in the webview"              "probe: .*\"webgl2\":\"WebGL 2"
check "both layers hold splats"                      "probe: .*\"layers\":\{\"dynamic\":[1-9][0-9]+,\"static\":[1-9][0-9]+\}"
check "frames were rendered with splats drawn"       "probe: .*\"viewer\":\{\"frames\":[0-9]{2,},\"drawn\":[1-9][0-9]+"
check "no error reported by the interface"           "probe: .*\"errors\":\[\]"
check "clean shutdown on SIGTERM"                    "server stopped"
# The stream must keep coming and keep being drawn between two reports.
python3 - "$FIRST" "$LAST" <<'PY' || FAIL=1
import json, sys
try:
    first, last = json.loads(sys.argv[1]), json.loads(sys.argv[2])
    d_layers = last["counts"]["layers"] - first["counts"]["layers"]
    d_frames = last["viewer"]["frames"] - first["viewer"]["frames"]
    ok = d_layers >= 5 and d_frames >= 5 and last["link"] == "connected"
    print(f"{'ok  ' if ok else 'FAIL'} stream and rendering advance between the first and the last report "
          f"(+{d_layers} layers, +{d_frames} frames, {last['viewer']['fps']:.1f} frames/s)")
    sys.exit(0 if ok else 1)
except Exception as error:  # no usable report at all
    print(f"FAIL stream and rendering advance between the first and the last report ({error})")
    sys.exit(1)
PY
[ -s "$PNG" ] && echo "screenshot: $PNG" || { echo "FAIL no screenshot"; FAIL=1; }
echo "log: $LOG"
exit $FAIL
