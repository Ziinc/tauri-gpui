"""Checks the embedded-webview chaos screenshots.

For each checkpoint the window's content area (right of the 180px sidebar,
between the 44px header and the 28px status bar) must be covered exactly by
the web page on the Reports screen, and show GPUI on the Overview screen.
With a menu, GPUI (and so all of the above) starts below a visible GTK menu
bar; without one, at the top of the window. `menu-open` checkpoints
must show the View menu's popup over GPUI.
"""

import json
import sys
from pathlib import Path

from PIL import Image

WEB = (0xEF, 0xF1, 0xF5)
GPUI_BG = (0x1E, 0x1E, 0x2E)
SIDEBAR = (0x18, 0x18, 0x25)
SIDEBAR_W, HEADER_H, STATUS_H = 180, 44, 28
MENU_MIN, MENU_MAX = 10, 45


def near(pixel, color, tolerance=8):
    return all(abs(a - b) <= tolerance for a, b in zip(pixel[:3], color))


def gpui_top(image, s):
    """Logical y where GPUI's sidebar starts: the menu bar's height."""
    for row in range(image.height):
        # Right of the nav items and of the View menu popup.
        if near(image.getpixel((int(175 * s), row)), SIDEBAR):
            return row / s
    return None


def check_menu_bar(image, s, w, expected):
    top = gpui_top(image, s)
    if top is None:
        return None, ["no GPUI sidebar found"]
    if not expected:
        return top, [] if top == 0 else [f"GPUI starts {top}px down, expected 0 (no menu)"]
    if not MENU_MIN <= top <= MENU_MAX:
        return top, [f"GPUI starts {top}px down, expected a menu bar above it"]
    errors = []
    for x in (5, w / 2, w - 5):
        pixel = image.getpixel((int(x * s), int(top / 2 * s)))
        # GTK's light menu bar is close to the page's color: the header
        # check below catches a webview placed too high.
        if any(near(pixel, color) for color in (GPUI_BG, SIDEBAR)):
            errors.append(f"menu bar at x={x} is {pixel}: covered by GPUI")
    return top, errors


def check_menu_open(image, checkpoint):
    window = image
    top, errors = check_menu_bar(window, 1, 100, True)
    if errors:
        return errors
    pixel = window.getpixel((30, int(top + 12)))
    if any(near(pixel, color) for color in (GPUI_BG, SIDEBAR)):
        errors.append(f"no menu popup below the menu bar: {pixel}")
    return errors


def check(out, checkpoint):
    image = Image.open(out / checkpoint["file"]).convert("RGB")
    if checkpoint.get("kind") == "menu-open":
        return check_menu_open(image, checkpoint)
    s = checkpoint["scale"]
    w, h = checkpoint["width"], checkpoint["height"]
    if image.size != (round(w * s), round(h * s)):
        return [f"screenshot is {image.size}, window is {w}x{h}@{s}"]
    menu, errors = check_menu_bar(image, s, w, checkpoint["menu"])
    if menu is None:
        return errors
    left, top, right, bottom = SIDEBAR_W, menu + HEADER_H, w, h - STATUS_H
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
