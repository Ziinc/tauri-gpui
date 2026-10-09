//! Headless UI tests for the demo, following https://gpui-kit.com/docs/test/.
//!
//! Views render in gpui-kit test windows (no Tauri, no display server); clicks
//! and keystrokes go through GPUI's native event dispatch, and assertions read
//! the elements' accessibility facts plus the shared `TodoStore`.
//!
//! Imports are explicit: `gpui_kit::*` would shadow Rust's `#[test]` with GPUI's.
use super::{Filter, LIST_TOP, MAIN_SIZE, ROW_H, Store, SummaryView, TodoStore, TodoView};
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{
    App, AppContext as _, Bounds, ElementId, Entity, Point, Render, TestAppContext, Window,
    WindowBounds, WindowHandle, WindowOptions, base, px, size,
};

/// Row element ids are keyed by the todo's `usize` id.
fn id(name: &'static str, todo: usize) -> ElementId {
    (name, todo).into()
}

// ---- Pure data logic --------------------------------------------------------

#[test]
fn add_trims_and_rejects_blank_titles() {
    let mut store = TodoStore::default();
    assert!(store.add("  Ship it  "));
    assert!(!store.add("   "));
    assert_eq!(store.todos.len(), 1);
    assert_eq!(store.todos[0].title, "Ship it");
    assert!(store.add("Again"));
    assert_ne!(store.todos[0].id, store.todos[1].id);
}

#[test]
fn filters_and_remaining_count() {
    let mut store = TodoStore::seeded();
    assert_eq!(store.remaining(), 1);
    let titles = |store: &TodoStore| -> Vec<String> {
        store.visible().into_iter().map(|t| t.title).collect()
    };
    assert_eq!(titles(&store).len(), 2);
    store.filter = Filter::Active;
    assert_eq!(titles(&store), ["Attach GPUI to a Tauri window"]);
    store.filter = Filter::Completed;
    assert_eq!(titles(&store), ["Read the PRD"]);
}

// ---- UI integration ---------------------------------------------------------

/// Inits gpui-kit, installs a seeded store and opens `build` in a 560x560
/// window wrapped in gpui-kit's `Root`, mirroring `attach_with_root`.
fn open<V: Render>(
    cx: &mut TestAppContext,
    build: impl FnOnce(&mut Window, &mut App) -> Entity<V>,
) -> (WindowHandle<base::Root>, Entity<TodoStore>) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        let store = cx.new(|_| TodoStore::seeded());
        cx.set_global(Store(store.clone()));
        let bounds = Bounds {
            origin: Point::default(),
            size: size(px(MAIN_SIZE.0 as f32), px(MAIN_SIZE.1 as f32)),
        };
        let (window, _) = gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            cx,
            build,
        )
        .expect("open test window");
        (window.downcast().expect("Root window"), store)
    })
}

fn open_todos(cx: &mut TestAppContext) -> (WindowHandle<base::Root>, Entity<TodoStore>) {
    open(cx, |window, cx| cx.new(|cx| TodoView::new(window, cx)))
}

fn titles(store: &Entity<TodoStore>, cx: &mut TestAppContext) -> Vec<String> {
    store.read_with(cx, |s, _| s.todos.iter().map(|t| t.title.clone()).collect())
}

#[gpui_kit::test]
fn renders_the_seeded_list(cx: &mut TestAppContext) {
    let (handle, _) = open_todos(cx);
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find("remaining").label(), Some("1 of 2 left"));
        assert_eq!(window.find(id("done", 1)).checked(), Some(true));
        assert_eq!(window.find(id("done", 2)).checked(), Some(false));
        assert!(window.find(id("row", 1)).visible());
        assert!(window.find(id("row", 2)).visible());
        // Rows stack at the fixed metrics the xdotool autotest relies on.
        let first = window.find(id("row", 1)).bounds();
        assert_eq!(first.top(), px(LIST_TOP));
        assert_eq!(first.size.height, px(ROW_H));
        assert_eq!(window.find(id("row", 2)).bounds().top(), first.bottom());
        // The input is focused on open.
        assert_eq!(window.find("new-todo").focused(), Some(true));
    })
    .unwrap();
}

