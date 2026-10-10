#!/usr/bin/env bash
# Runs the gpui-demo autotest on a virtual X display and collects screenshots
# taken by tauri-plugin-screenshots. Requires: Xvfb, openbox (any EWMH window
# manager; xcap lists windows via _NET_CLIENT_LIST_STACKING), xdotool and a
# Vulkan driver (mesa-vulkan-drivers provides the lavapipe software renderer).
#
# Usage: examples/gpui-demo/scripts/screenshot-test.sh [output-dir]
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../../.." && pwd)"
OUT="$(realpath -m "${1:-$ROOT/target/gpui-demo-screenshots}")"
DISPLAY_NUM="${DISPLAY_NUM:-99}"

rm -rf "$OUT" && mkdir -p "$OUT"
cargo build --manifest-path "$ROOT/examples/gpui-demo/Cargo.toml"

Xvfb ":$DISPLAY_NUM" -screen 0 1600x1000x24 -nolisten tcp &
XVFB_PID=$!
trap 'kill $WM_PID $XVFB_PID 2>/dev/null || true' EXIT
export DISPLAY=":$DISPLAY_NUM"
for _ in $(seq 50); do xdotool getdisplaygeometry >/dev/null 2>&1 && break; sleep 0.1; done
openbox >/dev/null 2>&1 &
WM_PID=$!
sleep 1

# WebKitGTK cannot use GPU compositing on a virtual display.
export WEBKIT_DISABLE_COMPOSITING_MODE=1
export RUST_LOG="${RUST_LOG:-warn}"
GPUI_DEMO_AUTOTEST="$OUT" timeout 180 "$ROOT/examples/gpui-demo/target/debug/gpui-demo"
STATUS=$?
echo "screenshots and report.json written to $OUT"
exit $STATUS
