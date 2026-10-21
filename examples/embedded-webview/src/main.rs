//! A GPUI shell that still hosts one legacy web screen: the end state of an
//! incremental migration from a web UI to GPUI.
//!
//! * The window chrome, sidebar, status bar and the Overview screen are GPUI.
//! * The Reports screen is the existing web frontend (`dist/index.html`),
//!   loaded in a Tauri child webview and placed by `webview_frame`.
//! * Both sides share one counter that lives in Rust. The web screen reaches
//!   it through Tauri commands and events; GPUI reads it directly.
//!
//! * A native Tauri menu (View > Overview / Reports, Ctrl+1 / Ctrl+2)
//!   switches screens. On Linux it is a GTK menu bar above the GPUI content,
//!   and its accelerators work whether GPUI or the web screen has focus.
//!
//! Set `EMBEDDED_WEBVIEW_CHAOS=<dir>` to run the stress scenario in
//! `chaos.rs` instead (Linux/X11; see `scripts/chaos.sh`).
//! `EMBEDDED_WEBVIEW_MENU=none` leaves the menu out and `=late` adds it
//! after GPUI is attached.

mod chaos;

use std::sync::{
    Arc,
    atomic::{AtomicU32, Ordering},
};

use tauri::{
    AppHandle, Emitter, LogicalPosition, LogicalSize, Manager, State, Webview, WebviewBuilder,
    WebviewUrl, Wry,
    menu::{Menu, MenuBuilder, MenuItemBuilder, SubmenuBuilder},
};
use tauri_plugin_gpui::{
    GpuiWindowExt,
    gpui::{self, Global, WeakEntity, prelude::*, px, rgb},
    webview_frame,
};

/// State shared by the GPUI screens and the web screen.
#[derive(Clone, Default)]
struct Counter(Arc<AtomicU32>);

impl Counter {
    fn get(&self) -> u32 {
        self.0.load(Ordering::SeqCst)
    }

    /// Increments the counter and tells the web screen.
    fn increment(&self, app: &AppHandle) -> u32 {
        let count = self.0.fetch_add(1, Ordering::SeqCst) + 1;
        // Tauri calls must not run inside a GPUI update; emit from a task.
        let app = app.clone();
        tauri::async_runtime::spawn(async move {
            let _ = app.emit("count-changed", count);
        });
        count
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Screen {
    Overview,
    Reports,
}

struct Shell {
    app: AppHandle,
    counter: Counter,
    screen: Screen,
    reports: gpui::Entity<ReportsPane>,
}

/// The legacy web screen. A view of its own, rendered cached like a dock
/// panel would be: the shell re-renders (say, the status bar) without it.
struct ReportsPane {
    /// Added once the window is attached.
    webview: Option<Webview<Wry>>,
}

impl Render for ReportsPane {
    fn render(&mut self, _: &mut gpui::Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        gpui::div()
            .size_full()
            .when_some(self.webview.as_ref(), |this, webview| {
                this.child(webview_frame(webview))
            })
    }
}

/// Lets Tauri commands reach the shell.
struct ShellRef(WeakEntity<Shell>);

impl Global for ShellRef {}

fn update_shell(
    app: &AppHandle,
    f: impl FnOnce(&mut Shell, &mut gpui::Context<Shell>) + Send + 'static,
) {
    let _ = app.run_on_main_thread(move || {
        let _ = tauri_plugin_gpui::with_app(|cx| {
            let shell = cx.global::<ShellRef>().0.clone();
            let _ = shell.update(cx, f);
        });
    });
}

impl Shell {
    fn nav_item(
        &self,
        screen: Screen,
        label: &str,
        cx: &mut gpui::Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let active = self.screen == screen;
        gpui::div()
            .id(gpui::SharedString::from(label.to_string()))
            .px_3()
            .py_2()
            .rounded_md()
            .when(active, |this| this.bg(rgb(0x313244)))
            .hover(|style| style.bg(rgb(0x45475a)))
            .child(label.to_string())
            .on_click(cx.listener(move |this, _, _, cx| {
                this.screen = screen;
                cx.notify();
            }))
    }

    fn overview(&self, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        gpui::div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_4()
            .child(gpui::div().text_xl().child("Overview, rendered by GPUI"))
            .child(
                gpui::div()
                    .text_size(px(48.))
                    .child(format!("{}", self.counter.get())),
            )
            .child(
                gpui::div()
                    .id("increment")
                    .px_4()
                    .py_2()
                    .rounded_md()
                    .bg(rgb(0x89b4fa))
                    .text_color(rgb(0x1e1e2e))
                    .hover(|style| style.bg(rgb(0xb4befe)))
                    .child("Increment from GPUI")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.counter.increment(&this.app);
                        cx.notify();
                    })),
            )
    }
}

