//! Linux/X11: keeps GPUI below the window's menu bar (see `platform::x11`).
//!
//! Once GPUI renders into the content area, TAO's events still describe the
//! toplevel window: its size includes the menu bar, its pointer positions
//! are relative to whichever GTK window is under the pointer, and its
//! crossing events fire when the pointer moves between the menu bar and the
//! content area. Those are replaced with what the content area reports.

use std::rc::Rc;

use gpui_wgpu::WgpuSurfaceConfig;
use tauri::Wry;
use tauri_runtime_wry::tao::{
    dpi::PhysicalPosition,
    event::{DeviceId, ElementState, WindowEvent},
};

use super::{Runtime, SurfaceParams};
use crate::{
    events,
    platform::{
        window::{RawWindow, WindowInner},
        x11::{Pointer, X11Window},
    },
};

/// What to do with a TAO event for a window with a content area.
pub(super) enum Rewrite {
    Keep,
    Drop,
    Replace(WindowEvent<'static>),
}

impl Runtime {
    /// Renders GPUI below the menu bar of a window that has one.
    pub(super) fn x11_surface(&self, window: &tauri::Window<Wry>, params: &mut SurfaceParams) {
        let Some(x11) = X11Window::new(window, params.raw.window, &self.waker) else {
            return;
        };
        if x11.take_changes().menu_bar_added
            && let Some(raw) = x11.create_area(&self.waker)
            && let Some(size) = x11.content_size(params.scale_factor)
        {
            params.raw.window = raw;
            params.physical_size = size;
        }
        params.x11 = Some(x11);
    }

    /// Applies what GTK reported since the last look. Runs with the runtime
    /// entered, outside any GPUI update.
    pub(super) fn sync_x11(&self, inner: &Rc<WindowInner>) {
        let Some(x11) = &inner.x11 else {
            return;
        };
        let changes = x11.take_changes();
        if changes.menu_bar_added && !x11.has_area() {
            self.move_below_menu_bar(inner, x11);
        }
        let scale = inner.state.borrow().scale_factor;
        if changes.resized
            && let Some(size) = x11.content_size(scale)
        {
            inner.resized(size, scale);
        }
        if changes.crossed
            && x11.has_area()
            && let Some(pointer) = x11.pointer()
        {
            self.x11_hover(inner, &pointer);
        }
        if changes.exposed {
            inner.force_present.set(true);
            inner.schedule_frame();
        }
    }

    /// Tells GPUI the pointer entered or left the content area, if that
    /// changed. Runs with the runtime entered.
    fn x11_hover(&self, inner: &Rc<WindowInner>, pointer: &Pointer) {
        if inner.state.borrow().hovered == pointer.over {
            return;
        }
        // SAFETY: the id is only compared, never used to address a device.
        let device_id = unsafe { DeviceId::dummy() };
        let event = if pointer.over {
            WindowEvent::CursorEntered { device_id }
        } else {
            WindowEvent::CursorLeft { device_id }
        };
        self.track_window_state(inner, &event);
        events::dispatch(inner, &event);
        if pointer.over {
            // TAO need not report a move after the pointer comes back from
            // an embedded webview.
            self.x11_move(inner, pointer);
        }
    }

    /// Tells GPUI the pointer is at `pointer`, if it is not there already.
    fn x11_move(&self, inner: &WindowInner, pointer: &Pointer) {
        if inner.state.borrow().mouse_position == pointer.position {
            return;
        }
        // SAFETY: as above.
        let device_id = unsafe { DeviceId::dummy() };
        events::dispatch(inner, &cursor_moved(device_id, inner, pointer));
    }

