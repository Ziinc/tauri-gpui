//! Android: feeds `GpuiView` events (surface, touch, keys, IME, insets) to
//! the attached GPUI window. See `crate::android` for the JNI side.

mod input;

use std::{cell::RefCell, ffi::c_void, path::Path, rc::Rc};

use gpui::{
    AppLifecyclePhase, Edges, KeyDownEvent, KeyUpEvent, Keystroke, PlatformInput,
    PlatformTextSystem, TouchEvent, TouchId, TouchPhase, WindowAppearance, WindowInsets,
    WindowVisibility, point, px,
};
use gpui_wgpu::WgpuSurfaceConfig;
use ndk::native_window::NativeWindow;
use raw_window_handle::{
    AndroidDisplayHandle, AndroidNdkWindowHandle, RawDisplayHandle, RawWindowHandle,
};
use tauri::Wry;

use super::{OpenWindow, Runtime, SurfaceParams};
use crate::{
    GpuiError, GpuiOptions,
    android::{ViewEvent, keys},
    events::physical_size,
    platform::window::{RawWindow, WindowInner},
};

struct PendingAttach {
    window: tauri::Window<Wry>,
    options: GpuiOptions,
    open: OpenWindow,
}

/// The view's current surface. Holding the `NativeWindow` keeps a reference
/// on the `ANativeWindow` the renderer draws into.
struct Surface {
    window: NativeWindow,
    width: u32,
    height: u32,
    density: f32,
}

#[derive(Default)]
pub(crate) struct AndroidState {
    pending: RefCell<Option<PendingAttach>>,
    surface: RefCell<Option<Surface>>,
    /// Insets reported before GPUI was attached.
    insets: RefCell<WindowInsets>,
    input: RefCell<input::InputSync>,
}

fn raw_window(window: &NativeWindow) -> RawWindow {
    RawWindow {
        window: RawWindowHandle::AndroidNdk(AndroidNdkWindowHandle::new(
            window.ptr().cast::<c_void>(),
        )),
        display: RawDisplayHandle::Android(AndroidDisplayHandle::new()),
    }
}

/// Font files from `/system/fonts` worth loading: the UI, monospace and
/// emoji families. Loading every system font would cost tens of megabytes.
fn wanted_font(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    (name.ends_with(".ttf") || name.ends_with(".otf") || name.ends_with(".ttc"))
        && (name.starts_with("roboto")
            || name.starts_with("droidsansmono")
            || name.starts_with("cutivemono")
            || name.starts_with("notocoloremoji")
            || name.starts_with("notosanssymbols"))
}

pub(crate) fn load_system_fonts(text_system: &dyn PlatformTextSystem) {
    let dir = Path::new("/system/fonts");
    let fonts: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.file_name().to_str().is_some_and(wanted_font))
        .filter_map(|entry| std::fs::read(entry.path()).ok())
        .map(std::borrow::Cow::Owned)
        .collect();
    let count = fonts.len();
    if let Err(error) = text_system.add_fonts(fonts) {
        log::error!("tauri-plugin-gpui: loading system fonts failed: {error:#}");
    } else {
        log::info!("tauri-plugin-gpui: loaded {count} system fonts");
    }
}

impl Runtime {
    /// Android has one surface, owned by the GpuiView. The mount completes as
    /// soon as the view reports it (immediately, if it already has).
    pub(super) fn attach_android(
        &self,
        window: &tauri::Window<Wry>,
        options: GpuiOptions,
        open: OpenWindow,
    ) -> Result<(), GpuiError> {
        if !self.surfaces.borrow().is_empty() || self.android.pending.borrow().is_some() {
            return Err(GpuiError::NotEligible {
                label: window.label().to_string(),
                reason: "Android hosts a single GPUI window",
            });
        }
        *self.android.pending.borrow_mut() = Some(PendingAttach {
            window: window.clone(),
            options,
            open,
        });
        self.waker.wake();
        Ok(())
    }

