//! Translation of the TAO event subset GPUI needs.

use gpui::{
    Capslock, KeyDownEvent, KeyUpEvent, Keystroke, Modifiers, ModifiersChangedEvent, MouseButton,
    MouseDownEvent, MouseExitEvent, MouseMoveEvent, MouseUpEvent, NavigationDirection,
    PlatformInput, ScrollDelta, ScrollWheelEvent, TouchPhase, WindowAppearance, point, px,
};
use tauri_runtime_wry::tao::{
    dpi::PhysicalPosition,
    event::{
        ElementState, KeyEvent, MouseButton as TaoMouseButton, MouseScrollDelta,
        TouchPhase as TaoTouchPhase, WindowEvent,
    },
    keyboard::{Key, ModifiersState},
    window::Theme,
};

use crate::platform::window::WindowInner;
use gpui::{DevicePixels, size};

/// Routes one TAO window event into the attached GPUI window.
/// Returns `true` when the window was destroyed and must be unregistered.
pub(crate) fn dispatch(inner: &WindowInner, event: &WindowEvent<'_>) -> bool {
    match event {
        WindowEvent::Resized(physical) => {
            inner.window_mode_changed();
            let scale = inner.state.borrow().scale_factor;
            inner.resized(physical_size(physical.width, physical.height), scale);
        }
        WindowEvent::ScaleFactorChanged {
            scale_factor,
            new_inner_size,
        } => {
            inner.window_mode_changed();
            inner.resized(
                physical_size(new_inner_size.width, new_inner_size.height),
                *scale_factor as f32,
            );
        }
        WindowEvent::Moved(position) => {
            inner.window_moved();
            {
                let mut state = inner.state.borrow_mut();
                let scale = state.scale_factor;
                state.origin = point(px(position.x as f32 / scale), px(position.y as f32 / scale));
            }
            inner.call_unit(|c| &mut c.moved);
        }
        WindowEvent::Focused(focused) => {
            // Minimizing and restoring change focus on every desktop.
            inner.window_mode_changed();
            inner.state.borrow_mut().active = *focused;
            if !focused {
                inner.state.borrow_mut().typed_text.reset();
            }
            inner.call_bool(|c| &mut c.active_status_change, *focused);
        }
        WindowEvent::CursorEntered { .. } => {
            inner.state.borrow_mut().hovered = true;
            inner.call_bool(|c| &mut c.hover_status_change, true);
        }
        WindowEvent::CursorLeft { .. } => {
            let (position, pressed_button, modifiers) = {
                let mut state = inner.state.borrow_mut();
                state.hovered = false;
                (state.mouse_position, state.pressed_button, state.modifiers)
            };
            inner.handle_input(PlatformInput::MouseExited(MouseExitEvent {
                position,
                pressed_button,
                modifiers,
            }));
            inner.call_bool(|c| &mut c.hover_status_change, false);
        }
        WindowEvent::CursorMoved { position, .. } => {
            let (position, pressed_button, modifiers) = {
                let mut state = inner.state.borrow_mut();
                state.mouse_position = logical_position(*position, state.scale_factor);
                (state.mouse_position, state.pressed_button, state.modifiers)
            };
            inner.handle_input(PlatformInput::MouseMove(MouseMoveEvent {
                position,
                pressed_button,
                modifiers,
            }));
        }
        WindowEvent::MouseInput { state, button, .. } => {
            let Some(button) = mouse_button(*button, Os::CURRENT) else {
                return false;
            };
            let (position, modifiers) = {
                let window_state = inner.state.borrow();
                (window_state.mouse_position, window_state.modifiers)
            };
            match state {
                ElementState::Pressed => {
                    inner.state.borrow_mut().pressed_button = Some(button);
                    let click_count = inner.register_click(button, position);
                    inner.handle_input(PlatformInput::MouseDown(MouseDownEvent {
                        button,
                        position,
                        modifiers,
                        click_count,
                        first_mouse: false,
                    }));
                }
                ElementState::Released => {
                    let click_count = {
                        let mut window_state = inner.state.borrow_mut();
                        window_state.pressed_button = None;
                        window_state.click.count.max(1)
                    };
                    inner.handle_input(PlatformInput::MouseUp(MouseUpEvent {
                        button,
                        position,
                        modifiers,
                        click_count,
                    }));
                }
                _ => {}
            }
        }
        WindowEvent::MouseWheel { delta, phase, .. } => {
            let (position, modifiers, scale) = {
                let state = inner.state.borrow();
                (state.mouse_position, state.modifiers, state.scale_factor)
            };
            let delta = match delta {
                MouseScrollDelta::LineDelta(x, y) => ScrollDelta::Lines(point(*x, *y)),
                MouseScrollDelta::PixelDelta(p) => {
                    ScrollDelta::Pixels(point(px(p.x as f32 / scale), px(p.y as f32 / scale)))
                }
                _ => return false,
            };
            inner.handle_input(PlatformInput::ScrollWheel(ScrollWheelEvent {
                position,
                delta,
                modifiers,
                touch_phase: touch_phase(*phase),
            }));
        }
        WindowEvent::ModifiersChanged(state) => {
            let modifiers = modifiers(*state);
            let capslock = {
                let mut window_state = inner.state.borrow_mut();
                window_state.modifiers = modifiers;
                window_state.capslock
            };
            inner.handle_input(PlatformInput::ModifiersChanged(ModifiersChangedEvent {
                modifiers,
                capslock,
            }));
        }
        WindowEvent::KeyboardInput { event, .. } => {
            let modifiers = inner.state.borrow().modifiers;
            if matches!(event.logical_key, Key::CapsLock) && event.state == ElementState::Pressed {
                let mut state = inner.state.borrow_mut();
                state.capslock = Capslock {
                    on: !state.capslock.on,
                };
            }
            let Some(keystroke) = keystroke(event, modifiers) else {
                return false;
            };
            match event.state {
                ElementState::Pressed => {
                    inner.handle_input(PlatformInput::KeyDown(KeyDownEvent {
                        keystroke,
                        is_held: event.repeat,
                        prefer_character_input: false,
                    }));
                }
                ElementState::Released => {
                    inner.handle_input(PlatformInput::KeyUp(KeyUpEvent { keystroke }));
                }
                _ => {}
            }
        }
        WindowEvent::ReceivedImeText(text) => inner.handle_ime_commit(text),
        WindowEvent::ThemeChanged(theme) => {
            inner.state.borrow_mut().appearance = appearance(*theme);
            inner.call_unit(|c| &mut c.appearance_changed);
        }
        WindowEvent::Destroyed => return true,
        _ => {}
    }
    false
}

