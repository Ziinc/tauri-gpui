//! Window state flowing both ways between Tauri and GPUI.

use std::time::Duration;

use tauri_plugin_gpui::gpui::WindowAppearance;

use crate::{
    ensure,
    support::{Ctx, TestResult, WAIT},
};

crate::tests!["window_state" =>
    starts_with_tauri_mode_and_theme,
    resize_storm_ends_on_final_size,
    hide_show_keeps_rendering,
    tauri_set_position_moves_gpui_bounds,
    gpui_set_title_renames_tauri_window,
    tauri_set_theme_reaches_gpui,
    tauri_maximize_reaches_gpui,
    tauri_fullscreen_reaches_gpui,
    tauri_set_focus_activates_gpui_window,
    gpui_zoom_maximizes,
    gpui_toggle_fullscreen_fullscreens,
    gpui_minimize_minimizes,
    gpui_activate_focuses,
];

fn starts_with_tauri_mode_and_theme(cx: &mut Ctx) -> TestResult {
    let label = cx.open_probe("initial", (400., 300.));
    let theme = cx.window(&label, |w| w.theme().unwrap()).unwrap();
    let expected = match theme {
        tauri::Theme::Dark => WindowAppearance::Dark,
        _ => WindowAppearance::Light,
    };
    let seen = cx.seen(&label).unwrap();
    ensure!(
        !seen.maximized && !seen.fullscreen && seen.appearance == Some(expected),
        "{seen:?}"
    );
    Ok(())
}

fn resize_storm_ends_on_final_size(cx: &mut Ctx) -> TestResult {
    let label = cx.open_probe("resize", (400., 300.));
    let mut last = (400., 300.);
    for _ in 0..40 {
        last = cx.random_size(150..900, 120..700);
        cx.window(&label, move |w| {
            w.set_size(tauri::LogicalSize::new(last.0, last.1)).ok()
        });
        if cx.rng.range(0..3) == 0 {
            std::thread::sleep(Duration::from_millis(cx.rng.range(0..30)));
        }
    }
    let expected = (last.0 as f32, last.1 as f32);
    let settled = cx.wait_seen(&label, |s| s.viewport == expected);
    let seen = cx.seen(&label).unwrap_or_default();
    ensure!(settled, "final {expected:?}, GPUI {:?}", seen.viewport);
    Ok(())
}

fn hide_show_keeps_rendering(cx: &mut Ctx) -> TestResult {
    let label = cx.open_probe("hide", (300., 200.));
    for _ in 0..5 {
        cx.window(&label, |w| w.hide().ok());
        cx.refresh(&label);
        std::thread::sleep(Duration::from_millis(cx.rng.range(0..60)));
        cx.window(&label, |w| w.show().ok());
    }
    let before = cx.seen(&label).unwrap().renders;
    ensure!(
        cx.wait_seen(&label, |s| s.renders > before),
        "no frame after show"
    );
    ensure!(cx.is_attached(&label));
    Ok(())
}

fn tauri_set_position_moves_gpui_bounds(cx: &mut Ctx) -> TestResult {
    let label = cx.open_probe("move", (300., 200.));
    cx.window(&label, |w| {
        w.set_position(tauri::LogicalPosition::new(123., 77.)).ok()
    });
    let moved = cx.wait(WAIT, |cx| {
        let tauri = cx.window(&label, |w| {
            let p = w.outer_position().unwrap();
            let s = w.scale_factor().unwrap();
            ((p.x as f64 / s) as f32, (p.y as f64 / s) as f32)
        });
        let gpui = cx.gpui_window(&label, |window, _| {
            let o = window.bounds().origin;
            (f32::from(o.x), f32::from(o.y))
        });
        gpui.is_some() && gpui == tauri
    });
    ensure!(moved, "GPUI bounds never matched the Tauri position");
    Ok(())
}

fn gpui_set_title_renames_tauri_window(cx: &mut Ctx) -> TestResult {
    let label = cx.open_probe("title", (300., 200.));
    cx.gpui_window(&label, |window, _| window.set_window_title("Renamed"));
    let renamed = cx.wait(WAIT, |cx| {
        cx.window(&label, |w| w.title().unwrap()).as_deref() == Some("Renamed")
    });
    ensure!(renamed);
    Ok(())
}

