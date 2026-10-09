//! The public API's documented errors.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use tauri::{WebviewUrl, WebviewWindowBuilder};
use tauri_plugin_gpui::{
    GpuiError, GpuiWindowExt,
    gpui::{AppContext as _, Context, IntoElement, Render, Window, div},
};

use crate::{
    ensure,
    support::{Ctx, INIT_ERRORS, Probe, TestResult, WAIT, attach_probe},
};

crate::tests!["api" =>
    with_app_before_init_fails,
    second_init_fails,
    with_app_off_main_thread_fails,
    attach_off_main_thread_fails,
    second_attach_fails,
    with_app_inside_gpui_is_reentrant,
    with_app_flushes_queued_effects,
    gpui_open_window_is_unsupported,
    webview_window_is_not_eligible,
];

fn with_app_before_init_fails(_: &mut Ctx) -> TestResult {
    let errors = INIT_ERRORS.get().unwrap().lock().unwrap();
    ensure!(
        matches!(errors.before_init, Some(GpuiError::NotInitialized)),
        "{:?}",
        errors.before_init
    );
    Ok(())
}

fn second_init_fails(_: &mut Ctx) -> TestResult {
    let errors = INIT_ERRORS.get().unwrap().lock().unwrap();
    ensure!(
        matches!(errors.second_init, Some(GpuiError::AlreadyInitialized)),
        "{:?}",
        errors.second_init
    );
    Ok(())
}

fn with_app_off_main_thread_fails(_: &mut Ctx) -> TestResult {
    let error = tauri_plugin_gpui::with_app(|_| ()).err();
    ensure!(matches!(error, Some(GpuiError::NotMainThread)), "{error:?}");
    Ok(())
}

fn attach_off_main_thread_fails(cx: &mut Ctx) -> TestResult {
    let label = cx.label("off-thread");
    let window = cx.create_window(&label, (300., 200.));
    let error = attach_probe(&window).err();
    ensure!(matches!(error, Some(GpuiError::NotMainThread)), "{error:?}");
    ensure!(!window.is_gpui_attached(), "attached off the main thread");
    ensure!(!cx.is_attached(&label), "attached after a failed attach");
    Ok(())
}

fn second_attach_fails(cx: &mut Ctx) -> TestResult {
    let label = cx.label("twice");
    let window = cx.create_window(&label, (300., 200.));
    let (first, second) =
        cx.main(move |_| (attach_probe(&window).err(), attach_probe(&window).err()));
    ensure!(first.is_none(), "first attach: {first:?}");
    ensure!(
        matches!(second, Some(GpuiError::AlreadyAttached { .. })),
        "{second:?}"
    );
    ensure!(cx.is_attached(&label));
    ensure!(cx.rendered(&label), "attached window never rendered");
    Ok(())
}

fn with_app_inside_gpui_is_reentrant(cx: &mut Ctx) -> TestResult {
    let error = cx.gpui(|_| tauri_plugin_gpui::with_app(|_| ()).err());
    ensure!(matches!(error, Some(GpuiError::Reentrant)), "{error:?}");
    Ok(())
}

/// Runs with no window open (the runner closes them between tests), so
/// nothing else can trigger the GPUI update that would flush the effect.
fn with_app_flushes_queued_effects(cx: &mut Ctx) -> TestResult {
    let ran = Arc::new(AtomicBool::new(false));
    let r = ran.clone();
    cx.gpui(move |cx| cx.defer(move |_| r.store(true, Ordering::SeqCst)));
    ensure!(
        cx.wait(WAIT, |_| ran.load(Ordering::SeqCst)),
        "a cx.defer queued in with_app never ran"
    );
    Ok(())
}

fn gpui_open_window_is_unsupported(cx: &mut Ctx) -> TestResult {
    let error = cx.gpui(|cx| {
        cx.open_window(Default::default(), |window, cx| {
            cx.new(|cx| Probe::new(window, cx))
        })
        .err()
        .map(|e| format!("{e:#}"))
    });
    ensure!(
        error
            .as_deref()
            .is_some_and(|e| e.contains("unsupported operation")),
        "{error:?}"
    );
    Ok(())
}

struct Empty;
impl Render for Empty {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

fn webview_window_is_not_eligible(cx: &mut Ctx) -> TestResult {
    let label = cx.label("webview");
    let l = label.clone();
    let error = cx.main(move |app| {
        let window = WebviewWindowBuilder::new(app, &l, WebviewUrl::App("index.html".into()))
            .inner_size(300., 200.)
            .build()
            .expect("webview window");
        window
            .as_ref()
            .window()
            .attach_gpui(|cx| cx.new(|_| Empty))
            .err()
    });
    ensure!(
        matches!(error, Some(GpuiError::NotEligible { .. })),
        "{error:?}"
    );
    ensure!(
        cx.window(&label, |_| ()).is_some(),
        "the WebView window did not survive the rejected attach"
    );
    Ok(())
}
