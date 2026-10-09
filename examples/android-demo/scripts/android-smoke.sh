#!/usr/bin/env bash
# Installs the android-demo APK on a running emulator/device, drives it with
# `adb shell input` and asserts on the demo's logcat lines. Screenshots and
# the full logcat go to the output directory.
#
#   examples/android-demo/scripts/android-smoke.sh <app.apk> [output-dir]
set -euo pipefail

APK=$1
OUT=${2:-target/android-smoke}
PKG=dev.taurigpui.androiddemo
mkdir -p "$OUT"
# Bound every adb call so a wedged device fails the run instead of hanging it.
ADB_BIN=$(command -v adb)
adb() { timeout 90 "$ADB_BIN" "$@"; }

timeout 300 "$ADB_BIN" wait-for-device
adb install -r -g "$APK"
adb logcat -c
# Keep a live capture so the logs survive an emulator crash.
"$ADB_BIN" logcat > "$OUT/logcat-stream.txt" 2>&1 &
LOGCAT_PID=$!
trap 'kill "$LOGCAT_PID" 2>/dev/null || true' EXIT
adb shell am start -n "$PKG/.MainActivity"

failures=0
log() { adb logcat -d -s android-demo:I RustStdoutStderr:I tauri-plugin-gpui:D '*:E' > "$OUT/logcat.txt" || true; }
shot() { adb exec-out screencap -p > "$OUT/$1.png"; }
# wait_for <regex> <timeout seconds>
wait_for() {
  local deadline=$((SECONDS + $2))
  while (( SECONDS < deadline )); do
    if adb logcat -d -s android-demo:I | grep -Eq "$1"; then return 0; fi
    sleep 1
  done
  echo "FAIL: no log line matching /$1/ within $2 s"
  failures=$((failures + 1))
  return 1
}

wait_for "layout top=" 120 || { log; shot 00-no-layout || true; adb logcat -d > "$OUT/logcat-full.txt" || true; exit 1; }
sleep 2
shot 01-launched

density=$(adb shell wm density | awk '/Physical/ {print $3}' | tr -d '\r')
scale=$(awk "BEGIN {print $density / 160}")
layout=$(adb logcat -d -s android-demo:I | grep "layout top=" | tail -1)
field() { sed -E "s/.*$1=([0-9.]+).*/\1/" <<<"$layout"; }
to_px() { awk "BEGIN {printf \"%d\", $1 * $scale}"; }
width=$(field width)
x=$(to_px "$(awk "BEGIN {print $width / 2}")")
button_y=$(to_px "$(field button_y)")
input_y=$(to_px "$(field input_y)")
echo "density=$density button=($x,$button_y) input=($x,$input_y)"

# Taps.
adb shell input tap "$x" "$button_y"
wait_for "tapped count=1" 20 || true
adb shell input tap "$x" "$button_y"
wait_for "tapped count=2" 20 || true
shot 02-tapped

# Drag scrolling and fling.
screen_h=$(adb shell wm size | awk -F'x' '/Physical/ {print $2}' | tr -d '\r')
adb shell input swipe "$x" $((screen_h * 85 / 100)) "$x" $((screen_h * 45 / 100)) 300
wait_for "scrolled offset=" 20 || true
adb shell input swipe "$x" $((screen_h * 85 / 100)) "$x" $((screen_h * 50 / 100)) 60
sleep 2
shot 03-scrolled

# Soft keyboard and text input.
adb shell input tap "$x" "$input_y"
sleep 2
shot 04-keyboard
adb shell input text "hello"
adb shell input keyevent 66
wait_for 'submitted text="hello"' 20 || true
sleep 1
shot 05-submitted

# Background and foreground: the surface is destroyed and recreated.
adb shell input keyevent 3
sleep 2
adb shell am start -n "$PKG/.MainActivity"
sleep 3
shot 06-resumed
if ! adb shell pidof "$PKG" > /dev/null; then
  echo "FAIL: app is not running after returning to the foreground"
  failures=$((failures + 1))
fi
adb shell input tap "$x" "$button_y"
wait_for "tapped count=3" 20 || true

log
adb logcat -d > "$OUT/logcat-full.txt"
if grep -E "FATAL EXCEPTION|panicked at" "$OUT/logcat-full.txt"; then
  echo "FAIL: crash in logcat"
  failures=$((failures + 1))
fi
echo "android smoke test: $failures failure(s)"
exit $(( failures > 0 ))
