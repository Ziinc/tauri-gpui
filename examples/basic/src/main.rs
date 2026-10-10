//! Minimal tauri-plugin-gpui app: one Tauri window rendered by plain GPUI,
//! with no component library. A counter shows state and pointer input.

use tauri_plugin_gpui::{
    GpuiWindowExt,
    gpui::{self, prelude::*, px, rgb},
};

struct Counter {
    count: usize,
}

impl Render for Counter {
    fn render(&mut self, _: &mut gpui::Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        gpui::div()
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_4()
            .bg(rgb(0x1e1e2e))
            .text_color(rgb(0xcdd6f4))
            .child(
                gpui::div()
                    .text_xl()
                    .child("Hello from GPUI in a Tauri window"),
            )
            .child(
                gpui::div()
                    .text_size(px(48.))
                    .child(format!("{}", self.count)),
            )
            .child(
                gpui::div()
                    .id("increment")
                    .px_4()
                    .py_2()
                    .rounded_md()
                    .bg(rgb(0x89b4fa))
                    .text_color(rgb(0x1e1e2e))
                    .cursor_pointer()
                    .hover(|style| style.bg(rgb(0xb4befe)))
                    .child("Click me")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.count += 1;
                        cx.notify();
                    })),
            )
    }
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            tauri_plugin_gpui::init(app)?;
            let window = tauri::WindowBuilder::new(app, "main")
                .title("tauri-plugin-gpui basic")
                .inner_size(480.0, 320.0)
                .build()?;
            window.attach_gpui(|cx| cx.new(|_| Counter { count: 0 }))?;
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
