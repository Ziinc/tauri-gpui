//! tauri-plugin-gpui demo: a to-do list built with gpui-kit components.
//!
//! * `main`    – Tauri window with GPUI attached: the to-do list
//! * `summary` – second GPUI-backed Tauri window on the same GPUI App,
//!   showing live stats from the shared store; opened/closed from `main`
//! * `webview` – ordinary WebView window coexisting with the GPUI windows
//!
//! Set `GPUI_DEMO_AUTOTEST=<dir>` to run the scripted scenario in
//! `autotest.rs`, which drives real OS input with `xdotool` and captures
//! screenshots through `tauri-plugin-screenshots`.
//!
//! `cargo test -p gpui-demo` runs headless gpui-kit UI tests (`tests.rs`).

mod autotest;
#[cfg(test)]
mod tests;

use gpui_kit::{
    component::{
        ActiveTheme, IconName, Sizable, Theme, ThemeMode,
        button::{Button, ButtonVariants},
        checkbox::Checkbox,
        input::{Input, InputEvent, InputState},
        progress::Progress,
    },
    prelude::FluentBuilder as _,
    *,
};
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindowBuilder, Wry};
use tauri_plugin_gpui::{GpuiConfig, GpuiOptions, GpuiWindowExt};

pub const MAIN_SIZE: (f64, f64) = (560.0, 560.0);
pub const MAIN_POS: (f64, f64) = (20.0, 20.0);
pub const SUMMARY_POS: (f64, f64) = (620.0, 20.0);
pub const WEBVIEW_POS: (f64, f64) = (620.0, 360.0);

// Fixed layout metrics, shared with the autotest's click targets.
pub const PAD: f32 = 20.;
pub const HEADER_H: f32 = 40.;
pub const GAP: f32 = 12.;
pub const ROW_H: f32 = 44.;
pub const CONTROL_H: f32 = 36.;
pub const LIST_TOP: f32 = PAD + HEADER_H + GAP + CONTROL_H + GAP;

#[derive(Clone, Debug, serde::Serialize)]
pub struct Todo {
    pub id: usize,
    pub title: String,
    pub done: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub enum Filter {
    #[default]
    All,
    Active,
    Completed,
}

/// The to-do store, shared by every GPUI window through the single GPUI App.
#[derive(Default, serde::Serialize)]
pub struct TodoStore {
    pub todos: Vec<Todo>,
    pub filter: Filter,
    next_id: usize,
    /// Observed by the autotest only.
    pub scroll_events: usize,
    pub viewport: (f32, f32),
}

impl TodoStore {
    /// The store the demo launches with.
    pub fn seeded() -> Self {
        let mut store = Self::default();
        store.add("Read the PRD");
        store.add("Attach GPUI to a Tauri window");
        store.todos[0].done = true;
        store
    }

    pub fn add(&mut self, title: &str) -> bool {
        let title = title.trim();
        if title.is_empty() {
            return false;
        }
        self.next_id += 1;
        self.todos.push(Todo {
            id: self.next_id,
            title: title.to_string(),
            done: false,
        });
        true
    }

    pub fn remaining(&self) -> usize {
        self.todos.iter().filter(|t| !t.done).count()
    }

    pub fn visible(&self) -> Vec<Todo> {
        self.todos
            .iter()
            .filter(|t| match self.filter {
                Filter::All => true,
                Filter::Active => !t.done,
                Filter::Completed => t.done,
            })
            .cloned()
            .collect()
    }
}

pub struct Store(pub Entity<TodoStore>);
impl Global for Store {}

pub struct TauriApp(pub AppHandle<Wry>);
impl Global for TauriApp {}

/// The main window's input, exposed for the autotest.
pub struct MainInput(pub Entity<InputState>);
impl Global for MainInput {}

/// Empty view used by the autotest's `open_window` rejection check.
pub struct Blank;
impl Render for Blank {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

pub fn store(cx: &App) -> Entity<TodoStore> {
    cx.global::<Store>().0.clone()
}

/// `TauriApp` is absent in headless tests, where no summary window exists.
fn summary_open(cx: &App) -> bool {
    cx.try_global::<TauriApp>()
        .is_some_and(|app| app.0.get_window("summary").is_some())
}

struct TodoView {
    input: Entity<InputState>,
}

impl TodoView {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("What needs to be done?"));
        cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.add(window, cx);
            }
        })
        .detach();
        cx.observe(&store(cx), |_, _, cx| cx.notify()).detach();
        input.update(cx, |input, cx| input.focus(window, cx));
        cx.set_global(MainInput(input.clone()));
        Self { input }
    }

    fn add(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let title = self.input.read(cx).value().to_string();
        let added = store(cx).update(cx, |store, cx| {
            let added = store.add(&title);
            cx.notify();
            added
        });
        if added {
            self.input
                .update(cx, |input, cx| input.set_value("", window, cx));
        }
    }
}