pub(crate) fn physical_size(width: u32, height: u32) -> gpui::Size<DevicePixels> {
    size(DevicePixels(width as i32), DevicePixels(height as i32))
}

pub(crate) fn appearance(theme: Theme) -> WindowAppearance {
    match theme {
        Theme::Dark => WindowAppearance::Dark,
        _ => WindowAppearance::Light,
    }
}

fn logical_position(position: PhysicalPosition<f64>, scale: f32) -> gpui::Point<gpui::Pixels> {
    point(px(position.x as f32 / scale), px(position.y as f32 / scale))
}

/// The desktop OS whose TAO backend produced an event. A parameter rather
/// than `cfg!` so every platform's mapping is unit-tested on every host.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Os {
    Linux,
    Windows,
    Mac,
}

impl Os {
    pub(crate) const CURRENT: Os = if cfg!(target_os = "windows") {
        Os::Windows
    } else if cfg!(target_os = "macos") {
        Os::Mac
    } else {
        Os::Linux
    };
}

/// Maps a TAO button; `None` for buttons GPUI has no equivalent for.
fn mouse_button(button: TaoMouseButton, os: Os) -> Option<MouseButton> {
    // Back/forward: X11 buttons 8/9; Win32 XBUTTON1/XBUTTON2. macOS TAO
    // reports every extra button as Middle.
    let (back, forward) = match os {
        Os::Windows => (1, 2),
        _ => (8, 9),
    };
    Some(match button {
        TaoMouseButton::Left => MouseButton::Left,
        TaoMouseButton::Right => MouseButton::Right,
        TaoMouseButton::Middle => MouseButton::Middle,
        TaoMouseButton::Other(n) if n == back => MouseButton::Navigate(NavigationDirection::Back),
        TaoMouseButton::Other(n) if n == forward => {
            MouseButton::Navigate(NavigationDirection::Forward)
        }
        _ => return None,
    })
}

