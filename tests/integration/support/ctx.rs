//! Per-test context: main-thread round trips, window helpers and polling.

use std::{
    ops::Range,
    process::Command,
    sync::{
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

use tauri::{AppHandle, Manager, Wry};
use tauri_plugin_gpui::gpui::App;

use super::{
    probe::{Seen, attach_probe, probe, with_gpui_window},
    runner::elapsed,
};

/// How a test ends when it does not pass.
#[derive(Debug)]
pub enum Fail {
    Fail(String),
    Skip(&'static str),
}

pub type TestResult = Result<(), Fail>;

/// Fails the test with a message unless `cond` holds.
#[macro_export]
macro_rules! ensure {
    ($cond:expr $(,)?) => {
        $crate::ensure!($cond, "{}", stringify!($cond))
    };
    ($cond:expr, $($fmt:tt)+) => {
        if !$cond {
            return Err($crate::support::Fail::Fail(format!($($fmt)+)));
        }
    };
}

/// Default polling timeout for state that changes asynchronously.
pub const WAIT: Duration = Duration::from_secs(5);
/// Main-thread round trips slower than this are reported with their caller.
const SLOW_CALL: Duration = Duration::from_secs(1);

pub struct Ctx {
    pub app: AppHandle<Wry>,
    pub rng: Rng,
    pub has_wm: bool,
}

/// Labels are unique across tests, so a window a test leaked can never be
/// mistaken for one a later test creates.
static NEXT_LABEL: AtomicUsize = AtomicUsize::new(0);

impl Ctx {
    pub(super) fn new(app: AppHandle<Wry>, seed: u64, has_wm: bool) -> Self {
        Self {
            app,
            rng: Rng(seed | 1),
            has_wm,
        }
    }

    /// Skips the test unless an EWMH window manager is running.
    pub fn require_wm(&self) -> TestResult {
        if self.has_wm {
            Ok(())
        } else {
            Err(Fail::Skip("needs a window manager"))
        }
    }

    /// Runs `f` on the Tauri main thread and waits for the result.
    #[track_caller]
    pub fn main<R: Send + 'static>(
        &self,
        f: impl FnOnce(&AppHandle<Wry>) -> R + Send + 'static,
    ) -> R {
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
                "    [{:6.1}s] slow main-thread call ({:.1}s) at {caller}",
                elapsed(),
                start.elapsed().as_secs_f32()
            );
        }
        result
    }

    /// Runs `f` with the shared GPUI App on the main thread.
    #[track_caller]
    pub fn gpui<R: Send + 'static>(&self, f: impl FnOnce(&mut App) -> R + Send + 'static) -> R {
        self.main(move |_| tauri_plugin_gpui::with_app(f).expect("with_app"))
    }

    /// Runs `f` with the GPUI `Window` of the Tauri window `label`.
    #[track_caller]
    pub fn gpui_window<R: Send + 'static>(
        &self,
        label: &str,
        f: impl FnOnce(&mut tauri_plugin_gpui::gpui::Window, &mut App) -> R + Send + 'static,
    ) -> Option<R> {
        let label = label.to_string();
        self.gpui(move |cx| with_gpui_window(cx, &label, f))
    }

    /// Runs `f` with the Tauri window `label`, if it still exists.
    #[track_caller]
    pub fn window<R: Send + 'static>(
        &self,
        label: &str,
        f: impl FnOnce(tauri::Window<Wry>) -> R + Send + 'static,
    ) -> Option<R> {
        let label = label.to_string();
        self.main(move |app| app.get_window(&label).map(f))
    }

    /// What the probe in `label` has observed, if its GPUI root is alive.
    pub fn seen(&self, label: &str) -> Option<Seen> {
        let label = label.to_string();
        self.gpui(move |cx| Some(probe(cx, &label)?.read(cx).seen()))
    }

    /// Re-renders `label` so its probe re-reads window state.
    pub fn refresh(&self, label: &str) {
        let label = label.to_string();
        self.gpui(move |cx| {
            if let Some(p) = probe(cx, &label) {
                p.update(cx, |_, cx| cx.notify());
            }
        });
    }

    /// Re-renders `label` until its probe satisfies `cond`.
    pub fn wait_seen(&self, label: &str, mut cond: impl FnMut(&Seen) -> bool) -> bool {
        self.wait(WAIT, |cx| {
            cx.refresh(label);
            cx.seen(label).is_some_and(|s| cond(&s))
        })
    }

    /// Polls `cond` until it holds or `timeout` passes.
    pub fn wait(&self, timeout: Duration, mut cond: impl FnMut(&Self) -> bool) -> bool {
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

    pub fn label(&self, prefix: &str) -> String {
        format!("{prefix}-{}", NEXT_LABEL.fetch_add(1, Ordering::Relaxed))
    }

    /// Creates a plain Tauri window (no WebView) at a random position.
    #[track_caller]
    pub fn create_window(&mut self, label: &str, size: (f64, f64)) -> tauri::Window<Wry> {
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

    /// Creates a window, attaches a probe and waits for its first frame.
    #[track_caller]
    pub fn open_probe(&mut self, prefix: &str, size: (f64, f64)) -> String {
        let label = self.label(prefix);
        let window = self.create_window(&label, size);
        self.main(move |_| attach_probe(&window)).expect("attach");
        assert!(self.rendered(&label), "`{label}` never rendered");
        label
    }

    /// A random window size within the given ranges.
    pub fn random_size(&mut self, w: Range<u64>, h: Range<u64>) -> (f64, f64) {
        (self.rng.range(w) as f64, self.rng.range(h) as f64)
    }

    pub fn rendered(&self, label: &str) -> bool {
        self.wait(WAIT, |cx| cx.seen(label).is_some_and(|s| s.renders > 0))
    }

    pub fn window_gone(&self, label: &str) -> bool {
        self.wait(WAIT, |cx| cx.window(label, |_| ()).is_none())
    }

    pub fn is_attached(&self, label: &str) -> bool {
        use tauri_plugin_gpui::GpuiWindowExt;
        self.window(label, |w| w.is_gpui_attached())
            .unwrap_or(false)
    }

    pub fn gpui_window_count(&self) -> usize {
        self.gpui(|cx| cx.windows().len())
    }

    pub fn close(&self, label: &str) {
        self.window(label, |w| w.close().ok());
    }

    pub fn destroy(&self, label: &str) {
        self.window(label, |w| w.destroy().ok());
    }

    /// X11 window id of the window titled `title`.
    pub fn xid(&self, title: &str) -> Option<String> {
        let out = xdotool(&["search", "--name", &format!("^{title}$")]).ok()?;
        out.lines().last().map(str::to_string)
    }
}

/// xorshift64*: deterministic, dependency-free randomness.
pub struct Rng(u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    pub fn range(&mut self, range: Range<u64>) -> u64 {
        range.start + self.next() % (range.end - range.start)
    }
}

pub fn xdotool(args: &[&str]) -> Result<String, String> {
    let output = Command::new("xdotool")
        .args(args)
        .output()
        .map_err(|e| format!("xdotool: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned());
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
