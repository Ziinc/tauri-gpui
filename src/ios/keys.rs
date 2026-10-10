//! Hardware keyboard presses (`UIKey`) to GPUI keystrokes.
//!
//! Keys that type text go through `UIKeyInput` like the software keyboard;
//! only navigation and function keys, and shortcuts, become keystrokes here.

use gpui::{Keystroke, Modifiers};

// UIKeyModifierFlags.
const SHIFT: u64 = 1 << 17;
const CONTROL: u64 = 1 << 18;
const ALTERNATE: u64 = 1 << 19;
const COMMAND: u64 = 1 << 20;

// UIKeyboardHIDUsage (USB HID keyboard usage IDs).
const RETURN: u32 = 0x28;
const ESCAPE: u32 = 0x29;
const BACKSPACE: u32 = 0x2A;
const TAB: u32 = 0x2B;
const SPACE: u32 = 0x2C;
const F1: u32 = 0x3A;
const F12: u32 = 0x45;
const INSERT: u32 = 0x49;
const HOME: u32 = 0x4A;
const PAGE_UP: u32 = 0x4B;
const DELETE_FORWARD: u32 = 0x4C;
const END: u32 = 0x4D;
const PAGE_DOWN: u32 = 0x4E;
const RIGHT: u32 = 0x4F;
const LEFT: u32 = 0x50;
const DOWN: u32 = 0x51;
const UP: u32 = 0x52;
const KEYPAD_ENTER: u32 = 0x58;

/// A press GPUI receives as a keystroke rather than as text.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct HardwareKey(pub Keystroke);

pub(crate) fn modifiers(flags: u64) -> Modifiers {
    Modifiers {
        control: flags & CONTROL != 0,
        alt: flags & ALTERNATE != 0,
        shift: flags & SHIFT != 0,
        platform: flags & COMMAND != 0,
        function: false,
    }
}

fn named_key(code: u32) -> Option<&'static str> {
    Some(match code {
        RETURN | KEYPAD_ENTER => "enter",
        ESCAPE => "escape",
        BACKSPACE => "backspace",
        TAB => "tab",
        SPACE => "space",
        F1..=F12 => FUNCTION_KEYS[(code - F1) as usize],
        INSERT => "insert",
        HOME => "home",
        PAGE_UP => "pageup",
        DELETE_FORWARD => "delete",
        END => "end",
        PAGE_DOWN => "pagedown",
        RIGHT => "right",
        LEFT => "left",
        DOWN => "down",
        UP => "up",
        _ => return None,
    })
}

const FUNCTION_KEYS: [&str; 12] = [
    "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10", "f11", "f12",
];

/// The keystroke for a hardware press, or `None` when UIKit should deliver
/// it as text (`insertText:`/`deleteBackward`). `characters` is the key's
/// `charactersIgnoringModifiers`.
pub(crate) fn hardware_key(code: u32, flags: u64, characters: &str) -> Option<HardwareKey> {
    let modifiers = modifiers(flags);
    let shortcut = modifiers.control || modifiers.platform;
    let name = named_key(code);
    // Return, backspace and space type (or delete) text; UIKeyInput already
    // reports them unless a modifier is held. Tab is always a keystroke, as
    // on desktop, so it can move focus.
    let types_text = matches!(code, RETURN | KEYPAD_ENTER | BACKSPACE | SPACE);
    if let Some(name) = name {
        if types_text && !shortcut && !modifiers.alt {
            return None;
        }
        return Some(HardwareKey(Keystroke {
            modifiers,
            key: name.to_string(),
            key_char: None,
        }));
    }
    if !shortcut {
        return None;
    }
    let mut chars = characters.chars();
    let c = chars.next().filter(|c| !c.is_control())?;
    if chars.next().is_some() {
        return None;
    }
    Some(HardwareKey(Keystroke {
        modifiers,
        key: c.to_lowercase().to_string(),
        key_char: None,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn navigation_keys_are_keystrokes() {
        let left = hardware_key(LEFT, 0, "").unwrap().0;
        assert_eq!(left.key, "left");
        let select = hardware_key(RIGHT, SHIFT, "").unwrap().0;
        assert!(select.modifiers.shift);
        assert_eq!(hardware_key(F1 + 4, 0, "").unwrap().0.key, "f5");
        assert_eq!(hardware_key(ESCAPE, 0, "").unwrap().0.key, "escape");
        assert_eq!(hardware_key(TAB, SHIFT, "\t").unwrap().0.key, "tab");
    }

    #[test]
    fn typing_keys_are_left_to_uikit() {
        assert_eq!(hardware_key(0x04, 0, "a"), None);
        assert_eq!(hardware_key(0x04, SHIFT, "a"), None);
        assert_eq!(hardware_key(RETURN, 0, "\r"), None);
        assert_eq!(hardware_key(BACKSPACE, 0, "\u{8}"), None);
    }

    #[test]
    fn shortcuts_are_keystrokes() {
        let copy = hardware_key(0x06, COMMAND, "c").unwrap().0;
        assert_eq!(copy.key, "c");
        assert!(copy.modifiers.platform);
        assert_eq!(copy.key_char, None);
        let word_delete = hardware_key(BACKSPACE, ALTERNATE, "\u{8}").unwrap().0;
        assert_eq!(word_delete.key, "backspace");
        assert!(word_delete.modifiers.alt);
    }
}
