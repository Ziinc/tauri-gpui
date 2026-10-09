//! The OS clipboard, shared between GPUI and tauri-plugin-clipboard-manager.

use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_gpui::gpui::ClipboardItem;

use crate::{
    ensure,
    support::{Ctx, TestResult},
};

crate::tests!["clipboard" =>
    gpui_reads_plugin_text,
    plugin_reads_gpui_text,
    gpui_metadata_round_trips,
    external_write_drops_metadata,
];

fn gpui_text(cx: &Ctx) -> Option<String> {
    cx.gpui(|cx| cx.read_from_clipboard().and_then(|i| i.text()))
}

fn gpui_metadata(cx: &Ctx) -> Option<String> {
    cx.gpui(|cx| cx.read_from_clipboard().and_then(|i| i.metadata().cloned()))
}

fn write_with_metadata(cx: &Ctx, text: &'static str, metadata: &'static str) {
    cx.gpui(move |cx| {
        cx.write_to_clipboard(ClipboardItem::new_string_with_metadata(
            text.into(),
            metadata.into(),
        ))
    });
}

fn gpui_reads_plugin_text(cx: &mut Ctx) -> TestResult {
    cx.main(|app| app.clipboard().write_text("from tauri").unwrap());
    let text = gpui_text(cx);
    ensure!(text.as_deref() == Some("from tauri"), "{text:?}");
    Ok(())
}

fn plugin_reads_gpui_text(cx: &mut Ctx) -> TestResult {
    cx.gpui(|cx| cx.write_to_clipboard(ClipboardItem::new_string("from gpui".into())));
    let text = cx.main(|app| app.clipboard().read_text().ok());
    ensure!(text.as_deref() == Some("from gpui"), "{text:?}");
    Ok(())
}

fn gpui_metadata_round_trips(cx: &mut Ctx) -> TestResult {
    write_with_metadata(cx, "meta", "{\"k\":1}");
    let metadata = gpui_metadata(cx);
    ensure!(metadata.as_deref() == Some("{\"k\":1}"), "{metadata:?}");
    Ok(())
}

/// Metadata is reattached by text equality (as zed does on Linux), so only a
/// write of different text can be told apart from GPUI's own.
fn external_write_drops_metadata(cx: &mut Ctx) -> TestResult {
    write_with_metadata(cx, "meta", "{\"k\":1}");
    cx.main(|app| app.clipboard().write_text("other").unwrap());
    let metadata = gpui_metadata(cx);
    ensure!(metadata.is_none(), "{metadata:?}");
    Ok(())
}
