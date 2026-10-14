//! Guards for code that runs where a panic or a poisoned lock would abort the
//! process: callbacks from Java (JNI) and Objective-C, and the locks they share
//! with the event loop.

#![cfg_attr(not(gpui_mobile), allow(dead_code))]

use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Mutex, MutexGuard, PoisonError},
};

/// Locks `mutex`, recovering the data if a panic poisoned it. Every lock in
/// the plugin guards plain data (queues, flags, handles) that stays valid when
/// a holder panics, so recovering is always sound and a panic elsewhere can't
/// cascade into every later caller.
pub(crate) fn lock<T: ?Sized>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Runs the body of a foreign callback. A panic must not unwind into Java or
/// Objective-C frames (Rust aborts the process when it reaches an `extern "C"`
/// boundary), so it is logged and the callback returns `fallback` instead.
pub(crate) fn callback<R>(name: &str, fallback: R, f: impl FnOnce() -> R) -> R {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(value) => value,
        Err(payload) => {
            let message = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("non-string panic payload");
            log::error!("tauri-plugin-gpui: panic in {name}: {message}");
            fallback
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_poisoned_lock_still_hands_out_its_data() {
        let mutex = Mutex::new(1);
        let _ = std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    let _guard = mutex.lock().unwrap();
                    panic!("poison");
                })
                .join()
        });
        assert!(mutex.is_poisoned());
        *lock(&mutex) += 1;
        assert_eq!(*lock(&mutex), 2);
    }

    #[test]
    fn a_panicking_callback_returns_the_fallback() {
        assert_eq!(callback("ok", 0, || 7), 7);
        assert_eq!(callback("panics", 0, || panic!("boom")), 0);
        assert!(!callback("panics", false, || -> bool {
            panic!("{}", String::from("x"))
        }));
    }
}
