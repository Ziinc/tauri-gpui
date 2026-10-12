"""Checks the embedded-webview chaos screenshots.

For each checkpoint the window's content area (right of the 180px sidebar,
between the 44px header and the 28px status bar) must be covered exactly by
the web page on the Reports screen, and show GPUI on the Overview screen.
"""

import json
import sys
from pathlib import Path

from PIL import Image

WEB = (0xEF, 0xF1, 0xF5)
GPUI_BG = (0x1E, 0x1E, 0x2E)
SIDEBAR = (0x18, 0x18, 0x25)
SIDEBAR_W, HEADER_H, STATUS_H = 180, 44, 28


def near(pixel, color, tolerance=8):
    return all(abs(a - b) <= tolerance for a, b in zip(pixel[:3], color))


def check(out, checkpoint):
    image = Image.open(out / checkpoint["file"]).convert("RGB")
    s = checkpoint["scale"]
    w, h = checkpoint["width"], checkpoint["height"]
    left, top, right, bottom = SIDEBAR_W, HEADER_H, w, h - STATUS_H
    cx, cy = (left + right) / 2, (top + bottom) / 2
    inside = {
        "top-left": (left + 3, top + 3),
        "top-right": (right - 3, top + 3),
        "bottom-left": (left + 3, bottom - 3),
        "bottom-right": (right - 3, bottom - 3),
    }
    outside = {
        "sidebar": ((left - 3, cy), SIDEBAR),
        "status bar": ((cx, bottom + 3), SIDEBAR),
        "header": ((left + 3, top - 4), GPUI_BG),
    }
    errors = []
    if image.size != (round(w * s), round(h * s)):
        errors.append(f"screenshot is {image.size}, window is {w}x{h}@{s}")
        return errors
    expected = WEB if checkpoint["reports"] else GPUI_BG
    for name, (x, y) in inside.items():
        pixel = image.getpixel((int(x * s), int(y * s)))
        if not near(pixel, expected):
            errors.append(f"content {name} is {pixel}, expected {expected}")
    for name, ((x, y), color) in outside.items():
        pixel = image.getpixel((int(x * s), int(y * s)))
        if not near(pixel, color):
            errors.append(f"{name} is {pixel}, expected {color} (webview out of bounds?)")
    return errors


def main():
    out = Path(sys.argv[1])
    checkpoints = json.loads((out / "checkpoints.json").read_text())
    failed = 0
    for checkpoint in checkpoints:
        errors = check(out, checkpoint)
        status = "FAIL" if errors else "ok"
        print(f"{status:4} {checkpoint['name']}")
        for error in errors:
            print(f"     {error}")
        failed += bool(errors)
    print(f"{len(checkpoints) - failed}/{len(checkpoints)} checkpoints passed")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
