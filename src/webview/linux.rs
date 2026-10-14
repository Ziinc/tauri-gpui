//! Linux: Tauri builds child webviews as GTK widgets inside the window's own
//! GTK container, which paints into the same X11 window GPUI presents to, so
//! the two overwrite each other. Each embedded webview is moved into an X11
//! child window of its own instead, which the X server stacks above GPUI.
//!
//! Wayland windows are not handled; they keep Tauri's positioning.

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    ffi::c_ulong,
    rc::Rc,
};

use gdkx11::{X11Display, ffi::gdk_x11_window_foreign_new_for_display};
use gpui::{Bounds, Pixels};
use gtk::{
    glib::translate::{FromGlibPtrFull as _, ToGlibPtr as _, from_glib_none},
    prelude::*,
};
use tauri::{Webview, Wry};
use x11_dl::xlib::{CurrentTime, Display, RevertToParent, Xlib};

/// An X11 child window holding one webview.
struct Host {
    xlib: Xlib,
    gdk_display: X11Display,
    display: *mut Display,
    window: c_ulong,
    /// GPUI's X11 window, which this one is a child of.
    parent: c_ulong,
    /// The window that gets keyboard focus back: the toplevel, which is
    /// `parent` unless GPUI renders below a menu bar.
    focus: c_ulong,
    /// Keeps the GTK side of the foreign window, and the webview, alive.
    gtk_window: gtk::Window,
    /// The webview widget this host holds; a webview recreated under the
    /// same label gets a new host.
    widget: gtk::Widget,
    /// Applied geometry, so unchanged frames make no X11 calls.
    shown: Cell<Option<Bounds<Pixels>>>,
    /// Set when a click gave the webview keyboard focus.
    focused: Rc<Cell<bool>>,
    /// Cleared on drop, for the widget's click handler.
    alive: Rc<Cell<bool>>,
}

impl Drop for Host {
    fn drop(&mut self) {
        self.alive.set(false);
        // Keep the webview: Tauri still owns it and may move it elsewhere.
        if self.widget.parent().as_ref() == Some(self.gtk_window.upcast_ref()) {
            self.gtk_window.remove(&self.widget);
        }
        // The X server destroys child windows with their parent, so the
        // window may already be gone: ignore the resulting BadWindow.
        self.gdk_display.error_trap_push();
        self.release_focus();
        // SAFETY: the host owns this GTK toplevel; nothing else refers to it.
        unsafe { self.gtk_window.destroy() };
        // SAFETY: the window was created by `Host::new` on this display.
        unsafe { (self.xlib.XDestroyWindow)(self.display, self.window) };
        self.gdk_display.error_trap_pop_ignored();
    }
}

thread_local! {
    /// Hosts by webview label. GTK objects never leave the main thread.
    static HOSTS: RefCell<HashMap<String, Host>> = RefCell::default();
}

/// Returns keyboard focus to GPUI after a click outside the webview.
pub(super) fn focus_parent(label: &str) {
    HOSTS.with(|hosts| {
        if let Some(host) = hosts.borrow().get(label) {
            host.return_focus();
            host.flush();
        }
    });
}

/// Destroys the host of a webview whose GPUI window is gone.
pub(super) fn forget(label: &str) {
    // Runs from the window's teardown, which may happen during thread-local
    // destruction at exit; there is nothing left to clean up then.
    let _ = HOSTS.try_with(|hosts| hosts.borrow_mut().remove(label));
}

