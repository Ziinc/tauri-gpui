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
# System ANR dialogs ("Pixel Launcher isn't responding") cover the app and eat
# `input` events, so close them before interacting.
dismiss_dialogs() { adb shell am broadcast -a android.intent.action.CLOSE_SYSTEM_DIALOGS > /dev/null || true; }

timeout 300 "$ADB_BIN" wait-for-device
adb shell settings put global hide_error_dialogs 1 || true
adb shell am broadcast -a android.intent.action.CLOSE_SYSTEM_DIALOGS > /dev/null || true
adb logcat -c
adb install -r -g "$APK"
# Let the install settle before the first launch. The system's post-install
# handling (PACKAGE_ADDED broadcast, `onPackageAdded` in the launcher) relaunches
# an activity that is already running, and tauri-plugin-gpui does not yet
# re-attach to a recreated activity, so the screen would go blank. Both waits
# are best-effort: `wait-for-broadcast-idle` needs API 30+, the logcat wait is
# capped at 20 s.
adb shell am wait-for-broadcast-idle || sleep 5
for _ in $(seq 20); do
  if adb logcat -d | grep "onPackageAdded: $PKG" > /dev/null; then break; fi
  sleep 1
done
# Keep a live capture so the logs survive an emulator crash.
"$ADB_BIN" logcat > "$OUT/logcat-stream.txt" 2>&1 &
LOGCAT_PID=$!
trap 'kill "$LOGCAT_PID" 2>/dev/null || true' EXIT
adb shell am start -n "$PKG/.MainActivity"

failures=0
# wait_for_logcat <regex> <timeout seconds>: like wait_for, over every tag.
wait_for_logcat() {
  local deadline=$((SECONDS + $2))
  while (( SECONDS < deadline )); do
    if adb logcat -d | grep -Eq "$1"; then return 0; fi
    sleep 1
  done
  echo "FAIL: no logcat line matching /$1/ within $2 s"
  failures=$((failures + 1))
  return 1
}
log() { adb logcat -d -s android-demo:I RustStdoutStderr:I tauri-plugin-gpui:D '*:E' > "$OUT/logcat.txt" || true; }
shot() { adb exec-out screencap -p > "$OUT/$1.png"; }
# ui_has <text> <timeout seconds>: whether a UI node (any window, so also the
# floating selection toolbar) shows this text before the timeout.
ui_has() {
  local deadline=$((SECONDS + $2))
  while (( SECONDS < deadline )); do
    adb shell rm -f /sdcard/ui.xml > /dev/null 2>&1 || true
    rm -f "$OUT/ui-dump.xml"
    # `--windows` (newer Android) includes popup windows; older ones only have the plain dump.
    adb shell uiautomator dump --windows /sdcard/ui.xml > /dev/null 2>&1 \
      || adb shell uiautomator dump /sdcard/ui.xml > /dev/null 2>&1 || true
    adb exec-out cat /sdcard/ui.xml > "$OUT/ui-dump.xml" 2>/dev/null || true
    if grep -Eq "(text|content-desc)=\"$1\"" "$OUT/ui-dump.xml"; then return 0; fi
    sleep 1
  done
  return 1
}
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
dismiss_dialogs
adb shell input tap "$x" "$button_y"
wait_for "tapped count=1" 20 || true
adb shell input tap "$x" "$button_y"
wait_for "tapped count=2" 20 || true
shot 02-tapped

# Drag scrolling and fling.
dismiss_dialogs
screen_h=$(adb shell wm size | awk -F'x' '/Physical/ {print $2}' | tr -d '\r')
adb shell input swipe "$x" $((screen_h * 85 / 100)) "$x" $((screen_h * 45 / 100)) 300
wait_for "scrolled offset=" 20 || true
adb shell input swipe "$x" $((screen_h * 85 / 100)) "$x" $((screen_h * 50 / 100)) 60
sleep 2
shot 03-scrolled

# Soft keyboard and text input.
dismiss_dialogs
adb shell input tap "$x" "$input_y"
sleep 2
shot 04-keyboard
adb shell input text "hello"
adb shell input keyevent 66
wait_for 'submitted text="hello"' 20 || true
sleep 1
shot 05-submitted

# ui_center <text> prints the centre "x y" of the UI node with this text or
# content-desc in the last dump (see `ui_has`).
ui_center() {
  sed -nE "s/.*(text|content-desc)=\"$1\"[^>]*bounds=\"\[([0-9]+),([0-9]+)\]\[([0-9]+),([0-9]+)\]\".*/\2 \3 \4 \5/p" "$OUT/ui-dump.xml" \
    | head -1 | awk '{printf "%d %d", ($1 + $3) / 2, ($2 + $4) / 2}'
}

# The demo clears its input on submit: type a word, then long press the
# (focused) input. Android's floating selection toolbar appears, and so does
# the native insertion handle under the caret. "Select all" is always offered;
# Paste and Autofill depend on the clipboard and the autofill service, so they
# are not asserted.
dismiss_dialogs
adb shell input text "world"
sleep 1
adb shell input swipe "$x" "$input_y" "$x" "$input_y" 1200
wait_for_logcat "selection toolbar shown" 20 || true
sleep 1
shot 05b-selection-toolbar
if ui_has "Select all" 15; then
  cp "$OUT/ui-dump.xml" "$OUT/ui-dump-toolbar.xml"
else
  echo "FAIL: the selection toolbar does not show \"Select all\" after a long press"
  failures=$((failures + 1))
fi
wait_for_logcat "selection handles shown" 20 || true

# Selecting everything puts the native handles on both ends of the text. The
# end handle is then dragged left: gpui-kit's input cannot place the selection
# from outside yet, so this only checks that the drag is harmless.
if ui_has "Select all" 5; then
  read -r select_x select_y <<<"$(ui_center "Select all")"
  adb shell input tap "$select_x" "$select_y" || true
  wait_for_logcat "selection range handles shown" 20 || true
  sleep 1
  shot 05d-selection-handles
  if ui_has "Selection end" 10; then
    read -r handle_x handle_y <<<"$(ui_center "Selection end")"
    adb shell input swipe "$handle_x" "$handle_y" $((handle_x - density / 8)) "$handle_y" 400 || true
    sleep 1
    shot 05e-handle-dragged
  else
    echo "NOTE: the end selection handle is not in the UI dump"
  fi
fi
# Touching elsewhere dismisses the toolbar.
adb shell input tap "$x" $((screen_h * 7 / 10))
sleep 1
if ui_has "Select all" 3; then
  echo "FAIL: the selection toolbar is still open after tapping elsewhere"
  failures=$((failures + 1))
fi
shot 05c-toolbar-dismissed

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
dismiss_dialogs
adb shell input tap "$x" "$button_y"
wait_for "tapped count=3" 20 || true

log
adb logcat -d > "$OUT/logcat-full.txt"
if grep -E "FATAL EXCEPTION|panicked at|attaching GPUI failed|loading Android plugins failed" "$OUT/logcat-full.txt"; then
  echo "FAIL: crash in logcat"
  failures=$((failures + 1))
fi
if grep -Eq "finishDrawing of relaunch: .*$PKG" "$OUT/logcat-full.txt"; then
  echo "NOTE: the activity was relaunched"
fi
# Tauri skips Plugin.load without a WebView; GpuiView loads the plugins itself.
if ! grep -Eq "tauri-plugin-gpui.*loaded Android plugins" "$OUT/logcat-full.txt"; then
  echo "FAIL: the Android plugins were not loaded"
  failures=$((failures + 1))
fi
echo "android smoke test: $failures failure(s)"
exit $(( failures > 0 ))