impl Render for Shell {
    fn render(&mut self, _: &mut gpui::Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let title = match self.screen {
            Screen::Overview => "Overview",
            Screen::Reports => "Reports (web screen in a child webview)",
        };
        gpui::div()
            .size_full()
            .flex()
            .bg(rgb(0x1e1e2e))
            .text_color(rgb(0xcdd6f4))
            .child(
                gpui::div()
                    .w(px(180.))
                    .h_full()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .p_2()
                    .bg(rgb(0x181825))
                    .child(
                        gpui::div()
                            .px_3()
                            .py_2()
                            .text_color(rgb(0x6c7086))
                            .child("GPUI sidebar"),
                    )
                    .child(self.nav_item(Screen::Overview, "Overview", cx))
                    .child(self.nav_item(Screen::Reports, "Reports", cx)),
            )
            .child(
                gpui::div()
                    .flex_1()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(
                        gpui::div()
                            .h(px(44.))
                            .px_4()
                            .flex()
                            .items_center()
                            .border_b_1()
                            .border_color(rgb(0x313244))
                            .child(title),
                    )
                    .child(gpui::div().flex_1().min_h_0().map(|this| {
                        match self.screen {
                            Screen::Reports => this.child(
                                gpui::AnyView::from(self.reports.clone())
                                    .cached(gpui::StyleRefinement::default().size_full()),
                            ),
                            Screen::Overview => this.child(self.overview(cx)),
                        }
                    }))
                    .child(
                        gpui::div()
                            .h(px(28.))
                            .px_4()
                            .flex()
                            .items_center()
                            .text_sm()
                            .bg(rgb(0x181825))
                            .child(format!(
                                "GPUI status bar · shared count: {}",
                                self.counter.get()
                            )),
                    ),
            )
    }
}

#[tauri::command]
fn count(counter: State<'_, Counter>) -> u32 {
    counter.get()
}

#[tauri::command]
fn increment(app: AppHandle, counter: State<'_, Counter>) -> u32 {
    let count = counter.increment(&app);
    update_shell(&app, |_, cx| cx.notify());
    count
}

#[tauri::command]
fn show_overview(app: AppHandle) {
    update_shell(&app, |shell, cx| {
        shell.screen = Screen::Overview;
        cx.notify();
    });
}

/// Adds the web screen to the GPUI window and hands it to the Reports pane.
fn add_reports(app: &AppHandle, window: &tauri::Window<Wry>) -> tauri::Result<()> {
    // Child webviews are added after GPUI is attached; `webview_frame`
    // shows and positions it from the next frame on.
    let webview = window.add_child(
        WebviewBuilder::new("reports", WebviewUrl::App("index.html".into())),
        LogicalPosition::new(0., 0.),
        LogicalSize::new(1., 1.),
    )?;
    webview.hide()?;
    update_shell(app, move |shell, cx| {
        shell.reports.update(cx, |pane, cx| {
            pane.webview = Some(webview);
            cx.notify();
        });
    });
    Ok(())
}

/// When the menu is added, from `EMBEDDED_WEBVIEW_MENU`.
#[derive(Clone, Copy, PartialEq)]
enum MenuMode {
    None,
    /// Set as the app menu, so the window is built with it.
    AtBuild,
    /// Set on the window after GPUI is attached.
    Late,
}

impl MenuMode {
    fn from_env() -> Self {
        match std::env::var("EMBEDDED_WEBVIEW_MENU").as_deref() {
            Ok("none") => Self::None,
            Ok("late") => Self::Late,
            _ => Self::AtBuild,
        }
    }
}

fn menu(app: &AppHandle) -> tauri::Result<Menu<Wry>> {
    let overview = MenuItemBuilder::with_id("overview", "Overview")
        .accelerator("CmdOrCtrl+1")
        .build(app)?;
    let reports = MenuItemBuilder::with_id("reports", "Reports")
        .accelerator("CmdOrCtrl+2")
        .build(app)?;
    let view = SubmenuBuilder::new(app, "View")
        .item(&overview)
        .item(&reports)
        .build()?;
    MenuBuilder::new(app).item(&view).build()
}

fn main() {
    tauri::Builder::default()
        .manage(Counter::default())
        .invoke_handler(tauri::generate_handler![count, increment, show_overview])
        .on_menu_event(|app, event| {
            let screen = match event.id().as_ref() {
                "overview" => Screen::Overview,
                "reports" => Screen::Reports,
                _ => return,
            };
            update_shell(app, move |shell, cx| {
                shell.screen = screen;
                cx.notify();
            });
        })
        .setup(|app| {
            tauri_plugin_gpui::init(app)?;
            let menu_mode = MenuMode::from_env();
            if menu_mode == MenuMode::AtBuild {
                app.set_menu(menu(app.handle())?)?;
            }
            let handle = app.handle().clone();
            let counter = app.state::<Counter>().inner().clone();
            let window = tauri::WindowBuilder::new(app, "main")
                .title("GPUI shell with an embedded web screen")
                .inner_size(900.0, 600.0)
                .build()?;
            window.attach_gpui(move |cx| {
                let reports = cx.new(|_| ReportsPane { webview: None });
                let shell = cx.new(|_| Shell {
                    app: handle,
                    counter,
                    screen: Screen::Reports,
                    reports,
                });
                cx.set_global(ShellRef(shell.downgrade()));
                shell
            })?;
            add_reports(app.handle(), &window)?;
            if menu_mode == MenuMode::Late {
                window.set_menu(menu(app.handle())?)?;
            }
            if let Some(dir) = std::env::var_os("EMBEDDED_WEBVIEW_CHAOS") {
                chaos::spawn(
                    app.handle().clone(),
                    dir.into(),
                    menu_mode != MenuMode::None,
                );
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
