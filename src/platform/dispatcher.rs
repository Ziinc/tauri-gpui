//! GPUI task dispatcher backed by the Tauri/TAO event loop.
//!
//! Main-thread runnables are queued and drained by the plugin from inside the
//! TAO event callback; no competing UI loop is created. Background work runs
//! on a small worker pool, timers on a dedicated timer thread.

use std::{
    collections::BinaryHeap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    thread::{self, ThreadId},
    time::{Duration, Instant},
};

use gpui::{
    PlatformDispatcher, Priority, PriorityQueueReceiver, PriorityQueueSender, RunnableVariant,
};

/// Wakes the TAO event loop so queued GPUI work gets drained.
pub(crate) struct LoopWaker {
    wake: Mutex<Option<Box<dyn Fn() + Send>>>,
    pending: AtomicBool,
}

impl LoopWaker {
    pub(crate) fn new(fallback: Box<dyn Fn() + Send>) -> Self {
        Self {
            wake: Mutex::new(Some(fallback)),
            pending: AtomicBool::new(false),
        }
    }

    /// Replaces the wake function (with one that posts through the TAO proxy).
    pub(crate) fn set(&self, wake: Box<dyn Fn() + Send>) {
        *self.wake.lock().unwrap() = Some(wake);
    }

    /// Requests one event-loop iteration. Coalesced until the next drain.
    pub(crate) fn wake(&self) {
        if !self.pending.swap(true, Ordering::AcqRel)
            && let Some(wake) = &*self.wake.lock().unwrap()
        {
            wake();
        }
    }

    pub(crate) fn clear(&self) {
        self.pending.store(false, Ordering::Release);
    }
}

struct Timer {
    at: Instant,
    seq: u64,
    runnable: RunnableVariant,
}

impl PartialEq for Timer {
    fn eq(&self, other: &Self) -> bool {
        (self.at, self.seq) == (other.at, other.seq)
    }
}
impl Eq for Timer {}
impl PartialOrd for Timer {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Timer {
    // Reversed so the BinaryHeap pops the earliest deadline first.
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        (other.at, other.seq).cmp(&(self.at, self.seq))
    }
}

pub(crate) struct TauriDispatcher {
    main_thread: ThreadId,
    main_sender: PriorityQueueSender<RunnableVariant>,
    main_receiver: Mutex<PriorityQueueReceiver<RunnableVariant>>,
    main_len: AtomicUsize,
    background_sender: PriorityQueueSender<RunnableVariant>,
    timer_sender: Mutex<mpsc::Sender<(Duration, RunnableVariant)>>,
    waker: Arc<LoopWaker>,
}

impl TauriDispatcher {
    pub(crate) fn new(waker: Arc<LoopWaker>) -> Self {
        let (main_sender, main_receiver) = PriorityQueueReceiver::new();
        let (background_sender, background_receiver) = PriorityQueueReceiver::new();

        let threads = thread::available_parallelism().map_or(2, |n| n.get().max(2));
        for i in 0..threads {
            let receiver: PriorityQueueReceiver<RunnableVariant> = background_receiver.clone();
            thread::Builder::new()
                .name(format!("gpui-worker-{i}"))
                .spawn(move || {
                    for runnable in receiver.iter() {
                        runnable.run();
                    }
                })
                .expect("failed to spawn GPUI worker thread");
        }

        let (timer_sender, timer_receiver) = mpsc::channel::<(Duration, RunnableVariant)>();
        thread::Builder::new()
            .name("gpui-timer".into())
            .spawn(move || timer_loop(timer_receiver))
            .expect("failed to spawn GPUI timer thread");

        Self {
            main_thread: thread::current().id(),
            main_sender,
            main_receiver: Mutex::new(main_receiver),
            main_len: AtomicUsize::new(0),
            background_sender,
            timer_sender: Mutex::new(timer_sender),
            waker,
        }
    }

    /// Pops the next main-thread runnable. Must be called on the main thread.
    pub(crate) fn pop_main(&self) -> Option<RunnableVariant> {
        debug_assert!(self.is_main_thread());
        let runnable = self.main_receiver.lock().unwrap().try_pop().ok().flatten();
        if runnable.is_some() {
            self.main_len.fetch_sub(1, Ordering::AcqRel);
        }
        runnable
    }

    pub(crate) fn has_main_work(&self) -> bool {
        self.main_len.load(Ordering::Acquire) > 0
    }
}

fn timer_loop(receiver: mpsc::Receiver<(Duration, RunnableVariant)>) {
    let mut heap = BinaryHeap::<Timer>::new();
    let mut seq = 0u64;
    loop {
        let message = match heap.peek() {
            Some(next) => {
                let timeout = next.at.saturating_duration_since(Instant::now());
                match receiver.recv_timeout(timeout) {
                    Ok(message) => Some(message),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                }
            }
            None => match receiver.recv() {
                Ok(message) => Some(message),
                Err(_) => return,
            },
        };
        if let Some((duration, runnable)) = message {
            seq += 1;
            heap.push(Timer {
                at: Instant::now() + duration,
                seq,
                runnable,
            });
        }
        while heap.peek().is_some_and(|t| t.at <= Instant::now()) {
            // Timer runnables only complete a Send future that wakes the
            // awaiting task, so running them off the main thread is fine.
            heap.pop().unwrap().runnable.run();
        }
    }
}

impl PlatformDispatcher for TauriDispatcher {
    fn is_main_thread(&self) -> bool {
        thread::current().id() == self.main_thread
    }

    fn dispatch(&self, runnable: RunnableVariant, priority: Priority) {
        if self.background_sender.send(priority, runnable).is_err() {
            log::error!("GPUI background executor is shut down");
        }
    }

    fn dispatch_on_main_thread(&self, runnable: RunnableVariant, priority: Priority) {
        match self.main_sender.send(priority, runnable) {
            Ok(()) => {
                self.main_len.fetch_add(1, Ordering::AcqRel);
                self.waker.wake();
            }
            // The runnable may wrap a !Send future; never drop it off-thread.
            Err(error) => std::mem::forget(error),
        }
    }

    fn dispatch_after(&self, duration: Duration, runnable: RunnableVariant) {
        if let Err(error) = self.timer_sender.lock().unwrap().send((duration, runnable)) {
            std::mem::forget(error);
        }
    }

    fn spawn_realtime(&self, f: Box<dyn FnOnce() + Send>) {
        thread::spawn(f);
    }
}
