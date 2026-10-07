//! System clipboard via `arboard` (the same backend `tauri-plugin-clipboard-manager` uses).
//!
//! Text only: GPUI metadata and images are not round-tripped.

use std::cell::RefCell;

use gpui::ClipboardItem;

#[derive(Default)]
pub(crate) struct Clipboard(RefCell<Option<arboard::Clipboard>>);

impl Clipboard {
    /// Runs `f` on the lazily opened clipboard, logging failures.
    fn with<T>(
        &self,
        op: &str,
        f: impl FnOnce(&mut arboard::Clipboard) -> Result<T, arboard::Error>,
    ) -> Option<T> {
        let mut slot = self.0.borrow_mut();
        if slot.is_none() {
            match arboard::Clipboard::new() {
                Ok(cb) => *slot = Some(cb),
                Err(e) => {
                    log::warn!("tauri-plugin-gpui: cannot open clipboard: {e}");
                    return None;
                }
            }
        }
        match f(slot.as_mut()?) {
            Ok(v) => Some(v),
            Err(arboard::Error::ContentNotAvailable) => None,
            Err(e) => {
                log::warn!("tauri-plugin-gpui: {op} failed: {e}");
                None
            }
        }
    }

    pub(crate) fn read(&self) -> Option<ClipboardItem> {
        self.with("read_from_clipboard", |cb| cb.get_text())
            .map(ClipboardItem::new_string)
    }

    pub(crate) fn write(&self, item: ClipboardItem) {
        if let Some(text) = item.text() {
            self.with("write_to_clipboard", |cb| cb.set_text(text));
        }
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    pub(crate) fn read_primary(&self) -> Option<ClipboardItem> {
        use arboard::{GetExtLinux, LinuxClipboardKind};
        self.with("read_from_primary", |cb| {
            cb.get().clipboard(LinuxClipboardKind::Primary).text()
        })
        .map(ClipboardItem::new_string)
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    pub(crate) fn write_primary(&self, item: ClipboardItem) {
        use arboard::{LinuxClipboardKind, SetExtLinux};
        if let Some(text) = item.text() {
            self.with("write_to_primary", |cb| {
                cb.set().clipboard(LinuxClipboardKind::Primary).text(text)
            });
        }
    }
}
