//! Close requests and app exit: Tauri and GPUI vetoes, `destroy`, `quit`.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use crate::{
    ensure,
    support::{Ctx, QUIT_CODES, TestResult, WAIT},
};

crate::tests!["close" =>
    tauri_prevent_close_keeps_window_rendering,
    destroy_bypasses_tauri_veto,
    gpui_should_close_vetoes_close,
    gpui_should_close_allows_close,
    gpui_quit_requests_tauri_exit,
];

/// Opens a probe whose Tauri `CloseRequested` handler always vetoes.
fn open_vetoed(cx: &mut Ctx) -> (String, Arc<AtomicUsize>) {
    let label = cx.open_probe("tauri-veto", (300., 200.));
    let vetoes = Arc::new(AtomicUsize::new(0));
    let v = vetoes.clone();
    cx.window(&label, move |w| {
        w.on_window_event(move |event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                v.fetch_add(1, Ordering::SeqCst);
                api.prevent_close();
            }
        })
    });
    (label, vetoes)
}

fn tauri_prevent_close_keeps_window_rendering(cx: &mut Ctx) -> TestResult {
    let (label, vetoes) = open_vetoed(cx);
    cx.close(&label);
    ensure!(
        cx.wait(WAIT, |_| vetoes.load(Ordering::SeqCst) > 0),
        "never vetoed"
    );
    thread::sleep(Duration::from_millis(300));
    ensure!(cx.is_attached(&label), "detached after a vetoed close");
    let before = cx.seen(&label).unwrap().renders;
    ensure!(
        cx.wait_seen(&label, |s| s.renders > before),
        "stopped rendering"
    );
    Ok(())
}

fn destroy_bypasses_tauri_veto(cx: &mut Ctx) -> TestResult {
    let (label, _) = open_vetoed(cx);
    cx.destroy(&label);
    ensure!(cx.window_gone(&label));
    Ok(())
}

/// Registers a GPUI `on_window_should_close` answering `allow`; returns how
/// often GPUI was asked.
fn gpui_close_guard(cx: &mut Ctx, label: &str, allow: bool) -> Arc<AtomicUsize> {
    let asked = Arc::new(AtomicUsize::new(0));
    let a = asked.clone();
    cx.gpui_window(label, move |window, cx| {
        window.on_window_should_close(cx, move |_, _| {
            a.fetch_add(1, Ordering::SeqCst);
            allow
        })
    });
    asked
}

fn gpui_should_close_vetoes_close(cx: &mut Ctx) -> TestResult {
    let label = cx.open_probe("gpui-veto", (300., 200.));
    let asked = gpui_close_guard(cx, &label, false);
    cx.close(&label);
    ensure!(
        cx.wait(WAIT, |_| asked.load(Ordering::SeqCst) > 0),
        "GPUI was never asked"
    );
    thread::sleep(Duration::from_millis(300));
    ensure!(cx.is_attached(&label), "closed despite the GPUI veto");
    Ok(())
}

fn gpui_should_close_allows_close(cx: &mut Ctx) -> TestResult {
    let label = cx.open_probe("gpui-allow", (300., 200.));
    let asked = gpui_close_guard(cx, &label, true);
    cx.close(&label);
    ensure!(cx.window_gone(&label));
    ensure!(asked.load(Ordering::SeqCst) > 0, "GPUI was never asked");
    Ok(())
}

fn gpui_quit_requests_tauri_exit(cx: &mut Ctx) -> TestResult {
    *QUIT_CODES.lock().unwrap() = Some(Vec::new());
    cx.gpui(|cx| cx.quit());
    let requested = cx.wait(WAIT, |_| {
        QUIT_CODES
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|c| !c.is_empty())
    });
    let codes = QUIT_CODES.lock().unwrap().take();
    ensure!(
        requested && codes.as_deref() == Some(&[0]),
        "exit codes {codes:?}"
    );
    // A prevented exit leaves the shared App usable.
    let label = cx.open_probe("after-quit", (300., 200.));
    ensure!(cx.is_attached(&label));
    Ok(())
}