/// Whether a key pressed with `modifiers` types its character (GPUI's
/// `key_char`) rather than being a shortcut.
fn produces_text(modifiers: &Modifiers, os: Os) -> bool {
    if modifiers.platform {
        return false;
    }
    match os {
        // Option composes characters (Option+L is `@` on a German layout).
        Os::Mac => !modifiers.control,
        // AltGr arrives as Ctrl+Alt; Ctrl or Alt alone is a shortcut.
        Os::Windows => modifiers.control == modifiers.alt,
        // AltGr is ISO_Level3_Shift, not a reported modifier.
        Os::Linux => !modifiers.control && !modifiers.alt,
    }
}

fn touch_phase(phase: TaoTouchPhase) -> TouchPhase {
    match phase {
        TaoTouchPhase::Started => TouchPhase::Started,
        TaoTouchPhase::Moved => TouchPhase::Moved,
        _ => TouchPhase::Ended,
    }
}

fn modifiers(state: ModifiersState) -> Modifiers {
    Modifiers {
        control: state.control_key(),
        alt: state.alt_key(),
        shift: state.shift_key(),
        platform: state.super_key(),
        function: false,
    }
}

/// The text a key press types (GPUI's `key_char`): `character` is the
/// layout-aware character, `key` the unmodified key name. Shortcuts type none.
fn typed_char(character: Option<&str>, key: &str, modifiers: &Modifiers, os: Os) -> Option<String> {
    let character = character.filter(|_| produces_text(modifiers, os))?;
    // Ctrl+Alt is AltGr only if the layout's AltGr layer changed the key.
    if os == Os::Windows && modifiers.control && character.to_lowercase() == key {
        return None;
    }
    Some(character.to_owned())
}

/// Builds a GPUI keystroke. `key` follows GPUI's naming (lowercase, unshifted
/// character or a named key such as `enter`); `key_char` is the typed text.
fn keystroke(event: &KeyEvent, modifiers: Modifiers) -> Option<Keystroke> {
    let key = match &event.logical_key {
        Key::Character(_) | Key::Dead(_) | Key::Unidentified(_) => {
            match event.key_without_modifiers() {
                Key::Character(c) => c.to_lowercase(),
                _ => return None,
            }
        }
        named => named_key(named)?.to_string(),
    };
    // `logical_key` carries the layout- and shift-aware character on every
    // TAO backend (on Linux `text` ignores modifiers).
    let character = match &event.logical_key {
        Key::Space => Some(" "),
        Key::Character(c) => Some(&**c),
        _ => None,
    };
    let key_char = typed_char(character, &key, &modifiers, Os::CURRENT);
    Some(Keystroke {
        modifiers,
        key,
        key_char,
    })
}

