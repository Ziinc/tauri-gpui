//! Scripted end-to-end scenario for the PRD success criteria.
//!
//! Drives real X11 input with `xdotool` (so events travel X11 → GTK → TAO →
//! tauri-plugin-gpui → GPUI), captures window screenshots with
//! `tauri-plugin-screenshots`, asserts on GPUI state and writes
//! `report.json` next to the screenshots. Exits 0 on success, 1 on failure.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::mpsc,
    thread,
    time::Duration,
};

use tauri::{AppHandle, Manager, Wry};
use tauri_plugin_gpui::{
    GpuiWindowExt,
    gpui::{App, AppContext as _},
};

use crate::{Observed, ObservedState, Shared};

/// Window-relative click targets; the demo views position these absolutely.
pub const INCREMENT_BTN: (i32, i32) = (84, 372);
pub const TOGGLE_BTN: (i32, i32) = (244, 372);
pub const INSPECTOR_INCREMENT_BTN: (i32, i32) = (154, 222);

#[derive(serde::Serialize)]
struct Check {
    name: String,
    passed: bool,
    detail: String,
}

#[derive(serde::Serialize, Default)]
struct Report {
    checks: Vec<Check>,
    screenshots: Vec<String>,
}

struct Scenario {
    app: AppHandle<Wry>,
    dir: PathBuf,
    report: Report,
}

pub fn spawn(app: AppHandle<Wry>, dir: PathBuf) {
    thread::spawn(move || {
        fs::create_dir_all(&dir).expect("create autotest output dir");
        let mut scenario = Scenario {
            app: app.clone(),
            dir: dir.clone(),
            report: Report::default(),
        };
        if let Err(error) = scenario.run() {
            scenario.check("scenario completed", false, error);
        }
        let passed = scenario.report.checks.iter().all(|c| c.passed);
        fs::write(
            dir.join("report.json"),
            serde_json::to_string_pretty(&scenario.report).unwrap(),
        )
        .unwrap();
        for check in &scenario.report.checks {
            println!(
                "[{}] {} — {}",
                if check.passed { "PASS" } else { "FAIL" },
                check.name,
                check.detail
            );
        }
        app.exit(if passed { 0 } else { 1 });
    });
}

fn sleep(ms: u64) {
    thread::sleep(Duration::from_millis(ms));
}

