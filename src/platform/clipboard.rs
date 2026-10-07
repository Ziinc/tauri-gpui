//! System clipboard via `arboard` (the same backend `tauri-plugin-clipboard-manager` uses).
//!
//! Text and images go through the OS clipboard. GPUI string metadata has no
//! portable OS representation, so it is kept in-process and reattached when the
//! clipboard still holds the text it was written with (as zed does on Linux).

use std::{borrow::Cow, cell::RefCell, io::Cursor};

use gpui::{ClipboardEntry, ClipboardItem, Image, ImageFormat};

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Kind {
    Clipboard,
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    Primary,
}

#[derive(Default)]
pub(crate) struct Clipboard {
    inner: RefCell<Option<arboard::Clipboard>>,
    /// `(kind, text, metadata)` of the last string written with metadata.
    metadata: RefCell<Option<(Kind, String, String)>>,
}

impl Clipboard {
    /// Runs `f` on the lazily opened clipboard, logging failures.
    fn with<T>(
        &self,
        op: &str,
        f: impl FnOnce(&mut arboard::Clipboard) -> Result<T, arboard::Error>,
    ) -> Option<T> {
        let mut slot = self.inner.borrow_mut();
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

    pub(crate) fn read(&self, kind: Kind) -> Option<ClipboardItem> {
        if let Some(text) = self.with("clipboard read", |cb| get(cb, kind).text()) {
            let metadata = self.metadata.borrow();
            return Some(match &*metadata {
                Some((k, t, m)) if *k == kind && *t == text => {
                    ClipboardItem::new_string_with_metadata(text, m.clone())
                }
                _ => ClipboardItem::new_string(text),
            });
        }
        let image = self.with("clipboard image read", |cb| get(cb, kind).image())?;
        encode_png(&image)
            .map(|png| ClipboardItem::new_image(&Image::from_bytes(ImageFormat::Png, png)))
    }

    pub(crate) fn write(&self, kind: Kind, item: ClipboardItem) {
        let mut metadata = self.metadata.borrow_mut();
        if metadata.as_ref().is_some_and(|(k, ..)| *k == kind) {
            *metadata = None;
        }
        if let Some(text) = item.text() {
            if let Some(m) = item.metadata() {
                *metadata = Some((kind, text.clone(), m.clone()));
            }
            self.with("clipboard write", |cb| set(cb, kind).text(text));
            return;
        }
        let image = item.entries().iter().find_map(|e| match e {
            ClipboardEntry::Image(image) => Some(image),
            _ => None,
        });
        if let Some(image) = image.and_then(decode_rgba) {
            self.with("clipboard image write", |cb| set(cb, kind).image(image));
        }
    }
}

fn get(cb: &mut arboard::Clipboard, kind: Kind) -> arboard::Get<'_> {
    match kind {
        Kind::Clipboard => cb.get(),
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        Kind::Primary => {
            use arboard::{GetExtLinux, LinuxClipboardKind};
            cb.get().clipboard(LinuxClipboardKind::Primary)
        }
    }
}

fn set(cb: &mut arboard::Clipboard, kind: Kind) -> arboard::Set<'_> {
    match kind {
        Kind::Clipboard => cb.set(),
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        Kind::Primary => {
            use arboard::{LinuxClipboardKind, SetExtLinux};
            cb.set().clipboard(LinuxClipboardKind::Primary)
        }
    }
}

/// Decodes a GPUI image (PNG, JPEG, …) into the RGBA buffer the OS clipboard takes.
fn decode_rgba(image: &Image) -> Option<arboard::ImageData<'static>> {
    let decoded = image::ImageFormat::from_mime_type(image.format.mime_type())
        .ok_or_else(|| format!("unsupported format {:?}", image.format))
        .and_then(|format| {
            image::load_from_memory_with_format(&image.bytes, format).map_err(|e| e.to_string())
        });
    match decoded {
        Ok(decoded) => {
            let rgba = decoded.into_rgba8();
            Some(arboard::ImageData {
                width: rgba.width() as usize,
                height: rgba.height() as usize,
                bytes: Cow::Owned(rgba.into_raw()),
            })
        }
        Err(e) => {
            log::warn!("tauri-plugin-gpui: cannot copy image to clipboard: {e}");
            None
        }
    }
}

/// Encodes an OS clipboard RGBA buffer as PNG for GPUI.
fn encode_png(image: &arboard::ImageData) -> Option<Vec<u8>> {
    let rgba = image::RgbaImage::from_raw(
        image.width as u32,
        image.height as u32,
        image.bytes.to_vec(),
    )?;
    let mut png = Vec::new();
    match rgba.write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png) {
        Ok(()) => Some(png),
        Err(e) => {
            log::warn!("tauri-plugin-gpui: cannot read clipboard image: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_round_trips_through_png() {
        let rgba = arboard::ImageData {
            width: 2,
            height: 1,
            bytes: Cow::Owned(vec![255, 0, 0, 255, 0, 0, 255, 128]),
        };
        let png = encode_png(&rgba).unwrap();
        let back = decode_rgba(&Image::from_bytes(ImageFormat::Png, png)).unwrap();
        assert_eq!((back.width, back.height), (2, 1));
        assert_eq!(back.bytes, rgba.bytes);
    }
}
