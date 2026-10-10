//! Android key events to GPUI keystrokes.

use gpui::{Keystroke, Modifiers};

// android.view.KeyEvent meta state bits.
const META_SHIFT_ON: i32 = 0x1;
const META_ALT_ON: i32 = 0x2;
const META_CTRL_ON: i32 = 0x1000;
const META_META_ON: i32 = 0x10000;

pub(crate) fn modifiers(meta: i32) -> Modifiers {
    Modifiers {
        control: meta & META_CTRL_ON != 0,
        alt: meta & META_ALT_ON != 0,
        shift: meta & META_SHIFT_ON != 0,
        platform: meta & META_META_ON != 0,
        function: false,
    }
}

/// GPUI name of a non-character Android key code.
pub(crate) fn named_key(key_code: i32) -> Option<&'static str> {
    Some(match key_code {
        19 => "up",
        20 => "down",
        21 => "left",
        22 => "right",
        61 => "tab",
        62 => "space",
        66 | 160 => "enter",
        67 => "backspace",
        92 => "pageup",
        93 => "pagedown",
        111 => "escape",
        112 => "delete",
        122 => "home",
        123 => "end",
        124 => "insert",
        131..=142 => FUNCTION_KEYS[(key_code - 131) as usize],
        _ => return None,
    })
}

const FUNCTION_KEYS: [&str; 12] = [
    "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10", "f11", "f12",
];

/// Builds the keystroke for a key event. `unicode` is
/// `KeyEvent.getUnicodeChar(metaState)`: the shift-aware character, or 0.
pub(crate) fn keystroke(key_code: i32, unicode: i32, meta: i32) -> Option<Keystroke> {
    let modifiers = modifiers(meta);
    let shortcut = modifiers.control || modifiers.platform;
    if let Some(name) = named_key(key_code) {
        let key_char = match name {
            "space" if !shortcut => Some(" ".to_string()),
            _ => None,
        };
        return Some(Keystroke {
            modifiers,
            key: name.to_string(),
            key_char,
        });
    }
    let c = char::from_u32(u32::try_from(unicode).ok()?).filter(|c| !c.is_control())?;
    Some(Keystroke {
        modifiers,
        key: c.to_lowercase().to_string(),
        key_char: (!shortcut).then(|| c.to_string()),
    })
}

/// A text editing action from the native selection toolbar. The codes match
/// `GpuiView.ACTION_*`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EditAction {
    Cut,
    Copy,
    Paste,
    SelectAll,
}

impl EditAction {
    pub(crate) fn from_code(code: i32) -> Option<Self> {
        Some(match code {
            0 => Self::Cut,
            1 => Self::Copy,
            2 => Self::Paste,
            3 => Self::SelectAll,
            _ => return None,
        })
    }

    /// The shortcut text inputs bind for the action (`secondary-x` and so on,
    /// which is `ctrl` off macOS). Sending the keystroke makes the focused
    /// input run its own action and use GPUI's clipboard.
    pub(crate) fn keystroke(self) -> Keystroke {
        Keystroke {
            modifiers: Modifiers {
                control: true,
                ..Modifiers::default()
            },
            key: match self {
                Self::Cut => "x",
                Self::Copy => "c",
                Self::Paste => "v",
                Self::SelectAll => "a",
            }
            .into(),
            key_char: None,
        }
    }
}

/// Keystroke for one character of IME-committed text.
pub(crate) fn char_keystroke(c: char) -> Keystroke {
    match c {
        '\n' => Keystroke {
            modifiers: Modifiers::default(),
            key: "enter".into(),
            key_char: None,
        },
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
    fn named_keys() {
        assert_eq!(keystroke(67, 0, 0).unwrap().key, "backspace");
        assert_eq!(keystroke(66, 10, 0).unwrap().key, "enter");
        assert_eq!(keystroke(135, 0, 0).unwrap().key, "f5");
        let space = keystroke(62, 32, 0).unwrap();
        assert_eq!(space.key_char.as_deref(), Some(" "));
    }

    #[test]
    fn characters_are_lowercase_keys_with_typed_text() {
        let a = keystroke(29, 'A' as i32, META_SHIFT_ON).unwrap();
        assert_eq!(a.key, "a");
        assert_eq!(a.key_char.as_deref(), Some("A"));
        assert!(a.modifiers.shift);
    }

    #[test]
    fn shortcuts_type_nothing() {
        let copy = keystroke(31, 'c' as i32, META_CTRL_ON).unwrap();
        assert_eq!(copy.key, "c");
        assert!(copy.modifiers.control);
        assert_eq!(copy.key_char, None);
    }

    #[test]
    fn modifier_keys_alone_are_ignored() {
        // KEYCODE_SHIFT_LEFT has no character.
        assert!(keystroke(59, 0, META_SHIFT_ON).is_none());
    }

    #[test]
    fn edit_actions_are_ctrl_shortcuts() {
        let keys: Vec<_> = (0..4)
            .map(|code| EditAction::from_code(code).unwrap().keystroke())
            .collect();
        let names: Vec<_> = keys.iter().map(|k| k.key.as_str()).collect();
        assert_eq!(names, ["x", "c", "v", "a"]);
        assert!(keys.iter().all(|k| k.modifiers == modifiers(META_CTRL_ON)));
        assert!(keys.iter().all(|k| k.key_char.is_none()));
        assert_eq!(EditAction::from_code(4), None);
    }

    #[test]
    fn committed_newline_is_enter() {
        assert_eq!(char_keystroke('\n').key, "enter");
        let upper = char_keystroke('Q');
        assert_eq!((upper.key.as_str(), upper.modifiers.shift), ("q", true));
    }
}
