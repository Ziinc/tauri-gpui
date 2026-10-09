//! Test runner. Tauri's event loop must own the main thread and exists once
//! per process, so tests run sequentially on a driver thread against one
//! shared app. After each test the runner destroys every window left open
//! and fails the test if any GPUI window or root outlived its Tauri window.

use std::{
    panic::{self, AssertUnwindSafe},
    sync::{Mutex, OnceLock, atomic::AtomicUsize},
    thread,
    time::{Duration, Instant},
};

use tauri::{AppHandle, Manager, Wry};
use tauri_plugin_gpui::{GpuiConfig, GpuiError, gpui::Global};

use super::{
    ctx::{Ctx, Fail, xdotool},
    probe::Registry,
};

pub struct Test {
    pub name: &'static str,
    pub run: fn(&mut Ctx) -> super::TestResult,
}

/// Builds a module's `TESTS` list: `tests![module => test_a, test_b]`.
#[macro_export]
macro_rules! tests {
    ($module:literal => $($test:ident),+ $(,)?) => {
        pub const TESTS: &[$crate::support::Test] = &[$(
            $crate::support::Test {
                name: concat!($module, "::", stringify!($test)),
                run: $test,
            },
        )+];
    };
}

/// The Tauri `AppHandle`, as a GPUI global for code running inside GPUI.
pub struct TauriHandle(pub AppHandle<Wry>);
impl Global for TauriHandle {}

/// Tauri managed state used by the `tauri_apis` tests.
#[derive(Default)]
pub struct Counter(pub AtomicUsize);

/// API errors observed in `setup`, before and right after `init`.
pub struct InitErrors {
    pub before_init: Option<GpuiError>,
    pub second_init: Option<GpuiError>,
}
pub static INIT_ERRORS: OnceLock<Mutex<InitErrors>> = OnceLock::new();

/// While `Some`, exit requests with a code are recorded and prevented.
pub static QUIT_CODES: Mutex<Option<Vec<i32>>> = Mutex::new(None);

static START: OnceLock<Instant> = OnceLock::new();

/// Seconds since the suite started.
pub fn elapsed() -> f32 {
    START.get_or_init(Instant::now).elapsed().as_secs_f32()
}

