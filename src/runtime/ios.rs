//! iOS: TAO's own window events (touches, resizes, focus) plus the
//! `GpuiInputView` events (keyboard, insets, appearance, lifecycle) for the
//! attached GPUI window. See `crate::ios` for the UIKit side.

use std::{
    cell::{Cell, RefCell},
    path::Path,
    rc::Rc,
};

use gpui::{
    AppLifecyclePhase, Edges, KeyDownEvent, KeyUpEvent, PlatformInput, PlatformTextSystem,
    TouchPhase, WindowAppearance, WindowInsets, px,
};
use raw_window_handle::RawWindowHandle;
use tauri::Wry;
use tauri_runtime_wry::tao::event::{TouchPhase as TaoTouchPhase, WindowEvent};

use super::{OpenWindow, Runtime};
use crate::{
    GpuiError, GpuiOptions,
    events::mobile,
    ios::{ViewEvent, keys::HardwareKey},
    platform::window::WindowInner,
};

#[derive(Default)]
pub(crate) struct IosState {
    /// The app is in the background, where iOS forbids GPU work: frame
    /// requests wait until it returns.
    pub(super) background: Cell<bool>,
    insets: RefCell<WindowInsets>,
}

/// Directories under `/System/Library/Fonts` an app may read.
const FONT_DIRS: [&str; 3] = ["Core", "CoreUI", "CoreAddition"];

/// Font files worth loading: the UI, monospace and emoji families. Loading
/// every system font would cost tens of megabytes.
fn wanted_font(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    (name.ends_with(".ttf") || name.ends_with(".otf") || name.ends_with(".ttc"))
        && (name.starts_with("helvetica")
            || name.starts_with("sfui")
            || name.starts_with("sfns")
            || name.starts_with("sfpro")
            || name.starts_with("sfmono")
            || name.starts_with("menlo")
            || name.starts_with("courier")
            || name.starts_with("applecoloremoji"))
}

pub(crate) fn load_system_fonts(text_system: &dyn PlatformTextSystem) {
    let root = Path::new("/System/Library/Fonts");
    let fonts: Vec<_> = FONT_DIRS
        .iter()
        .flat_map(|dir| std::fs::read_dir(root.join(dir)).into_iter().flatten())
        .flatten()
        .filter(|entry| entry.file_name().to_str().is_some_and(wanted_font))
        .filter_map(|entry| std::fs::read(entry.path()).ok())
        .map(std::borrow::Cow::Owned)
        .collect();
    let count = fonts.len();
    if count == 0 {
        log::error!("tauri-plugin-gpui: found no system fonts under {root:?}");
    }
    if let Err(error) = text_system.add_fonts(fonts) {
        log::error!("tauri-plugin-gpui: loading system fonts failed: {error:#}");
    } else {
        log::info!("tauri-plugin-gpui: loaded {count} system fonts");
    }
}

impl Runtime {
    /// iOS renders into TAO's own `UIView`, like desktop, and lays the
    /// keyboard and insets view over it.
    pub(super) fn attach_ios(
        &self,
        window: &tauri::Window<Wry>,
        options: GpuiOptions,
        open: OpenWindow,
    ) -> Result<(), GpuiError> {
        if !self.surfaces.borrow().is_empty() {
            return Err(GpuiError::NotEligible {
                label: window.label().to_string(),
                reason: "iOS hosts a single GPUI window",
            });
        }
        let mut params = Self::desktop_surface(window)?;
        let RawWindowHandle::UiKit(handle) = params.raw.window else {
            return Err(GpuiError::PlatformInitialization(
                "expected a UIKit window handle".into(),
            ));
        };
        // SAFETY: the handle is TAO's live view for this window.
        unsafe { crate::ios::install(handle.ui_view.as_ptr()) };
        params.appearance = if crate::ios::is_dark() {
            WindowAppearance::Dark
        } else {
            WindowAppearance::Light
        };
        let result = self.attach_surface(window, options, open, params);
        if result.is_err() {
            crate::ios::uninstall();
        }
        result
    }

    fn attached(&self) -> Option<Rc<WindowInner>> {
        self.surfaces.borrow().values().next().cloned()
    }

    /// Applies queued input view events. Runs at the start of every drain.
    pub(super) fn pump_ios(&self) {
        let events = crate::ios::take_events();
        if events.is_empty() {
            return;
        }
        let inner = self.attached();
        let _guard = self.enter();
        for event in events {
            self.ios_event(inner.as_deref(), event);
        }
    }

