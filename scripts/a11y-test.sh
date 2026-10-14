#!/usr/bin/env bash
# Checks that GPUI's AccessKit tree reaches AT-SPI on Linux: runs
# examples/basic on a virtual X display inside a private session D-Bus with
# the AT-SPI bus, then reads the window's accessible tree and clicks its
# button through AT-SPI (`a11y_check.py`). Requires: Xvfb, openbox, xdotool,
# dbus (dbus-run-session), at-spi2-core, python3-pyatspi and a Vulkan driver.
#
# Usage: scripts/a11y-test.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TARGET_DIR="$(cargo metadata --manifest-path "$ROOT/Cargo.toml" --format-version 1 --no-deps \
  | python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"])')"

if [ "${A11Y_TEST_IN_SESSION:-}" != 1 ]; then
  cargo build --manifest-path "$ROOT/Cargo.toml" -p basic
  # Only the AT-SPI bus is activatable on the private bus. TAO asks the XDG
  # desktop portal for the theme over the session bus and blocks for seconds
  # when a portal is installed but never answers (see chaos-test.sh); with
  # no portal service the lookup fails at once and TAO falls back to GTK.
  CONF_DIR="$(mktemp -d)"
  trap 'rm -rf "$CONF_DIR"' EXIT
  mkdir "$CONF_DIR/services"
  cp /usr/share/dbus-1/services/org.a11y.Bus.service "$CONF_DIR/services/"
  cat >"$CONF_DIR/session.conf" <<CONF
<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:tmpdir=/tmp</listen>
  <auth>EXTERNAL</auth>
  <servicedir>$CONF_DIR/services</servicedir>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
CONF
  # The AT-SPI bus launcher keeps `IsEnabled` in GSettings; without a
  # writable dconf it reverts the setting, so keep it in memory.
  export GSETTINGS_BACKEND=memory
  A11Y_TEST_IN_SESSION=1 dbus-run-session --config-file="$CONF_DIR/session.conf" -- "$0" "$@"
  exit
fi

DISPLAY_NUM="${DISPLAY_NUM:-97}"
Xvfb ":$DISPLAY_NUM" -screen 0 1600x1000x24 -nolisten tcp 2>/dev/null &
XVFB_PID=$!
APP_PID=
trap 'kill $APP_PID $WM_PID $XVFB_PID 2>/dev/null || true' EXIT
export DISPLAY=":$DISPLAY_NUM"
for _ in $(seq 50); do xdotool getdisplaygeometry >/dev/null 2>&1 && break; sleep 0.1; done
openbox >/dev/null 2>&1 &
WM_PID=$!
sleep 1

# What a screen reader starting up does: AccessKit only joins the AT-SPI bus
# once `IsEnabled` is set.
busctl --user call org.a11y.Bus /org/a11y/bus org.a11y.Bus GetAddress >/dev/null
busctl --user set-property org.a11y.Bus /org/a11y/bus org.a11y.Status IsEnabled b true

export RUST_LOG="${RUST_LOG:-warn}"
LOG="$TARGET_DIR/a11y-test.log"
"$TARGET_DIR/debug/basic" 2>"$LOG" &
APP_PID=$!

# PyGObject is built for the distribution's Python, which need not be the
# first `python3` on PATH.
PYTHON=
for candidate in python3 /usr/bin/python3 /usr/bin/python3.*[0-9]; do
  if "$candidate" -c 'import gi' 2>/dev/null; then PYTHON="$candidate"; break; fi
done
if [ -z "$PYTHON" ]; then
  echo "no Python with PyGObject (python3-gi) found"
  exit 1
fi

set +e
timeout 120 "$PYTHON" "$(dirname "$0")/a11y_check.py"
STATUS=$?
set -e
if [ $STATUS -ne 0 ]; then
  echo "a11y check failed (app log: $LOG)"
fi
exit $STATUS
