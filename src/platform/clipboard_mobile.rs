//! Mobile clipboard (plain text): `ClipboardManager` on Android, the general
//! `UIPasteboard` on iOS.
//!
//! GPUI string metadata is kept in-process and reattached while the clipboard
//! still holds the text it was written with, as on desktop.

use std::cell::RefCell;

use gpui::ClipboardItem;

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Kind {
    Clipboard,
}

#[derive(Default)]
pub(crate) struct Clipboard {
    /// `(text, metadata)` of the last string written with metadata.
    metadata: RefCell<Option<(String, String)>>,
}

impl Clipboard {
    pub(crate) fn read(&self, _kind: Kind) -> Option<ClipboardItem> {
        let text = crate::mobile::clipboard_text()?;
        let metadata = self
            .metadata
            .borrow()
            .as_ref()
            .filter(|(written, _)| *written == text)
            .map(|(_, metadata)| metadata.clone());
        Some(match metadata {
            Some(metadata) => ClipboardItem::new_string_with_metadata(text, metadata),
            None => ClipboardItem::new_string(text),
        })
    }

    pub(crate) fn write(&self, _kind: Kind, item: ClipboardItem) {
        let Some(text) = item.text() else {
            log::debug!("tauri-plugin-gpui: only text can be copied on mobile");
            return;
        };
        *self.metadata.borrow_mut() = item
            .metadata()
            .map(|metadata| (text.clone(), metadata.clone()));
        crate::mobile::set_clipboard_text(&text);
    }
}
