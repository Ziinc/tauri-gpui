//! Stress scenario for `webview_frame`, run with
//! `EMBEDDED_WEBVIEW_CHAOS=<dir>` on an X11 display (`scripts/chaos.sh`).
//!
//! Drives the app from a background thread the way a user and the app
//! itself would, only faster: screen switches, re-renders of the shell
//! around the cached Reports pane, resizes, maximize, webview replacement.
//! With a menu it also drives the GTK menu bar and its accelerators with
//! real X11 input (`xdotool`), with GPUI and with the webview focused.
//! After each step it saves a screenshot of the window and records what the
//! content area must show in `checkpoints.json`; `scripts/check.py` verifies
//! the pixels. Finally it destroys the window while the webview is shown.

use std::{
    fmt::Write as _,
    path::PathBuf,
    process::Command,
    thread::sleep,
    time::{Duration, Instant},
};

use tauri::{AppHandle, LogicalSize, Manager, WebviewBuilder, WebviewUrl};

use crate::{Counter, Screen, update_shell};

/// Time for queued updates, frames and X11 to settle before a screenshot.
const SETTLE: Duration = Duration::from_millis(900);

pub fn spawn(app: AppHandle, dir: PathBuf, menu: bool) {
    std::thread::spawn(move || {
        let mut run = Run {
            app,
            dir,
            menu,
            checkpoints: String::new(),
            seed: 0x2545_f491_4f6c_dd1d,
        };
        run.scenario();
    });
}

struct Run {
    app: AppHandle,
    dir: PathBuf,
    /// The window has a menu bar.
    menu: bool,
    checkpoints: String,
    seed: u64,
}

impl Run {
    fn scenario(&mut self) {
        // The page loads and the first frames are drawn.
        sleep(Duration::from_secs(4));
        self.checkpoint("initial", Screen::Reports);
        if self.menu {
            self.menu_scenario();
        }

        for i in 0..200u32 {
            self.show(if i.is_multiple_of(2) {
                Screen::Overview
            } else {
                Screen::Reports
            });
            sleep(Duration::from_millis(2));
        }
        self.checkpoint("toggled-200-ending-reports", Screen::Reports);

        for i in 0..201u32 {
            self.show(if i.is_multiple_of(2) {
                Screen::Overview
            } else {
                Screen::Reports
            });
        }
        self.checkpoint("toggled-201-unthrottled-ending-overview", Screen::Overview);
        self.show(Screen::Reports);

        // The shell re-renders while the cached Reports pane is reused.
        let counter = self.app.state::<Counter>().inner().clone();
        for _ in 0..150 {
            counter.increment(&self.app);
            update_shell(&self.app, |_, cx| cx.notify());
            sleep(Duration::from_millis(3));
        }
        self.checkpoint("shell-rerendered-150", Screen::Reports);

        let window = self.app.get_window("main").expect("main window");
        for _ in 0..40 {
            let width = 500 + self.random() % 700;
            let height = 300 + self.random() % 450;
            let _ = window.set_size(LogicalSize::new(width as f64, height as f64));
            if self.random().is_multiple_of(3) {
                self.show(Screen::Overview);
                self.show(Screen::Reports);
            }
            sleep(Duration::from_millis(15));
        }
        let _ = window.set_size(LogicalSize::new(1000., 640.));
        self.checkpoint("resized-40-then-1000x640", Screen::Reports);

        let _ = window.maximize();
        self.checkpoint("maximized", Screen::Reports);
        let _ = window.unmaximize();
        self.checkpoint("unmaximized", Screen::Reports);

        // Hidden while the webview is replaced, then shown again.
        self.show(Screen::Overview);
        self.replace_webview(1);
        self.checkpoint("replaced-while-hidden", Screen::Overview);
        self.show(Screen::Reports);
        self.checkpoint("shown-after-replace", Screen::Reports);

        // Replaced while shown, repeatedly and quickly.
        let started = Instant::now();
        for n in 2..12 {
            self.replace_webview(n);
            if n.is_multiple_of(2) {
                self.show(Screen::Overview);
                self.show(Screen::Reports);
            }
        }
        eprintln!("chaos: 10 replacements took {:?}", started.elapsed());
        self.checkpoint("replaced-10-while-shown", Screen::Reports);

        std::fs::write(
            self.dir.join("checkpoints.json"),
            format!("[{}]", self.checkpoints),
        )
        .expect("write checkpoints");
        // Tear the window down with the webview shown.
        let _ = window.destroy();
    }

