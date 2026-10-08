//! GPUI's executors driven by the Tauri event loop, and cross-thread load.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use tauri_plugin_gpui::{GpuiError, gpui::AppContext as _};

use crate::{
    ensure,
    support::{Ctx, TestResult, probe},
};

crate::tests!["tasks" =>
    timer_resumes_foreground_task,
    task_and_timer_floods_complete,
    loop_stays_responsive_during_burst,
    cross_thread_calls_during_window_churn,
];

/// A background timer must wake the idle Tauri loop to resume a task.
fn timer_resumes_foreground_task(cx: &mut Ctx) -> TestResult {
    let done = Arc::new(AtomicBool::new(false));
    let d = done.clone();
    cx.gpui(move |cx| {
        cx.spawn(async move |cx| {
            cx.background_executor()
                .timer(Duration::from_millis(150))
                .await;
            let value = cx.background_spawn(async { 21 * 2 }).await;
            d.store(value == 42, Ordering::SeqCst);
        })
        .detach();
    });
    ensure!(cx.wait(Duration::from_secs(3), |_| done.load(Ordering::SeqCst)));
    Ok(())
}

fn task_and_timer_floods_complete(cx: &mut Ctx) -> TestResult {
    const TASKS: usize = 5000;
    const TIMERS: usize = 300;
    let (done, fired) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let (d, f) = (done.clone(), fired.clone());
    let delays: Vec<u64> = (0..TIMERS).map(|_| cx.rng.range(0..400)).collect();
    cx.gpui(move |cx| {
        for _ in 0..TASKS {
            let d = d.clone();
            cx.spawn(async move |_| d.fetch_add(1, Ordering::SeqCst))
                .detach();
        }
        for delay in delays {
            let f = f.clone();
            cx.spawn(async move |cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(delay))
                    .await;
                f.fetch_add(1, Ordering::SeqCst);
            })
            .detach();
        }
    });
    let all = cx.wait(Duration::from_secs(10), |_| {
        done.load(Ordering::SeqCst) == TASKS && fired.load(Ordering::SeqCst) == TIMERS
    });
    ensure!(
        all,
        "tasks {}/{TASKS}, timers {}/{TIMERS}",
        done.load(Ordering::SeqCst),
        fired.load(Ordering::SeqCst)
    );
    Ok(())
}

/// Draining is time-boxed, so a burst cannot starve the event loop.
fn loop_stays_responsive_during_burst(cx: &mut Ctx) -> TestResult {
    let start = Instant::now();
    cx.gpui(|cx| {
        for _ in 0..20_000 {
            cx.spawn(async move |_| {}).detach();
        }
    });
    cx.main(|_| ());
    let latency = start.elapsed();
    ensure!(
        latency < Duration::from_secs(5),
        "answered after {latency:?}"
    );
    Ok(())
}

fn cross_thread_calls_during_window_churn(cx: &mut Ctx) -> TestResult {
    const THREADS: usize = 4;
    const CALLS: usize = 150;
    let label = cx.open_probe("hammer", (300., 200.));
    let (ok, off_thread) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let workers: Vec<_> = (0..THREADS)
        .map(|_| {
            let (app, ok, off_thread, label) = (
                cx.app.clone(),
                ok.clone(),
                off_thread.clone(),
                label.clone(),
            );
            thread::spawn(move || {
                for _ in 0..CALLS {
                    if let Err(GpuiError::NotMainThread) = tauri_plugin_gpui::with_app(|_| ()) {
                        off_thread.fetch_add(1, Ordering::SeqCst);
                    }
                    let (ok, label) = (ok.clone(), label.clone());
                    app.run_on_main_thread(move || {
                        let r = tauri_plugin_gpui::with_app(|cx| {
                            if let Some(p) = probe(cx, &label) {
                                p.update(cx, |_, cx| cx.notify());
                            }
                        });
                        if r.is_ok() {
                            ok.fetch_add(1, Ordering::SeqCst);
                        }
                    })
                    .unwrap();
                }
            })
        })
        .collect();
    for _ in 0..4 {
        let churn = cx.open_probe("churn", (200., 150.));
        cx.close(&churn);
    }
    for w in workers {
        w.join().unwrap();
    }
    let total = THREADS * CALLS;
    let all = cx.wait(Duration::from_secs(10), |_| {
        ok.load(Ordering::SeqCst) == total
    });
    ensure!(
        all,
        "{}/{total} main-thread calls succeeded",
        ok.load(Ordering::SeqCst)
    );
    let off = off_thread.load(Ordering::SeqCst);
    ensure!(
        off == total,
        "{off}/{total} off-thread calls reported NotMainThread"
    );
    Ok(())
}
