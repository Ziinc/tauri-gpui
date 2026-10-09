#!/usr/bin/env bash
# Runs the integration/chaos suite (tests/integration) on a virtual X display
# with a window manager, so maximize/fullscreen/focus and xdotool input
# checks run too. Requires: Xvfb, openbox (any EWMH window manager), xdotool
# and a Vulkan driver (mesa-vulkan-drivers provides lavapipe).
#
# Usage: scripts/integration-test.sh                   # random seed (printed)
#        CHAOS_SEED=42 scripts/integration-test.sh
#        scripts/integration-test.sh lifecycle:: clipboard  # filter by name
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DISPLAY_NUM="${DISPLAY_NUM:-98}"

cargo build --manifest-path "$ROOT/Cargo.toml" --test integration

Xvfb ":$DISPLAY_NUM" -screen 0 1600x1000x24 -nolisten tcp &
XVFB_PID=$!
trap 'kill $WM_PID $XVFB_PID 2>/dev/null || true' EXIT
export DISPLAY=":$DISPLAY_NUM"
for _ in $(seq 50); do xdotool getdisplaygeometry >/dev/null 2>&1 && break; sleep 0.1; done
openbox >/dev/null 2>&1 &
WM_PID=$!
sleep 1

export RUST_LOG="${RUST_LOG:-warn}"
# TAO reads the theme from the XDG desktop portal over the session D-Bus,
# blocking up to 5s per window (25s on first use while the bus tries to
# activate it). CI runners have a session bus whose portal never answers, so
# run without one: the lookup then fails immediately and TAO falls back to GTK.
export DBUS_SESSION_BUS_ADDRESS=disabled:
# The suite's own watchdog (CHAOS_TIMEOUT_SECS, default 600) fires first.
timeout $(( ${CHAOS_TIMEOUT_SECS:-600} + 60 )) cargo test --manifest-path "$ROOT/Cargo.toml" --test integration -- "$@"
