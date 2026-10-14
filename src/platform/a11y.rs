//! Hands the AccessKit tree GPUI builds to the desktop accessibility API:
//! AT-SPI on Linux, NSAccessibility on macOS and UI Automation on Windows.
//!
//! GPUI's own platforms own their native view or window procedure. Here TAO
//! does, so macOS uses AccessKit's subclassing adapter on TAO's `NSView` and
//! Windows hooks `WM_GETOBJECT` with a comctl32 subclass of TAO's `HWND`.
//!
//! The adapters may call the handlers on other threads; GPUI's callbacks
//! only flip a flag and send on channels, which wake the event loop.

use gpui::{A11yCallbacks, accesskit};

use super::window::RawWindow;

struct ActivationHandler(Box<dyn Fn() -> Option<accesskit::TreeUpdate> + Send>);

impl accesskit::ActivationHandler for ActivationHandler {
    fn request_initial_tree(&mut self) -> Option<accesskit::TreeUpdate> {
        (self.0)()
    }
}

struct ActionHandler(Box<dyn Fn(accesskit::ActionRequest) + Send>);

impl accesskit::ActionHandler for ActionHandler {
    fn do_action(&mut self, request: accesskit::ActionRequest) {
        (self.0)(request);
    }
}

#[cfg_attr(not(any(target_os = "linux", target_os = "freebsd")), allow(dead_code))]
struct DeactivationHandler(Box<dyn Fn() + Send>);

impl accesskit::DeactivationHandler for DeactivationHandler {
    fn deactivate_accessibility(&mut self) {
        (self.0)();
    }
}

/// The platform adapter of one window. Main thread only.
pub(crate) struct A11yAdapter(imp::Adapter);

impl A11yAdapter {
    /// `None` when the window handle is not the kind this OS expects.
    pub fn new(raw: RawWindow, focused: bool, callbacks: A11yCallbacks) -> Option<Self> {
        let A11yCallbacks {
            activation,
            action,
            deactivation,
        } = callbacks;
        imp::Adapter::new(
            raw,
            focused,
            ActivationHandler(activation),
            ActionHandler(action),
            DeactivationHandler(deactivation),
        )
        .map(Self)
    }

    /// Applies a tree update if assistive technology is listening.
    pub fn update(&mut self, update: accesskit::TreeUpdate) {
        self.0.update(update);
    }

    pub fn set_focused(&mut self, focused: bool) {
        self.0.set_focused(focused);
    }

    /// Screen rectangles of the window with (`outer`) and without (`inner`)
    /// its decorations, in physical pixels. Only AT-SPI needs them: the
    /// macOS and Windows APIs ask the view or `HWND` for its frame.
    pub fn set_bounds(&mut self, outer: accesskit::Rect, inner: accesskit::Rect) {
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        self.0.set_bounds(outer, inner);
        #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
        let _ = (outer, inner);
    }
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
mod imp {
    use super::*;

    pub struct Adapter(accesskit_unix::Adapter);

    impl Adapter {
        pub fn new(
            _raw: RawWindow,
            focused: bool,
            activation: ActivationHandler,
            action: ActionHandler,
            deactivation: DeactivationHandler,
        ) -> Option<Self> {
            // Talks to the AT-SPI bus from its own thread; the window needs
            // no native hook.
            let mut adapter = accesskit_unix::Adapter::new(activation, action, deactivation);
            adapter.update_window_focus_state(focused);
            Some(Self(adapter))
        }

        pub fn update(&mut self, update: accesskit::TreeUpdate) {
            self.0.update_if_active(|| update);
        }

        pub fn set_focused(&mut self, focused: bool) {
            self.0.update_window_focus_state(focused);
        }

        pub fn set_bounds(&mut self, outer: accesskit::Rect, inner: accesskit::Rect) {
            self.0.set_root_window_bounds(outer, inner);
        }
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use raw_window_handle::RawWindowHandle;

    use super::*;

    pub struct Adapter(accesskit_macos::SubclassingAdapter);

    impl Adapter {
        pub fn new(
            raw: RawWindow,
            focused: bool,
            activation: ActivationHandler,
            action: ActionHandler,
            _deactivation: DeactivationHandler,
        ) -> Option<Self> {
            let RawWindowHandle::AppKit(handle) = raw.window else {
                return None;
            };
            // SAFETY: TAO's content view, alive until Tauri destroys the
            // window; the adapter retains it and restores its class on drop.
            let adapter = unsafe {
                accesskit_macos::SubclassingAdapter::new(
                    handle.ns_view.as_ptr(),
                    activation,
                    action,
                )
            };
            let mut adapter = Self(adapter);
            adapter.set_focused(focused);
            Some(adapter)
        }

