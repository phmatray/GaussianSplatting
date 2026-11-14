#!/usr/bin/env bash
# Smoke test of the Tauri shell without the real frontend.
#
# Builds src-tauri with the stand-in page of e2e/server/standin as its
# frontend, runs the binary under Xvfb and checks, from the log and from a
# screenshot, that the window shows the page, that the `server_info`
# command answers and that WebSocket frames arrive.
#
#   e2e/server/tauri-smoke.sh [--release] [output-dir]
#
# Needs xvfb-run and ImageMagick (`import`). Exit status 0 means success.
set -u
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
PROFILE=debug
CARGO_FLAGS=()
if [ "${1:-}" = "--release" ]; then PROFILE=release; CARGO_FLAGS=(--release); shift; fi
OUT=${1:-$ROOT/target/tauri-smoke}
STANDIN=$ROOT/e2e/server/standin
mkdir -p "$OUT"

echo "== build ($PROFILE, stand-in frontend)"
( cd "$ROOT" && TAURI_CONFIG="{\"build\":{\"frontendDist\":\"$STANDIN\"}}" GS_DIST_DIR="$STANDIN" \
    cargo build -p splat-live --features custom-protocol "${CARGO_FLAGS[@]}" ) || exit 1
BIN=$ROOT/target/$PROFILE/splat-live
LOG=$OUT/tauri-$PROFILE.log
PNG=$OUT/tauri-$PROFILE.png

echo "== run under Xvfb"
export BIN LOG PNG
# The application data (certificate) goes to a scratch home.
export XDG_DATA_HOME=$OUT/data SPLAT_LIVE_VERBOSE=1
xvfb-run -a -s "-screen 0 1280x800x24" bash -c '
  "$BIN" > "$LOG" 2>&1 &
  PID=$!
  # Wait until the page reports frames through its logged request.
  for i in $(seq 1 80); do
    grep -q "report=.*%22frames%22%3A[1-9][0-9]" "$LOG" 2>/dev/null && break
    kill -0 $PID 2>/dev/null || { echo "the application exited early"; break; }
    sleep 0.5
  done
  sleep 2
  import -window root "$PNG" 2>/dev/null || echo "screenshot failed"
  # A termination signal must end the application cleanly.
  kill -TERM $PID 2>/dev/null
  for i in $(seq 1 20); do kill -0 $PID 2>/dev/null || break; sleep 0.25; done
  if kill -0 $PID 2>/dev/null; then echo "the application ignored SIGTERM"; kill -KILL $PID; fi
  wait $PID 2>/dev/null; echo "exit status: $?"
'

echo "== checks"
FAIL=0
check() { if grep -Eq "$2" "$LOG"; then echo "ok   $1"; else echo "FAIL $1"; FAIL=1; fi; }
REPORT=$(grep -o "report=[^ ]*" "$LOG" | tail -1 | python3 -c "import sys, urllib.parse; print(urllib.parse.unquote(sys.stdin.read().strip()[7:]))")
echo "last report from the page: $REPORT"
check "server started in-process"                 "server ready on http://127.0.0.1:[0-9]+"
check "page loaded from the bundled frontend"     "report=.*origin%22%3A%22(tauri%3A%2F%2Flocalhost|http%3A%2F%2Ftauri.localhost)"
check "server_info command answered"              "report=.*%22command%22%3Atrue"
check "GET /api/info from the webview (CORS)"     "report=.*%22fetch%22%3Atrue"
check "WebSocket accepted from the webview"       "GET /ws \(origin (tauri://localhost|http://tauri.localhost)\) -> 101"
check "frames arrive"                             "report=.*%22frames%22%3A[1-9][0-9]"
check "frames are codec messages"                 "report=.*%22magic%22%3A%22GSL1%22"
check "POST /api/lan from the webview"            "POST /api/lan \(origin [a-z:/.]+\) -> 200"
check "QR image shown"                            "report=.*%22qr%22%3A%22[0-9]+x[0-9]+%22"
check "WebAssembly allowed by the CSP"            "report=.*%22wasm%22%3Atrue"
check "no error reported by the page"             "report=.*%22errors%22%3A%5B%5D"
check "clean shutdown on SIGTERM"                 "server stopped"
[ -s "$PNG" ] && echo "screenshot: $PNG" || { echo "FAIL no screenshot"; FAIL=1; }
echo "log: $LOG"
exit $FAIL
