//! tauri-plugin-gpui demo.
//!
//! * `main`      – Tauri window with GPUI attached (interactive view)
//! * `inspector` – second GPUI-backed Tauri window sharing the same GPUI App,
//!   opened/closed/reopened from the main window
//! * `webview`   – ordinary WebView window coexisting with the GPUI windows
//!
//! Set `GPUI_DEMO_AUTOTEST=<dir>` to run the scripted scenario in
//! `autotest.rs`, which drives real OS input with `xdotool` and captures
//! screenshots through `tauri-plugin-screenshots`.

mod autotest;

use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder, Wry};
use tauri_plugin_gpui::{
    GpuiConfig, GpuiWindowExt,
    gpui::{
        self, App, Context, Entity, FocusHandle, Global, KeyDownEvent, MouseMoveEvent,
        ScrollWheelEvent, SharedString, Window, div, prelude::*, px, rgb,
    },
};

pub const MAIN_POS: (f64, f64) = (20.0, 20.0);
pub const INSPECTOR_POS: (f64, f64) = (720.0, 20.0);
pub const WEBVIEW_POS: (f64, f64) = (720.0, 480.0);

/// State shared by every GPUI window through the single GPUI App.
pub struct SharedCounter {
    pub count: usize,
}

pub struct Shared(pub Entity<SharedCounter>);
impl Global for Shared {}

pub struct TauriApp(pub AppHandle<Wry>);
impl Global for TauriApp {}

/// Snapshot of what the main view observed; read by the autotest.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Observed {
    pub typed: String,
    pub mouse: (f32, f32),
    pub scroll_events: usize,
    pub viewport: (f32, f32),
    pub clicks: usize,
}

pub struct ObservedState(pub Entity<Observed>);
impl Global for ObservedState {}

impl gpui::Render for Observed {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

struct MainView {
    focus: FocusHandle,
}

impl MainView {
    fn new(cx: &mut Context<Self>) -> Self {
        let counter = cx.global::<Shared>().0.clone();
        cx.observe(&counter, |_, _, cx| cx.notify()).detach();
        let observed = cx.global::<ObservedState>().0.clone();
        cx.observe(&observed, |_, _, cx| cx.notify()).detach();
        Self {
            focus: cx.focus_handle(),
        }
    }
}

/// Buttons sit at fixed positions so the autotest can click them.
pub fn button(
    id: &'static str,
    label: &'static str,
    (left, top, width): (f32, f32, f32),
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .absolute()
        .left(px(left))
        .top(px(top))
        .w(px(width))
        .h(px(36.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_md()
        .bg(rgb(0x6c5ce7))
        .hover(|style| style.bg(rgb(0x8e7ff0)))
        .text_color(rgb(0xffffff))
        .cursor_pointer()
        .child(label)
}

fn row(label: &'static str, value: impl Into<SharedString>) -> impl IntoElement {
    div()
        .flex()
        .gap_2()
        .child(div().w(px(150.)).text_color(rgb(0x9aa0b4)).child(label))
        .child(div().text_color(rgb(0xffffff)).child(value.into()))
}

pub fn increment(cx: &mut App) {
    let counter = cx.global::<Shared>().0.clone();
    counter.update(cx, |counter, cx| {
        counter.count += 1;
        cx.notify();
    });
}

impl gpui::Render for MainView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = cx.global::<Shared>().0.read(cx).count;
        let observed = cx.global::<ObservedState>().0.clone();
        let viewport = window.viewport_size();
        observed.update(cx, |o, _| {
            o.viewport = (f32::from(viewport.width), f32::from(viewport.height));
        });
        let o = observed.read(cx).clone();
        let inspector_open = cx.global::<TauriApp>().0.get_window("inspector").is_some();

        div()
            .id("main-root")
            .track_focus(&self.focus)
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .gap_3()
            .p_6()
            .bg(rgb(0x1e1f2e))
            .text_color(rgb(0xffffff))
            .text_size(px(16.))
            .on_key_down({
                let observed = observed.clone();
                move |event: &KeyDownEvent, _, cx| {
                    if let Some(ch) = &event.keystroke.key_char {
                        observed.update(cx, |o, cx| {
                            o.typed.push_str(ch);
                            cx.notify();
                        });
                    } else if event.keystroke.key == "backspace" {
                        observed.update(cx, |o, cx| {
                            o.typed.pop();
                            cx.notify();
                        });
                    }
                }
            })
            .on_mouse_move({
                let observed = observed.clone();
                move |event: &MouseMoveEvent, _, cx| {
                    observed.update(cx, |o, cx| {
                        o.mouse = (f32::from(event.position.x), f32::from(event.position.y));
                        cx.notify();
                    });
                }
            })
            .on_scroll_wheel({
                let observed = observed.clone();
                move |_: &ScrollWheelEvent, _, cx| {
                    observed.update(cx, |o, cx| {
                        o.scroll_events += 1;
                        cx.notify();
                    });
                }
            })
            .child(div().text_size(px(24.)).child("GPUI inside a Tauri window"))
            .child(
                div()
                    .text_color(rgb(0x9aa0b4))
                    .child("Tauri owns the window and event loop; GPUI renders the content."),
            )
            .child(row("Shared counter", count.to_string()))
            .child(row("Typed", format!("\"{}\"", o.typed)))
            .child(row("Mouse", format!("{:.0}, {:.0}", o.mouse.0, o.mouse.1)))
            .child(row("Scroll events", o.scroll_events.to_string()))
            .child(row(
                "Viewport",
                format!("{:.0} x {:.0}", o.viewport.0, o.viewport.1),
            ))
            .child(
                button("increment", "Increment", (24., 354., 120.)).on_click({
                    let observed = observed.clone();
                    move |_, _, cx| {
                        observed.update(cx, |o, _| o.clicks += 1);
                        increment(cx);
                    }
                }),
            )
            .child(if inspector_open {
                button("toggle-inspector", "Close inspector", (164., 354., 160.))
                    .on_click(|_, _, cx| close_inspector(cx))
            } else {
                button("toggle-inspector", "Open inspector", (164., 354., 160.))
                    .on_click(|_, _, cx| open_inspector(cx))
            })
    }
}

struct InspectorView;

impl InspectorView {
    fn new(cx: &mut Context<Self>) -> Self {
        let counter = cx.global::<Shared>().0.clone();
        cx.observe(&counter, |_, _, cx| cx.notify()).detach();
        Self
    }
}

impl gpui::Render for InspectorView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let count = cx.global::<Shared>().0.read(cx).count;
        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .gap_3()
            .p_6()
            .bg(rgb(0x123524))
            .text_color(rgb(0xffffff))
            .text_size(px(16.))
            .child(
                div()
                    .text_size(px(22.))
                    .child("Inspector (second GPUI window)"),
            )
            .child(row("Shared counter", count.to_string()))
            .child(
                button(
                    "inspector-increment",
                    "Increment from inspector",
                    (24., 204., 260.),
                )
                .on_click(|_, _, cx| increment(cx)),
            )
    }
}