        pub fn update(&mut self, update: accesskit::TreeUpdate) {
            if let Some(events) = self.0.update_if_active(|| update) {
                events.raise();
            }
        }

        pub fn set_focused(&mut self, focused: bool) {
            if let Some(events) = self.0.update_view_focus_state(focused) {
                events.raise();
            }
        }
    }
}

#[cfg(target_os = "windows")]
mod imp {
    use std::cell::RefCell;

    use accesskit_windows::{HWND as AkHwnd, LPARAM as AkLparam, WPARAM as AkWparam};
    use raw_window_handle::RawWindowHandle;
    use windows::Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        UI::{
            Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass},
            WindowsAndMessaging::{WM_GETOBJECT, WM_NCDESTROY},
        },
    };

    use super::*;

    /// Identifies our subclass among the others on TAO's window.
    const SUBCLASS_ID: usize = 0x6131_3179; // "a11y"

    struct State {
        adapter: accesskit_windows::Adapter,
        activation: ActivationHandler,
    }

    /// Unlike `accesskit_windows::SubclassingAdapter`, which panics on a
    /// visible window (Tauri windows usually are by attach time), a comctl32
    /// subclass can be added at any time and chains with TAO's own.
    pub struct Adapter {
        hwnd: HWND,
        /// Boxed: the subclass procedure holds its address.
        state: Box<RefCell<State>>,
    }

    impl Adapter {
        pub fn new(
            raw: RawWindow,
            focused: bool,
            activation: ActivationHandler,
            action: ActionHandler,
            _deactivation: DeactivationHandler,
        ) -> Option<Self> {
            let RawWindowHandle::Win32(handle) = raw.window else {
                return None;
            };
            let hwnd = HWND(handle.hwnd.get() as *mut core::ffi::c_void);
            let adapter = accesskit_windows::Adapter::new(AkHwnd(hwnd.0), focused, action);
            let state = Box::new(RefCell::new(State {
                adapter,
                activation,
            }));
            // SAFETY: called on the thread that owns the window; `state`
            // outlives the subclass, which `Drop` (or `WM_NCDESTROY`) removes.
            let installed = unsafe {
                SetWindowSubclass(
                    hwnd,
                    Some(subclass_proc),
                    SUBCLASS_ID,
                    &*state as *const RefCell<State> as usize,
                )
            };
            if !installed.as_bool() {
                log::warn!("tauri-plugin-gpui: could not subclass the window for UI Automation");
                return None;
            }
            Some(Self { hwnd, state })
        }

        pub fn update(&mut self, update: accesskit::TreeUpdate) {
            let events = self.state.borrow_mut().adapter.update_if_active(|| update);
            // Raised after the borrow ends: raising may send a nested
            // `WM_GETOBJECT`, which borrows the state again.
            if let Some(events) = events {
                events.raise();
            }
        }

        pub fn set_focused(&mut self, focused: bool) {
            let events = self
                .state
                .borrow_mut()
                .adapter
                .update_window_focus_state(focused);
            if let Some(events) = events {
                events.raise();
            }
        }
    }

    impl Drop for Adapter {
        fn drop(&mut self) {
            // Fails harmlessly when the window (and with it the subclass) is
            // already gone.
            // SAFETY: same thread and procedure as `SetWindowSubclass`.
            let _ = unsafe { RemoveWindowSubclass(self.hwnd, Some(subclass_proc), SUBCLASS_ID) };
        }
    }

    unsafe extern "system" fn subclass_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        id: usize,
        data: usize,
    ) -> LRESULT {
        match message {
            WM_GETOBJECT => {
                // SAFETY: `data` is the `Adapter`'s boxed state, which lives
                // until the subclass is removed.
                let state = unsafe { &*(data as *const RefCell<State>) };
                // A nested `WM_GETOBJECT` while the state is borrowed falls
                // through to the default handling.
                let result = state.try_borrow_mut().ok().and_then(|mut state| {
                    let State {
                        adapter,
                        activation,
                    } = &mut *state;
                    adapter.handle_wm_getobject(AkWparam(wparam.0), AkLparam(lparam.0), activation)
                });
                // Converted after the borrow ends: `UiaReturnRawElementProvider`
                // may send a nested `WM_GETOBJECT`.
                if let Some(result) = result {
                    let result: accesskit_windows::LRESULT = result.into();
                    return LRESULT(result.0);
                }
            }
            WM_NCDESTROY => {
                // SAFETY: removing our own subclass from its procedure.
                let _ = unsafe { RemoveWindowSubclass(hwnd, Some(subclass_proc), id) };
            }
            _ => {}
        }
        // SAFETY: forwards to the next procedure in the chain.
        unsafe { DefSubclassProc(hwnd, message, wparam, lparam) }
    }
}
