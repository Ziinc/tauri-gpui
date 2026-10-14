//! GPUI's surface on Wayland.
//!
//! On X11 GPUI presents straight into the toplevel's window. On Wayland that
//! window is a single `wl_surface` GTK also commits to: it draws client-side
//! decorations (shadow and title bar) into it, and destroys it whenever the
//! window is hidden. So GPUI gets a sub-surface of its own instead, placed
//! over the window's content area and kept alive for the window's lifetime;
//! only its sub-surface role is re-attached to each new toplevel surface. TAO
//! reports the toplevel surface's size (decorations included) as the inner
//! size there, so the content area's size and offset come from GTK's
//! allocation of the window's child.

use std::{
    cell::{Cell, RefCell},
    ffi::c_void,
    ptr::NonNull,
    rc::Weak,
};

use gpui::{Pixels, Point, point, px};
use gtk::prelude::*;
use raw_window_handle::{RawDisplayHandle, RawWindowHandle, WaylandWindowHandle};
use wayland_client::{
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
    backend::{Backend, ObjectId},
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_compositor::WlCompositor, wl_region::WlRegion, wl_registry::WlRegistry,
        wl_subcompositor::WlSubcompositor, wl_subsurface::WlSubsurface, wl_surface::WlSurface,
    },
};

use super::window::WindowInner;
use crate::events;

/// The content area of a Wayland window, as GPUI's own sub-surface.
pub(crate) struct Content {
    connection: Connection,
    queue: RefCell<EventQueue<Globals>>,
    subcompositor: WlSubcompositor,
    surface: WlSurface,
    /// `None` while the window is hidden (GTK destroyed its surface).
    subsurface: RefCell<Option<WlSubsurface>>,
    /// The content area in the toplevel surface (x, y, width, height), in
    /// logical pixels.
    area: Cell<(i32, i32, i32, i32)>,
    scale: Cell<i32>,
}

impl Content {
    /// Creates GPUI's surface for a window whose native handles are `window`
    /// and `display`; `None` when they are not Wayland handles.
    pub(crate) fn new(window: RawWindowHandle, display: RawDisplayHandle) -> Option<Self> {
        let (RawWindowHandle::Wayland(window), RawDisplayHandle::Wayland(display)) =
            (window, display)
        else {
            return None;
        };
        // SAFETY: GDK's display outlives every window on it.
        let backend = unsafe { Backend::from_foreign_display(display.display.as_ptr().cast()) };
        let connection = Connection::from_backend(backend);
        let (globals, queue) = registry_queue_init::<Globals>(&connection)
            .map_err(|error| log::warn!("tauri-plugin-gpui: Wayland registry: {error}"))
            .ok()?;
        let qh = queue.handle();
        let bind = |error| log::warn!("tauri-plugin-gpui: Wayland globals: {error}");
        let compositor: WlCompositor = globals.bind(&qh, 1..=4, ()).map_err(bind).ok()?;
        let subcompositor: WlSubcompositor = globals.bind(&qh, 1..=1, ()).map_err(bind).ok()?;
        let surface = compositor.create_surface(&qh, ());
        // Pointer and touch input go to the toplevel, where GTK (and so TAO)
        // receives it.
        let region = compositor.create_region(&qh, ());
        surface.set_input_region(Some(&region));
        region.destroy();
        let content = Self {
            connection,
            queue: RefCell::new(queue),
            subcompositor,
            surface,
            subsurface: RefCell::default(),
            area: Cell::new((0, 0, 0, 0)),
            scale: Cell::new(1),
        };
        content.attach(window.surface.as_ptr());
        Some(content)
    }

    /// The handle GPUI's renderer presents to.
    pub(crate) fn window_handle(&self) -> RawWindowHandle {
        let surface = NonNull::new(self.surface.id().as_ptr().cast::<c_void>())
            .expect("a live wl_surface has a pointer");
        RawWindowHandle::Wayland(WaylandWindowHandle::new(surface))
    }

    pub(crate) fn is_attached(&self) -> bool {
        self.subsurface.borrow().is_some()
    }

    /// The content area's offset in the toplevel, where GTK reports pointer
    /// and drag positions.
    pub(crate) fn origin(&self) -> Point<Pixels> {
        let (x, y, ..) = self.area.get();
        point(px(x as f32), px(y as f32))
    }

    /// Makes the surface a sub-surface of the toplevel surface `parent`.
    fn attach(&self, parent: *mut c_void) {
        self.detach();
        if parent.is_null() {
            return;
        }
        // SAFETY: `parent` is GDK's live `wl_surface` for the toplevel.
        let parent = unsafe { ObjectId::from_ptr(WlSurface::interface(), parent.cast()) }
            .and_then(|id| WlSurface::from_id(&self.connection, id));
        let parent = match parent {
            Ok(parent) => parent,
            Err(error) => {
                log::warn!("tauri-plugin-gpui: not a wl_surface: {error}");
                return;
            }
        };
        let qh = self.queue.borrow().handle();
        let subsurface = self
            .subcompositor
            .get_subsurface(&self.surface, &parent, &qh, ());
        // GPUI presents on its own schedule, not on GTK's commits.
        subsurface.set_desync();
        let (x, y, ..) = self.area.get();
        subsurface.set_position(x, y);
        self.surface.commit();
        *self.subsurface.borrow_mut() = Some(subsurface);
    }