/// Moves `webview` to `painted` (logical, window-relative) or hides it.
pub(super) fn apply(
    webview: &Webview<Wry>,
    parent: c_ulong,
    toplevel: Option<c_ulong>,
    painted: Option<Bounds<Pixels>>,
) {
    let label = webview.label().to_string();
    let result = webview.with_webview(move |platform| {
        let widget = platform.inner().upcast::<gtk::Widget>();
        HOSTS.with(|hosts| {
            let mut hosts = hosts.borrow_mut();
            if hosts
                .get(&label)
                .is_some_and(|host| host.widget != widget || host.parent != parent)
            {
                hosts.remove(&label);
            }
            if !hosts.contains_key(&label) {
                match Host::new(parent, toplevel.unwrap_or(parent), &widget) {
                    Some(host) => {
                        hosts.insert(label.clone(), host);
                    }
                    None => return,
                }
            }
            let host = &hosts[&label];
            match painted {
                Some(bounds) => host.show(bounds),
                None => host.hide(),
            }
        });
    });
    if let Err(error) = result {
        log::warn!("failed to update webview: {error}");
    }
}

impl Host {
    fn new(parent: c_ulong, focus: c_ulong, widget: &gtk::Widget) -> Option<Self> {
        let xlib = Xlib::open()
            .inspect_err(|error| log::warn!("embedded webview: cannot load Xlib: {error}"))
            .ok()?;
        let gdk_display = gtk::gdk::Display::default()?;
        let Some(x11_display) = gdk_display.downcast_ref::<X11Display>() else {
            log::warn!("embedded webview: GTK is not using its X11 backend");
            return None;
        };
        // SAFETY: a valid GdkX11Display; the Xlib display lives as long as it.
        let display = unsafe { gdkx11::ffi::gdk_x11_display_get_xdisplay(x11_display.as_ptr()) }
            as *mut Display;
        // SAFETY: `parent` is the live X11 window of the attached Tauri
        // window; the child starts unmapped at 1x1 and is sized on `show`.
        let window = unsafe { (xlib.XCreateSimpleWindow)(display, parent, 0, 0, 1, 1, 0, 0, 0) };

        // SAFETY: wraps the X11 window just created, as wry does for its own
        // X11 child webviews.
        let gdk_window = unsafe {
            gtk::gdk::Window::from_glib_full(gdk_x11_window_foreign_new_for_display(
                x11_display.as_ptr() as *mut _,
                window,
            ))
        };
        let gtk_window = gtk::Window::new(gtk::WindowType::Toplevel);
        gtk_window.connect_realize(move |w| {
            w.set_window(gdk_window.clone());
            // So GTK can unregister it when the host destroys the window.
            w.register_window(&gdk_window);
        });
        gtk_window.set_has_window(true);
        gtk_window.realize();

        if let Some(container) = widget
            .parent()
            .and_then(|parent| parent.downcast::<gtk::Container>().ok())
        {
            container.remove(widget);
        }
        gtk_window.add(widget);
        // Keys go to this window while the webview has focus: give it the
        // menu accelerators GTK would otherwise only find on the toplevel.
        let toplevel = gtk_window_for(x11_display, focus);
        if let Some(toplevel) = &toplevel {
            share_accel_groups(toplevel, &gtk_window);
        }

        // X11 sends keys to the focused X window, which stays the Tauri
        // window when the page is clicked: hand focus to the webview's own
        // window, and back to GPUI on a click outside (`focus_parent`).
        let set_input_focus = xlib.XSetInputFocus;
        let focused = Rc::new(Cell::new(false));
        let alive = Rc::new(Cell::new(true));
        widget.connect_button_press_event({
            let (focused, alive) = (focused.clone(), alive.clone());
            let toplevel = toplevel.as_ref().map(|toplevel| toplevel.downgrade());
            let host = gtk_window.downgrade();
            move |_, _| {
                // The widget can outlive the host when it moves to another.
                // On every click: the window manager or Tauri may have moved
                // focus back to the GPUI window since the last one.
                if alive.get() {
                    focused.set(true);
                    // The window's menu may have changed since.
                    if let (Some(toplevel), Some(host)) =
                        (toplevel.as_ref().and_then(|t| t.upgrade()), host.upgrade())
                    {
                        share_accel_groups(&toplevel, &host);
                    }
                    // SAFETY: `alive` says the host, and its window, exist.
                    unsafe { set_input_focus(display, window, RevertToParent, CurrentTime) };
                }
                gtk::glib::Propagation::Proceed
            }
        });
        Some(Self {
            xlib,
            gdk_display: x11_display.clone(),
            display,
            window,
            parent,
            focus,
            gtk_window,
            widget: widget.clone(),
            shown: Cell::new(None),
            focused,
            alive,
        })
    }

