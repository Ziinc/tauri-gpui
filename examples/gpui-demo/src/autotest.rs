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

use gpui_kit::{App, AppContext as _};
use tauri::{AppHandle, Manager, Wry};
use tauri_plugin_gpui::GpuiWindowExt;

use crate::{LIST_TOP, ROW_H, Store, Todo, TodoStore};

// Window-relative click targets in the 560x560 main window (see the layout
// constants in main.rs).
const INPUT: (i32, i32) = (150, 90);
const ADD_BTN: (i32, i32) = (500, 90);
const THEME_BTN: (i32, i32) = (522, 40);
const FILTER_ALL: (i32, i32) = (52, 522);
const FILTER_ACTIVE: (i32, i32) = (132, 522);
const SUMMARY_BTN: (i32, i32) = (454, 522);

fn row_y(index: usize) -> i32 {
    (LIST_TOP + ROW_H * index as f32 + ROW_H / 2.) as i32
}
fn checkbox(index: usize) -> (i32, i32) {
    (38, row_y(index))
}
fn delete(index: usize) -> (i32, i32) {
    (522, row_y(index))
}

#[derive(serde::Serialize, Clone)]
struct Snapshot {
    todos: Vec<Todo>,
    visible: Vec<Todo>,
    scroll_events: usize,
    viewport: (f32, f32),
    input: String,
    dark: bool,
}

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

    fn snapshot(&self) -> Result<Snapshot, String> {
        self.gpui(|cx| {
            use gpui_kit::component::ActiveTheme;
            let store: &TodoStore = cx.global::<Store>().0.read(cx);
            let input = cx
                .global::<crate::MainInput>()
                .0
                .read(cx)
                .value()
                .to_string();
            Snapshot {
                todos: store.todos.clone(),
                visible: store.visible(),
                scroll_events: store.scroll_events,
                viewport: store.viewport,
                input,
                dark: cx.theme().mode.is_dark(),
            }
        })
    }

    fn titles(&self) -> Result<Vec<String>, String> {
        Ok(self
            .snapshot()?
            .todos
            .into_iter()
            .map(|t| t.title)
            .collect())
    }

    fn type_text(&self, title: &str, text: &str) -> Result<(), String> {
        let xid = self.xid(title)?;
        xdotool(&["windowfocus", "--sync", &xid]).ok();
        xdotool(&["type", "--delay", "60", text])?;
        sleep(300);
        Ok(())
    }

    fn summary_attached(&self) -> Result<bool, String> {
        self.on_main(|app| {
            app.get_window("summary")
                .map(|w| w.is_gpui_attached())
                .unwrap_or(false)
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
        const MAIN: &str = "Todos";
        const SUMMARY: &str = "Todo summary";
        sleep(3000);

        let attached = self.on_main(|app| {
            app.get_window("main")
                .map(|w| w.is_gpui_attached())
                .unwrap_or(false)
        })?;
        self.check("GPUI attached to Tauri window `main`", attached, "");
        let webview = self.on_main(|app| app.get_webview_window("webview").is_some())?;
        self.check("WebView window coexists", webview, "");
        self.shot(MAIN, "01-todos-initial")?;
        self.shot("WebView window", "01-webview-window")?;

        // Text input through gpui-kit's Input: click, type, Enter.
        self.click(MAIN, INPUT)?;
        self.type_text(MAIN, "Buy milk")?;
        self.shot(MAIN, "02-typing")?;
        xdotool(&["key", "Return"])?;
        sleep(400);
        let snap = self.snapshot()?;
        let titles: Vec<_> = snap.todos.iter().map(|t| t.title.clone()).collect();
        self.check(
            "typing + Enter adds a to-do",
            titles.last().map(String::as_str) == Some("Buy milk") && snap.input.is_empty(),
            format!("todos = {titles:?}, input = {:?}", snap.input),
        );

        // Typing then clicking the Add button.
        self.click(MAIN, INPUT)?;
        self.type_text(MAIN, "Ship tauri-plugin-gpui")?;
        xdotool(&["key", "BackSpace", "BackSpace", "BackSpace", "BackSpace"])?;
        self.type_text(MAIN, "GPUI!")?;
        self.click(MAIN, ADD_BTN)?;
        let titles = self.titles()?;
        self.check(
            "Add button, backspace and shifted characters",
            titles.last().map(String::as_str) == Some("Ship tauri-plugin-GPUI!"),
            format!("todos = {titles:?}"),
        );

        // Checkbox toggles and delete buttons.
        self.click(MAIN, checkbox(1))?;
        let snap = self.snapshot()?;
        self.check(
            "checkbox click toggles a to-do",
            snap.todos.get(1).is_some_and(|t| t.done),
            format!("{:?}", snap.todos.get(1)),
        );
        self.click(MAIN, delete(0))?;
        let titles = self.titles()?;
        self.check(
            "delete button removes a to-do",
            titles
                == [
                    "Attach GPUI to a Tauri window",
                    "Buy milk",
                    "Ship tauri-plugin-GPUI!",
                ],
            format!("todos = {titles:?}"),
        );
        self.shot(MAIN, "03-todos-edited")?;

        // Filters.
        self.click(MAIN, FILTER_ACTIVE)?;
        let snap = self.snapshot()?;
        self.check(
            "Active filter hides completed to-dos",
            snap.visible.len() == 2 && snap.visible.iter().all(|t| !t.done),
            format!(
                "visible = {:?}",
                snap.visible.iter().map(|t| &t.title).collect::<Vec<_>>()
            ),
        );
        self.shot(MAIN, "04-filter-active")?;
        self.click(MAIN, FILTER_ALL)?;

        // Scroll wheel.
        let xid = self.xid(MAIN)?;
        xdotool(&["mousemove", "--window", &xid, "280", "300"])?;
        for _ in 0..3 {
            xdotool(&["click", "5"])?;
            sleep(80);
        }
        sleep(300);
        let snap = self.snapshot()?;
        self.check(
            "scroll wheel reaches GPUI",
            snap.scroll_events >= 3,
            format!("scroll events = {}", snap.scroll_events),
        );

        // Theme toggle (gpui-kit theme change re-renders every window).
        self.click(MAIN, THEME_BTN)?;
        let snap = self.snapshot()?;
        self.check("theme toggle switches to dark", snap.dark, "");
        self.shot(MAIN, "05-todos-dark")?;

        // Second GPUI window created from inside a GPUI click handler.
        self.click(MAIN, SUMMARY_BTN)?;
        sleep(1500);
        let attached = self.summary_attached()?;
        self.check(
            "summary window attached to the shared GPUI App",
            attached,
            "",
        );
        self.shot(SUMMARY, "06-summary")?;

        // Shared state: a change in `main` shows up in `summary`.
        self.click(MAIN, checkbox(1))?;
        let snap = self.snapshot()?;
        let done = snap.todos.iter().filter(|t| t.done).count();
        self.check(
            "state shared between GPUI windows",
            done == 2,
            format!("done = {done}"),
        );
        self.shot(SUMMARY, "07-summary-updated")?;

        // Close and recreate without restarting the GPUI App.
        self.click(MAIN, SUMMARY_BTN)?;
        sleep(1500);
        let closed = self.on_main(|app| app.get_window("summary").is_none())?;
        self.check("GPUI-backed window closes through Tauri", closed, "");
        self.click(MAIN, SUMMARY_BTN)?;
        sleep(1500);
        let reopened = self.summary_attached()?;
        let titles = self.titles()?;
        self.check(
            "GPUI window recreated on the same GPUI App",
            reopened && titles.len() == 3,
            format!("reopened = {reopened}, todos = {}", titles.len()),
        );
        self.shot(SUMMARY, "08-summary-reopened")?;

        // Resize through Tauri; GPUI must re-layout and redraw.
        self.on_main(|app| {
            app.get_window("main")
                .unwrap()
                .set_size(tauri::LogicalSize::new(720.0, 640.0))
        })?
        .map_err(|e| e.to_string())?;
        sleep(1000);
        let snap = self.snapshot()?;
        self.check(
            "resize reaches GPUI",
            snap.viewport == (720.0, 640.0),
            format!("viewport = {:?}", snap.viewport),
        );
        self.shot(MAIN, "09-todos-resized")?;

        // `cx.open_window` must fail clearly: GPUI cannot create native windows.
        let open_window_error = self.gpui(|cx| {
            cx.open_window(Default::default(), |_, cx| cx.new(|_| crate::Blank))
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

        self.monitor_shot("10-desktop")?;
        Ok(())
    }
}