fn filter_button(
    id: &'static str,
    label: &'static str,
    filter: Filter,
    current: Filter,
    width: f32,
) -> Button {
    let button = Button::new(id).label(label).w(px(width)).h(px(CONTROL_H));
    let button = if filter == current {
        button.primary()
    } else {
        button.ghost()
    };
    button.on_click(move |_, _, cx| {
        store(cx).update(cx, |store, cx| {
            store.filter = filter;
            cx.notify();
        });
    })
}

impl Render for TodoView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewport = window.viewport_size();
        let store_entity = store(cx);
        store_entity.update(cx, |s, _| {
            s.viewport = (f32::from(viewport.width), f32::from(viewport.height));
        });
        let summary_open = summary_open(cx);
        let state = store_entity.read(cx);
        let visible = state.visible();
        let remaining = state.remaining();
        let total = state.todos.len();
        let filter = state.filter;
        let theme = cx.theme();
        let dark = theme.mode.is_dark();

        div()
            .id("todo-root")
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .gap(px(GAP))
            .p(px(PAD))
            .bg(theme.background)
            .text_color(theme.foreground)
            .on_scroll_wheel({
                let store_entity = store_entity.clone();
                move |_, _, cx| store_entity.update(cx, |s, _| s.scroll_events += 1)
            })
            // Header: title, remaining count, theme toggle.
            .child(
                div()
                    .h(px(HEADER_H))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .items_baseline()
                            .gap_3()
                            .child(
                                div()
                                    .text_size(px(26.))
                                    .font_weight(FontWeight::BOLD)
                                    .child("Todos"),
                            )
                            .child({
                                let left = format!("{remaining} of {total} left");
                                div()
                                    .id("remaining")
                                    .test_support()
                                    .aria_label(left.clone())
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .child(left)
                            }),
                    )
                    .child(
                        Button::new("theme-toggle")
                            .ghost()
                            .icon(if dark { IconName::Sun } else { IconName::Moon })
                            .w(px(CONTROL_H))
                            .h(px(CONTROL_H))
                            .on_click(move |_, window, cx| {
                                let mode = if dark {
                                    ThemeMode::Light
                                } else {
                                    ThemeMode::Dark
                                };
                                Theme::change(mode, Some(window), cx);
                                cx.refresh_windows();
                            }),
                    ),
            )
            // New to-do input.
            .child(
                div()
                    .h(px(CONTROL_H))
                    .flex()
                    .gap_2()
                    .child(div().flex_1().child(Input::new(&self.input).id("new-todo")))
                    .child(
                        Button::new("add")
                            .primary()
                            .icon(IconName::Plus)
                            .label("Add")
                            .w(px(80.))
                            .h(px(CONTROL_H))
                            .on_click(cx.listener(|this, _, window, cx| this.add(window, cx))),
                    ),
            )
            // The list.
            .child(
                div()
                    .flex()
                    .flex_col()
                    .children(visible.into_iter().enumerate().map(|(index, todo)| {
                        let id = todo.id;
                        div()
                            .id(("row", id))
                            .test_support()
                            .h(px(ROW_H))
                            .flex()
                            .items_center()
                            .justify_between()
                            .px_2()
                            .border_b_1()
                            .border_color(theme.border)
                            .when(index % 2 == 1, |row| row.bg(theme.muted.opacity(0.35)))
                            .child(
                                Checkbox::new(("done", id))
                                    .checked(todo.done)
                                    .label(todo.title.clone())
                                    .when(todo.done, |c| c.text_color(theme.muted_foreground))
                                    .on_click(move |checked, _, cx| {
                                        let checked = *checked;
                                        store(cx).update(cx, |store, cx| {
                                            if let Some(t) =
                                                store.todos.iter_mut().find(|t| t.id == id)
                                            {
                                                t.done = checked;
                                            }
                                            cx.notify();
                                        });
                                    }),
                            )
                            .child(
                                Button::new(("delete", id))
                                    .ghost()
                                    .small()
                                    .icon(IconName::Close)
                                    .on_click(move |_, _, cx| {
                                        store(cx).update(cx, |store, cx| {
                                            store.todos.retain(|t| t.id != id);
                                            cx.notify();
                                        });
                                    }),
                            )
                    }))
                    .when(total == 0, |list| {
                        list.child(
                            div()
                                .h(px(ROW_H * 2.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_color(theme.muted_foreground)
                                .child("Nothing to do. Add something above."),
                        )
                    }),
            )
            // Footer: filters and the summary window toggle.
            .child(
                div()
                    .absolute()
                    .left(px(PAD))
                    .right(px(PAD))
                    .bottom(px(PAD))
                    .h(px(CONTROL_H))
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .flex()
                            .gap_2()
                            .child(filter_button("filter-all", "All", Filter::All, filter, 64.))
                            .child(filter_button(
                                "filter-active",
                                "Active",
                                Filter::Active,
                                filter,
                                80.,
                            ))
                            .child(filter_button(
                                "filter-completed",
                                "Completed",
                                Filter::Completed,
                                filter,
                                104.,
                            )),
                    )
                    .child(
                        Button::new("summary-toggle")
                            .outline()
                            .icon(IconName::PanelRight)
                            .label(if summary_open {
                                "Close summary"
                            } else {
                                "Open summary"
                            })
                            .w(px(172.))
                            .h(px(CONTROL_H))
                            .on_click(move |_, _, cx| {
                                if summary_open {
                                    close_summary(cx)
                                } else {
                                    open_summary(cx)
                                }
                            }),
                    ),
            )
    }
}

