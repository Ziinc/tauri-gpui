"""Reads examples/basic's accessible tree over AT-SPI (see a11y-test.sh).

Checks that the roles and labels the GPUI view sets reach AT-SPI, that the
button sits inside the window on screen, also after the window moves (the
window bounds the plugin reports), and that clicking it through AT-SPI updates
the view.
"""

import subprocess
import sys
import time
import warnings

import gi

gi.require_version("Atspi", "2.0")
from gi.repository import Atspi  # noqa: E402

WINDOW_TITLE = "tauri-plugin-gpui basic"
TIMEOUT = 60


def walk(node, depth=0):
    yield node, depth
    for i in range(node.get_child_count()):
        child = node.get_child_at_index(i)
        if child is not None:
            yield from walk(child, depth + 1)


def describe(node):
    return f"{node.get_role_name()} {node.get_name()!r}"


def find_window():
    desktop = Atspi.get_desktop(0)
    for i in range(desktop.get_child_count()):
        app = desktop.get_child_at_index(i)
        if app is None:
            continue
        for j in range(app.get_child_count()):
            window = app.get_child_at_index(j)
            # GTK's own (empty) window has the same title: pick the one
            # GPUI's tree hangs off.
            if (
                window is not None
                and window.get_name() == WINDOW_TITLE
                and any(n.get_name() == "Click me" for n, _ in walk(window))
            ):
                return app, window
    return None, None


def find(window, role, name):
    for node, _ in walk(window):
        if node.get_role() == role and node.get_name() == name:
            return node
    return None


def wait_for(what, check):
    deadline = time.monotonic() + TIMEOUT
    while time.monotonic() < deadline:
        try:
            result = check()
        except gi.repository.GLib.GError:
            result = None
        if result:
            return result
        time.sleep(0.25)
    dump_desktop()
    sys.exit(f"FAIL: timed out waiting for {what}")


def dump_desktop():
    desktop = Atspi.get_desktop(0)
    print("AT-SPI desktop:")
    for node, depth in walk(desktop):
        print("  " * (depth + 1) + describe(node))


def x_window():
    return subprocess.check_output(
        ["xdotool", "search", "--sync", "--name", WINDOW_TITLE], text=True
    ).split()[0]


def x_window_geometry(window_id):
    geometry = subprocess.check_output(
        ["xdotool", "getwindowgeometry", "--shell", window_id], text=True
    )
    values = dict(line.split("=", 1) for line in geometry.split())
    return tuple(int(values[key]) for key in ("X", "Y", "WIDTH", "HEIGHT"))


def inside_window(button, window_id):
    """The button's screen extents, if they lie inside the X window."""
    x, y, width, height = x_window_geometry(window_id)
    extents = button.get_extents(Atspi.CoordType.SCREEN)
    inside = (
        extents.width > 0
        and x <= extents.x
        and y <= extents.y
        and extents.x + extents.width <= x + width
        and extents.y + extents.height <= y + height
    )
    if inside:
        print(
            f"button at {extents.x},{extents.y} {extents.width}x{extents.height}; "
            f"window at {x},{y} {width}x{height}"
        )
        return extents
    return None


def main():
    app, window = wait_for(
        "the GPUI window on the AT-SPI bus",
        lambda: (lambda found: found if found[1] else None)(find_window()),
    )
    print(f"app {app.get_name()!r}, window {describe(window)}")
    for node, depth in walk(window):
        print("  " * (depth + 1) + describe(node))

    heading = find(window, Atspi.Role.HEADING, "Hello from GPUI in a Tauri window")
    count = find(window, Atspi.Role.LABEL, "Count: 0")
    button = find(window, Atspi.Role.PUSH_BUTTON, "Click me")
    for name, node in [("heading", heading), ("count label", count), ("button", button)]:
        if node is None:
            sys.exit(f"FAIL: no {name} in the tree")

    # Screen positions come from the window bounds the plugin reports, so
    # they must follow the window when it moves.
    window_id = x_window()
    before = wait_for(
        "the button's screen extents inside the window",
        lambda: inside_window(button, window_id),
    )
    subprocess.check_call(["xdotool", "windowmove", "--sync", window_id, "40", "30"])
    wait_for(
        "the button's screen extents to follow the moved window",
        lambda: (lambda e: e if e and (e.x, e.y) != (before.x, before.y) else None)(
            inside_window(button, window_id)
        ),
    )

    # Deprecated, but the replacement (`get_localized_name`) is translated.
    warnings.simplefilter("ignore", DeprecationWarning)
    actions = [button.get_action_name(i) for i in range(button.get_n_actions())]
    print(f"button actions: {actions}")
    if "click" not in actions:
        sys.exit("FAIL: the button has no click action")
    button.do_action(actions.index("click"))
    wait_for(
        "'Count: 1' after clicking through AT-SPI",
        lambda: find(window, Atspi.Role.LABEL, "Count: 1"),
    )
    print("PASS: GPUI's tree reaches AT-SPI and its button clicks")


if __name__ == "__main__":
    main()
