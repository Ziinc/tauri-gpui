#!/usr/bin/env bash
# Runs the chaos/integration suite (tests/chaos.rs) on a virtual X display
# with a window manager, so maximize/fullscreen/focus and xdotool input
# checks run too. Requires: Xvfb, openbox (any EWMH window manager), xdotool
# and a Vulkan driver (mesa-vulkan-drivers provides lavapipe).
#
# Usage: scripts/chaos-test.sh            # random seed (printed)
#        CHAOS_SEED=42 scripts/chaos-test.sh
#        CHAOS_ONLY=lifecycle_storm,clipboard scripts/chaos-test.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DISPLAY_NUM="${DISPLAY_NUM:-98}"

cargo build --manifest-path "$ROOT/Cargo.toml" --test chaos

Xvfb ":$DISPLAY_NUM" -screen 0 1600x1000x24 -nolisten tcp &
XVFB_PID=$!
trap 'kill $WM_PID $XVFB_PID 2>/dev/null || true' EXIT
export DISPLAY=":$DISPLAY_NUM"
for _ in $(seq 50); do xdotool getdisplaygeometry >/dev/null 2>&1 && break; sleep 0.1; done
openbox >/dev/null 2>&1 &
WM_PID=$!
sleep 1

export RUST_LOG="${RUST_LOG:-warn}"
timeout 400 cargo test --manifest-path "$ROOT/Cargo.toml" --test chaos