    fn detach(&self) {
        if let Some(subsurface) = self.subsurface.borrow_mut().take() {
            subsurface.destroy();
        }
    }

    /// Moves and sizes GPUI's surface over the content area `(x, y, width,
    /// height)`. The position applies on the toplevel's next commit, the
    /// scale with the renderer's next frame (which is sized for it).
    fn place(&self, inner: &WindowInner, area: (i32, i32, i32, i32), scale: i32) {
        let (x, y, width, height) = area;
        let old = self.area.replace(area);
        if (old.0, old.1) != (x, y)
            && let Some(subsurface) = &*self.subsurface.borrow()
        {
            subsurface.set_position(x, y);
        }
        if self.scale.replace(scale) != scale {
            self.surface.set_buffer_scale(scale);
        }
        inner.resized(
            events::physical_size((width * scale) as u32, (height * scale) as u32),
            scale as f32,
        );
    }

    /// Follows a monitor scale change; GTK keeps the logical layout.
    pub(crate) fn rescale(&self, inner: &WindowInner, scale: f64) {
        let area = self.area.get();
        if area.2 > 0 && area.3 > 0 {
            self.place(inner, area, (scale.round() as i32).max(1));
        }
    }

    /// Drops events the compositor sent for these objects (`enter` and
    /// `leave` outputs); none of them matter, but they queue up otherwise.
    pub(crate) fn flush(&self) {
        if let Err(error) = self.queue.borrow_mut().dispatch_pending(&mut Globals) {
            log::warn!("tauri-plugin-gpui: Wayland dispatch failed: {error}");
        }
    }
}

impl Drop for Content {
    fn drop(&mut self) {
        self.detach();
        self.surface.destroy();
        let _ = self.connection.flush();
    }
}

/// Follows the window's content area and its mapping. Call once the window
/// has its renderer.
pub(crate) fn install(window: &tauri::Window<tauri::Wry>, inner: Weak<WindowInner>) {
    let gtk_window = match window.gtk_window() {
        Ok(window) => window,
        Err(error) => {
            log::warn!("tauri-plugin-gpui: no GTK window for the Wayland surface: {error}");
            return;
        }
    };

    // GTK creates the toplevel surface in its own `map` handler and destroys
    // it in `unmap`; both run before these.
    gtk_window.connect_map({
        let inner = inner.clone();
        move |window| {
            crate::ffi::callback("map", (), || {
                let Some(inner) = inner.upgrade() else { return };
                let Some(content) = &inner.wayland else {
                    return;
                };
                let parent = window.window().map_or(std::ptr::null_mut(), |window| {
                    // SAFETY: a GDK window on a Wayland display.
                    unsafe {
                        gdk_wayland_sys::gdk_wayland_window_get_wl_surface(window.as_ptr().cast())
                    }
                });
                content.attach(parent);
                inner.window_mode_changed();
                inner.waker.wake();
            })
        }
    });
    gtk_window.connect_unmap({
        let inner = inner.clone();
        move |_| {
            crate::ffi::callback("unmap", (), || {
                let Some(inner) = inner.upgrade() else { return };
                let Some(content) = &inner.wayland else {
                    return;
                };
                content.detach();
                inner.window_mode_changed();
                inner.waker.wake();
            })
        }
    });

    // TAO's window holds a single box with the content; it is laid out
    // inside the decorations.
    let Some(child) = gtk_window.child() else {
        log::warn!("tauri-plugin-gpui: the GTK window has no content widget");
        return;
    };
    let resize = move |widget: &gtk::Widget| {
        let Some(inner) = inner.upgrade() else { return };
        let Some(content) = &inner.wayland else {
            return;
        };
        let allocation = widget.allocation();
        if allocation.width() <= 1 || allocation.height() <= 1 {
            return;
        }
        let area = (
            allocation.x(),
            allocation.y(),
            allocation.width(),
            allocation.height(),
        );
        content.place(&inner, area, widget.scale_factor().max(1));
        // Commits the new sub-surface position along with GTK's next frame.
        widget.queue_draw();
    };
    resize(&child);
    child.connect_size_allocate(move |widget, _| {
        crate::ffi::callback("size-allocate", (), || resize(widget))
    });
}

/// Dispatch target for the globals and objects above; none of their events
/// are used.
struct Globals;

impl Dispatch<WlRegistry, GlobalListContents> for Globals {
    fn event(
        _: &mut Self,
        _: &WlRegistry,
        _: <WlRegistry as Proxy>::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

macro_rules! ignore_events {
    ($($proxy:ty),*) => {$(
        impl Dispatch<$proxy, ()> for Globals {
            fn event(
                _: &mut Self,
                _: &$proxy,
                _: <$proxy as Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
    )*};
}

ignore_events!(
    WlCompositor,
    WlSubcompositor,
    WlSurface,
    WlSubsurface,
    WlRegion
);