    fn show(&self, bounds: Bounds<Pixels>) {
        let was_shown = self.shown.replace(Some(bounds)).is_some();
        let (x, y) = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
        let (width, height) = (f32::from(bounds.size.width), f32::from(bounds.size.height));
        let (width, height) = (
            (width.round() as i32).max(1),
            (height.round() as i32).max(1),
        );
        if !was_shown {
            self.widget.show();
            // Showing a GTK toplevel applies its default geometry, so
            // position the window after it.
            self.gtk_window.show_all();
        }
        // GDK coordinates are in GTK's logical pixels.
        if let Some(window) = self.gtk_window.window() {
            window.move_resize(x.round() as i32, y.round() as i32, width, height);
        }
        self.gtk_window
            .size_allocate(&gtk::Allocation::new(0, 0, width, height));
        if !was_shown {
            // SAFETY: `self.window` is alive until `Host` drops.
            unsafe { (self.xlib.XMapWindow)(self.display, self.window) };
        }
        self.flush();
    }

    fn hide(&self) {
        if self.shown.take().is_none() {
            return;
        }
        self.release_focus();
        // SAFETY: `self.window` is alive until `Host` drops.
        unsafe { (self.xlib.XUnmapWindow)(self.display, self.window) };
        self.flush();
    }

    /// Before the window goes away: gives keyboard focus back to GPUI if the
    /// webview has it. X11 would revert it to the parent, which below a menu
    /// bar is not the toplevel GTK reads keys from.
    fn release_focus(&self) {
        let mut window = 0;
        let mut revert = 0;
        // SAFETY: plain out parameters on GDK's display.
        unsafe { (self.xlib.XGetInputFocus)(self.display, &mut window, &mut revert) };
        // Not when the user has moved on to another window since.
        self.focused.set(window == self.window);
        self.return_focus();
    }

    /// Gives keyboard focus back to GPUI after the webview took it.
    fn return_focus(&self) {
        if self.focused.replace(false) {
            // SAFETY: both windows are alive while the host is.
            unsafe {
                (self.xlib.XSetInputFocus)(self.display, self.focus, RevertToParent, CurrentTime)
            };
        }
    }

    fn flush(&self) {
        // SAFETY: the display is GDK's, open for the life of the process.
        unsafe { (self.xlib.XFlush)(self.display) };
    }
}

/// The GTK window that owns X11 window `xid`.
fn gtk_window_for(display: &X11Display, xid: c_ulong) -> Option<gtk::Window> {
    let window = gdkx11::X11Window::lookup_for_display(display, xid)?;
    let mut widget = std::ptr::null_mut();
    // SAFETY: a live GdkWindow; GTK sets its user data to the owning widget.
    unsafe {
        gtk::gdk::ffi::gdk_window_get_user_data(
            window.upcast_ref::<gtk::gdk::Window>().to_glib_none().0,
            &mut widget,
        )
    };
    if widget.is_null() {
        return None;
    }
    // SAFETY: non-null user data of a GTK-owned window is a GtkWidget.
    let widget: gtk::Widget = unsafe { from_glib_none(widget.cast::<gtk::ffi::GtkWidget>()) };
    widget.downcast().ok()
}

/// Attaches `toplevel`'s accelerator groups (Tauri's menu accelerators) to
/// `host`, and detaches the ones `toplevel` no longer has.
fn share_accel_groups(toplevel: &gtk::Window, host: &gtk::Window) {
    let wanted = gtk::accel_groups_from_object(toplevel);
    let attached = gtk::accel_groups_from_object(host);
    for group in attached.iter().filter(|group| !wanted.contains(group)) {
        host.remove_accel_group(group);
    }
    for group in wanted.iter().filter(|group| !attached.contains(group)) {
        host.add_accel_group(group);
    }
}
