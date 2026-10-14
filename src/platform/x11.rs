//! Linux/X11: GPUI's surface in a Tauri window that has a menu.
//!
//! TAO puts a vertical `GtkBox` in every window, and Tauri packs the
//! window's `GtkMenuBar` at its top. GTK draws the menu bar into the
//! toplevel X11 window, so GPUI, presenting to that same window, would cover
//! it. Once the box holds a menu bar, GPUI renders into a `GtkDrawingArea`
//! packed below it instead: the area's native X11 window is the surface, its
//! allocation is the size GPUI sees, and its top-left corner is GPUI's
//! origin.
//!
//! Windows without a menu keep rendering into the toplevel window.

use std::{
    cell::{Cell, RefCell},
    ffi::c_ulong,
    num::NonZeroU32,
    rc::Rc,
    sync::Arc,
};

use gpui::{DevicePixels, Pixels, Point, Size, point, px, size};
use gtk::{gdk, glib, prelude::*};
use raw_window_handle::{RawWindowHandle, XcbWindowHandle, XlibWindowHandle};
use tauri::Wry;

use super::dispatcher::LoopWaker;

/// The X11 window id of a raw window handle, when it is an X11 window.
pub(crate) fn xid(window: RawWindowHandle) -> Option<c_ulong> {
    match window {
        RawWindowHandle::Xlib(handle) => Some(handle.window),
        RawWindowHandle::Xcb(handle) => Some(handle.window.get().into()),
        _ => None,
    }
}

/// `toplevel` with its window id replaced by `xid`.
fn with_xid(toplevel: RawWindowHandle, xid: c_ulong) -> Option<RawWindowHandle> {
    Some(match toplevel {
        RawWindowHandle::Xlib(handle) => {
            let mut child = XlibWindowHandle::new(xid);
            child.visual_id = handle.visual_id;
            child.into()
        }
        RawWindowHandle::Xcb(handle) => {
            let mut child = XcbWindowHandle::new(NonZeroU32::new(u32::try_from(xid).ok()?)?);
            child.visual_id = handle.visual_id;
            child.into()
        }
        _ => return None,
    })
}

fn has_menu_bar(vbox: &gtk::Box) -> bool {
    vbox.children()
        .iter()
        .any(|child| child.is::<gtk::MenuBar>())
}

/// GTK side of a GPUI-backed Tauri window on X11.
pub(crate) struct X11Window {
    /// The Tauri window's GTK toplevel.
    pub toplevel: gtk::Window,
    /// Its X11 window, which keeps keyboard focus while GPUI has it.
    pub toplevel_xid: c_ulong,
    toplevel_raw: RawWindowHandle,
    vbox: gtk::Box,
    added: Option<glib::SignalHandlerId>,
    /// GPUI's surface once the window has a menu bar.
    area: RefCell<Option<gtk::DrawingArea>>,
    signals: Rc<Signals>,
}

/// What GTK reported about the window since the runtime last looked.
#[derive(Default)]
struct Signals {
    menu_bar_added: Cell<bool>,
    /// Stays set once a menu bar was found: GPUI stays in the content area.
    menu_bar_seen: Cell<bool>,
    resized: Cell<bool>,
    exposed: Cell<bool>,
    /// The pointer crossed the content area's edge.
    crossed: Cell<bool>,
}

/// Changes to apply to GPUI, from [`X11Window::take_changes`].
pub(crate) struct Changes {
    pub menu_bar_added: bool,
    pub resized: bool,
    pub exposed: bool,
    pub crossed: bool,
}

/// Where the pointer is, from [`X11Window::pointer`].
pub(crate) struct Pointer {
    /// Directly over the content area: not over the menu bar, an embedded
    /// webview or another window.
    pub over: bool,
    /// Relative to the content area, in logical pixels.
    pub position: Point<Pixels>,
}

impl X11Window {
    /// Returns `None` when the window is not an X11 window.
    pub fn new(
        window: &tauri::Window<Wry>,
        toplevel_raw: RawWindowHandle,
        waker: &Arc<LoopWaker>,
    ) -> Option<Self> {
        let toplevel_xid = xid(toplevel_raw)?;
        let toplevel = window.gtk_window().ok()?.upcast::<gtk::Window>();
        let vbox = window.default_vbox().ok()?;
        let signals = Rc::new(Signals::default());
        signals.menu_bar_added.set(has_menu_bar(&vbox));
        signals.menu_bar_seen.set(signals.menu_bar_added.get());
        // Tauri adds the menu bar of `set_menu` asynchronously, possibly
        // after GPUI was attached. `gtk_box_pack_start` emits no `add`, but
        // the box is laid out again.
        let added = vbox.connect_size_allocate({
            let signals = signals.clone();
            let waker = waker.clone();
            move |vbox, _| {
                if !signals.menu_bar_seen.get() && has_menu_bar(vbox) {
                    signals.menu_bar_seen.set(true);
                    signals.menu_bar_added.set(true);
                    waker.wake();
                }
            }
        });
        Some(Self {
            toplevel,
            toplevel_xid,
            toplevel_raw,
            vbox,
            added: Some(added),
            area: RefCell::default(),
            signals,
        })
    }

    /// Whether GPUI renders into the content area below a menu bar.
    pub fn has_area(&self) -> bool {
        self.area.borrow().is_some()
    }

