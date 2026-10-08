//! Tauri facilities used from GPUI code and vice versa.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use tauri::{Emitter, Listener, Manager};

use crate::{
    ensure,
    support::{Counter, Ctx, TauriHandle, TestResult, WAIT, probe},
};

crate::tests!["tauri_apis" =>
    gpui_emits_tauri_events,
    background_events_update_gpui_state,
    managed_state_is_reachable_from_gpui,
    on_window_event_fires_for_gpui_windows,
    async_runtime_reaches_gpui,
];

fn gpui_emits_tauri_events(cx: &mut Ctx) -> TestResult {
    let received = Arc::new(Mutex::new(Vec::<String>::new()));
    let r = received.clone();
    let id = cx.main(move |app| {
        app.listen("from-gpui", move |event| {
            r.lock().unwrap().push(event.payload().to_string());
        })
    });
    cx.gpui(|cx| {
        cx.global::<TauriHandle>()
            .0
            .emit("from-gpui", "hello")
            .unwrap()
    });
    let got = cx.wait(WAIT, |_| {
        received.lock().unwrap().iter().any(|p| p == "\"hello\"")
    });
    cx.main(move |app| app.unlisten(id));
    ensure!(got, "listener saw {:?}", received.lock().unwrap());
    Ok(())
}

fn background_events_update_gpui_state(cx: &mut Ctx) -> TestResult {
    let label = cx.open_probe("events", (300., 200.));
    let l = label.clone();
    let id = cx.main(move |app| {
        let handle = app.clone();
        app.listen("to-gpui", move |event| {
            let (text, l) = (event.payload().to_string(), l.clone());
            handle
                .run_on_main_thread(move || {
                    tauri_plugin_gpui::with_app(|cx| {
                        if let Some(p) = probe(cx, &l) {
                            p.update(cx, |p, cx| {
                                p.push_text(&text);
                                cx.notify();
                            });
                        }
                    })
                    .unwrap();
                })
                .unwrap();
        })
    });
    let app = cx.app.clone();
    thread::spawn(move || app.emit("to-gpui", "evt").unwrap())
        .join()
        .unwrap();
    let updated = cx.wait_seen(&label, |s| s.text.contains("\"evt\""));
    cx.main(move |app| app.unlisten(id));
    ensure!(updated, "GPUI state never changed");
    Ok(())
}

fn managed_state_is_reachable_from_gpui(cx: &mut Ctx) -> TestResult {
    let bump = |cx: &mut Ctx| {
        cx.gpui(|cx| {
            let app = cx.global::<TauriHandle>().0.clone();
            app.state::<Counter>().0.fetch_add(1, Ordering::SeqCst) + 1
        })
    };
    let first = bump(cx);
    let second = bump(cx);
    ensure!(second == first + 1, "{first} then {second}");
    Ok(())
}

fn on_window_event_fires_for_gpui_windows(cx: &mut Ctx) -> TestResult {
    let label = cx.open_probe("window-event", (300., 200.));
    let resized = Arc::new(AtomicUsize::new(0));
    let r = resized.clone();
    cx.window(&label, move |w| {
        w.on_window_event(move |event| {
            if let tauri::WindowEvent::Resized(_) = event {
                r.fetch_add(1, Ordering::SeqCst);
            }
        });
        w.set_size(tauri::LogicalSize::new(333., 222.)).ok();
    });
    ensure!(
        cx.wait(WAIT, |_| resized.load(Ordering::SeqCst) > 0),
        "no Resized event"
    );
    Ok(())
}

fn async_runtime_reaches_gpui(cx: &mut Ctx) -> TestResult {
    let (tx, rx) = mpsc::channel();
    let app = cx.app.clone();
    tauri::async_runtime::spawn(async move {
        app.run_on_main_thread(move || {
            let result = tauri_plugin_gpui::with_app(|cx| cx.windows().len());
            tx.send(result.is_ok()).ok();
        })
        .ok();
    });
    let ok = rx.recv_timeout(Duration::from_secs(3)).unwrap_or(false);
    ensure!(ok, "with_app failed from an async_runtime task");
    Ok(())
}