    fn ios_event(&self, inner: Option<&WindowInner>, event: ViewEvent) {
        match event {
            ViewEvent::Lifecycle(phase) => {
                self.platform.app_lifecycle(phase);
                match phase {
                    AppLifecyclePhase::Background => {
                        self.ios.background.set(true);
                        if let Some(inner) = inner {
                            inner.set_visible(false);
                        }
                    }
                    AppLifecyclePhase::Foreground => {
                        self.ios.background.set(false);
                        if let Some(inner) = inner {
                            inner.set_visible(true);
                            inner.force_present.set(true);
                            inner.schedule_frame();
                        }
                    }
                    _ => {}
                }
            }
            ViewEvent::SafeArea(safe_area) => {
                self.update_insets(inner, |insets| {
                    insets.safe_area = Edges {
                        top: px(safe_area.top as f32),
                        right: px(safe_area.right as f32),
                        bottom: px(safe_area.bottom as f32),
                        left: px(safe_area.left as f32),
                    }
                });
            }
            ViewEvent::KeyboardBottom(bottom) => {
                self.update_insets(inner, |insets| {
                    insets.ime = Edges {
                        bottom: px(bottom as f32),
                        ..Default::default()
                    }
                });
            }
            ViewEvent::Appearance { dark } => {
                if let Some(inner) = inner {
                    inner.state.borrow_mut().appearance = if dark {
                        WindowAppearance::Dark
                    } else {
                        WindowAppearance::Light
                    };
                    inner.call_unit(|c| &mut c.appearance_changed);
                }
            }
            ViewEvent::InsertText(text) => {
                if let Some(inner) = inner {
                    mobile::commit_text(inner, &text);
                }
            }
            ViewEvent::DeleteBackward => {
                if let Some(inner) = inner {
                    mobile::press(inner, mobile::named("backspace"));
                }
            }
            ViewEvent::Key {
                down,
                key: HardwareKey(keystroke),
            } => {
                if let Some(inner) = inner {
                    inner.state.borrow_mut().modifiers = keystroke.modifiers;
                    inner.handle_input(if down {
                        PlatformInput::KeyDown(KeyDownEvent {
                            keystroke,
                            is_held: false,
                            prefer_character_input: false,
                        })
                    } else {
                        PlatformInput::KeyUp(KeyUpEvent { keystroke })
                    });
                }
            }
        }
    }

    fn update_insets(&self, inner: Option<&WindowInner>, update: impl FnOnce(&mut WindowInsets)) {
        let insets = {
            let mut insets = self.ios.insets.borrow_mut();
            update(&mut insets);
            insets.clone()
        };
        if let Some(inner) = inner {
            inner.set_insets(insets);
        }
    }

    /// Insets reported before the GPUI window was mounted.
    pub(super) fn ios_insets(&self) -> WindowInsets {
        self.ios.insets.borrow().clone()
    }

    /// TAO window events with an iOS meaning. Returns `true` when handled.
    pub(super) fn ios_window_event(&self, inner: &WindowInner, event: &WindowEvent<'_>) -> bool {
        match event {
            WindowEvent::Touch(touch) => {
                let phase = match touch.phase {
                    TaoTouchPhase::Started => TouchPhase::Started,
                    TaoTouchPhase::Moved => TouchPhase::Moved,
                    TaoTouchPhase::Ended => TouchPhase::Ended,
                    _ => TouchPhase::Cancelled,
                };
                let _guard = self.enter();
                mobile::touch(
                    inner,
                    touch.id,
                    phase,
                    touch.location.x as f32,
                    touch.location.y as f32,
                );
                true
            }
            // `UIApplication` notifications drive the lifecycle instead: TAO
            // suspends on resigning active but only resumes on entering the
            // foreground, which misses e.g. dismissing Control Center.
            WindowEvent::Suspended | WindowEvent::Resumed => true,
            WindowEvent::Destroyed => {
                crate::ios::uninstall();
                false
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn loads_only_ui_mono_and_emoji_fonts() {
        assert!(super::wanted_font("HelveticaNeue.ttc"));
        assert!(super::wanted_font("SFUI.ttf"));
        assert!(super::wanted_font("Menlo.ttc"));
        assert!(super::wanted_font("AppleColorEmoji@2x.ttc"));
        assert!(!super::wanted_font("PingFang.ttc"));
        assert!(!super::wanted_font("Helvetica.plist"));
    }
}