    pub fn take_changes(&self) -> Changes {
        let signals = &self.signals;
        Changes {
            menu_bar_added: signals.menu_bar_added.take(),
            resized: signals.resized.take(),
            exposed: signals.exposed.take(),
            crossed: signals.crossed.take(),
        }
    }

    /// Packs the content area below the menu bar and returns its X11 window
    /// as a raw handle for GPUI's renderer. `None` if GTK cannot give it a
    /// native window, in which case GPUI keeps the toplevel.
    pub fn create_area(&self, waker: &Arc<LoopWaker>) -> Option<RawWindowHandle> {
        let area = gtk::DrawingArea::new();
        // GPUI paints the area; GTK must neither clear it nor copy a
        // double buffer over it.
        area.set_app_paintable(true);
        // SAFETY: a live widget. Deprecated, but it still applies to native
        // X11 windows, which is what the area gets.
        unsafe {
            gtk::ffi::gtk_widget_set_double_buffered(
                area.upcast_ref::<gtk::Widget>().as_ptr(),
                glib::ffi::GFALSE,
            )
        };
        // Pointer events reach TAO through the toplevel either way, but
        // TAO's crossing events describe the toplevel, which includes the
        // menu bar, and miss moves into an embedded webview.
        area.add_events(gdk::EventMask::ENTER_NOTIFY_MASK | gdk::EventMask::LEAVE_NOTIFY_MASK);
        let signal = |set: fn(&Signals)| {
            let signals = self.signals.clone();
            let waker = waker.clone();
            move || {
                set(&signals);
                waker.wake();
            }
        };
        let resized = signal(|s| s.resized.set(true));
        area.connect_size_allocate(move |_, _| resized());
        let exposed = signal(|s| s.exposed.set(true));
        area.connect_draw(move |_, _| {
            exposed();
            glib::Propagation::Stop
        });
        let entered = signal(|s| s.crossed.set(true));
        area.connect_enter_notify_event(move |_, _| {
            entered();
            glib::Propagation::Proceed
        });
        let left = signal(|s| s.crossed.set(true));
        area.connect_leave_notify_event(move |_, _| {
            left();
            glib::Propagation::Proceed
        });

        self.vbox.pack_start(&area, true, true, 0);
        area.show();
        area.realize();
        let xid = area
            .window()
            .filter(|window| window.ensure_native())
            .and_then(|window| window.downcast::<gdkx11::X11Window>().ok())
            .map(|window| window.xid())
            .and_then(|xid| with_xid(self.toplevel_raw, xid));
        if xid.is_none() {
            log::warn!("tauri-plugin-gpui: no native X11 window below the menu bar");
            self.vbox.remove(&area);
            return None;
        }
        // Lay the area out now rather than on the next frame, so GPUI
        // starts at its real size.
        self.toplevel.check_resize();
        // The renderer's Vulkan driver asks the X server about the window.
        self.toplevel.display().flush();
        self.signals.resized.set(false);
        // The pointer may already be over it.
        self.signals.crossed.set(true);
        *self.area.borrow_mut() = Some(area);
        xid
    }

    /// Takes the content area out of the window again.
    pub fn remove_area(&self) {
        if let Some(area) = self.area.take()
            && area.parent().as_ref() == Some(self.vbox.upcast_ref())
        {
            self.vbox.remove(&area);
        }
    }

    /// The content area's size in physical pixels.
    pub fn content_size(&self, scale_factor: f32) -> Option<Size<DevicePixels>> {
        let area = self.area.borrow();
        let allocation = area.as_ref()?.allocation();
        let physical = |logical: i32| DevicePixels((logical.max(1) as f32 * scale_factor) as i32);
        Some(size(
            physical(allocation.width()),
            physical(allocation.height()),
        ))
    }

    /// Logical height of what sits above the content area (the menu bar).
    pub fn chrome_height(&self) -> f32 {
        self.area
            .borrow()
            .as_ref()
            .map_or(0., |area| area.allocation().y().max(0) as f32)
    }

    /// Where the pointer is. A round trip to the X server.
    pub fn pointer(&self) -> Option<Pointer> {
        let area = self.area.borrow();
        let window = area.as_ref()?.window()?;
        let pointer = window.display().default_seat()?.pointer()?;
        // What TAO asks too: the innermost GDK window under the pointer,
        // which takes stacking into account.
        let (under, x, y) = pointer.window_at_position_double();
        if under.as_ref() == Some(&window) {
            return Some(Pointer {
                over: true,
                position: point(px(x as f32), px(y as f32)),
            });
        }
        let (_, x, y, _) = window.device_position_double(&pointer);
        Some(Pointer {
            over: false,
            position: point(px(x as f32), px(y as f32)),
        })
    }
}

impl Drop for X11Window {
    fn drop(&mut self) {
        // Destroying the window has already dropped the handler.
        if let Some(added) = self.added.take()
            // SAFETY: a live object and a handler id it was given.
            && unsafe {
                glib::gobject_ffi::g_signal_handler_is_connected(
                    self.vbox.as_ptr().cast(),
                    added.as_raw(),
                ) != 0
            }
        {
            self.vbox.disconnect(added);
        }
        // GPUI is gone but the Tauri window may live on (a failed mount).
        self.remove_area();
    }
}