fn xdotool(args: &[&str]) -> Result<String, String> {
    let output = Command::new("xdotool")
        .args(args)
        .output()
        .map_err(|e| format!("xdotool: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "xdotool {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

impl Scenario {
    fn check(&mut self, name: &str, passed: bool, detail: impl Into<String>) {
        self.report.checks.push(Check {
            name: name.into(),
            passed,
            detail: detail.into(),
        });
    }

    /// Runs `f` on the Tauri main thread.
    fn on_main<R: Send + 'static>(
        &self,
        f: impl FnOnce(&AppHandle<Wry>) -> R + Send + 'static,
    ) -> Result<R, String> {
        let (tx, rx) = mpsc::channel();
        let app = self.app.clone();
        self.app
            .run_on_main_thread(move || {
                let _ = tx.send(f(&app));
            })
            .map_err(|e| e.to_string())?;
        rx.recv_timeout(Duration::from_secs(10))
            .map_err(|e| format!("main thread did not respond: {e}"))
    }

    /// Reads from the shared GPUI App on the main thread.
    fn gpui<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut App) -> R + Send + 'static,
    ) -> Result<R, String> {
        self.on_main(move |_| tauri_plugin_gpui::with_app(f).map_err(|e| e.to_string()))?
    }

    fn observed(&self) -> Result<(usize, Observed), String> {
        self.gpui(|cx| {
            let count = cx.global::<Shared>().0.read(cx).count;
            (count, cx.global::<ObservedState>().0.read(cx).clone())
        })
    }

    fn xid(&self, title: &str) -> Result<String, String> {
        let ids = xdotool(&["search", "--name", &format!("^{title}$")])?;
        ids.lines()
            .last()
            .map(str::to_string)
            .ok_or_else(|| format!("no X11 window titled {title:?}"))
    }

    fn click(&self, title: &str, (x, y): (i32, i32)) -> Result<(), String> {
        let xid = self.xid(title)?;
        xdotool(&["windowactivate", "--sync", &xid]).ok();
        xdotool(&["windowfocus", "--sync", &xid]).ok();
        xdotool(&[
            "mousemove",
            "--sync",
            "--window",
            &xid,
            &x.to_string(),
            &y.to_string(),
        ])?;
        sleep(150);
        xdotool(&["click", "1"])?;
        sleep(350);
        Ok(())
    }

    /// Captures a window by title with tauri-plugin-screenshots.
    fn shot(&mut self, title: &str, name: &str) -> Result<(), String> {
        sleep(400);
        let windows =
            tauri::async_runtime::block_on(tauri_plugin_screenshots::get_screenshotable_windows())?;
        let window = windows
            .iter()
            .find(|w| w.title == title)
            .ok_or_else(|| format!("tauri-plugin-screenshots found no window titled {title:?}"))?;
        let path = tauri::async_runtime::block_on(
            tauri_plugin_screenshots::get_window_screenshot(self.app.clone(), window.id),
        )?;
        self.save(&path, name)
    }

    fn monitor_shot(&mut self, name: &str) -> Result<(), String> {
        sleep(400);
        let monitors = tauri::async_runtime::block_on(
            tauri_plugin_screenshots::get_screenshotable_monitors(),
        )?;
        let monitor = monitors.first().ok_or("no monitor")?;
        let path = tauri::async_runtime::block_on(
            tauri_plugin_screenshots::get_monitor_screenshot(self.app.clone(), monitor.id),
        )?;
        self.save(&path, name)
    }

    fn save(&mut self, path: &Path, name: &str) -> Result<(), String> {
        let target = self.dir.join(format!("{name}.png"));
        fs::copy(path, &target).map_err(|e| e.to_string())?;
        self.report.screenshots.push(target.display().to_string());
        Ok(())
    }

    fn run(&mut self) -> Result<(), String> {
        sleep(3000);

        let attached = self.on_main(|app| {
            app.get_window("main")
                .map(|w| w.is_gpui_attached())
                .unwrap_or(false)
        })?;
        self.check("GPUI attached to Tauri window `main`", attached, "");
        let webview = self.on_main(|app| app.get_webview_window("webview").is_some())?;
        self.check("WebView window coexists", webview, "");
        self.shot("GPUI main", "01-main-initial")?;
        self.shot("WebView window", "01-webview-window")?;

        // Pointer + mouse buttons.
        for _ in 0..3 {
            self.click("GPUI main", INCREMENT_BTN)?;
        }
        let (count, observed) = self.observed()?;
        self.check(
            "mouse clicks reach GPUI",
            count == 3,
            format!("counter = {count}"),
        );
        let near = (observed.mouse.0 - INCREMENT_BTN.0 as f32).abs() <= 2.0
            && (observed.mouse.1 - INCREMENT_BTN.1 as f32).abs() <= 2.0;
        self.check(
            "cursor movement reaches GPUI",
            near,
            format!("mouse = {:?}", observed.mouse),
        );

        // Keyboard.
        let xid = self.xid("GPUI main")?;
        xdotool(&["windowfocus", "--sync", &xid]).ok();
        xdotool(&["type", "--delay", "80", "Hello GPUI"])?;
        xdotool(&["key", "BackSpace"])?;
        sleep(400);
        let (_, observed) = self.observed()?;
        self.check(
            "keyboard input reaches GPUI",
            observed.typed == "Hello GPU",
            format!("typed = {:?}", observed.typed),
        );

        // Scroll wheel.
        xdotool(&["mousemove", "--window", &xid, "300", "200"])?;
        for _ in 0..3 {
            xdotool(&["click", "5"])?;
            sleep(80);
        }
        sleep(300);
        let (_, observed) = self.observed()?;
        self.check(
            "scroll wheel reaches GPUI",
            observed.scroll_events >= 3,
            format!("scroll events = {}", observed.scroll_events),
        );
        self.shot("GPUI main", "02-main-after-input")?;

        // Resize through Tauri; GPUI must re-layout and redraw.
        self.on_main(|app| {
            app.get_window("main")
                .unwrap()
                .set_size(tauri::LogicalSize::new(820.0, 520.0))
        })?
        .map_err(|e| e.to_string())?;
        sleep(1000);
        let (_, observed) = self.observed()?;
        self.check(
            "resize reaches GPUI",
            observed.viewport == (820.0, 520.0),
            format!("viewport = {:?}", observed.viewport),
        );
        self.shot("GPUI main", "03-main-resized")?;

        // Second GPUI window created from inside a GPUI click handler.
        self.click("GPUI main", TOGGLE_BTN)?;
        sleep(1500);
        let inspector = self.on_main(|app| {
            app.get_window("inspector")
                .map(|w| w.is_gpui_attached())
                .unwrap_or(false)
        })?;
        self.check(
            "second Tauri window attached to the shared GPUI App",
            inspector,
            "",
        );
        self.shot("GPUI inspector", "04-inspector")?;

        self.click("GPUI inspector", INSPECTOR_INCREMENT_BTN)?;
        let (count, _) = self.observed()?;
        self.check(
            "state shared between GPUI windows",
            count == 4,
            format!("counter = {count}"),
        );
        self.shot("GPUI main", "05-main-after-inspector-click")?;

        // Close and recreate without restarting the GPUI App.
        self.click("GPUI main", TOGGLE_BTN)?;
        sleep(1500);
        let closed = self.on_main(|app| app.get_window("inspector").is_none())?;
        self.check("GPUI-backed window closes through Tauri", closed, "");
        self.click("GPUI main", TOGGLE_BTN)?;
        sleep(1500);
        let reopened = self.on_main(|app| {
            app.get_window("inspector")
                .map(|w| w.is_gpui_attached())
                .unwrap_or(false)
        })?;
        let (count, _) = self.observed()?;
        self.check(
            "GPUI window recreated on the same GPUI App",
            reopened && count == 4,
            format!("reopened = {reopened}, counter = {count}"),
        );
        self.shot("GPUI inspector", "06-inspector-reopened")?;

        // `cx.open_window` must fail clearly: GPUI cannot create native windows.
        let open_window_error = self.gpui(|cx| {
            cx.open_window(Default::default(), |_, cx| {
                cx.new(|_| crate::Observed::default())
            })
            .err()
            .map(|e| e.to_string())
        })?;
        self.check(
            "GPUI open_window is rejected",
            open_window_error
                .as_deref()
                .is_some_and(|e| e.contains("unsupported operation")),
            format!("{open_window_error:?}"),
        );

        self.monitor_shot("07-desktop")?;
        Ok(())
    }
}
