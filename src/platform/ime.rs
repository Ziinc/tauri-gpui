//! Desktop input method support beyond TAO's committed text.
//!
//! TAO reports only `ReceivedImeText` (the committed text) and cannot place
//! the candidate window from a Tauri window. Where the native API is
//! reachable from the window's handles, the plugin talks to the input method
//! itself: on Linux a GTK input method context composes into GPUI's input
//! handler (`linux`), and on Windows the IMM32 composition and candidate
//! windows follow GPUI's caret (`windows`).

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub(crate) mod linux;
#[cfg(target_os = "windows")]
pub(crate) mod windows;

use gpui::{Bounds, Pixels};

/// What an input method did with the text being composed.
#[cfg_attr(not(any(target_os = "linux", target_os = "freebsd")), allow(dead_code))]
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ImeEvent {
    /// The composition (preedit) text changed; `cursor` is the caret inside
    /// it, in UTF-16 code units. Empty text removes the composition.
    Preedit { text: String, cursor: usize },
    /// Text to insert in place of the composition.
    Commit(String),
    /// The composition ended.
    End,
}

/// Where a key press goes after the input method has seen it.
#[cfg_attr(not(any(target_os = "linux", target_os = "freebsd")), allow(dead_code))]
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum KeyRoute {
    /// The input method did not want the key: TAO delivers it, and GPUI key
    /// bindings see it.
    Key,
    /// The input method only typed the key's own character. TAO delivers
    /// the key instead, so key bindings still see it and the text is
    /// inserted once, through the key (see `typed_text`); the input method's
    /// commit is dropped.
    KeyInsteadOfCommit,
    /// The input method took the key (it composes, or committed something
    /// else): its events are applied and TAO never sees the key.
    Ime,
}

/// Decides where a key press goes. `consumed` is the input method's answer
/// for the key, `events` what it emitted while looking at it, `key_text` the
/// character the key types on its own, and `composing` whether a
/// composition was in progress before the key.
#[cfg_attr(not(any(target_os = "linux", target_os = "freebsd")), allow(dead_code))]
pub(crate) fn route_key(
    consumed: bool,
    events: &[ImeEvent],
    key_text: Option<char>,
    composing: bool,
) -> KeyRoute {
    if !consumed {
        return KeyRoute::Key;
    }
    match (events, key_text) {
        ([ImeEvent::Commit(text)], Some(key))
            if !composing && text.chars().eq(std::iter::once(key)) =>
        {
            KeyRoute::KeyInsteadOfCommit
        }
        _ => KeyRoute::Ime,
    }
}

/// The UTF-16 offset of the `chars`-th character of `text` (GTK reports the
/// preedit cursor in characters, GPUI expects UTF-16 code units).
#[cfg_attr(not(any(target_os = "linux", target_os = "freebsd")), allow(dead_code))]
pub(crate) fn utf16_offset(text: &str, chars: usize) -> usize {
    text.chars().take(chars).map(char::len_utf16).sum()
}

/// `bounds` (logical, window-relative) scaled by `scale` and rounded to whole
/// pixels: `[left, top, right, bottom]`. Never empty, so a zero-width caret
/// still has an area to place the candidate window against.
#[cfg_attr(
    not(any(target_os = "linux", target_os = "freebsd", target_os = "windows")),
    allow(dead_code)
)]
pub(crate) fn pixel_rect(bounds: Bounds<Pixels>, scale: f32) -> [i32; 4] {
    let scaled = |value: Pixels| (f32::from(value) * scale).round() as i32;
    let (left, top) = (scaled(bounds.origin.x), scaled(bounds.origin.y));
    let right = scaled(bounds.origin.x + bounds.size.width).max(left + 1);
    let bottom = scaled(bounds.origin.y + bounds.size.height).max(top + 1);
    [left, top, right, bottom]
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{point, px, size};

    fn commit(text: &str) -> ImeEvent {
        ImeEvent::Commit(text.into())
    }

    fn preedit(text: &str) -> ImeEvent {
        ImeEvent::Preedit {
            text: text.into(),
            cursor: utf16_offset(text, text.chars().count()),
        }
    }

    #[test]
    fn unwanted_keys_go_to_tao() {
        assert_eq!(route_key(false, &[], Some('a'), false), KeyRoute::Key);
        assert_eq!(route_key(false, &[], None, true), KeyRoute::Key);
    }

    #[test]
    fn a_plain_character_is_typed_through_its_key() {
        // GTK's simple context (and IBus in sync mode) commits plain keys.
        let events = [commit("a")];
        assert_eq!(
            route_key(true, &events, Some('a'), false),
            KeyRoute::KeyInsteadOfCommit
        );
        let events = [commit("A")];
        assert_eq!(
            route_key(true, &events, Some('A'), false),
            KeyRoute::KeyInsteadOfCommit
        );
    }

    #[test]
    fn composition_keeps_the_key() {
        // Dead key: the IME shows a preedit and takes the key.
        assert_eq!(
            route_key(
                true,
                &[ImeEvent::Preedit {
                    text: "´".into(),
                    cursor: 1
                }],
                None,
                false
            ),
            KeyRoute::Ime
        );
        // `e` completes `´e`: the commit differs from the key's character.
        let events = [commit("é"), preedit(""), ImeEvent::End];
        assert_eq!(route_key(true, &events, Some('e'), true), KeyRoute::Ime);
        // A commit that happens to match the key still ends a composition.
        assert_eq!(
            route_key(true, &[commit("a")], Some('a'), true),
            KeyRoute::Ime
        );
        // Pinyin: the key starts a preedit.
        assert_eq!(
            route_key(true, &[preedit("n")], Some('n'), false),
            KeyRoute::Ime
        );
    }

    #[test]
    fn asynchronous_input_methods_take_every_key_first() {
        // IBus answers later; it re-sends keys it does not want.
        assert_eq!(route_key(true, &[], Some('a'), false), KeyRoute::Ime);
        assert_eq!(route_key(true, &[], None, false), KeyRoute::Ime);
    }

    #[test]
    fn multi_character_commits_are_inserted() {
        assert_eq!(
            route_key(true, &[commit("ab")], Some('a'), false),
            KeyRoute::Ime
        );
        assert_eq!(
            route_key(true, &[commit("你好")], None, false),
            KeyRoute::Ime
        );
    }

    #[test]
    fn preedit_cursor_counts_utf16_units() {
        assert_eq!(utf16_offset("nihao", 2), 2);
        assert_eq!(utf16_offset("你好", 1), 1);
        assert_eq!(utf16_offset("😀a", 1), 2);
        assert_eq!(utf16_offset("😀a", 2), 3);
        // Past the end clamps to the text.
        assert_eq!(utf16_offset("ab", 5), 2);
    }

    #[test]
    fn caret_rect_is_scaled_and_never_empty() {
        let caret = Bounds::new(point(px(10.), px(20.)), size(px(0.), px(16.)));
        assert_eq!(pixel_rect(caret, 1.), [10, 20, 11, 36]);
        assert_eq!(pixel_rect(caret, 1.5), [15, 30, 16, 54]);
        let range = Bounds::new(point(px(10.5), px(4.)), size(px(30.), px(0.)));
        assert_eq!(pixel_rect(range, 2.), [21, 8, 81, 9]);
    }
}