fn tauri_set_theme_reaches_gpui(cx: &mut Ctx) -> TestResult {
    let label = cx.open_probe("theme", (300., 200.));
    cx.window(&label, |w| w.set_theme(Some(tauri::Theme::Dark)).ok());
    let dark = cx.wait_seen(&label, |s| {
        matches!(
            s.appearance,
            Some(WindowAppearance::Dark | WindowAppearance::VibrantDark)
        )
    });
    // GTK themes are application-wide: restore for later tests.
    cx.window(&label, |w| w.set_theme(Some(tauri::Theme::Light)).ok());
    ensure!(dark, "GPUI never saw the dark theme");
    ensure!(cx.wait_seen(&label, |s| s.appearance == Some(WindowAppearance::Light)));
    Ok(())
}

fn tauri_maximize_reaches_gpui(cx: &mut Ctx) -> TestResult {
    cx.require_wm()?;
    let label = cx.open_probe("maximize", (400., 300.));
    cx.window(&label, |w| w.maximize().ok());
    ensure!(
        cx.wait(WAIT, |cx| tauri_state(cx, &label).maximized),
        "Tauri never maximized"
    );
    ensure!(
        cx.wait_seen(&label, |s| s.maximized),
        "GPUI is_maximized stayed false"
    );
    cx.window(&label, |w| w.unmaximize().ok());
    ensure!(
        cx.wait_seen(&label, |s| !s.maximized),
        "GPUI is_maximized stayed true"
    );
    Ok(())
}

fn tauri_fullscreen_reaches_gpui(cx: &mut Ctx) -> TestResult {
    cx.require_wm()?;
    let label = cx.open_probe("fullscreen", (400., 300.));
    cx.window(&label, |w| w.set_fullscreen(true).ok());
    ensure!(
        cx.wait_seen(&label, |s| s.fullscreen),
        "GPUI is_fullscreen stayed false"
    );
    cx.window(&label, |w| w.set_fullscreen(false).ok());
    ensure!(
        cx.wait_seen(&label, |s| !s.fullscreen),
        "GPUI is_fullscreen stayed true"
    );
    Ok(())
}

fn tauri_set_focus_activates_gpui_window(cx: &mut Ctx) -> TestResult {
    cx.require_wm()?;
    let a = cx.open_probe("focus-a", (300., 200.));
    let b = cx.open_probe("focus-b", (300., 200.));
    for target in [&a, &b] {
        cx.window(target, |w| w.set_focus().ok());
        ensure!(
            cx.wait_seen(target, |s| s.active),
            "`{target}` never became active"
        );
    }
    ensure!(cx.wait_seen(&a, |s| !s.active), "`{a}` stayed active");
    Ok(())
}

fn gpui_zoom_maximizes(cx: &mut Ctx) -> TestResult {
    gpui_window_op(cx, |w| w.zoom_window(), |s| s.maximized)
}

fn gpui_toggle_fullscreen_fullscreens(cx: &mut Ctx) -> TestResult {
    gpui_window_op(cx, |w| w.toggle_fullscreen(), |s| s.fullscreen)
}

fn gpui_minimize_minimizes(cx: &mut Ctx) -> TestResult {
    gpui_window_op(cx, |w| w.minimize_window(), |s| s.minimized)
}

fn gpui_activate_focuses(cx: &mut Ctx) -> TestResult {
    cx.require_wm()?;
    let label = cx.open_probe("activate", (300., 200.));
    cx.open_probe("activate-other", (300., 200.));
    cx.gpui_window(&label, |w, _| w.activate_window());
    ensure!(
        cx.wait(WAIT, |cx| tauri_state(cx, &label).focused),
        "never focused"
    );
    Ok(())
}

/// Runs a GPUI window operation and waits until Tauri reports `done`.
fn gpui_window_op(
    cx: &mut Ctx,
    op: fn(&mut tauri_plugin_gpui::gpui::Window),
    done: fn(TauriState) -> bool,
) -> TestResult {
    cx.require_wm()?;
    let label = cx.open_probe("op", (300., 200.));
    cx.gpui_window(&label, move |w, _| op(w));
    ensure!(
        cx.wait(WAIT, |cx| done(tauri_state(cx, &label))),
        "Tauri never applied it"
    );
    Ok(())
}

/// Window state as Tauri reports it.
#[derive(Default)]
struct TauriState {
    maximized: bool,
    fullscreen: bool,
    minimized: bool,
    focused: bool,
}

fn tauri_state(cx: &Ctx, label: &str) -> TauriState {
    cx.window(label, |w| TauriState {
        maximized: w.is_maximized().unwrap(),
        fullscreen: w.is_fullscreen().unwrap(),
        minimized: w.is_minimized().unwrap(),
        focused: w.is_focused().unwrap(),
    })
    .unwrap_or_default()
}
