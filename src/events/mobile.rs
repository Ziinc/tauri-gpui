//! Input shared by the mobile platform layers: touches and text committed
//! by a software keyboard.

use gpui::{
    KeyDownEvent, KeyUpEvent, Keystroke, Modifiers, PlatformInput, TouchEvent, TouchId, TouchPhase,
    point, px,
};

use crate::platform::window::WindowInner;

/// Feeds one raw touch, in physical pixels, to GPUI's gesture arena.
pub(crate) fn touch(inner: &WindowInner, id: u64, phase: TouchPhase, x: f32, y: f32) {
    let scale = inner.state.borrow().scale_factor;
    let position = point(px(x / scale), px(y / scale));
    inner.state.borrow_mut().mouse_position = position;
    inner.handle_input(PlatformInput::Touch(TouchEvent {
        id: TouchId(id),
        phase,
        position,
        predicted_position: None,
        force: None,
    }));
}

/// Delivers a key press and release.
pub(crate) fn press(inner: &WindowInner, keystroke: Keystroke) {
    inner.handle_input(PlatformInput::KeyDown(KeyDownEvent {
        keystroke: keystroke.clone(),
        is_held: false,
        prefer_character_input: false,
    }));
    inner.handle_input(PlatformInput::KeyUp(KeyUpEvent { keystroke }));
}

pub(crate) fn named(key: &str) -> Keystroke {
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

/// Text the keyboard committed. One typed character is delivered as a key
/// press so key bindings see it (unhandled presses insert their text);
/// longer commits are inserted, with newlines as `enter` presses.
pub(crate) fn commit_text(inner: &WindowInner, text: &str) {
    let mut chars = text.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if !has_marked_text(inner) => press(inner, char_keystroke(c)),
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

/// Keystroke for one character of committed text.
pub(crate) fn char_keystroke(c: char) -> Keystroke {
    match c {
        '\n' => named("enter"),
        ' ' => Keystroke {
            modifiers: Modifiers::default(),
            key: "space".into(),
            key_char: Some(" ".into()),
        },
        c => Keystroke {
            modifiers: Modifiers {
                shift: c.is_uppercase(),
                ..Modifiers::default()
            },
            key: c.to_lowercase().to_string(),
            key_char: Some(c.to_string()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn committed_newline_is_enter() {
        assert_eq!(char_keystroke('\n').key, "enter");
        let upper = char_keystroke('Q');
        assert_eq!((upper.key.as_str(), upper.modifiers.shift), ("q", true));
    }
}