    fn attached(&self) -> Option<Rc<WindowInner>> {
        self.surfaces.borrow().values().next().cloned()
    }

    /// Applies queued view events. Runs at the start of every drain.
    pub(super) fn pump_android(&self) {
        let events = crate::android::take_events();
        for event in events {
            self.android_event(event);
        }
        self.mount_pending_android();
    }

    fn mount_pending_android(&self) {
        let params = {
            let surface = self.android.surface.borrow();
            let Some(surface) = surface.as_ref() else {
                return;
            };
            if self.android.pending.borrow().is_none() {
                return;
            }
            SurfaceParams {
                raw: raw_window(&surface.window),
                physical_size: physical_size(surface.width, surface.height),
                scale_factor: surface.density,
                origin: point(px(0.), px(0.)),
                appearance: if crate::android::is_dark() {
                    WindowAppearance::Dark
                } else {
                    WindowAppearance::Light
                },
            }
        };
        let Some(PendingAttach {
            window,
            options,
            open,
        }) = self.android.pending.borrow_mut().take()
        else {
            return;
        };
        let (width, height, density) = (
            params.physical_size.width.0,
            params.physical_size.height.0,
            params.scale_factor,
        );
        match self.attach_surface(&window, options, open, params) {
            Ok(()) => {
                log::info!(
                    "tauri-plugin-gpui: GPUI attached ({width}x{height} px, density {density})"
                );
                if let Some(inner) = self.attached() {
                    let insets = self.android.insets.borrow().clone();
                    inner.state.borrow_mut().insets = insets;
                    inner.state.borrow_mut().active = true;
                }
                // Tauri's setup has registered the app's plugins by now.
                crate::android::load_plugins();
            }
            Err(error) => log::error!("tauri-plugin-gpui: attaching GPUI failed: {error}"),
        }
    }

    fn android_event(&self, event: ViewEvent) {
        let inner = self.attached();
        let _guard = self.enter();
        match event {
            ViewEvent::SurfaceChanged {
                window,
                width,
                height,
                density,
            } => {
                if let Some(inner) = &inner {
                    let same = self
                        .android
                        .surface
                        .borrow()
                        .as_ref()
                        .is_some_and(|s| s.window.ptr() == window.ptr());
                    if !same {
                        self.replace_surface(inner, &window, width, height);
                    }
                    inner.resized(physical_size(width, height), density);
                    inner.force_present.set(true);
                    inner.schedule_frame();
                }
                *self.android.surface.borrow_mut() = Some(Surface {
                    window,
                    width,
                    height,
                    density,
                });
            }
            ViewEvent::SurfaceDestroyed(released) => {
                if let Some(inner) = &inner {
                    if let Some(renderer) = inner.state.borrow_mut().renderer.as_mut() {
                        renderer.unconfigure_surface();
                    }
                    set_visible(inner, false);
                }
                self.android.surface.borrow_mut().take();
                released.signal();
            }
            ViewEvent::Lifecycle { active } => {
                self.platform.app_lifecycle(if active {
                    AppLifecyclePhase::Active
                } else {
                    AppLifecyclePhase::Background
                });
                if let Some(inner) = &inner {
                    inner.state.borrow_mut().active = active;
                    inner.call_bool(|c| &mut c.active_status_change, active);
                    self.platform
                        .active_window
                        .set(active.then(|| inner.gpui_handle.get()).flatten());
                }
            }
            ViewEvent::Appearance { dark } => {
                if let Some(inner) = &inner {
                    inner.state.borrow_mut().appearance = if dark {
                        WindowAppearance::Dark
                    } else {
                        WindowAppearance::Light
                    };
                    inner.call_unit(|c| &mut c.appearance_changed);
                }
            }
            ViewEvent::Insets {
                left,
                top,
                right,
                bottom,
                ime_bottom,
            } => {
                let scale = crate::android::density().max(0.1);
                let logical = |v: i32| px(v as f32 / scale);
                let insets = WindowInsets {
                    safe_area: Edges {
                        top: logical(top),
                        right: logical(right),
                        bottom: logical(bottom),
                        left: logical(left),
                    },
                    ime: Edges {
                        bottom: logical(ime_bottom),
                        ..Default::default()
                    },
                };
                *self.android.insets.borrow_mut() = insets.clone();
                if let Some(inner) = &inner {
                    inner.state.borrow_mut().insets = insets.clone();
                    let callback = inner.callbacks.borrow_mut().insets_changed.take();
                    if let Some(mut callback) = callback {
                        callback(insets);
                        let mut callbacks = inner.callbacks.borrow_mut();
                        if callbacks.insets_changed.is_none() {
                            callbacks.insets_changed = Some(callback);
                        }
                    }
                    inner.schedule_frame();
                }
            }
            ViewEvent::LongPress { x, y } => {
                let scale = crate::android::density().max(0.1);
                self.android
                    .input
                    .borrow_mut()
                    .long_press(point(px(x / scale), px(y / scale)));
            }
            ViewEvent::HandleDrag {
                handle,
                phase,
                position,
            } => {
                if let Some(inner) = &inner {
                    self.handle_drag(inner, handle, phase, position);
                }
            }
            ViewEvent::Back => {
                // The app-level handler (`tauri_plugin_gpui::on_back`) wins;
                // otherwise GPUI's per-window back handler, if one is set.
                if let Some(handler) = crate::back_handler() {
                    self.cx.update(|cx| handler(cx));
                } else if let Some(inner) = &inner {
                    input_event(inner, ViewEvent::Back);
                }
            }
            event => {
                if let Some(inner) = &inner {
                    input_event(inner, event);
                }
            }
        }
    }

