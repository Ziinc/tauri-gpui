#!/usr/bin/env bash
# Installs examples/ios-demo on an iPhone simulator and checks, from the app's
# console, that GPUI attaches and renders, honours the safe area, follows dark
# mode, survives a trip to the background, and that the keyboard inset
# animates. Screenshots go to the output directory.
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
# A connected hardware keyboard would keep the software keyboard hidden.
defaults write com.apple.iphonesimulator ConnectHardwareKeyboard -bool false || true
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

# Relaunched with the input focused, the software keyboard slides in. The
# bottom inset must pass through intermediate values on its way from the
# safe area to the keyboard's height, rather than jump.
xcrun simctl launch --terminate-running-process \
  --stdout="$out/stdout.log" --stderr="$out/stderr.log" "$device" "$bundle" --focus-input
wait_for 'ios-demo: focusing input'
sleep 3
sync_log
bottoms=$(sed -n '/ios-demo: focusing input/,$p' "$log" | grep -oE 'layout bottom=[0-9.]+' | cut -d= -f2)
echo "ios-smoke: bottom insets:" $bottoms
echo "$bottoms" | awk '
  NR == 1 { first = $1 }
  { values[NR] = $1; last = $1 }
  END {
    if (last < first + 100) { print "the keyboard inset never grew"; exit 1 }
    for (i = 2; i < NR; i++) if (values[i] > first && values[i] < last) between++
    if (between < 3) { print "the keyboard inset jumped (" between " intermediate values)"; exit 1 }
  }' || fail "expected an animated keyboard inset"
xcrun simctl io "$device" screenshot "$out/keyboard.png" >/dev/null

sync_log
if grep -qE 'PANIC|tauri-plugin-gpui: .*failed' "$log"; then
  fail "errors in the console"
fi
xcrun simctl terminate "$device" "$bundle" || true
echo "ios-smoke: passed"
