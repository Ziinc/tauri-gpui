#!/usr/bin/env bash
# Installs examples/ios-demo on an iPhone simulator and checks, from the app's
# console, that GPUI attaches and renders, honours the safe area, exposes its
# accessibility tree, follows dark mode, and survives a trip to the background.
# Screenshots go to the output directory.
#
# The accessibility check reads the simulator's accessibility tree with AXe
# (`brew install cameroncooke/axe/axe`); without it the check is skipped,
# except on CI.
#
#   scripts/ios-smoke.sh <path/to/app> [output-dir]
set -euo pipefail

app=${1:?usage: ios-smoke.sh <path/to/app> [output-dir]}
out=${2:-target/ios-smoke}
bundle=dev.taurigpui.iosdemo
mkdir -p "$out"
out=$(cd "$out" && pwd)
# The demo logs to stderr and to tmp/ios-demo.log in its data container;
# `sync_log` copies the latter here.
log="$out/app.log"
: >"$log"
device=
app_log=

sync_log() {
  if [ -z "$app_log" ] && [ -n "$device" ]; then
    local data
    data=$(xcrun simctl get_app_container "$device" "$bundle" data 2>/dev/null) || return 0
    app_log="$data/tmp/ios-demo.log"
  fi
  if [ -n "$app_log" ] && [ -f "$app_log" ]; then
    cp "$app_log" "$log"
  fi
}

fail() {
  echo "ios-smoke: $*" >&2
  sync_log
  echo "--- app log ---" >&2
  cat "$log" >&2
  echo "--- stderr ---" >&2
  cat "$out/stderr.log" >&2 || true
  if [ -n "$device" ]; then
    xcrun simctl io "$device" screenshot "$out/failure.png" >/dev/null 2>&1 || true
    xcrun simctl spawn "$device" log show --last 10m --style compact \
      --predicate 'process CONTAINS "gpui-ios-demo"' >"$out/system.log" 2>&1 || true
    echo "--- system log (tail) ---" >&2
    tail -n 80 "$out/system.log" >&2 || true
  fi
  cp ~/Library/Logs/DiagnosticReports/*gpui-ios-demo* "$out/" 2>/dev/null || true
  for report in "$out"/*.ips; do
    [ -f "$report" ] || continue
    echo "--- crash report $report (head) ---" >&2
    head -n 120 "$report" >&2
  done
  exit 1
}

# Waits for an extended regex in the app's log.
wait_for() {
  local pattern=$1 timeout=${2:-60}
  for ((i = 0; i < timeout; i++)); do
    sync_log
    grep -qE "$pattern" "$log" && return 0
    sleep 1
  done
  fail "timed out waiting for /$pattern/"
}

# Any iPhone simulator; current models all have a notch or Dynamic Island.
device=$(xcrun simctl list devices available -j |
  jq -r '[.devices | to_entries[] | select(.key | test("iOS")) | .value[]
          | select(.name | startswith("iPhone"))][0].udid // empty')
[ -n "$device" ] || fail "no iPhone simulator is available"
echo "ios-smoke: using simulator $device"
xcrun simctl boot "$device" 2>/dev/null || true
xcrun simctl bootstatus "$device" -b >/dev/null
xcrun simctl ui "$device" appearance light

xcrun simctl install "$device" "$app"
xcrun simctl launch --terminate-running-process \
  --stdout="$out/stdout.log" --stderr="$out/stderr.log" "$device" "$bundle"

wait_for 'ios-demo: starting'
wait_for 'ios-demo: attached'
wait_for 'ios-demo: layout top='
wait_for 'appearance dark=false'

# The header is padded by the safe area; a zero top inset means the insets
# never reached GPUI.
top=$(grep -oE 'layout top=[0-9.]+' "$log" | tail -1 | cut -d= -f2)
awk -v top="$top" 'BEGIN { exit !(top > 0) }' || fail "expected a top safe-area inset, got $top"
sleep 2
xcrun simctl io "$device" screenshot "$out/light.png" >/dev/null

# Reading the tree activates the plugin's AccessKit adapter; GPUI sends the
# real tree with its next frame, so poll until the demo's labels show up.
if command -v axe >/dev/null; then
  labelled=
  for ((i = 0; i < 30; i++)); do
    axe describe-ui --udid "$device" >"$out/a11y.json" 2>"$out/a11y.err" || true
    if grep -q '"GPUI on iOS"' "$out/a11y.json" && grep -q '"Taps: 0"' "$out/a11y.json" &&
      grep -q '"Row 1"' "$out/a11y.json"; then
      labelled=1
      break
    fi
    sleep 1
  done
  [ -n "$labelled" ] || fail "the accessibility tree lacks the demo's labels (see a11y.json)"
  echo "ios-smoke: accessibility tree has the demo's labels"
elif [ -n "${CI:-}" ]; then
  fail "axe is not installed"
else
  echo "ios-smoke: axe not found, skipping the accessibility check" >&2
fi

xcrun simctl ui "$device" appearance dark
wait_for 'appearance dark=true' 30
sleep 2
xcrun simctl io "$device" screenshot "$out/dark.png" >/dev/null

# To the background (Settings comes to the front) and back.
xcrun simctl launch "$device" com.apple.Preferences >/dev/null
wait_for 'visibility visible=false' 30
xcrun simctl launch "$device" "$bundle" >/dev/null
wait_for 'visibility visible=true' 30
sleep 2
xcrun simctl io "$device" screenshot "$out/resumed.png" >/dev/null

sync_log
if grep -qE 'PANIC|tauri-plugin-gpui: .*failed' "$log"; then
  fail "errors in the console"
fi
xcrun simctl terminate "$device" "$bundle" || true
echo "ios-smoke: passed"