/// Creates the inspector through Tauri and attaches GPUI to it. Called from
/// inside a GPUI click handler, so the mount completes after the handler.
pub fn open_inspector(cx: &mut App) {
    let app = cx.global::<TauriApp>().0.clone();
    if app.get_window("inspector").is_some() {
        return;
    }
    let window = tauri::WindowBuilder::new(&app, "inspector")
        .title("GPUI inspector")
        .inner_size(420.0, 320.0)
        .position(INSPECTOR_POS.0, INSPECTOR_POS.1)
        .build();
    match window {
        Ok(window) => {
            if let Err(error) = window.attach_gpui(|cx| cx.new(InspectorView::new)) {
                eprintln!("failed to attach GPUI to inspector: {error}");
            }
        }
        Err(error) => eprintln!("failed to create inspector window: {error}"),
    }
}

pub fn close_inspector(cx: &mut App) {
    if let Some(window) = cx.global::<TauriApp>().0.get_window("inspector") {
        let _ = window.close();
    }
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_screenshots::init())
        .setup(|app| {
            let handle = app.handle().clone();
            tauri_plugin_gpui::init_with(
                app,
                GpuiConfig::new().on_launch(move |cx| {
                    let counter = cx.new(|_| SharedCounter { count: 0 });
                    cx.set_global(Shared(counter));
                    let observed = cx.new(|_| Observed::default());
                    cx.set_global(ObservedState(observed));
                    cx.set_global(TauriApp(handle));
                }),
            )?;

            let main = tauri::WindowBuilder::new(app, "main")
                .title("GPUI main")
                .inner_size(660.0, 420.0)
                .position(MAIN_POS.0, MAIN_POS.1)
                .build()?;
            main.attach_gpui(|cx| cx.new(MainView::new))?;

            WebviewWindowBuilder::new(app, "webview", WebviewUrl::App("index.html".into()))
                .title("WebView window")
                .inner_size(420.0, 240.0)
                .position(WEBVIEW_POS.0, WEBVIEW_POS.1)
                .build()?;

            if let Ok(dir) = std::env::var("GPUI_DEMO_AUTOTEST") {
                autotest::spawn(app.handle().clone(), dir.into());
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