    /// A menu bar was added after GPUI was attached: move GPUI's surface
    /// from the toplevel to a content area below it.
    fn move_below_menu_bar(&self, inner: &WindowInner, x11: &X11Window) {
        let Some(window) = x11.create_area(&self.waker) else {
            return;
        };
        let scale = inner.state.borrow().scale_factor;
        let raw = RawWindow {
            window,
            display: inner.raw.get().display,
        };
        let instance = self
            .platform
            .gpu_context
            .borrow()
            .as_ref()
            .map(|context| context.instance.clone());
        let (Some(size), Some(instance)) = (x11.content_size(scale), instance) else {
            x11.remove_area();
            return;
        };
        let result = inner.state.borrow_mut().renderer.as_mut().map(|renderer| {
            renderer.replace_surface(
                &raw,
                WgpuSurfaceConfig {
                    size,
                    transparent: false,
                    preferred_present_mode: None,
                },
                &instance,
            )
        });
        match result {
            Some(Ok(())) => {
                inner.raw.set(raw);
                // Embedded webviews move into the new surface on the next frame.
                crate::webview::invalidate(inner);
                inner.resized(size, scale);
                inner.force_present.set(true);
                inner.schedule_frame();
            }
            Some(Err(error)) => {
                log::error!("tauri-plugin-gpui: moving GPUI below the menu bar failed: {error:#}");
                x11.remove_area();
            }
            None => x11.remove_area(),
        }
    }

    /// Rewrites a TAO event for a window whose GPUI surface is its content
    /// area (see the module docs).
    pub(super) fn x11_window_event(
        &self,
        inner: &Rc<WindowInner>,
        event: &WindowEvent<'_>,
    ) -> Rewrite {
        if inner.x11.is_none() || inner.closed.get() {
            return Rewrite::Keep;
        }
        {
            // Crossings and size changes GTK reported come first.
            let _guard = self.enter();
            self.sync_x11(inner);
        }
        let Some(x11) = inner.x11.as_ref().filter(|x11| x11.has_area()) else {
            return Rewrite::Keep;
        };
        match event {
            // The content area's own allocation is GPUI's size.
            WindowEvent::Resized(_) => {
                inner.window_mode_changed();
                Rewrite::Drop
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                inner.window_mode_changed();
                let scale = *scale_factor as f32;
                if let Some(size) = x11.content_size(scale) {
                    let _guard = self.enter();
                    inner.resized(size, scale);
                }
                Rewrite::Drop
            }
            WindowEvent::CursorEntered { .. } | WindowEvent::CursorLeft { .. } => {
                if let Some(pointer) = x11.pointer() {
                    let _guard = self.enter();
                    self.x11_hover(inner, &pointer);
                }
                Rewrite::Drop
            }
            WindowEvent::CursorMoved { device_id, .. } => {
                let Some(pointer) = x11.pointer() else {
                    return Rewrite::Drop;
                };
                {
                    let _guard = self.enter();
                    self.x11_hover(inner, &pointer);
                }
                // Over the menu bar, unless a drag started in GPUI.
                let dragging = inner.state.borrow().pressed_button.is_some();
                if !pointer.over && !dragging {
                    return Rewrite::Drop;
                }
                Rewrite::Replace(cursor_moved(*device_id, inner, &pointer))
            }
            // Clicks land where the pointer is, even if TAO missed a move.
            WindowEvent::MouseInput {
                state: ElementState::Pressed,
                ..
            } => {
                if let Some(pointer) = x11.pointer().filter(|pointer| pointer.over) {
                    let _guard = self.enter();
                    self.x11_hover(inner, &pointer);
                    self.x11_move(inner, &pointer);
                }
                Rewrite::Keep
            }
            WindowEvent::MouseWheel { .. } if !inner.state.borrow().hovered => Rewrite::Drop,
            _ => Rewrite::Keep,
        }
    }
}

fn cursor_moved(
    device_id: DeviceId,
    inner: &WindowInner,
    pointer: &Pointer,
) -> WindowEvent<'static> {
    let scale = inner.state.borrow().scale_factor;
    #[allow(deprecated)]
    WindowEvent::CursorMoved {
        device_id,
        position: PhysicalPosition::new(
            (f32::from(pointer.position.x) * scale) as f64,
            (f32::from(pointer.position.y) * scale) as f64,
        ),
        modifiers: Default::default(),
    }
}
