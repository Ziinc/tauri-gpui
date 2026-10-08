//! Chaos / integration tests against a real Tauri (Wry) event loop.
//!
//! Runs as a custom harness (`harness = false`) because Tauri's event loop
//! must own the main thread and can only be created once per process. A
//! driver thread hammers the plugin through public Tauri and GPUI APIs —
//! randomized window lifecycles, resize storms, cross-thread calls, real X11
//! input via `xdotool`, Tauri events/state/close handling and the
//! `tauri-plugin-clipboard-manager` plugin — and asserts on what GPUI and
//! Tauri observe.
//!
//! Needs a display (and a window manager for the maximize/fullscreen/focus
//! checks), e.g. `xvfb-run` + `openbox`. Without `DISPLAY`/`WAYLAND_DISPLAY`
//! it skips. `CHAOS_SEED=<n>` reproduces a run; the seed is always printed.

use std::{
    collections::HashMap,
    ops::Range,
    process::Command,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use tauri::{AppHandle, Emitter, Listener, Manager, WebviewUrl, WebviewWindowBuilder, Wry};
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_gpui::{
    GpuiConfig, GpuiError, GpuiOptions, GpuiWindowExt,
    gpui::{
        self, App, AppContext as _, Bounds, ClipboardItem, Context, ElementInputHandler, Entity,
        EntityInputHandler, FocusHandle, Global, InteractiveElement as _, IntoElement,
        KeyDownEvent, MouseButton, ParentElement as _, Pixels, Render, Styled as _, UTF16Selection,
        WeakEntity, Window, WindowAppearance, canvas, div,
    },
};

// ---------------------------------------------------------------------------
// GPUI probe view
// ---------------------------------------------------------------------------

/// Root view mounted in every test window. Records what reaches GPUI.
struct Probe {
    focus: FocusHandle,
    renders: usize,
    viewport: (f32, f32),
    maximized: bool,
    fullscreen: bool,
    active: bool,
    appearance: Option<WindowAppearance>,
    mouse_downs: usize,
    mouse_ups: usize,
    scrolls: usize,
    keys: Vec<String>,
    text: String,
}

impl Probe {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        Self {
            focus,
            renders: 0,
            viewport: (0., 0.),
            maximized: false,
            fullscreen: false,
            active: false,
            appearance: None,
            mouse_downs: 0,
            mouse_ups: 0,
            scrolls: 0,
            keys: Vec::new(),
            text: String::new(),
        }
    }
}

impl Render for Probe {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.renders += 1;
        let viewport = window.viewport_size();
        self.viewport = (f32::from(viewport.width), f32::from(viewport.height));
        self.maximized = window.is_maximized();
        self.fullscreen = window.is_fullscreen();
        self.active = window.is_window_active();
        self.appearance = Some(window.appearance());
        let entity = cx.entity();
        let focus = self.focus.clone();
        div()
            .id("probe")
            .size_full()
            .track_focus(&self.focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.mouse_downs += 1),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.mouse_ups += 1),
            )
            .on_scroll_wheel(cx.listener(|this, _, _, _| this.scrolls += 1))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, _| {
                this.keys.push(event.keystroke.key.clone());
            }))
            .child(format!("renders: {} text: {}", self.renders, self.text))
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, cx| {
                        window.handle_input(&focus, ElementInputHandler::new(bounds, entity), cx)
                    },
                )
                .size_full(),
            )
    }
}

/// Minimal text sink so typed text travels the plugin's text-input path.
impl EntityInputHandler for Probe {
    fn text_for_range(
        &mut self,
        _: Range<usize>,
        _: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        None
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let end = self.text.encode_utf16().count();
        Some(UTF16Selection {
            range: end..end,
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        None
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {}

    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.text.push_str(text);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        _: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
    }

    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        None
    }

    fn character_index_for_point(
        &mut self,
        _: gpui::Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

/// Weak handles to every probe by window label: weak so the harness can
/// verify GPUI roots are released when their Tauri window is destroyed.
#[derive(Default)]
struct Probes(HashMap<String, WeakEntity<Probe>>);
impl Global for Probes {}

/// Per-window GPUI `Window` handles, for driving GPUI window APIs.
#[derive(Default)]
struct Handles(HashMap<String, gpui::AnyWindowHandle>);
impl Global for Handles {}

/// A snapshot of a probe, readable off the main thread.
#[derive(Clone, Debug, Default)]
struct Seen {
    renders: usize,
    viewport: (f32, f32),
    maximized: bool,
    fullscreen: bool,
    active: bool,
    appearance: Option<WindowAppearance>,
    mouse_downs: usize,
    mouse_ups: usize,
    scrolls: usize,
    keys: Vec<String>,
    text: String,
}

fn attach_probe(window: &tauri::Window<Wry>) -> Result<(), GpuiError> {
    let label = window.label().to_string();
    window.attach_gpui_view(GpuiOptions::default(), move |window, cx| {
        let probe = cx.new(|cx| Probe::new(window, cx));
        cx.default_global::<Probes>()
            .0
            .insert(label.clone(), probe.downgrade());
        cx.default_global::<Handles>()
            .0
            .insert(label, window.window_handle());
        probe
    })
}

fn probe(cx: &App, label: &str) -> Option<Entity<Probe>> {
    cx.try_global::<Probes>()?.0.get(label)?.upgrade()
}

/// Runs `f` with the GPUI `Window` attached to the Tauri window `label`.
fn with_gpui_window<R>(
    cx: &mut App,
    label: &str,
    f: impl FnOnce(&mut Window, &mut App) -> R,
) -> Option<R> {
    let handle = *cx.try_global::<Handles>()?.0.get(label)?;
    handle.update(cx, |_, window, cx| f(window, cx)).ok()
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

/// xorshift64*: deterministic, dependency-free randomness.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn range(&mut self, range: Range<u64>) -> u64 {
        range.start + self.next() % (range.end - range.start)
    }
}

struct Check {
    scenario: &'static str,
    name: String,
    passed: bool,
    detail: String,
}

struct Harness {
    app: AppHandle<Wry>,
    rng: Rng,
    scenario: &'static str,
    checks: Vec<Check>,
    next_label: usize,
    has_wm: bool,
}

/// Set from `setup` before `init`: checks of API calls made before the
/// runtime existed.
static PRE_INIT: Mutex<Vec<(String, bool, String)>> = Mutex::new(Vec::new());

static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

/// Seconds since the suite started, for timing slow CI runners.
fn elapsed() -> f32 {
    START.get_or_init(Instant::now).elapsed().as_secs_f32()
}

/// Main-thread round trips slower than this are reported with their caller.
const SLOW_CALL: Duration = Duration::from_secs(1);

impl Harness {
    fn check(&mut self, name: impl Into<String>, passed: bool, detail: impl Into<String>) {
        let check = Check {
            scenario: self.scenario,
            name: name.into(),
            passed,
            detail: detail.into(),
        };
        println!(
            "  [{:6.1}s] [{}] {}: {}{}",
            elapsed(),
            if check.passed { "PASS" } else { "FAIL" },
            check.scenario,
            check.name,
            if check.detail.is_empty() {
                String::new()
            } else {
                format!(" — {}", check.detail)
            }
        );
        self.checks.push(check);
    }

    /// Runs `f` on the Tauri main thread and waits for the result.
    #[track_caller]
    fn main<R: Send + 'static>(&self, f: impl FnOnce(&AppHandle<Wry>) -> R + Send + 'static) -> R {
        let caller = std::panic::Location::caller();
        let start = Instant::now();
        let (tx, rx) = mpsc::channel();
        let app = self.app.clone();
        self.app
            .run_on_main_thread(move || {
                let _ = tx.send(f(&app));
            })
            .expect("run_on_main_thread");
        let result = rx
            .recv_timeout(Duration::from_secs(20))
            .expect("main thread stopped responding");
        if start.elapsed() > SLOW_CALL {
            println!(
                "  [{:6.1}s] slow main-thread call ({:.1}s) at {caller}",
                elapsed(),
                start.elapsed().as_secs_f32()
            );
        }
        result
    }

    /// Runs `f` with the shared GPUI App on the main thread.
    #[track_caller]
    fn gpui<R: Send + 'static>(&self, f: impl FnOnce(&mut App) -> R + Send + 'static) -> R {
        self.main(move |_| tauri_plugin_gpui::with_app(f).expect("with_app"))
    }