    fn replace_surface(&self, inner: &WindowInner, window: &NativeWindow, width: u32, height: u32) {
        let raw = raw_window(window);
        let instance = self
            .platform
            .gpu_context
            .borrow()
            .as_ref()
            .map(|context| context.instance.clone());
        let Some(instance) = instance else {
            log::error!("tauri-plugin-gpui: no GPU context to recreate the surface with");
            return;
        };
        let result = {
            let mut state = inner.state.borrow_mut();
            let Some(renderer) = state.renderer.as_mut() else {
                return;
            };
            renderer.replace_surface(
                &raw,
                WgpuSurfaceConfig {
                    size: physical_size(width, height),
                    transparent: false,
                    preferred_present_mode: None,
                },
                &instance,
            )
        };
        match result {
            Ok(()) => {
                inner.raw.set(raw);
                set_visible(inner, true);
            }
            Err(error) => {
                log::error!("tauri-plugin-gpui: recreating the surface failed: {error:#}")
            }
        }
    }
}

fn set_visible(inner: &WindowInner, visible: bool) {
    if std::mem::replace(&mut inner.state.borrow_mut().visible, visible) == visible {
        return;
    }
    let callback = inner.callbacks.borrow_mut().visibility_change.take();
    if let Some(mut callback) = callback {
        callback(if visible {
            WindowVisibility::Visible
        } else {
            WindowVisibility::Hidden
        });
        let mut callbacks = inner.callbacks.borrow_mut();
        if callbacks.visibility_change.is_none() {
            callbacks.visibility_change = Some(callback);
        }
    }
}

fn press(inner: &WindowInner, keystroke: Keystroke) {
    inner.handle_input(PlatformInput::KeyDown(KeyDownEvent {
        keystroke: keystroke.clone(),
        is_held: false,
        prefer_character_input: false,
    }));
    inner.handle_input(PlatformInput::KeyUp(KeyUpEvent { keystroke }));
}

fn named(key: &str) -> Keystroke {
    Keystroke {
        modifiers: Default::default(),
        key: key.into(),
        key_char: None,
    }
}

