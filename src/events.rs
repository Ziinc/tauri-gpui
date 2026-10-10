//! Translation of the TAO event subset GPUI needs.

#[cfg(any(gpui_mobile, test))]
#[cfg_attr(not(gpui_mobile), allow(dead_code))]
pub(crate) mod mobile;

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
            inner.sync_window_mode();
            let scale = inner.state.borrow().scale_factor;
            inner.resized(physical_size(physical.width, physical.height), scale);
        }
        WindowEvent::ScaleFactorChanged {
            scale_factor,
            new_inner_size,
        } => {
            inner.sync_window_mode();
            inner.resized(
                physical_size(new_inner_size.width, new_inner_size.height),
                *scale_factor as f32,
            );
        }
        WindowEvent::Moved(position) => {
            inner.sync_window_mode();
            {
                let mut state = inner.state.borrow_mut();
                let scale = state.scale_factor;
                state.origin = point(px(position.x as f32 / scale), px(position.y as f32 / scale));
            }
            inner.call_unit(|c| &mut c.moved);
        }
        WindowEvent::Focused(focused) => {
            inner.state.borrow_mut().active = *focused;
            if !focused {
                inner.state.borrow_mut().last_key_text = None;
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
            let button = mouse_button(*button);
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

fn mouse_button(button: TaoMouseButton) -> MouseButton {
    match button {
        TaoMouseButton::Left => MouseButton::Left,
        TaoMouseButton::Right => MouseButton::Right,
        TaoMouseButton::Middle => MouseButton::Middle,
        TaoMouseButton::Other(8) => MouseButton::Navigate(NavigationDirection::Back),
        TaoMouseButton::Other(9) => MouseButton::Navigate(NavigationDirection::Forward),
        _ => MouseButton::Left,
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
    // TAO backend (on Linux `text` ignores modifiers). Shortcuts produce none.
    let key_char = match &event.logical_key {
        _ if modifiers.control || modifiers.platform => None,
        Key::Space => Some(" ".to_string()),
        Key::Character(c) => Some(c.to_string()),
        _ => None,
    };
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
        assert_eq!(
            mouse_button(TaoMouseButton::Other(8)),
            MouseButton::Navigate(NavigationDirection::Back)
        );
        assert_eq!(mouse_button(TaoMouseButton::Right), MouseButton::Right);
    }
}