    /// Real input through the menu bar, its accelerators and GPUI. Window
    /// coordinates assume a GTK menu bar between 10 and 45 pixels high.
    fn menu_scenario(&mut self) {
        // Clicking the menu bar opens the View menu; its first item
        // switches to the Overview.
        self.click(20, 8);
        sleep(SETTLE);
        self.screenshot_popup("menu-open");
        self.xdotool(&["key", "Down", "Return"]);
        self.checkpoint("menu-item-activated", Screen::Overview);

        // Accelerators with GPUI focused: click the empty sidebar first.
        self.click(90, 400);
        self.xdotool(&["key", "ctrl+2"]);
        self.checkpoint("accelerator-gpui-focused", Screen::Reports);

        // With the web screen focused by a click into the page.
        self.click(600, 450);
        self.xdotool(&["key", "ctrl+1"]);
        self.checkpoint("accelerator-webview-focused", Screen::Overview);
        // Hiding the webview gave keyboard focus back to GPUI.
        self.xdotool(&["key", "ctrl+2"]);
        self.checkpoint("accelerator-after-webview-hidden", Screen::Reports);

        // GPUI's coordinates start below the menu bar: the Overview item
        // is 54 to 96 GPUI pixels down, the Reports item 100 to 142.
        self.click(60, 100);
        self.checkpoint("clicked-overview-below-menu-bar", Screen::Overview);
        self.click(60, 146);
        self.checkpoint("clicked-reports-below-menu-bar", Screen::Reports);
    }

    /// Clicks at window coordinates (logical) with the X server's pointer.
    fn click(&self, x: u32, y: u32) {
        let xid = self.xid();
        self.xdotool(&[
            "mousemove",
            "--window",
            &xid,
            &x.to_string(),
            &y.to_string(),
            "click",
            "1",
        ]);
        sleep(Duration::from_millis(300));
    }

    fn xdotool(&self, args: &[&str]) {
        let status = Command::new("xdotool")
            .args(args)
            .status()
            .expect("xdotool");
        assert!(status.success(), "xdotool {args:?} failed");
    }

    fn xid(&self) -> String {
        Command::new("xdotool")
            .args(["search", "--name", "^GPUI shell"])
            .output()
            .ok()
            .and_then(|out| String::from_utf8(out.stdout).ok())
            .and_then(|ids| ids.lines().last().map(str::to_string))
            .expect("window id")
    }

    /// Screenshots the window's area of the screen, popups included: the
    /// X server has no compositor, so the window's image is what is shown.
    fn screenshot_popup(&mut self, name: &str) {
        let file = format!("{name}.png");
        let status = Command::new("import")
            .args(["-window", &self.xid()])
            .arg(self.dir.join(&file))
            .status()
            .expect("import");
        assert!(status.success(), "screenshot failed");
        self.record(&format!(
            r#"{{"name":"{name}","file":"{file}","kind":"menu-open","menu":true}}"#
        ));
    }

    fn record(&mut self, checkpoint: &str) {
        if !self.checkpoints.is_empty() {
            self.checkpoints.push(',');
        }
        self.checkpoints.push_str(checkpoint);
    }

    fn show(&self, screen: Screen) {
        update_shell(&self.app, move |shell, cx| {
            shell.screen = screen;
            cx.notify();
        });
    }

    /// Closes the current web screen and adds a new one in its place.
    fn replace_webview(&self, n: usize) {
        for (_, webview) in self.app.webviews() {
            let _ = webview.close();
        }
        let window = self.app.get_window("main").expect("main window");
        let webview = window
            .add_child(
                WebviewBuilder::new(format!("reports-{n}"), WebviewUrl::App("index.html".into())),
                tauri::LogicalPosition::new(0., 0.),
                LogicalSize::new(1., 1.),
            )
            .expect("add_child");
        let _ = webview.hide();
        update_shell(&self.app, move |shell, cx| {
            shell.reports.update(cx, |pane, cx| {
                pane.webview = Some(webview);
                cx.notify();
            });
        });
    }

    fn checkpoint(&mut self, name: &str, screen: Screen) {
        sleep(SETTLE);
        let window = self.app.get_window("main").expect("main window");
        let scale = window.scale_factor().unwrap_or(1.);
        let size = window.inner_size().expect("size").to_logical::<f64>(scale);
        let file = format!("{name}.png");
        let xid = self.xid();
        let status = Command::new("import")
            .args(["-window", &xid])
            .arg(self.dir.join(&file))
            .status()
            .expect("import");
        assert!(status.success(), "screenshot failed");
        let reports = screen == Screen::Reports;
        let menu = self.menu;
        let mut checkpoint = String::new();
        let _ = write!(
            checkpoint,
            r#"{{"name":"{name}","file":"{file}","reports":{reports},"menu":{menu},"width":{},"height":{},"scale":{scale}}}"#,
            size.width, size.height
        );
        self.record(&checkpoint);
    }

    fn random(&mut self) -> u64 {
        // xorshift64: deterministic across runs.
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        self.seed
    }
}
