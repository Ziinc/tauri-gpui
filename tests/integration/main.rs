//! Integration and chaos tests against a real Tauri (Wry) event loop with
//! `tauri-plugin-clipboard-manager` installed.
//!
//! One file per domain; shared helpers live in `support/`. Tauri's event
//! loop must own the main thread and exists once per process, so this is a
//! custom harness (`harness = false`) running every test sequentially
//! against one app. Randomized tests draw from `CHAOS_SEED` (printed).
//!
//! Needs a display, plus a window manager for focus/maximize/fullscreen and
//! xdotool for input; see `scripts/integration-test.sh`. Without a display
//! the suite skips. Arguments filter tests by name:
//! `cargo test --test integration -- lifecycle:: clipboard::gpui_reads`.

mod api;
mod clipboard;
mod close;
mod input;
mod lifecycle;
mod support;
mod tasks;
mod tauri_apis;
mod window_state;

fn main() {
    support::main(&[
        api::TESTS,
        lifecycle::TESTS,
        window_state::TESTS,
        close::TESTS,
        tauri_apis::TESTS,
        tasks::TESTS,
        clipboard::TESTS,
        input::TESTS,
    ]);
}