#[gpui_kit::test]
fn adds_todos_with_enter_and_the_add_button(cx: &mut TestAppContext) {
    let (handle, store) = open_todos(cx);
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.input("Write tests", cx);
        assert_eq!(window.find("new-todo").value(), Some("Write tests"));
        window.press("enter", cx);
    })
    .unwrap();
    // Enter reaches `TodoView` as an `InputEvent` subscription, which GPUI
    // delivers when the update above flushes its effects.
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find("new-todo").value(), Some(""));
        assert_eq!(window.find("remaining").label(), Some("2 of 3 left"));

        window.click("new-todo", cx);
        window.input("Ship", cx);
        window.click("add", cx);
        assert_eq!(window.find("remaining").label(), Some("3 of 4 left"));
        assert!(window.find(id("row", 4)).visible());

        // Blank input is rejected and left in place.
        window.click("new-todo", cx);
        window.input("   ", cx);
        window.click("add", cx);
        assert_eq!(window.find("remaining").label(), Some("3 of 4 left"));
        assert_eq!(window.find("new-todo").value(), Some("   "));
    })
    .unwrap();
    assert_eq!(
        titles(&store, cx),
        [
            "Read the PRD",
            "Attach GPUI to a Tauri window",
            "Write tests",
            "Ship"
        ]
    );
}

#[gpui_kit::test]
fn toggles_and_deletes_todos(cx: &mut TestAppContext) {
    let (handle, store) = open_todos(cx);
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click(id("done", 2), cx);
        assert_eq!(window.find(id("done", 2)).checked(), Some(true));
        assert_eq!(window.find("remaining").label(), Some("0 of 2 left"));

        window.click(id("done", 1), cx);
        assert_eq!(window.find(id("done", 1)).checked(), Some(false));
        assert_eq!(window.find("remaining").label(), Some("1 of 2 left"));

        window.click(id("delete", 1), cx);
        assert!(window.try_find(id("row", 1)).is_none());
        assert_eq!(window.find("remaining").label(), Some("0 of 1 left"));
    })
    .unwrap();
    store.read_with(cx, |s, _| {
        assert_eq!(s.todos.len(), 1);
        assert!(s.todos[0].done);
    });
}

#[gpui_kit::test]
fn filter_buttons_change_the_visible_rows(cx: &mut TestAppContext) {
    let (handle, store) = open_todos(cx);
    cx.update_window(handle.into(), |_, window, cx| {
        window.render_frame(cx);
        window.click("filter-active", cx);
        assert!(window.try_find(id("row", 1)).is_none());
        assert!(window.find(id("row", 2)).visible());

        window.click("filter-completed", cx);
        assert!(window.find(id("row", 1)).visible());
        assert!(window.try_find(id("row", 2)).is_none());

        window.click("filter-all", cx);
        assert!(window.find(id("row", 1)).visible());
        assert!(window.find(id("row", 2)).visible());
    })
    .unwrap();
    store.read_with(cx, |s, _| assert_eq!(s.filter, Filter::All));
}

#[gpui_kit::test]
fn theme_toggle_switches_modes(cx: &mut TestAppContext) {
    let (handle, _) = open_todos(cx);
    let dark = |cx: &mut TestAppContext| cx.update(|cx| cx.theme().mode.is_dark());
    let initial = dark(cx);
    for expected in [!initial, initial] {
        cx.update_window(handle.into(), |_, window, cx| {
            window.click("theme-toggle", cx)
        })
        .unwrap();
        assert_eq!(dark(cx), expected);
    }
}

#[gpui_kit::test]
fn summary_tracks_the_shared_store(cx: &mut TestAppContext) {
    let (handle, store) = open(cx, |_, cx| cx.new(SummaryView::new));
    let stats = |handle: WindowHandle<base::Root>, cx: &mut TestAppContext| {
        cx.update_window(handle.into(), |_, window, cx| {
            window.render_frame(cx);
            ["stat-total", "stat-active", "stat-done"]
                .map(|id| window.find(id).label().unwrap_or_default().to_string())
        })
        .unwrap()
    };
    assert_eq!(stats(handle, cx), ["2", "1", "1"]);

    // Mutations from elsewhere (e.g. the main window) re-render the summary.
    store.update(cx, |s, cx| {
        s.add("From another window");
        cx.notify();
    });
    assert_eq!(stats(handle, cx), ["3", "2", "1"]);
}
