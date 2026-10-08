//! Attaching, tearing down and recreating GPUI-backed windows. The runner
//! fails any test that leaks a GPUI window or root.

use crate::{
    ensure,
    support::{Ctx, TauriHandle, TestResult, attach_probe, probe},
};

crate::tests!["lifecycle" =>
    random_storm_releases_every_window,
    attach_then_close_in_one_turn,
    attach_then_destroy_in_one_turn,
    deferred_attach_then_destroy,
    deferred_attach_then_close,
    many_windows_torn_down_in_one_turn,
    label_is_reusable,
    gpui_remove_window_destroys_tauri_window,
    attach_from_gpui_task,
    attach_from_gpui_defer,
];

/// Random create/attach then close, destroy, GPUI `remove_window` or leave
/// open; afterwards the shared App still attaches new windows.
fn random_storm_releases_every_window(cx: &mut Ctx) -> TestResult {
    let mut labels = Vec::new();
    for _ in 0..16 {
        let size = cx.random_size(120..700, 90..500);
        let label = cx.open_probe("storm", size);
        match cx.rng.range(0..4) {
            0 => cx.close(&label),
            1 => cx.destroy(&label),
            2 => {
                cx.gpui_window(&label, |window, _| window.remove_window());
            }
            _ => {}
        }
        labels.push(label);
    }
    for label in &labels {
        cx.close(label);
    }
    for label in &labels {
        ensure!(cx.window_gone(label), "`{label}` was not closed");
    }
    let fresh = cx.open_probe("after-storm", (300., 200.));
    cx.close(&fresh);
    ensure!(cx.window_gone(&fresh));
    Ok(())
}

fn attach_then_close_in_one_turn(cx: &mut Ctx) -> TestResult {
    attach_then(cx, |w| w.close())
}

fn attach_then_destroy_in_one_turn(cx: &mut Ctx) -> TestResult {
    attach_then(cx, |w| w.destroy())
}

fn attach_then(cx: &mut Ctx, teardown: fn(&tauri::Window) -> tauri::Result<()>) -> TestResult {
    let label = cx.label("same-turn");
    let window = cx.create_window(&label, (300., 200.));
    let error = cx.main(move |_| {
        let error = attach_probe(&window).err();
        teardown(&window).ok();
        error
    });
    ensure!(error.is_none(), "{error:?}");
    ensure!(cx.window_gone(&label));
    Ok(())
}

/// Attaching from inside GPUI defers the mount; tearing the window down
/// before it runs must drop the mount rather than render into a dead window.
fn deferred_attach_then_destroy(cx: &mut Ctx) -> TestResult {
    deferred_attach_then(cx, |w| w.destroy())
}

fn deferred_attach_then_close(cx: &mut Ctx) -> TestResult {
    deferred_attach_then(cx, |w| w.close())
}

fn deferred_attach_then(
    cx: &mut Ctx,
    teardown: fn(&tauri::Window) -> tauri::Result<()>,
) -> TestResult {
    let label = cx.label("deferred");
    let window = cx.create_window(&label, (300., 200.));
    let error = cx.gpui(move |_| {
        let error = attach_probe(&window).err();
        teardown(&window).ok();
        error
    });
    ensure!(error.is_none(), "deferred attach rejected: {error:?}");
    ensure!(cx.window_gone(&label));
    Ok(())
}

fn many_windows_torn_down_in_one_turn(cx: &mut Ctx) -> TestResult {
    let labels: Vec<String> = (0..10)
        .map(|_| {
            let size = cx.random_size(150..400, 100..300);
            cx.open_probe("many", size)
        })
        .collect();
    let count = cx.gpui_window_count();
    ensure!(count == 10, "the shared App sees {count} windows");
    let ls = labels.clone();
    cx.main(move |app| {
        use tauri::Manager;
        for (i, l) in ls.iter().enumerate() {
            let w = app.get_window(l).unwrap();
            if i % 2 == 0 {
                w.close().ok()
            } else {
                w.destroy().ok()
            };
        }
    });
    for label in &labels {
        ensure!(cx.window_gone(label), "`{label}` survived");
    }
    Ok(())
}

fn label_is_reusable(cx: &mut Ctx) -> TestResult {
    for _ in 0..3 {
        let window = cx.create_window("reused", (300., 200.));
        let error = cx.main(move |_| attach_probe(&window).err());
        ensure!(error.is_none(), "{error:?}");
        ensure!(cx.rendered("reused"), "a reused label never rendered");
        cx.close("reused");
        ensure!(cx.window_gone("reused"));
    }
    Ok(())
}

/// GPUI has already torn its side down, so a Tauri close veto must not keep
/// a blank window alive.
fn gpui_remove_window_destroys_tauri_window(cx: &mut Ctx) -> TestResult {
    let label = cx.open_probe("remove", (300., 200.));
    cx.window(&label, |w| {
        w.on_window_event(|event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
            }
        })
    });
    cx.gpui_window(&label, |window, _| window.remove_window());
    ensure!(
        cx.window_gone(&label),
        "the Tauri window outlived its GPUI window"
    );
    Ok(())
}

fn attach_from_gpui_task(cx: &mut Ctx) -> TestResult {
    attach_from_gpui(cx, |cx, attach| cx.spawn(async move |_| attach()).detach())
}

fn attach_from_gpui_defer(cx: &mut Ctx) -> TestResult {
    attach_from_gpui(cx, |cx, attach| cx.defer(move |_| attach()))
}

/// Builds a Tauri window and attaches GPUI from a GPUI async context.
fn attach_from_gpui(
    cx: &mut Ctx,
    schedule: fn(&mut tauri_plugin_gpui::gpui::App, Box<dyn FnOnce()>),
) -> TestResult {
    let label = cx.label("from-gpui");
    let l = label.clone();
    cx.gpui(move |cx| {
        let app = cx.global::<TauriHandle>().0.clone();
        schedule(
            cx,
            Box::new(move || {
                let w = tauri::WindowBuilder::new(&app, &l)
                    .title(&l)
                    .build()
                    .unwrap();
                attach_probe(&w).unwrap();
            }),
        );
    });
    ensure!(
        cx.rendered(&label),
        "the window attached from GPUI never rendered"
    );
    let l = label.clone();
    ensure!(cx.gpui(move |cx| probe(cx, &l).is_some()));
    Ok(())
}
