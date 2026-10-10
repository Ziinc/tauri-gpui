//! The text-editing model behind `GpuiInputView`'s `UITextInput`.
//!
//! UIKit asks a `UITextInput` for its document synchronously: the selection,
//! the marked (composing) range, text and geometry. Those questions are
//! answered by GPUI's focused input handler whenever GPUI is free. While GPUI
//! is mid-update (UIKit asks from inside `becomeFirstResponder`, which GPUI
//! calls while drawing), they are answered from the last [`TextState`], and
//! edits are queued as [`TextEdit`]s and applied to that state so UIKit's
//! follow-up questions see them.
//!
//! Offsets are UTF-16 code units, as both UIKit and GPUI count them.

use std::ops::Range;

/// What the keyboard last saw of the focused text input.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TextState {
    /// The part of the document the keyboard may read and edit
    /// (`InputHandler::text_input_editable_range`, else all of it).
    pub document: Range<usize>,
    pub selection: Range<usize>,
    pub marked: Option<Range<usize>>,
}

/// An edit UIKit makes to the document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TextEdit {
    /// `insertText:`: replaces the marked text, else the selection, and ends
    /// composition.
    Insert(String),
    /// `deleteBackward`.
    DeleteBackward,
    /// `setMarkedText:selectedRange:`: replaces the marked text, else the
    /// selection, and marks the result. `selected` is relative to `text`.
    Mark {
        text: String,
        selected: Range<usize>,
    },
    /// `unmarkText`: keeps the marked text as typed.
    Unmark,
    /// `replaceRange:withText:`.
    Replace { range: Range<usize>, text: String },
    /// `setSelectedTextRange:`.
    Select(Range<usize>),
}

pub(crate) fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

impl TextState {
    /// Mirrors what GPUI does for `edit`, as far as offsets go.
    pub fn apply(&mut self, edit: &TextEdit) {
        match edit {
            TextEdit::Insert(text) => {
                let range = self.marked.take().unwrap_or(self.selection.clone());
                let end = self.replace(range, text);
                self.selection = end..end;
            }
            TextEdit::DeleteBackward => {
                let range = if self.selection.is_empty() {
                    self.selection
                        .start
                        .saturating_sub(1)
                        .max(self.document.start)..self.selection.start
                } else {
                    self.selection.clone()
                };
                self.marked = None;
                let end = self.replace(range, "");
                self.selection = end..end;
            }
            TextEdit::Mark { text, selected } => {
                let range = self.marked.take().unwrap_or(self.selection.clone());
                let start = range.start;
                let end = self.replace(range, text);
                if text.is_empty() {
                    self.selection = start..start;
                } else {
                    self.marked = Some(start..end);
                    let clamp = |offset: usize| (start + offset).min(end);
                    self.selection = clamp(selected.start)..clamp(selected.end);
                }
            }
            TextEdit::Unmark => self.marked = None,
            TextEdit::Replace { range, text } => {
                let range = self.clamp(range.clone());
                let end = self.replace(range, text);
                self.marked = None;
                self.selection = end..end;
            }
            TextEdit::Select(range) => self.selection = self.clamp(range.clone()),
        }
    }

    /// Clamps `range` to the document.
    pub fn clamp(&self, range: Range<usize>) -> Range<usize> {
        let clamp = |offset: usize| offset.clamp(self.document.start, self.document.end);
        let (start, end) = (clamp(range.start), clamp(range.end));
        start.min(end)..start.max(end)
    }

    /// Replaces `range` with `text`, returning the offset after it.
    fn replace(&mut self, range: Range<usize>, text: &str) -> usize {
        let range = self.clamp(range);
        let inserted = utf16_len(text);
        self.document.end = self.document.end - range.len() + inserted;
        range.start + inserted
    }
}

/// How the keyboard should hear about a change GPUI made on its own (a key
/// binding, a tap that moved the caret, a different input taking focus).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Change {
    None,
    /// `selectionWillChange:`/`selectionDidChange:`.
    Selection,
    /// `textWillChange:`/`textDidChange:`, which also makes the keyboard drop
    /// a composition GPUI ended.
    Text,
}

pub(crate) fn change(old: Option<&TextState>, new: Option<&TextState>) -> Change {
    match (old, new) {
        (Some(old), Some(new)) if old == new => Change::None,
        (Some(old), Some(new)) if old.document == new.document && old.marked == new.marked => {
            Change::Selection
        }
        (None, None) => Change::None,
        _ => Change::Text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(len: usize, caret: usize) -> TextState {
        TextState {
            document: 0..len,
            selection: caret..caret,
            marked: None,
        }
    }

    fn mark(text: &str, selected: usize) -> TextEdit {
        TextEdit::Mark {
            text: text.into(),
            selected: selected..selected,
        }
    }

    #[test]
    fn composition_replaces_its_marked_text() {
        let mut text = state(3, 3);
        text.apply(&mark("n", 1));
        assert_eq!(
            (text.marked.clone(), text.selection.clone()),
            (Some(3..4), 4..4)
        );
        text.apply(&mark("にほ", 2));
        assert_eq!((text.marked.clone(), text.document.end), (Some(3..5), 5));
        text.apply(&mark("日本", 2));
        text.apply(&TextEdit::Unmark);
        assert_eq!((text.marked.clone(), text.selection.clone()), (None, 5..5));
    }

    #[test]
    fn insert_commits_the_marked_text() {
        let mut text = state(0, 0);
        text.apply(&mark("한", 1));
        text.apply(&TextEdit::Insert("한글".into()));
        assert_eq!(text, state(2, 2));
    }

    #[test]
    fn empty_marked_text_cancels_the_composition() {
        let mut text = state(4, 2);
        text.apply(&mark("ka", 2));
        text.apply(&mark("", 0));
        assert_eq!(text, state(4, 2));
    }

    #[test]
    fn marked_selection_stays_inside_the_marked_text() {
        let mut text = state(0, 0);
        text.apply(&TextEdit::Mark {
            text: "abc".into(),
            selected: 1..9,
        });
        assert_eq!(text.selection, 1..3);
    }

    #[test]
    fn counts_utf16_code_units() {
        let mut text = state(0, 0);
        text.apply(&TextEdit::Insert("🎉".into()));
        assert_eq!(text, state(2, 2));
    }

    #[test]
    fn delete_backward_and_replace_stay_in_the_document() {
        let mut text = state(5, 0);
        text.apply(&TextEdit::DeleteBackward);
        assert_eq!(text, state(5, 0));
        text.apply(&TextEdit::Select(2..4));
        text.apply(&TextEdit::DeleteBackward);
        assert_eq!(text, state(3, 2));
        text.apply(&TextEdit::Replace {
            range: 1..99,
            text: "xy".into(),
        });
        assert_eq!(text, state(3, 3));
    }

    #[test]
    fn classifies_changes_for_the_keyboard() {
        let a = state(3, 3);
        let mut moved = a.clone();
        moved.selection = 1..1;
        let mut composing = a.clone();
        composing.marked = Some(2..3);
        assert_eq!(change(Some(&a), Some(&a)), Change::None);
        assert_eq!(change(Some(&a), Some(&moved)), Change::Selection);
        assert_eq!(change(Some(&composing), Some(&a)), Change::Text);
        assert_eq!(change(None, Some(&a)), Change::Text);
        assert_eq!(change(Some(&a), None), Change::Text);
        assert_eq!(change(None, None), Change::None);
    }
}
