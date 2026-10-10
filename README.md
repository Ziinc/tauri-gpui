# tauri-plugin-gpui

Render [GPUI](https://www.gpui.rs) views inside ordinary Tauri windows, with no WebView.

Tauri/TAO keeps ownership of the application lifecycle, the event loop and native windows. The plugin permanently attaches GPUI as the content renderer of an existing Tauri window. Every attached window shares one GPUI `App` that runs on Tauri's event-loop thread. GPUI-backed windows and ordinary WebView windows can run side by side in the same app. See [`PRD.md`](PRD.md) for the design.

![A gpui-kit to-do app in two GPUI-backed Tauri windows, next to a WebView window](https://raw.githubusercontent.com/ziinc/tauri-gpui/main/docs/demo.png)

*Taken by `tauri-plugin-screenshots` during the automated demo run. `Todos` and `Todo summary` are Tauri windows rendered by GPUI with [gpui-kit](https://crates.io/crates/gpui-kit) components, sharing one to-do store; `WebView window` is a normal Tauri/Wry window.*

## Features

- **No WebView:** GPUI renders straight into native Tauri windows via wgpu (Vulkan/GL, Metal, DX12).
- **Tauri stays in charge:** lifecycle, event loop and windows remain Tauri/TAO's; GPUI never runs its own loop.
- **Mix and match:** GPUI-backed windows and ordinary WebView windows run side by side in one app.
- **One shared `App`:** every attached window shares a single GPUI `App`, so entities, globals and state are shared.
- **Component libraries work:** [gpui-kit](https://crates.io/crates/gpui-kit) / gpui-component via `attach_gpui_view`.
- **Full input:** mouse, wheel, keyboard with repeat, modifiers, IME commit text, focus, resize and scale-factor changes.
- **Demand-driven redraw:** an idle app uses 0% CPU.
- **Clean teardown:** destroying a Tauri window tears down its GPUI root, renderer and surface.

## Quickstart

```toml
[dependencies]
tauri = { version = "2.12", features = ["unstable"] } # `unstable` enables tauri::WindowBuilder
tauri-plugin-gpui = "0.1"
```

Requires Rust 1.88+. On Linux, install the usual [Tauri prerequisites](https://v2.tauri.app/start/prerequisites/) plus a Vulkan driver.

```rust
use tauri_plugin_gpui::{GpuiWindowExt, gpui::{self, prelude::*}};

struct Hello;

impl gpui::Render for Hello {
    fn render(&mut self, _: &mut gpui::Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        gpui::div().size_full().child("Hello from GPUI")
    }
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            tauri_plugin_gpui::init(app)?;

            let window = tauri::WindowBuilder::new(app, "main").title("Example").build()?;
            window.attach_gpui(|cx| cx.new(|_| Hello))?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
```

- `tauri_plugin_gpui::init(app)` / `init_with(app, GpuiConfig)` is called from `setup` instead of going through `Builder::plugin`. The plugin needs raw TAO events, and Tauri only exposes those through `App::wry_plugin`. `GpuiConfig` lets you set a fallback font, a GPUI `AssetSource` and an `on_launch(|cx| …)` hook for app-level GPUI setup (globals, key bindings, fonts).
- `window.attach_gpui(|cx| …)` / `attach_gpui_with(GpuiOptions, …)` is a one-way call for the life of the window. A second call returns `GpuiError::AlreadyAttached`. A window that hosts a WebView returns `GpuiError::NotEligible`. You can call it from inside GPUI code, such as a click handler that builds a new Tauri window; the mount then finishes as soon as the current GPUI update returns.
- `attach_gpui_view(GpuiOptions, |window, cx| …)` also passes the GPUI `Window` to the root builder. Component libraries need it to wrap content in their root view, for example gpui-kit's `base::Root::new(view, window, cx)`.
- `tauri_plugin_gpui::with_app(|cx| …)` gives main-thread code outside GPUI access to the shared `App`. It returns `GpuiError::Reentrant` instead of panicking when the `App` is already borrowed.
- To open more windows, use Tauri: build the window with `tauri::WindowBuilder`, then call `attach_gpui`. GPUI's `cx.open_window(…)` returns `unsupported operation: open_window …`.
- When a Tauri window is destroyed, its GPUI root, renderer and surface are destroyed with it. If GPUI removes a window itself (`window.remove_window()`), the plugin asks Tauri to destroy the native window (not `close`, so a Tauri `prevent_close` handler cannot leave a blank window behind). Closing the last GPUI window leaves the shared `App` running, and a later window can attach to it again.
- Close requests (the window manager's close button or `tauri::Window::close()`) consult GPUI's `window.on_window_should_close(…)` first; returning `false` cancels the close before Tauri sees it. Otherwise Tauri's own `CloseRequested`/`prevent_close` flow runs as usual. `destroy()` bypasses both.

## How it works

| Concern | Implementation |
|---|---|
| Event loop | A `tauri_runtime_wry::Plugin` taps TAO events on Tauri's thread. GPUI never runs a loop of its own. |
| GPUI `App` | `Application::with_platform(TauriPlatform)` plus `run_embedded`: one `App` per Tauri app, deliberately leaked so it lives until the process exits. |
| Task scheduling | A custom `PlatformDispatcher`. Main-thread runnables go into a queue that is drained from the TAO callback. Wakeups are posted through the TAO event-loop proxy. Background work runs on a worker pool and timers on their own thread. |
| Window creation | `Platform::open_window` only accepts the surface `attach_gpui` prepared. Every other call fails explicitly. |
| Rendering | `gpui_wgpu::WgpuRenderer` is built on the Tauri window's raw window and display handles (Vulkan/GL on Linux, Metal on macOS, DX12 on Windows). Text uses `gpui_wgpu::CosmicTextSystem`. |
| Redraw | Demand-driven. GPUI's `frame_waker`/`schedule_frame` set a per-window flag and wake TAO. Pending frames are delivered at `MainEventsCleared`. TAO `RedrawRequested` (expose, resize) forces a present. An idle app uses 0% CPU. |
| Input | TAO `WindowEvent` is translated to GPUI `PlatformInput` (see below). |
| Window management from GPUI | `set_title`, `activate`, `minimize`, `zoom`, `toggle_fullscreen`, `resize` and cursor styles are forwarded to the Tauri `Window` API and run outside GPUI updates. |

### Event coverage

Handled: resize, scale-factor change, move, focus, cursor enter/leave/move, mouse buttons with click counting, mouse wheel (line and pixel deltas), keyboard down/up with repeat, modifier and caps-lock state, IME commit text, theme change (including GTK's application-wide theme change, which TAO reports without a window id), close requests (GPUI can veto) and destroy (teardown).

`RedrawRequested` forces a present.

TAO has no maximize/fullscreen events, so that state is re-read from Tauri at attach and on every resize and move; GPUI's `is_maximized`, `is_fullscreen` and `window_bounds` follow changes made through Tauri or the window manager.

Keystrokes follow GPUI naming (`enter`, `left`, `f5`, lowercase characters). `key_char` comes from TAO's shift-aware logical key. When a TAO backend sends the same typed character twice (once as a key press, once as an IME commit), the duplicate is dropped.

## Minimal platform adapter: Phase 0 findings

The spike's central question was whether GPUI can render into an existing Tauri/TAO window without forking either project. The answer is yes. The plugin builds on [`gpui-pre`](https://crates.io/crates/gpui-pre) 0.3.8 and `gpui-pre-wgpu`, crates.io snapshots of zed's GPUI and the same crates gpui-kit and gpui-component use, renamed to `gpui`/`gpui_wgpu` in `Cargo.toml`. That GPUI exposes `Platform`, `PlatformWindow`, `PlatformDispatcher`, `Application::with_platform` and `Application::run_embedded`, and `gpui-pre-wgpu` exposes a renderer that accepts raw window handles. The crates.io `gpui` 0.2.x keeps `Platform` crate-private, so it cannot be used.

The pin is exact (`=0.3.8`), because every crate in a GPUI app must share one GPUI version. If you use gpui-kit, use a release built on the same `gpui-pre` (gpui-kit 0.7.1 is).

What GPUI needs for rendering and interaction:

- **Platform:** executors and dispatcher, text system, `run` (returns immediately), `open_window`, displays (the primary monitor via Tauri), `active_window`, cursor style, keyboard layout and mapper (US layout and the dummy mapper).
- **PlatformWindow:** geometry, scale factor and appearance getters; the input handler; the `on_*` callback registrations; `draw`, `sprite_atlas` and `frame_waker`/`schedule_frame`.

Everything else does one of three things:

- **System clipboard:** text and images via `arboard`, including the Linux primary selection. Images are converted to and from RGBA (pasted images arrive as PNG; SVG cannot be copied). GPUI string metadata is kept in-process and reattached while the clipboard still holds the same text.
- **Delegated to Tauri:** `quit`, `restart`, and the window title, focus, minimize, maximize, fullscreen and resize operations.
- **Explicitly unsupported:** these return `GpuiError::UnsupportedOperation` through `anyhow`, or are logged at debug level when the GPUI signature has no error channel.
  - Windows: `open_window` outside `attach_gpui`, non-normal window kinds, background appearance (blur/transparency).
  - System integration: credentials, menus and dock menus, path prompts (use `tauri-plugin-dialog`), `open_url` (use `tauri-plugin-opener`), URL schemes, `reveal_path`/`open_with_system`, app hide/unhide, idle-sleep prevention.
  - Input and accessibility: IME candidate positioning, accessibility (AccessKit).
- **Handled by GPUI's built-in fallback:** prompts (`PlatformWindow::prompt` returns `None`).

## Platform status

| Target | Status |
|---|---|
| Linux (X11) | Verified end to end by the screenshot test below (Xvfb, Mesa lavapipe Vulkan). |
| Linux (Wayland) | Builds. TAO hands out Wayland handles, but the GTK subsurface interaction has not been tested. |
| macOS, Windows | Implemented against the same cross-platform APIs but **not yet built or run**. On Windows, `tauri-runtime-wry` paints window-only windows with softbuffer, which may conflict with the DX12 swapchain. |
| Android (`mobile` feature) | Builds for `aarch64`/`x86_64`. CI drives [`examples/android-demo`](examples/android-demo) on an emulator (taps, scrolling, soft keyboard, background and resume). See [Android](#android). |
| iOS | Not implemented: `init` returns `UnsupportedOperation`. |

## Android

Enable the `mobile` feature. The Android dependencies are target-gated, so desktop builds are unaffected:

```toml
tauri-plugin-gpui = { version = "0.1", features = ["mobile"] }
```

The app code is the same as on desktop: call `init` in `setup`, build a `tauri::WindowBuilder` window (no WebView) and attach GPUI to it. Mark the entry point with `#[cfg_attr(mobile, tauri::mobile_entry_point)]` as usual.

Tauri's Android activity has no native surface, so the plugin ships a small Android library (`android/`, picked up by `tauri android build`). It installs `GpuiView`, a `SurfaceView`, as the activity's content view. GPUI renders into its surface through wgpu (Vulkan, or GLES as a fallback).

| Concern | Implementation |
|---|---|
| Threads | View callbacks run on the Android UI thread. TAO, and with it GPUI, runs on its own thread. Each callback is queued and the TAO loop is woken. `surfaceDestroyed` blocks until the renderer has released the surface. |
| Surface lifecycle | The mount waits for the first surface. When the app goes to the background the surface is unconfigured, and when it returns the surface is replaced. The device, atlas and GPUI window survive. |
| Touch | Raw `MotionEvent`s become GPUI `PlatformInput::Touch`. GPUI's gesture arena turns them into taps (clicks), pans and flings (scrolling with Android `OverScroller` physics) and long presses. |
| Keyboard | Focus on a GPUI text input shows the soft keyboard; losing focus hides it. Typed characters arrive as key presses, so key bindings still see them. Longer commits and composition go through GPUI's input handler. Hardware keys map to GPUI key names. |
| Insets | The app draws edge to edge. System bars, display cutouts and the keyboard are reported as `WindowInsets`; use `window.fully_visible_bounds()` to keep content clear of them. |
| Back | `set_back_enabled(true)` routes the back button and gesture to GPUI's back handler; otherwise back moves the app to the background. |
| Appearance, fonts, clipboard | Dark mode follows the system. Roboto, Droid Sans Mono and Noto Color Emoji are loaded from `/system/fonts`. Plain-text clipboard. |

One GPUI window per app is supported on Android.

Debug builds must embed GPUI assets, because the device cannot read the build machine's files. When using gpui-kit, enable the `debug-embed` feature of `rust-embed`.

## Example and screenshot testing

Run the demo with `cargo run -p gpui-demo`.

[`examples/gpui-demo`](examples/gpui-demo) is the PRD's sample application: a to-do list built with [gpui-kit](https://crates.io/crates/gpui-kit) components (Input, Checkbox, Button, Progress, light/dark theme). It contains:

- a GPUI-backed `main` window with the to-do list: add, check off, delete and filter, plus a theme toggle;
- a `summary` window that a GPUI click handler creates, closes and recreates. It shows live stats from the same store;
- an ordinary WebView window.

```sh
# needs: Xvfb, openbox (or another EWMH WM), xdotool, a Vulkan driver (mesa-vulkan-drivers)
examples/gpui-demo/scripts/screenshot-test.sh [output-dir]
```

With `GPUI_DEMO_AUTOTEST=<dir>` set, the demo:

1. drives real X11 input with `xdotool` (X11 → GTK → TAO → plugin → GPUI);
2. asserts on GPUI state for each PRD success criterion;
3. captures every window with [`tauri-plugin-screenshots`](https://crates.io/crates/tauri-plugin-screenshots);
4. writes `report.json` and exits non-zero if any check fails.

## Chaos and integration tests

[`tests/chaos.rs`](tests/chaos.rs) runs a real Tauri (Wry) event loop with `tauri-plugin-clipboard-manager` installed and hammers the plugin from a driver thread, asserting on what GPUI and Tauri observe:

- API contract: `NotInitialized`, `AlreadyInitialized`, `NotMainThread`, `AlreadyAttached`, `NotEligible` (WebView windows), `Reentrant`, and `open_window` rejection;
- randomized create/attach/close/destroy/`remove_window` storms, attach-then-teardown in one turn, 10 concurrent windows torn down at once, label reuse; every GPUI root must be released;
- resize storms, hide/show cycles, and window state in both directions (title, maximize, fullscreen, focus, minimize, position, theme);
- close handling (Tauri `prevent_close`, `destroy`) and `cx.quit()` routed through Tauri's `ExitRequested`;
- Tauri events, managed state, `on_window_event`, `async_runtime` and `run_on_main_thread` used together with GPUI; GPUI timers, background tasks and floods of 20k foreground tasks;
- the clipboard shared with `tauri-plugin-clipboard-manager`;
- 600 cross-thread calls while windows churn, and random real X11 keyboard/mouse input via `xdotool` (typed text must arrive exactly once).

```sh
# needs: Xvfb, openbox, xdotool, a Vulkan driver; prints its seed
scripts/chaos-test.sh
CHAOS_SEED=42 CHAOS_ONLY=lifecycle_storm,clipboard scripts/chaos-test.sh
```

Without a display, `cargo test` skips the suite.

## Remaining open questions

- GPUI versioning: the crate tracks the `gpui-pre` snapshots, the same ones gpui-kit tracks, until upstream publishes a `gpui` with a public `Platform`.
- IME positioning: it has no Tauri core API and would need a TAO change.
- Accessibility (AccessKit through Tauri windows).
- Building and testing on macOS and Windows.
- iOS.
- Android: IME composition beyond committed text, multiple GPUI windows, and accessibility.
