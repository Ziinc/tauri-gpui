#!/usr/bin/env bash
# Runs the chaos/integration suite (tests/chaos.rs) on a virtual X display
# with a window manager, so maximize/fullscreen/focus and xdotool input
# checks run too. Requires: Xvfb, openbox (any EWMH window manager), xdotool
# and a Vulkan driver (mesa-vulkan-drivers provides lavapipe).
#
# With --wayland it runs on a headless Weston compositor with GTK's Wayland
# backend instead (requires: weston). Weston's headless backend has no seat,
# so the window manager, xdotool and clipboard checks skip there.
#
# Usage: scripts/chaos-test.sh            # random seed (printed)
#        scripts/chaos-test.sh --wayland
#        CHAOS_SEED=42 scripts/chaos-test.sh
#        CHAOS_ONLY=lifecycle_storm,clipboard scripts/chaos-test.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DISPLAY_NUM="${DISPLAY_NUM:-98}"

cargo build --manifest-path "$ROOT/Cargo.toml" --features test-support --test chaos

if [[ "${1:-}" == "--wayland" ]]; then
  # A fresh compositor each run: one that saw a client fail can stop
  # sending frame callbacks to later ones.
  XDG_RUNTIME_DIR="$(mktemp -d)"
  export XDG_RUNTIME_DIR
  SOCKET=wayland-chaos
  weston --backend=headless --socket="$SOCKET" --width=1600 --height=1000 \
    --idle-time=0 >"$XDG_RUNTIME_DIR/weston.log" 2>&1 &
  WESTON_PID=$!
  trap 'status=$?; kill $WESTON_PID 2>/dev/null || true
        [[ $status -eq 0 ]] || cat "$XDG_RUNTIME_DIR/weston.log" >&2' EXIT
  for _ in $(seq 50); do [[ -S "$XDG_RUNTIME_DIR/$SOCKET" ]] && break; sleep 0.1; done
  export WAYLAND_DISPLAY="$SOCKET" GDK_BACKEND=wayland
  unset DISPLAY
else
  Xvfb ":$DISPLAY_NUM" -screen 0 1600x1000x24 -nolisten tcp &
  XVFB_PID=$!
  trap 'kill $WM_PID $XVFB_PID 2>/dev/null || true' EXIT
  export DISPLAY=":$DISPLAY_NUM"
  for _ in $(seq 50); do xdotool getdisplaygeometry >/dev/null 2>&1 && break; sleep 0.1; done
  openbox >/dev/null 2>&1 &
  WM_PID=$!
  sleep 1
fi

export RUST_LOG="${RUST_LOG:-warn}"
# TAO reads the theme from the XDG desktop portal over the session D-Bus,
# blocking up to 5s per window (25s on first use while the bus tries to
# activate it). CI runners have a session bus whose portal never answers, so
# run without one: the lookup then fails immediately and TAO falls back to GTK.
export DBUS_SESSION_BUS_ADDRESS=disabled:
# The suite's own watchdog (CHAOS_TIMEOUT_SECS, default 600) fires first.
timeout $(( ${CHAOS_TIMEOUT_SECS:-600} + 60 )) cargo test --manifest-path "$ROOT/Cargo.toml" --features test-support --test chaos