fn has_marked_text(inner: &WindowInner) -> bool {
    let mut marked = false;
    inner.with_input_handler(|handler| marked = handler.marked_text_range().is_some());
    marked
}

fn input_event(inner: &WindowInner, event: ViewEvent) {
    if !matches!(event, ViewEvent::Touch { .. } | ViewEvent::Back) {
        // Typing and toolbar actions can change the text without moving the
        // caret (delete, autofill): have it read again after the frame.
        inner.text_input_changed(None);
    }
    match event {
        ViewEvent::Touch { phase, id, x, y } => {
            let scale = inner.state.borrow().scale_factor;
            let position = point(px(x / scale), px(y / scale));
            inner.state.borrow_mut().mouse_position = position;
            inner.handle_input(PlatformInput::Touch(TouchEvent {
                id: TouchId(id as u64),
                phase: match phase {
                    0 => TouchPhase::Started,
                    1 => TouchPhase::Moved,
                    2 => TouchPhase::Ended,
                    _ => TouchPhase::Cancelled,
                },
                position,
                predicted_position: None,
                force: None,
            }));
        }
        ViewEvent::Key {
            down,
            key_code,
            unicode,
            meta,
            repeat,
        } => {
            inner.state.borrow_mut().modifiers = keys::modifiers(meta);
            let Some(keystroke) = keys::keystroke(key_code, unicode, meta) else {
                return;
            };
            inner.handle_input(if down {
                PlatformInput::KeyDown(KeyDownEvent {
                    keystroke,
                    is_held: repeat,
                    prefer_character_input: false,
                })
            } else {
                PlatformInput::KeyUp(KeyUpEvent { keystroke })
            });
        }
        ViewEvent::CommitText(text) => {
            let mut chars = text.chars();
            match (chars.next(), chars.next()) {
                // One typed character: deliver it as a key press so key
                // bindings see it; unhandled presses insert their text.
                (Some(c), None) if !has_marked_text(inner) => press(inner, keys::char_keystroke(c)),
                _ => {
                    for (i, line) in text.split('\n').enumerate() {
                        if i > 0 {
                            press(inner, named("enter"));
                        }
                        if !line.is_empty() {
                            inner.insert_text(line);
                        }
                    }
                }
            }
        }
        ViewEvent::ComposingText(text) => inner.with_input_handler(|handler| {
            if text.is_empty() {
                handler.replace_text_in_range(None, "");
                handler.unmark_text();
            } else {
                handler.replace_and_mark_text_in_range(None, &text, None);
            }
        }),
        ViewEvent::FinishComposing => inner.with_input_handler(|handler| handler.unmark_text()),
        ViewEvent::DeleteSurrounding { before, after } => {
            for _ in 0..before {
                press(inner, named("backspace"));
            }
            for _ in 0..after {
                press(inner, named("delete"));
            }
        }
        ViewEvent::EditAction(action) => press(inner, action.keystroke()),
        ViewEvent::Autofill(text) => {
            // Autofill replaces the whole value: select it, then type over it.
            press(inner, keys::EditAction::SelectAll.keystroke());
            inner.insert_text(&text);
        }
        ViewEvent::Back => {
            let callback = inner.callbacks.borrow_mut().back.take();
            if let Some(mut callback) = callback {
                callback();
                let mut callbacks = inner.callbacks.borrow_mut();
                if callbacks.back.is_none() {
                    callbacks.back = Some(callback);
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn loads_only_ui_mono_and_emoji_fonts() {
        assert!(super::wanted_font("Roboto-Regular.ttf"));
        assert!(super::wanted_font("NotoColorEmoji.ttf"));
        assert!(super::wanted_font("DroidSansMono.ttf"));
        assert!(!super::wanted_font("NotoSansCJK-Regular.ttc"));
        assert!(!super::wanted_font("fonts.xml"));
    }
}