    fn seen(&self, label: &str) -> Option<Seen> {
        let label = label.to_string();
        self.gpui(move |cx| {
            let p = probe(cx, &label)?;
            let p = p.read(cx);
            Some(Seen {
                renders: p.renders,
                viewport: p.viewport,
                maximized: p.maximized,
                fullscreen: p.fullscreen,
                active: p.active,
                appearance: p.appearance,
                mouse_downs: p.mouse_downs,
                mouse_ups: p.mouse_ups,
                scrolls: p.scrolls,
                keys: p.keys.clone(),
                text: p.text.clone(),
            })
        })
    }

    /// Forces a GPUI re-render of `label` (so the probe re-reads window state).
    fn refresh(&self, label: &str) {
        let label = label.to_string();
        self.gpui(move |cx| {
            if let Some(p) = probe(cx, &label) {
                p.update(cx, |_, cx| cx.notify());
            }
        });
    }

    /// Polls `cond` until it holds or `timeout` passes.
    fn wait(&self, timeout: Duration, mut cond: impl FnMut(&Self) -> bool) -> bool {
        let start = Instant::now();
        loop {
            if cond(self) {
                return true;
            }
            if start.elapsed() > timeout {
                return false;
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn label(&mut self, prefix: &str) -> String {
        self.next_label += 1;
        format!("{prefix}-{}", self.next_label)
    }

    #[track_caller]
    fn create_window(&mut self, label: &str, size: (f64, f64)) -> tauri::Window<Wry> {
        let label = label.to_string();
        let (x, y) = (self.rng.range(0..400) as f64, self.rng.range(0..300) as f64);
        self.main(move |app| {
            tauri::WindowBuilder::new(app, &label)
                .title(&label)
                .inner_size(size.0, size.1)
                .position(x, y)
                .build()
                .expect("create window")
        })
    }

    #[track_caller]
    fn open_probe(&mut self, prefix: &str, size: (f64, f64)) -> String {
        let label = self.label(prefix);
        let window = self.create_window(&label, size);
        self.main(move |_| attach_probe(&window)).expect("attach");
        label
    }

    fn rendered(&self, label: &str) -> bool {
        let label = label.to_string();
        self.wait(Duration::from_secs(5), |h| {
            h.seen(&label).is_some_and(|s| s.renders > 0)
        })
    }

    fn window_gone(&self, label: &str) -> bool {
        let label = label.to_string();
        self.wait(Duration::from_secs(5), |h| {
            let l = label.clone();
            h.main(move |app| app.get_window(&l).is_none())
        })
    }

    fn gpui_window_count(&self) -> usize {
        self.gpui(|cx| cx.windows().len())
    }

    fn close(&self, label: &str) {
        let label = label.to_string();
        self.main(move |app| {
            if let Some(w) = app.get_window(&label) {
                w.close().ok();
            }
        });
    }

    fn destroy(&self, label: &str) {
        let label = label.to_string();
        self.main(move |app| {
            if let Some(w) = app.get_window(&label) {
                w.destroy().ok();
            }
        });
    }

    fn xid(&self, title: &str) -> Option<String> {
        let out = xdotool(&["search", "--name", &format!("^{title}$")]).ok()?;
        out.lines().last().map(str::to_string)
    }
}

fn xdotool(args: &[&str]) -> Result<String, String> {
    let output = Command::new("xdotool")
        .args(args)
        .output()
        .map_err(|e| format!("xdotool: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn wm_running() -> bool {
    xdotool(&["get_desktop"]).is_ok()
}

// ---------------------------------------------------------------------------
// Scenarios
// ---------------------------------------------------------------------------

/// Misuse of the public API must fail with the documented errors.
fn api_contract(h: &mut Harness) {
    for (name, passed, detail) in PRE_INIT.lock().unwrap().drain(..) {
        h.check(name, passed, detail);
    }

    let off_thread = tauri_plugin_gpui::with_app(|_| ()).err();
    h.check(
        "with_app off the main thread fails with NotMainThread",
        matches!(off_thread, Some(GpuiError::NotMainThread)),
        format!("{off_thread:?}"),
    );

    let label = h.label("contract");
    let window = h.create_window(&label, (300., 200.));
    let off_thread = attach_probe(&window).err();
    h.check(
        "attach_gpui off the main thread fails with NotMainThread",
        matches!(off_thread, Some(GpuiError::NotMainThread)),
        format!("{off_thread:?}"),
    );
    h.check(
        "is_gpui_attached off the main thread is false",
        !window.is_gpui_attached(),
        "",
    );

    let w = window.clone();
    let (first, second, attached) = h.main(move |_| {
        let first = attach_probe(&w);
        let second = attach_probe(&w);
        (first.err(), second.err(), w.is_gpui_attached())
    });
    h.check(
        "first attach succeeds",
        first.is_none(),
        format!("{first:?}"),
    );
    h.check(
        "second attach fails with AlreadyAttached",
        matches!(second, Some(GpuiError::AlreadyAttached { .. })),
        format!("{second:?}"),
    );
    h.check("is_gpui_attached on the main thread", attached, "");
    let rendered = h.rendered(&label);
    h.check("attached window renders", rendered, "");

    let reentrant = h.gpui(|_| tauri_plugin_gpui::with_app(|_| ()).err());
    h.check(
        "with_app inside GPUI fails with Reentrant",
        matches!(reentrant, Some(GpuiError::Reentrant)),
        format!("{reentrant:?}"),
    );

    let open_window = h.gpui(|cx| {
        cx.open_window(Default::default(), |window, cx| {
            cx.new(|cx| Probe::new(window, cx))
        })
        .err()
        .map(|e| format!("{e:#}"))
    });
    h.check(
        "GPUI open_window is rejected as unsupported",
        open_window
            .as_deref()
            .is_some_and(|e| e.contains("unsupported operation")),
        format!("{open_window:?}"),
    );

    let webview = h.main(|app| {
        let window =
            WebviewWindowBuilder::new(app, "webview", WebviewUrl::App("index.html".into()))
                .title("webview")
                .inner_size(300., 200.)
                .build()
                .expect("webview window");
        window
            .as_ref()
            .window()
            .attach_gpui(|cx| cx.new(|_| Empty))
            .err()
    });
    h.check(
        "attach to a WebView window fails with NotEligible",
        matches!(webview, Some(GpuiError::NotEligible { .. })),
        format!("{webview:?}"),
    );
    let still_there = h.main(|app| app.get_webview_window("webview").is_some());
    h.check("WebView window coexists with GPUI windows", still_there, "");

    h.close(&label);
    let gone = h.window_gone(&label);
    h.check("closed window disappears from Tauri", gone, "");
}

struct Empty;
impl Render for Empty {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// Randomized create / attach / close / destroy / GPUI-remove cycles. Every
/// GPUI root must be released and the shared App must survive.
fn lifecycle_storm(h: &mut Harness) {
    let baseline = h.gpui_window_count();
    let mut labels = Vec::new();
    let mut never_rendered = Vec::new();
    for _ in 0..16 {
        let size = (h.rng.range(120..700) as f64, h.rng.range(90..500) as f64);
        let label = h.open_probe("storm", size);
        if !h.rendered(&label) {
            never_rendered.push(label.clone());
        }
        match h.rng.range(0..4) {
            0 => h.close(&label),
            1 => h.destroy(&label),
            2 => {
                let l = label.clone();
                h.gpui(move |cx| with_gpui_window(cx, &l, |window, _| window.remove_window()));
            }
            _ => {} // left open; closed in bulk below
        }
        labels.push(label);
    }
    h.check(
        "every attached window rendered",
        never_rendered.is_empty(),
        format!("never rendered: {never_rendered:?}"),
    );
    for label in &labels {
        h.close(label);
    }
    let all_gone = labels.iter().all(|l| h.window_gone(l));
    h.check("all storm windows closed through Tauri", all_gone, "");
    let settled = h.wait(Duration::from_secs(5), |h| {
        h.gpui_window_count() == baseline
    });
    h.check(
        "GPUI window count returns to baseline",
        settled,
        format!("baseline {baseline}, now {}", h.gpui_window_count()),
    );
    let labels2 = labels.clone();
    let leaked: Vec<String> = h.gpui(move |cx| {
        labels2
            .into_iter()
            .filter(|l| probe(cx, l).is_some())
            .collect()
    });
    h.check(
        "GPUI roots are released with their Tauri windows",
        leaked.is_empty(),
        format!("still alive: {leaked:?}"),
    );
    let attached: Vec<String> = {
        let labels = labels.clone();
        h.main(move |app| {
            labels
                .into_iter()
                .filter(|l| app.get_window(l).is_some_and(|w| w.is_gpui_attached()))
                .collect()
        })
    };
    h.check(
        "no surface outlives its window",
        attached.is_empty(),
        format!("{attached:?}"),
    );

    // The shared App survives: a fresh window attaches and renders.
    let label = h.open_probe("after-storm", (300., 200.));
    let rendered = h.rendered(&label);
    h.check(
        "shared GPUI App still attaches after the storm",
        rendered,
        "",
    );
    h.close(&label);
    h.window_gone(&label);
}

/// Attach and tear down in the very same main-thread turn.
fn same_turn_teardown(h: &mut Harness) {
    for destroy in [false, true] {
        let label = h.label("same-turn");
        let window = h.create_window(&label, (300., 200.));
        let result = h.main(move |_| {
            let result = attach_probe(&window);
            if destroy {
                window.destroy().ok();
            } else {
                window.close().ok();
            }
            result.err()
        });
        let how = if destroy { "destroy" } else { "close" };
        h.check(
            format!("attach then {how} in one turn: attach ok"),
            result.is_none(),
            format!("{result:?}"),
        );
        let gone = h.window_gone(&label);
        h.check(
            format!("attach then {how} in one turn: window gone"),
            gone,
            "",
        );
    }

    // Attach from inside GPUI (deferred mount), then destroy before the
    // mount runs: the mount must be dropped, not run against a dead window.
    let baseline = h.gpui_window_count();
    let label = h.label("deferred");
    let window = h.create_window(&label, (300., 200.));
    let result = h.gpui(move |_| {
        let result = attach_probe(&window);
        window.destroy().ok();
        result.err()
    });
    h.check(
        "deferred attach accepted",
        result.is_none(),
        format!("{result:?}"),
    );
    let gone = h.window_gone(&label);
    h.check("deferred attach then destroy: window gone", gone, "");
    let settled = h.wait(Duration::from_secs(3), |h| {
        h.gpui_window_count() == baseline
    });
    h.check(
        "deferred attach then destroy: no orphan GPUI window",
        settled,
        format!("baseline {baseline}, now {}", h.gpui_window_count()),
    );
    let l = label.clone();
    let alive = h.gpui(move |cx| probe(cx, &l).is_some());
    h.check("deferred attach then destroy: no leaked root", !alive, "");

    // Same, but closing (CloseRequested path) instead of destroying.
    let label = h.label("deferred-close");
    let window = h.create_window(&label, (300., 200.));
    h.gpui(move |_| {
        attach_probe(&window).ok();
        window.close().ok();
    });
    let gone = h.window_gone(&label);
    h.check("deferred attach then close: window gone", gone, "");
}

/// Rapid Tauri-side resizes: GPUI must end on the final size.
fn resize_storm(h: &mut Harness) {
    let label = h.open_probe("resize", (400., 300.));
    h.rendered(&label);
    let mut last = (400., 300.);
    for _ in 0..40 {
        last = (h.rng.range(150..900) as f64, h.rng.range(120..700) as f64);
        let l = label.clone();
        h.main(move |app| {
            app.get_window(&l)
                .unwrap()
                .set_size(tauri::LogicalSize::new(last.0, last.1))
                .ok();
        });
        if h.rng.range(0..3) == 0 {
            thread::sleep(Duration::from_millis(h.rng.range(0..30)));
        }
    }
    let l = label.clone();
    let settled = h.wait(Duration::from_secs(5), |h| {
        let (tauri_size, scale) = {
            let l = l.clone();
            h.main(move |app| {
                let w = app.get_window(&l).unwrap();
                (w.inner_size().unwrap(), w.scale_factor().unwrap())
            })
        };
        h.refresh(&l);
        let expected = (
            (tauri_size.width as f64 / scale) as f32,
            (tauri_size.height as f64 / scale) as f32,
        );
        h.seen(&l).is_some_and(|s| s.viewport == expected)
            && expected == (last.0 as f32, last.1 as f32)
    });
    let seen = h.seen(&label).unwrap_or_default();
    h.check(
        "GPUI viewport matches the final Tauri size",
        settled,
        format!("final {last:?}, GPUI {:?}", seen.viewport),
    );
    h.close(&label);
    h.window_gone(&label);
}

/// Window state set through Tauri must be visible to GPUI, and window
/// operations requested by GPUI must be carried out by Tauri.
fn window_state(h: &mut Harness) {
    let label = h.open_probe("state", (400., 300.));
    h.rendered(&label);

    // GPUI starts from the Tauri window's mode and theme.
    let l = label.clone();
    let theme = h.main(move |app| app.get_window(&l).unwrap().theme().unwrap());
    let expected = match theme {
        tauri::Theme::Dark => WindowAppearance::Dark,
        _ => WindowAppearance::Light,
    };
    let seen = h.seen(&label).unwrap_or_default();
    h.check(
        "GPUI starts with the Tauri window's mode and theme",
        !seen.maximized && !seen.fullscreen && seen.appearance == Some(expected),
        format!("{seen:?}"),
    );

    // GPUI → Tauri: title.
    let l = label.clone();
    h.gpui(move |cx| with_gpui_window(cx, &l, |window, _| window.set_window_title("Renamed")));
    let l = label.clone();
    let renamed = h.wait(Duration::from_secs(3), |h| {
        let l = l.clone();
        h.main(move |app| app.get_window(&l).unwrap().title().unwrap()) == "Renamed"
    });
    h.check("GPUI set_window_title updates the Tauri title", renamed, "");

    if !h.has_wm {
        h.check(
            "maximize/fullscreen/focus checks",
            true,
            "skipped: no window manager",
        );
        h.close(&label);
        h.window_gone(&label);
        return;
    }

    // Tauri → GPUI: maximize.
    let l = label.clone();
    h.main(move |app| app.get_window(&l).unwrap().maximize().ok());
    let l = label.clone();
    let tauri_max = h.wait(Duration::from_secs(3), |h| {
        let l = l.clone();
        h.main(move |app| app.get_window(&l).unwrap().is_maximized().unwrap())
    });
    let gpui_max = h.wait(Duration::from_secs(3), |h| {
        h.refresh(&l);
        h.seen(&l).is_some_and(|s| s.maximized)
    });
    h.check(
        "Tauri maximize is reflected in GPUI is_maximized",
        tauri_max && gpui_max,
        format!("tauri {tauri_max}, gpui {gpui_max}"),
    );
    let l = label.clone();
    h.main(move |app| app.get_window(&l).unwrap().unmaximize().ok());
    let l = label.clone();
    let gpui_unmax = h.wait(Duration::from_secs(3), |h| {
        h.refresh(&l);
        h.seen(&l).is_some_and(|s| !s.maximized)
    });
    h.check("Tauri unmaximize is reflected in GPUI", gpui_unmax, "");

    // GPUI → Tauri: zoom toggles maximize.
    let l = label.clone();
    h.gpui(move |cx| with_gpui_window(cx, &l, |window, _| window.zoom_window()));
    let l = label.clone();
    let zoomed = h.wait(Duration::from_secs(3), |h| {
        let l = l.clone();
        h.main(move |app| app.get_window(&l).unwrap().is_maximized().unwrap())
    });
    h.check("GPUI zoom_window maximizes the Tauri window", zoomed, "");
    let l = label.clone();
    h.main(move |app| app.get_window(&l).unwrap().unmaximize().ok());

    // Tauri → GPUI: fullscreen.
    let l = label.clone();
    h.main(move |app| app.get_window(&l).unwrap().set_fullscreen(true).ok());
    let l = label.clone();
    let gpui_fs = h.wait(Duration::from_secs(3), |h| {
        h.refresh(&l);
        h.seen(&l).is_some_and(|s| s.fullscreen)
    });
    h.check(
        "Tauri fullscreen is reflected in GPUI is_fullscreen",
        gpui_fs,
        "",
    );
    let l = label.clone();
    h.main(move |app| app.get_window(&l).unwrap().set_fullscreen(false).ok());
    let l = label.clone();
    let gpui_not_fs = h.wait(Duration::from_secs(3), |h| {
        h.refresh(&l);
        h.seen(&l).is_some_and(|s| !s.fullscreen)
    });
    h.check("leaving fullscreen is reflected in GPUI", gpui_not_fs, "");

    // Focus: Tauri set_focus → GPUI active window.
    let other = h.open_probe("state-other", (300., 200.));
    h.rendered(&other);
    for target in [&label, &other] {
        let t = target.clone();
        h.main(move |app| app.get_window(&t).unwrap().set_focus().ok());
        let t = target.clone();
        let active = h.wait(Duration::from_secs(3), |h| {
            h.refresh(&t);
            h.seen(&t).is_some_and(|s| s.active)
        });
        h.check(
            format!("Tauri set_focus({target}) activates the GPUI window"),
            active,
            "",
        );
    }
    let l = label.clone();
    let inactive = h.wait(Duration::from_secs(3), |h| {
        h.refresh(&l);
        h.seen(&l).is_some_and(|s| !s.active)
    });
    h.check(
        "the previously focused GPUI window deactivates",
        inactive,
        "",
    );

    h.close(&other);
    h.close(&label);
    h.window_gone(&other);
    h.window_gone(&label);
}

/// Close vetoes from Tauri's `prevent_close`, and `destroy` bypassing them.
fn close_vetoes(h: &mut Harness) {
    // Tauri on_window_event prevent_close keeps the GPUI surface alive.
    let label = h.open_probe("tauri-veto", (300., 200.));
    h.rendered(&label);
    let vetoes = Arc::new(AtomicUsize::new(0));
    let l = label.clone();
    let v = vetoes.clone();
    h.main(move |app| {
        app.get_window(&l).unwrap().on_window_event(move |event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                v.fetch_add(1, Ordering::SeqCst);
                api.prevent_close();
            }
        });
    });
    h.close(&label);
    let vetoed = h.wait(Duration::from_secs(3), |_| {
        vetoes.load(Ordering::SeqCst) > 0
    });
    thread::sleep(Duration::from_millis(300));
    let l = label.clone();
    let still_attached =
        h.main(move |app| app.get_window(&l).is_some_and(|w| w.is_gpui_attached()));
    let before = h.seen(&label).map(|s| s.renders).unwrap_or(0);
    h.refresh(&label);
    let l = label.clone();
    let renders_after = h.wait(Duration::from_secs(3), |h| {
        h.seen(&l).is_some_and(|s| s.renders > before)
    });
    h.check(
        "Tauri prevent_close keeps the GPUI window alive and rendering",
        vetoed && still_attached && renders_after,
        format!("vetoed {vetoed}, attached {still_attached}, renders {renders_after}"),
    );
    h.destroy(&label);
    let gone = h.window_gone(&label);
    h.check("destroy bypasses the Tauri close veto", gone, "");
}

/// Tauri facilities used from GPUI code and vice versa.
fn tauri_integration(h: &mut Harness) {
    let label = h.open_probe("tauri", (300., 200.));
    h.rendered(&label);

    // Tauri events: emit from GPUI, received by a Tauri listener.
    let received = Arc::new(Mutex::new(Vec::<String>::new()));
    let r = received.clone();
    h.main(move |app| {
        app.listen("from-gpui", move |event| {
            r.lock().unwrap().push(event.payload().to_string());
        });
    });
    h.gpui(|cx| {
        let app = cx.global::<TauriHandle>().0.clone();
        app.emit("from-gpui", "hello").unwrap();
    });
    let got = h.wait(Duration::from_secs(3), |_| {
        received.lock().unwrap().iter().any(|p| p == "\"hello\"")
    });
    h.check(
        "GPUI code emits Tauri events",
        got,
        format!("{:?}", received.lock().unwrap()),
    );

    // Tauri events from a background thread drive GPUI state.
    let l = label.clone();
    h.main(move |app| {
        let app2 = app.clone();
        app.listen("to-gpui", move |event| {
            let text = event.payload().to_string();
            let l = l.clone();
            app2.run_on_main_thread(move || {
                tauri_plugin_gpui::with_app(|cx| {
                    if let Some(p) = probe(cx, &l) {
                        p.update(cx, |p, cx| {
                            p.text.push_str(&text);
                            cx.notify();
                        });
                    }
                })
                .unwrap();
            })
            .unwrap();
        });
    });
    let app = h.app.clone();
    thread::spawn(move || app.emit("to-gpui", "evt").unwrap())
        .join()
        .unwrap();
    let l = label.clone();
    let updated = h.wait(Duration::from_secs(3), |h| {
        h.seen(&l).is_some_and(|s| s.text.contains("\"evt\""))
    });
    h.check("background Tauri events update GPUI state", updated, "");

    // Managed Tauri state is reachable from GPUI.
    let value = h.gpui(|cx| {
        let app = cx.global::<TauriHandle>().0.clone();
        app.state::<Counter>().0.fetch_add(1, Ordering::SeqCst) + 1
    });
    h.check(
        "Tauri managed state is reachable from GPUI",
        value == 1,
        format!("{value}"),
    );

    // Tauri's window events still fire for GPUI-backed windows.
    let resized = Arc::new(AtomicUsize::new(0));
    let l = label.clone();
    let r = resized.clone();
    h.main(move |app| {
        let w = app.get_window(&l).unwrap();
        w.on_window_event(move |event| {
            if let tauri::WindowEvent::Resized(_) = event {
                r.fetch_add(1, Ordering::SeqCst);
            }
        });
        w.set_size(tauri::LogicalSize::new(333., 222.)).ok();
    });
    let fired = h.wait(Duration::from_secs(3), |_| {
        resized.load(Ordering::SeqCst) > 0
    });
    h.check("Tauri on_window_event fires for GPUI windows", fired, "");

    // GPUI async: a timer on the background executor resumes a foreground
    // task with no other event-loop activity.
    let done = h.gpui(|cx| {
        let flag = cx.new(|_| false);
        let f = flag.clone();
        cx.spawn(async move |cx| {
            cx.background_executor()
                .timer(Duration::from_millis(150))
                .await;
            let value = cx.background_spawn(async { 21 * 2 }).await;
            f.update(cx, |flag, _| *flag = value == 42);
        })
        .detach();
        flag
    });
    let fired = {
        let done = Arc::new(Mutex::new(Some(done)));
        h.wait(Duration::from_secs(3), move |h| {
            let d = done.clone();
            h.main(move |_| {
                tauri_plugin_gpui::with_app(|cx| *d.lock().unwrap().as_ref().unwrap().read(cx))
                    .unwrap()
            })
        })
    };
    h.check(
        "GPUI timers and background tasks wake the Tauri loop",
        fired,
        "",
    );

    // tauri::async_runtime → main thread → GPUI.
    let (tx, rx) = mpsc::channel();
    let app = h.app.clone();
    tauri::async_runtime::spawn(async move {
        let app2 = app.clone();
        app.run_on_main_thread(move || {
            let _ = app2;
            let count = tauri_plugin_gpui::with_app(|cx| cx.windows().len());
            tx.send(count.is_ok()).ok();
        })
        .ok();
    });
    let ok = rx.recv_timeout(Duration::from_secs(3)).unwrap_or(false);
    h.check(
        "tauri::async_runtime tasks reach GPUI via run_on_main_thread",
        ok,
        "",
    );

    h.close(&label);
    h.window_gone(&label);
}

/// The OS clipboard is shared between GPUI and tauri-plugin-clipboard-manager.
fn clipboard(h: &mut Harness) {
    let label = h.open_probe("clip", (300., 200.));
    h.rendered(&label);

    h.main(|app| app.clipboard().write_text("from tauri").unwrap());
    let read = h.gpui(|cx| cx.read_from_clipboard().and_then(|i| i.text()));
    h.check(
        "GPUI reads text written by tauri-plugin-clipboard-manager",
        read.as_deref() == Some("from tauri"),
        format!("{read:?}"),
    );

    h.gpui(|cx| cx.write_to_clipboard(ClipboardItem::new_string("from gpui".into())));
    let read = h.main(|app| app.clipboard().read_text().ok());
    h.check(
        "tauri-plugin-clipboard-manager reads text written by GPUI",
        read.as_deref() == Some("from gpui"),
        format!("{read:?}"),
    );

    h.gpui(|cx| {
        cx.write_to_clipboard(ClipboardItem::new_string_with_metadata(
            "meta".into(),
            "{\"k\":1}".into(),
        ))
    });
    let meta = h.gpui(|cx| cx.read_from_clipboard().and_then(|i| i.metadata().cloned()));
    h.check(
        "GPUI clipboard metadata round-trips",
        meta.as_deref() == Some("{\"k\":1}"),
        format!("{meta:?}"),
    );
    // Metadata is reattached by text equality (as zed does on Linux), so
    // only a write of different text can be told apart from GPUI's own.
    h.main(|app| app.clipboard().write_text("other").unwrap());
    let meta = h.gpui(|cx| cx.read_from_clipboard().and_then(|i| i.metadata().cloned()));
    h.check(
        "an external write drops GPUI clipboard metadata",
        meta.is_none(),
        format!("{meta:?}"),
    );

    h.close(&label);
    h.window_gone(&label);
}

/// Background threads hammer the main thread while windows churn.
fn thread_hammer(h: &mut Harness) {
    let label = h.open_probe("hammer", (300., 200.));
    h.rendered(&label);
    let ok = Arc::new(AtomicUsize::new(0));
    let off_thread_errors = Arc::new(AtomicUsize::new(0));
    let workers: Vec<_> = (0..4)
        .map(|_| {
            let app = h.app.clone();
            let ok = ok.clone();
            let errs = off_thread_errors.clone();
            let label = label.clone();
            thread::spawn(move || {
                for _ in 0..150 {
                    if matches!(
                        tauri_plugin_gpui::with_app(|_| ()),
                        Err(GpuiError::NotMainThread)
                    ) {
                        errs.fetch_add(1, Ordering::SeqCst);
                    }
                    let ok = ok.clone();
                    let label = label.clone();
                    app.run_on_main_thread(move || {
                        let r = tauri_plugin_gpui::with_app(|cx| {
                            if let Some(p) = probe(cx, &label) {
                                p.update(cx, |_, cx| cx.notify());
                            }
                        });
                        if r.is_ok() {
                            ok.fetch_add(1, Ordering::SeqCst);
                        }
                    })
                    .unwrap();
                }
            })
        })
        .collect();
    // Churn windows meanwhile.
    for _ in 0..4 {
        let l = h.open_probe("hammer-churn", (200., 150.));
        h.close(&l);
    }
    for w in workers {
        w.join().unwrap();
    }
    let all = h.wait(Duration::from_secs(10), |_| {
        ok.load(Ordering::SeqCst) == 600
    });
    h.check(
        "600 cross-thread with_app calls all succeed on the main thread",
        all,
        format!("{}", ok.load(Ordering::SeqCst)),
    );
    h.check(
        "off-thread with_app always reports NotMainThread",
        off_thread_errors.load(Ordering::SeqCst) == 600,
        format!("{}", off_thread_errors.load(Ordering::SeqCst)),
    );
    h.close(&label);
    h.window_gone(&label);
}

/// Random real X11 input (via xdotool) into a GPUI window.
fn input_chaos(h: &mut Harness) {
    if !h.has_wm || xdotool(&["version"]).is_err() {
        h.check(
            "input chaos",
            true,
            "skipped: needs xdotool and a window manager",
        );
        return;
    }
    let label = h.open_probe("input", (500., 400.));
    h.rendered(&label);
    let Some(xid) = h.xid(&label) else {
        h.check("input window found by xdotool", false, "");
        return;
    };
    xdotool(&["windowactivate", "--sync", &xid]).ok();
    xdotool(&["windowfocus", "--sync", &xid]).ok();
    thread::sleep(Duration::from_millis(300));

    // Typed text must arrive exactly once, with shifted characters.
    xdotool(&["type", "--delay", "40", "Hello, World 42!"]).unwrap();
    let l = label.clone();
    let typed = h.wait(Duration::from_secs(5), |h| {
        h.seen(&l).is_some_and(|s| s.text == "Hello, World 42!")
    });
    let seen = h.seen(&label).unwrap_or_default();
    h.check(
        "typed text reaches GPUI exactly once",
        typed,
        format!("text = {:?}", seen.text),
    );

    // Shortcut keys produce key events but no text.
    xdotool(&["key", "ctrl+a", "Return", "Escape", "Left"]).unwrap();
    let l = label.clone();
    let keys_ok = h.wait(Duration::from_secs(3), |h| {
        h.seen(&l).is_some_and(|s| {
            s.keys
                .ends_with(&["a".into(), "enter".into(), "escape".into(), "left".into()])
        })
    });
    let seen = h.seen(&label).unwrap_or_default();
    h.check(
        "named keys and shortcuts map to GPUI key names",
        keys_ok && seen.text == "Hello, World 42!",
        format!(
            "keys tail = {:?}, text = {:?}",
            &seen.keys[seen.keys.len().saturating_sub(4)..],
            seen.text
        ),
    );

    // Random mouse chaos.
    let (mut clicks, mut scrolls) = (0, 0);
    for _ in 0..60 {
        let (x, y) = (h.rng.range(5..495), h.rng.range(5..395));
        xdotool(&[
            "mousemove",
            "--window",
            &xid,
            &x.to_string(),
            &y.to_string(),
        ])
        .ok();
        match h.rng.range(0..3) {
            0 => {
                xdotool(&["click", "1"]).ok();
                clicks += 1;
            }
            1 => {
                xdotool(&["click", if h.rng.range(0..2) == 0 { "4" } else { "5" }]).ok();
                scrolls += 1;
            }
            _ => {}
        }
    }
    let l = label.clone();
    let mouse_ok = h.wait(Duration::from_secs(5), |h| {
        h.seen(&l).is_some_and(|s| {
            s.mouse_downs == clicks && s.mouse_ups == clicks && s.scrolls >= scrolls
        })
    });
    let seen = h.seen(&label).unwrap_or_default();
    h.check(
        "every click and scroll reaches GPUI",
        mouse_ok,
        format!(
            "sent {clicks} clicks/{scrolls} scrolls, GPUI saw {}/{} down/up, {} scrolls",
            seen.mouse_downs, seen.mouse_ups, seen.scrolls
        ),
    );

    h.close(&label);
    h.window_gone(&label);
}

/// Hidden windows keep their GPUI surface and repaint when shown again.
fn hide_show(h: &mut Harness) {
    let label = h.open_probe("hide", (300., 200.));
    h.rendered(&label);
    for _ in 0..5 {
        let l = label.clone();
        h.main(move |app| app.get_window(&l).unwrap().hide().ok());
        h.refresh(&label);
        thread::sleep(Duration::from_millis(h.rng.range(0..60)));
        let l = label.clone();
        h.main(move |app| app.get_window(&l).unwrap().show().ok());
    }
    let before = h.seen(&label).map(|s| s.renders).unwrap_or(0);
    h.refresh(&label);
    let l = label.clone();
    let renders = h.wait(Duration::from_secs(3), |h| {
        h.seen(&l).is_some_and(|s| s.renders > before)
    });
    let l = label.clone();
    let attached = h.main(move |app| app.get_window(&l).unwrap().is_gpui_attached());
    h.check(
        "hide/show cycles keep the surface attached and rendering",
        renders && attached,
        format!("renders {renders}, attached {attached}"),
    );
    h.close(&label);
    h.window_gone(&label);
}

/// Many GPUI windows at once, all torn down in a single main-thread turn.
fn many_windows(h: &mut Harness) {
    let baseline = h.gpui_window_count();
    let labels: Vec<String> = (0..10)
        .map(|_| {
            let size = (h.rng.range(150..400) as f64, h.rng.range(100..300) as f64);
            h.open_probe("many", size)
        })
        .collect();
    let all_rendered = labels.iter().all(|l| h.rendered(l));
    h.check("10 concurrent GPUI windows all render", all_rendered, "");
    let count = h.gpui_window_count();
    h.check(
        "the shared GPUI App sees every window",
        count == baseline + 10,
        format!("{count}"),
    );
    // Mixed close/destroy in one turn.
    let ls = labels.clone();
    h.main(move |app| {
        for (i, l) in ls.iter().enumerate() {
            let w = app.get_window(l).unwrap();
            if i % 2 == 0 {
                w.close().ok()
            } else {
                w.destroy().ok()
            };
        }
    });
    let gone = labels.iter().all(|l| h.window_gone(l));
    let settled = h.wait(Duration::from_secs(5), |h| {
        h.gpui_window_count() == baseline
    });
    h.check(
        "mass teardown in one turn releases every GPUI window",
        gone && settled,
        format!("gone {gone}, GPUI windows {}", h.gpui_window_count()),
    );

    // A label can be reused once its window is gone (close/recreate).
    for _ in 0..3 {
        let label = "reused".to_string();
        let window = h.create_window(&label, (300., 200.));
        let attached = h.main(move |_| attach_probe(&window).err());
        let rendered = h.rendered(&label);
        h.check(
            "a reused label attaches and renders again",
            attached.is_none() && rendered,
            format!("{attached:?}"),
        );
        h.close(&label);
        h.window_gone(&label);
    }
}

/// Floods of foreground tasks and timers all complete, in bounded drains.
fn task_flood(h: &mut Harness) {
    const TASKS: usize = 5000;
    const TIMERS: usize = 300;
    let done = Arc::new(AtomicUsize::new(0));
    let fired = Arc::new(AtomicUsize::new(0));
    let (d, f) = (done.clone(), fired.clone());
    let delays: Vec<u64> = (0..TIMERS).map(|_| h.rng.range(0..400)).collect();
    h.gpui(move |cx| {
        for _ in 0..TASKS {
            let d = d.clone();
            cx.spawn(async move |_| {
                d.fetch_add(1, Ordering::SeqCst);
            })
            .detach();
        }
        for delay in delays {
            let f = f.clone();
            cx.spawn(async move |cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(delay))
                    .await;
                f.fetch_add(1, Ordering::SeqCst);
            })
            .detach();
        }
    });
    let all = h.wait(Duration::from_secs(10), |_| {
        done.load(Ordering::SeqCst) == TASKS && fired.load(Ordering::SeqCst) == TIMERS
    });
    h.check(
        "5000 foreground tasks and 300 timers all complete",
        all,
        format!(
            "tasks {}, timers {}",
            done.load(Ordering::SeqCst),
            fired.load(Ordering::SeqCst)
        ),
    );
    // The main thread stays responsive under a burst.
    let start = Instant::now();
    h.gpui(|cx| {
        for _ in 0..20_000 {
            cx.spawn(async move |_| {}).detach();
        }
    });
    h.main(|_| ());
    let latency = start.elapsed();
    h.check(
        "the event loop answers while a 20k-task burst drains",
        latency < Duration::from_secs(5),
        format!("{latency:?}"),
    );
}

/// Attaching from GPUI async contexts (tasks, deferred callbacks).
fn attach_from_gpui(h: &mut Harness) {
    let a = h.label("from-task");
    let b = h.label("from-defer");
    let (la, lb) = (a.clone(), b.clone());
    h.gpui(move |cx| {
        let app = cx.global::<TauriHandle>().0.clone();
        let app2 = app.clone();
        cx.spawn(async move |_| {
            let w = tauri::WindowBuilder::new(&app, &la)
                .title(&la)
                .build()
                .unwrap();
            attach_probe(&w).unwrap();
        })
        .detach();
        cx.defer(move |_| {
            let w = tauri::WindowBuilder::new(&app2, &lb)
                .title(&lb)
                .build()
                .unwrap();
            attach_probe(&w).unwrap();
        });
    });
    let (ra, rb) = (h.rendered(&a), h.rendered(&b));
    h.check("attach from a GPUI task mounts and renders", ra, "");
    h.check("attach from cx.defer mounts and renders", rb, "");
    h.close(&a);
    h.close(&b);
    h.window_gone(&a);
    h.window_gone(&b);
}

/// GPUI window operations are carried out by Tauri.
fn gpui_window_ops(h: &mut Harness) {
    let label = h.open_probe("ops", (300., 200.));
    h.rendered(&label);

    // Tauri move → GPUI bounds.
    let l = label.clone();
    h.main(move |app| {
        app.get_window(&l)
            .unwrap()
            .set_position(tauri::LogicalPosition::new(123., 77.))
            .ok()
    });
    let l = label.clone();
    let moved = h.wait(Duration::from_secs(3), |h| {
        let l2 = l.clone();
        let tauri = h.main(move |app| {
            let w = app.get_window(&l2).unwrap();
            let p = w.outer_position().unwrap();
            let s = w.scale_factor().unwrap();
            ((p.x as f64 / s) as f32, (p.y as f64 / s) as f32)
        });
        let l2 = l.clone();
        let gpui = h.gpui(move |cx| {
            with_gpui_window(cx, &l2, |window, _| {
                let o = window.bounds().origin;
                (f32::from(o.x), f32::from(o.y))
            })
        });
        gpui == Some(tauri)
    });
    h.check("Tauri set_position is reflected in GPUI bounds", moved, "");

    if h.has_wm {
        let other = h.open_probe("ops-other", (300., 200.));
        h.rendered(&other);
        let l = label.clone();
        h.gpui(move |cx| with_gpui_window(cx, &l, |window, _| window.activate_window()));
        let l = label.clone();
        let focused = h.wait(Duration::from_secs(3), |h| {
            let l = l.clone();
            h.main(move |app| app.get_window(&l).unwrap().is_focused().unwrap())
        });
        h.check("GPUI activate_window focuses the Tauri window", focused, "");

        let l = label.clone();
        h.gpui(move |cx| with_gpui_window(cx, &l, |window, _| window.minimize_window()));
        let l = label.clone();
        let minimized = h.wait(Duration::from_secs(3), |h| {
            let l = l.clone();
            h.main(move |app| app.get_window(&l).unwrap().is_minimized().unwrap())
        });
        h.check(
            "GPUI minimize_window minimizes the Tauri window",
            minimized,
            "",
        );

        let l = label.clone();
        h.gpui(move |cx| with_gpui_window(cx, &l, |window, _| window.toggle_fullscreen()));
        let l = label.clone();
        let fullscreen = h.wait(Duration::from_secs(3), |h| {
            let l = l.clone();
            h.main(move |app| app.get_window(&l).unwrap().is_fullscreen().unwrap())
        });
        h.check(
            "GPUI toggle_fullscreen fullscreens the Tauri window",
            fullscreen,
            "",
        );
        h.close(&other);
        h.window_gone(&other);
    }
    h.close(&label);
    h.window_gone(&label);
}

/// `cx.quit()` is routed through Tauri's exit flow (ExitRequested).
fn gpui_quit(h: &mut Harness) {
    *QUIT_CODES.lock().unwrap() = Some(Vec::new());
    h.gpui(|cx| cx.quit());
    let requested = h.wait(Duration::from_secs(3), |_| {
        QUIT_CODES
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|c| !c.is_empty())
    });
    let codes = QUIT_CODES.lock().unwrap().take();
    h.check(
        "GPUI quit requests a Tauri exit with code 0",
        requested && codes.as_deref() == Some(&[0]),
        format!("{codes:?}"),
    );
    let alive = h.gpui(|cx| cx.windows().len() < usize::MAX);
    h.check("a prevented exit leaves the GPUI App usable", alive, "");
}

/// While `Some`, exit requests with a code are recorded and prevented.
static QUIT_CODES: Mutex<Option<Vec<i32>>> = Mutex::new(None);

// ---------------------------------------------------------------------------

struct TauriHandle(AppHandle<Wry>);
impl Global for TauriHandle {}

#[derive(Default)]
struct Counter(AtomicUsize);

fn has_display() -> bool {
    !cfg!(target_os = "linux")
        || std::env::var_os("DISPLAY").is_some()
        || std::env::var_os("WAYLAND_DISPLAY").is_some()
}

fn main() {
    if !has_display() {
        println!("chaos: skipped (no DISPLAY / WAYLAND_DISPLAY; run under xvfb-run)");
        return;
    }
    // WebKitGTK cannot composite on a virtual display.
    // SAFETY: single-threaded at this point.
    unsafe { std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1") };

    let seed = std::env::var("CHAOS_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64
                | 1
        });
    println!("chaos: CHAOS_SEED={seed}");

    elapsed();
    // Watchdog: a hung event loop is a failure, not a stuck CI job.
    let timeout = std::env::var("CHAOS_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(600);
    thread::spawn(move || {
        thread::sleep(Duration::from_secs(timeout));
        eprintln!("chaos: timed out after {timeout}s (CHAOS_TIMEOUT_SECS)");
        std::process::exit(2);
    });

    let context = tauri::test::mock_context(tauri::test::noop_assets());
    tauri::Builder::<Wry>::default()
        .plugin(tauri_plugin_clipboard_manager::init())
        .manage(Counter::default())
        .setup(move |app| {
            let mut pre = PRE_INIT.lock().unwrap();
            let before = tauri_plugin_gpui::with_app(|_| ()).err();
            pre.push((
                "with_app before init fails with NotInitialized".into(),
                matches!(before, Some(GpuiError::NotInitialized)),
                format!("{before:?}"),
            ));

            let handle = app.handle().clone();
            let start = Instant::now();
            tauri_plugin_gpui::init_with(
                app,
                GpuiConfig::new().on_launch(move |cx| cx.set_global(TauriHandle(handle))),
            )?;
            println!(
                "chaos: [{:6.1}s] GPUI runtime initialized in {:.1}s",
                elapsed(),
                start.elapsed().as_secs_f32()
            );
            let again = tauri_plugin_gpui::init(app).err();
            pre.push((
                "second init fails with AlreadyInitialized".into(),
                matches!(again, Some(GpuiError::AlreadyInitialized)),
                format!("{again:?}"),
            ));
            drop(pre);

            let handle = app.handle().clone();
            thread::spawn(move || run(handle, seed));
            Ok(())
        })
        .build(context)
        .expect("build tauri app")
        // Keep running when the last window closes between scenarios.
        .run(|_, event| {
            if let tauri::RunEvent::ExitRequested { code, api, .. } = event {
                match (code, QUIT_CODES.lock().unwrap().as_mut()) {
                    (None, _) => api.prevent_exit(),
                    (Some(code), Some(codes)) => {
                        codes.push(code);
                        api.prevent_exit();
                    }
                    (Some(_), None) => {}
                }
            }
        });
}

type Scenario = (&'static str, fn(&mut Harness));

fn run(app: AppHandle<Wry>, seed: u64) {
    // Let the event loop start.
    thread::sleep(Duration::from_millis(500));
    let mut h = Harness {
        app: app.clone(),
        rng: Rng(seed),
        scenario: "",
        checks: Vec::new(),
        next_label: 0,
        has_wm: wm_running(),
    };
    let scenarios: &[Scenario] = &[
        ("api_contract", api_contract),
        ("lifecycle_storm", lifecycle_storm),
        ("same_turn_teardown", same_turn_teardown),
        ("resize_storm", resize_storm),
        ("window_state", window_state),
        ("close_vetoes", close_vetoes),
        ("tauri_integration", tauri_integration),
        ("clipboard", clipboard),
        ("thread_hammer", thread_hammer),
        ("input_chaos", input_chaos),
        ("hide_show", hide_show),
        ("many_windows", many_windows),
        ("task_flood", task_flood),
        ("attach_from_gpui", attach_from_gpui),
        ("gpui_window_ops", gpui_window_ops),
        ("gpui_quit", gpui_quit),
    ];
    let only = std::env::var("CHAOS_ONLY").ok();
    for (name, scenario) in scenarios {
        if only
            .as_deref()
            .is_some_and(|o| !o.split(',').any(|o| o == *name))
        {
            continue;
        }
        println!("chaos: [{:6.1}s] {name}", elapsed());
        h.scenario = name;
        scenario(&mut h);
    }
    let failed: Vec<_> = h.checks.iter().filter(|c| !c.passed).collect();
    println!(
        "chaos: {} checks, {} failed (CHAOS_SEED={seed})",
        h.checks.len(),
        failed.len()
    );
    for c in &failed {
        println!("  FAILED {}: {} — {}", c.scenario, c.name, c.detail);
    }
    app.exit(if failed.is_empty() { 0 } else { 1 });
}