fn named_key(key: &Key<'_>) -> Option<&'static str> {
    Some(match key {
        Key::Enter => "enter",
        Key::Tab => "tab",
        Key::Space => "space",
        Key::Backspace => "backspace",
        Key::Delete => "delete",
        Key::Escape => "escape",
        Key::Insert => "insert",
        Key::Home => "home",
        Key::End => "end",
        Key::PageUp => "pageup",
        Key::PageDown => "pagedown",
        Key::ArrowUp => "up",
        Key::ArrowDown => "down",
        Key::ArrowLeft => "left",
        Key::ArrowRight => "right",
        Key::ContextMenu => "menu",
        Key::F1 => "f1",
        Key::F2 => "f2",
        Key::F3 => "f3",
        Key::F4 => "f4",
        Key::F5 => "f5",
        Key::F6 => "f6",
        Key::F7 => "f7",
        Key::F8 => "f8",
        Key::F9 => "f9",
        Key::F10 => "f10",
        Key::F11 => "f11",
        Key::F12 => "f12",
        // Modifier and lock keys arrive as `ModifiersChanged`.
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_keys_use_gpui_names() {
        assert_eq!(named_key(&Key::Enter), Some("enter"));
        assert_eq!(named_key(&Key::ArrowLeft), Some("left"));
        assert_eq!(named_key(&Key::PageDown), Some("pagedown"));
        assert_eq!(named_key(&Key::F12), Some("f12"));
        // Modifiers are reported through ModifiersChanged, not as keys.
        assert_eq!(named_key(&Key::Shift), None);
    }

    #[test]
    fn super_maps_to_platform_modifier() {
        let m = modifiers(ModifiersState::SUPER | ModifiersState::SHIFT);
        assert!(m.platform && m.shift && !m.control && !m.alt);
    }

    #[test]
    fn navigation_buttons() {
        let back = Some(MouseButton::Navigate(NavigationDirection::Back));
        let forward = Some(MouseButton::Navigate(NavigationDirection::Forward));
        // X11 reports buttons 8/9, Win32 reports XBUTTON1/XBUTTON2 as 1/2.
        assert_eq!(mouse_button(TaoMouseButton::Other(8), Os::Linux), back);
        assert_eq!(mouse_button(TaoMouseButton::Other(9), Os::Linux), forward);
        assert_eq!(mouse_button(TaoMouseButton::Other(1), Os::Windows), back);
        assert_eq!(mouse_button(TaoMouseButton::Other(2), Os::Windows), forward);
        assert_eq!(
            mouse_button(TaoMouseButton::Right, Os::Linux),
            Some(MouseButton::Right)
        );
    }

    #[test]
    fn unknown_buttons_are_dropped_not_left_clicks() {
        assert_eq!(mouse_button(TaoMouseButton::Other(1), Os::Linux), None);
        assert_eq!(mouse_button(TaoMouseButton::Other(8), Os::Windows), None);
        assert_eq!(mouse_button(TaoMouseButton::Other(12), Os::Mac), None);
    }

    fn mods(control: bool, alt: bool, shift: bool, platform: bool) -> Modifiers {
        Modifiers {
            control,
            alt,
            shift,
            platform,
            function: false,
        }
    }

    #[test]
    fn plain_and_shifted_keys_type_text_everywhere() {
        for os in [Os::Linux, Os::Windows, Os::Mac] {
            assert!(produces_text(&mods(false, false, false, false), os));
            assert!(produces_text(&mods(false, false, true, false), os));
        }
    }

    #[test]
    fn option_types_text_on_macos() {
        // Option+L is `@` on a German Mac layout.
        assert!(produces_text(&mods(false, true, false, false), Os::Mac));
        assert!(produces_text(&mods(false, true, true, false), Os::Mac));
        assert!(!produces_text(&mods(true, false, false, false), Os::Mac));
        assert!(!produces_text(&mods(false, false, false, true), Os::Mac));
    }

    #[test]
    fn altgr_types_text_on_windows() {
        // Windows reports AltGr as Ctrl+Alt (AltGr+Q is `@` on German layouts).
        assert!(produces_text(&mods(true, true, false, false), Os::Windows));
        assert!(!produces_text(
            &mods(true, false, false, false),
            Os::Windows
        ));
        assert!(!produces_text(
            &mods(false, true, false, false),
            Os::Windows
        ));
        assert!(!produces_text(&mods(true, true, false, true), Os::Windows));
    }

    #[test]
    fn windows_ctrl_alt_types_only_altgr_characters() {
        let ctrl_alt = mods(true, true, false, false);
        // German AltGr+Q: the layout produced a different character.
        assert_eq!(
            typed_char(Some("@"), "q", &ctrl_alt, Os::Windows).as_deref(),
            Some("@")
        );
        // US Ctrl+Alt+A: no AltGr layer, so it is a shortcut.
        assert_eq!(typed_char(Some("a"), "a", &ctrl_alt, Os::Windows), None);
        assert_eq!(typed_char(Some("A"), "a", &ctrl_alt, Os::Windows), None);
        // Without Ctrl+Alt the character is typed as is.
        let plain = mods(false, false, false, false);
        assert_eq!(
            typed_char(Some("a"), "a", &plain, Os::Windows).as_deref(),
            Some("a")
        );
        assert_eq!(typed_char(None, "enter", &plain, Os::Windows), None);
    }

    #[test]
    fn linux_shortcuts_type_no_text() {
        // AltGr is ISO_Level3_Shift on X11/Wayland, not a reported modifier.
        assert!(!produces_text(&mods(true, false, false, false), Os::Linux));
        assert!(!produces_text(&mods(false, true, false, false), Os::Linux));
        assert!(!produces_text(&mods(true, true, false, false), Os::Linux));
        assert!(!produces_text(&mods(false, false, false, true), Os::Linux));
    }
}
