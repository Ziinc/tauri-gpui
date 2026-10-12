#!/usr/bin/env bash
# Runs the embedded-webview stress scenario (`src/chaos.rs`) on a virtual X
# display and checks every screenshot (`check.py`). Requires: Xvfb, openbox,
# xdotool, ImageMagick (`import`), Python 3 with Pillow and a Vulkan driver.
#
# Usage: examples/embedded-webview/scripts/chaos.sh [output-dir]
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
OUT="$(realpath -m "${1:-$ROOT/target/embedded-webview-chaos}")"
DISPLAY_NUM="${DISPLAY_NUM:-93}"

rm -rf "$OUT" && mkdir -p "$OUT"
cargo build --manifest-path "$ROOT/Cargo.toml" -p embedded-webview

Xvfb ":$DISPLAY_NUM" -screen 0 1600x1000x24 -nolisten tcp 2>/dev/null &
XVFB_PID=$!
trap 'kill $WM_PID $XVFB_PID 2>/dev/null || true' EXIT
export DISPLAY=":$DISPLAY_NUM"
for _ in $(seq 50); do xdotool getdisplaygeometry >/dev/null 2>&1 && break; sleep 0.1; done
openbox >/dev/null 2>&1 &
WM_PID=$!
sleep 1

# WebKitGTK cannot use GPU compositing on a virtual display.
export WEBKIT_DISABLE_COMPOSITING_MODE=1
export DBUS_SESSION_BUS_ADDRESS=disabled:
export RUST_LOG="${RUST_LOG:-warn}"
set +e
EMBEDDED_WEBVIEW_CHAOS="$OUT" timeout 300 "$ROOT/target/debug/embedded-webview" 2>"$OUT/app.log"
STATUS=$?
set -e
if [ $STATUS -ne 0 ]; then
  echo "embedded-webview exited with $STATUS (see $OUT/app.log)"
  exit 1
fi
if grep -q "X Window System error" "$OUT/app.log"; then
  echo "X11 error in $OUT/app.log"
  exit 1
fi
python3 "$(dirname "$0")/check.py" "$OUT"
