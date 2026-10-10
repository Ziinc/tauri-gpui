//! Android: feeds `GpuiView` events (surface, touch, keys, IME, insets) to
//! the attached GPUI window. See `crate::android` for the JNI side.

mod input;

use std::{cell::RefCell, ffi::c_void, path::Path, rc::Rc};

use gpui::{
    AppLifecyclePhase, Edges, KeyDownEvent, KeyUpEvent, PlatformInput, PlatformTextSystem,
    TouchPhase, WindowAppearance, WindowInsets, point, px,
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
    events::{mobile, physical_size},
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
                    inner.set_visible(false);
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
                    inner.set_insets(insets);
                }
            }
            ViewEvent::LongPress { x, y } => {
                let scale = crate::android::density().max(0.1);
                self.android
                    .input
                    .borrow_mut()
                    .long_press(point(px(x / scale), px(y / scale)));
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
                inner.set_visible(true);
            }
            Err(error) => {
                log::error!("tauri-plugin-gpui: recreating the surface failed: {error:#}")
            }
        }
    }
}

fn input_event(inner: &WindowInner, event: ViewEvent) {
    if !matches!(event, ViewEvent::Touch { .. } | ViewEvent::Back) {
        // Typing and toolbar actions can change the text without moving the
        // caret (delete, autofill): have it read again after the frame.
        inner.text_input_changed(None);
    }
    match event {
        ViewEvent::Touch { phase, id, x, y } => {
            let phase = match phase {
                0 => TouchPhase::Started,
                1 => TouchPhase::Moved,
                2 => TouchPhase::Ended,
                _ => TouchPhase::Cancelled,
            };
            mobile::touch(inner, id as u64, phase, x, y);
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
        ViewEvent::CommitText(text) => mobile::commit_text(inner, &text),
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
                mobile::press(inner, mobile::named("backspace"));
            }
            for _ in 0..after {
                mobile::press(inner, mobile::named("delete"));
            }
        }
        ViewEvent::EditAction(action) => mobile::press(inner, action.keystroke()),
        ViewEvent::Autofill(text) => {
            // Autofill replaces the whole value: select it, then type over it.
            mobile::press(inner, keys::EditAction::SelectAll.keystroke());
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