/// Runs `tests` (filtered by the command-line arguments) and exits.
pub fn main(tests: &[&'static [Test]]) {
    if cfg!(target_os = "linux")
        && std::env::var_os("DISPLAY").is_none()
        && std::env::var_os("WAYLAND_DISPLAY").is_none()
    {
        println!(
            "integration: skipped (no DISPLAY / WAYLAND_DISPLAY; see scripts/integration-test.sh)"
        );
        return;
    }
    // WebKitGTK cannot composite on a virtual display.
    // SAFETY: no other threads exist yet.
    unsafe { std::env::set_var("WEBKIT_DISABLE_COMPOSITING_MODE", "1") };
    elapsed();

    let filters: Vec<String> = std::env::args()
        .skip(1)
        .filter(|a| !a.starts_with('-'))
        .collect();
    let selected: Vec<&'static Test> = tests
        .iter()
        .flat_map(|module| module.iter())
        .filter(|t| filters.is_empty() || filters.iter().any(|f| t.name.contains(f.as_str())))
        .collect();
    let seed = env_or("CHAOS_SEED", || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
    });
    println!("integration: {} tests, CHAOS_SEED={seed}", selected.len());

    let timeout = env_or("CHAOS_TIMEOUT_SECS", || 600);
    thread::spawn(move || {
        thread::sleep(Duration::from_secs(timeout));
        eprintln!("integration: timed out after {timeout}s (CHAOS_TIMEOUT_SECS)");
        std::process::exit(2);
    });

    tauri::Builder::<Wry>::default()
        .plugin(tauri_plugin_clipboard_manager::init())
        .manage(Counter::default())
        .setup(move |app| {
            let before_init = tauri_plugin_gpui::with_app(|_| ()).err();
            let handle = app.handle().clone();
            tauri_plugin_gpui::init_with(
                app,
                GpuiConfig::new().on_launch(move |cx| cx.set_global(TauriHandle(handle))),
            )?;
            let second_init = tauri_plugin_gpui::init(app).err();
            INIT_ERRORS.get_or_init(|| {
                Mutex::new(InitErrors {
                    before_init,
                    second_init,
                })
            });
            let handle = app.handle().clone();
            thread::spawn(move || run(handle, &selected, seed));
            Ok(())
        })
        .build(tauri::test::mock_context(tauri::test::noop_assets()))
        .expect("build tauri app")
        .run(|_, event| {
            if let tauri::RunEvent::ExitRequested { code, api, .. } = event {
                match (code, QUIT_CODES.lock().unwrap().as_mut()) {
                    // Keep running when the last window closes between tests.
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

fn env_or(name: &str, default: impl FnOnce() -> u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(default)
}

fn run(app: AppHandle<Wry>, tests: &[&'static Test], seed: u64) {
    // Let the event loop start.
    thread::sleep(Duration::from_millis(500));
    let has_wm = xdotool(&["get_desktop"]).is_ok();
    let (mut passed, mut skipped, mut failed) = (0, 0, Vec::new());

    for test in tests {
        let start = Instant::now();
        let mut cx = Ctx::new(app.clone(), seed ^ hash(test.name), has_wm);
        let outcome = panic::catch_unwind(AssertUnwindSafe(|| (test.run)(&mut cx)))
            .unwrap_or_else(|panic| Err(Fail::Fail(panic_message(&*panic))));
        let teardown = teardown(&cx);
        let outcome = outcome.and(teardown);
        let secs = start.elapsed().as_secs_f32();
        match outcome {
            Ok(()) => {
                passed += 1;
                println!("test {} ... ok ({secs:.1}s)", test.name);
            }
            Err(Fail::Skip(why)) => {
                skipped += 1;
                println!("test {} ... skipped ({why})", test.name);
            }
            Err(Fail::Fail(why)) => {
                println!("test {} ... FAILED ({secs:.1}s)\n    {why}", test.name);
                failed.push((test.name, why));
            }
        }
    }

    println!(
        "\nintegration: {passed} passed; {} failed; {skipped} skipped ({:.1}s, CHAOS_SEED={seed})",
        failed.len(),
        elapsed()
    );
    for (name, why) in &failed {
        println!("  FAILED {name}: {why}");
    }
    app.exit(if failed.is_empty() { 0 } else { 1 });
}

/// Destroys every window left open so the next test starts clean, then
/// fails the test if any GPUI window or root outlived its Tauri window.
fn teardown(cx: &Ctx) -> Result<(), Fail> {
    let labels: Vec<String> = cx.main(|app| {
        let windows = app.windows();
        for window in windows.values() {
            window.destroy().ok();
        }
        windows.into_keys().collect()
    });
    for label in &labels {
        cx.window_gone(label);
    }
    *QUIT_CODES.lock().unwrap() = None;
    let settled = cx.wait(Duration::from_secs(3), |cx| {
        cx.gpui(|app| app.windows().is_empty() && Registry::live(app).is_empty())
    });
    let (windows, live) = cx.gpui(|app| (app.windows().len(), Registry::live(app)));
    cx.gpui(Registry::clear);
    if settled {
        Ok(())
    } else {
        Err(Fail::Fail(format!(
            "leaked {windows} GPUI windows; live roots: {live:?}"
        )))
    }
}

fn hash(name: &str) -> u64 {
    name.bytes().fold(0xcbf2_9ce4_8422_2325, |h, b| {
        (h ^ b as u64).wrapping_mul(0x100_0000_01b3)
    })
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
        .unwrap_or_else(|| "panicked".into())
}