struct SummaryView;

impl SummaryView {
    fn new(cx: &mut Context<Self>) -> Self {
        cx.observe(&store(cx), |_, _, cx| cx.notify()).detach();
        Self
    }
}

impl Render for SummaryView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let store = store(cx).read(cx);
        let total = store.todos.len();
        let done = total - store.remaining();
        let pct = if total == 0 {
            0.
        } else {
            done as f32 * 100. / total as f32
        };
        let theme = cx.theme();
        let stat = |id: &'static str, label: &'static str, value: usize| {
            div()
                .id(id)
                .test_support()
                .aria_label(value.to_string())
                .flex_1()
                .flex()
                .flex_col()
                .gap_1()
                .p_3()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .child(
                    div()
                        .text_sm()
                        .text_color(theme.muted_foreground)
                        .child(label),
                )
                .child(
                    div()
                        .text_size(px(24.))
                        .font_weight(FontWeight::BOLD)
                        .child(value.to_string()),
                )
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .gap_4()
            .p(px(PAD))
            .bg(theme.background)
            .text_color(theme.foreground)
            .child(
                div()
                    .text_size(px(20.))
                    .font_weight(FontWeight::BOLD)
                    .child("Summary"),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(theme.muted_foreground)
                    .child("Second GPUI window: same GPUI App, same store."),
            )
            .child(
                div()
                    .flex()
                    .gap_3()
                    .child(stat("stat-total", "Total", total))
                    .child(stat("stat-active", "Active", total - done))
                    .child(stat("stat-done", "Done", done)),
            )
            .child(Progress::new("progress").value(pct))
            .child(div().text_sm().child(format!("{pct:.0}% complete")))
    }
}

/// Wraps a view in gpui-kit's `Root` (dialogs, notifications, theming).
fn attach_with_root<V: Render>(
    window: &tauri::Window<Wry>,
    build: impl FnOnce(&mut Window, &mut App) -> Entity<V> + 'static,
) -> Result<(), tauri_plugin_gpui::GpuiError> {
    window.attach_gpui_view(GpuiOptions::default(), move |window, cx| {
        let view = build(window, cx);
        cx.new(|cx| base::Root::new(view, window, cx))
    })
}

/// Creates the summary window through Tauri and attaches GPUI to it. Called
/// from inside a GPUI click handler, so the mount completes after the handler.
pub fn open_summary(cx: &mut App) {
    let Some(app) = cx.try_global::<TauriApp>().map(|app| app.0.clone()) else {
        return;
    };
    if app.get_window("summary").is_some() {
        return;
    }
    let window = tauri::WindowBuilder::new(&app, "summary")
        .title("Todo summary")
        .inner_size(400.0, 280.0)
        .position(SUMMARY_POS.0, SUMMARY_POS.1)
        .build();
    match window {
        Ok(window) => {
            if let Err(error) = attach_with_root(&window, |_, cx| cx.new(SummaryView::new)) {
                eprintln!("failed to attach GPUI to the summary window: {error}");
            }
        }
        Err(error) => eprintln!("failed to create the summary window: {error}"),
    }
}

pub fn close_summary(cx: &mut App) {
    if let Some(window) = cx
        .try_global::<TauriApp>()
        .and_then(|app| app.0.get_window("summary"))
    {
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
                GpuiConfig::new()
                    .assets(gpui_kit::assets::Assets)
                    .on_launch(move |cx| {
                        gpui_kit::init(cx);
                        let store = cx.new(|_| TodoStore::seeded());
                        cx.set_global(Store(store));
                        cx.set_global(TauriApp(handle));
                    }),
            )?;

            let main = tauri::WindowBuilder::new(app, "main")
                .title("Todos")
                .inner_size(MAIN_SIZE.0, MAIN_SIZE.1)
                .position(MAIN_POS.0, MAIN_POS.1)
                .build()?;
            attach_with_root(&main, |window, cx| cx.new(|cx| TodoView::new(window, cx)))?;

            WebviewWindowBuilder::new(app, "webview", WebviewUrl::App("index.html".into()))
                .title("WebView window")
                .inner_size(400.0, 220.0)
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
