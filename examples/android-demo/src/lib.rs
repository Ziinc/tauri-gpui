//! A touch-first tauri-plugin-gpui app for Android (it also runs on
//! desktop with `cargo run -p android-demo`).
//!
//! It exercises what a phone needs from the platform layer: taps (the
//! counter), drag and fling scrolling (the list), the soft keyboard (the
//! input), system bar insets (the padded header) and the back action. Every
//! interaction is logged under the `android-demo` target so the CI smoke test
//! can assert on logcat.

use gpui_kit::{
    component::{
        ActiveTheme, Sizable,
        button::{Button, ButtonVariants},
        input::{Input, InputEvent, InputState},
    },
    *,
};
use tauri_plugin_gpui::{GpuiConfig, GpuiOptions, GpuiWindowExt};

const ROWS: usize = 60;

// Fixed layout metrics (logical pixels), logged so the CI smoke test can tap
// the controls (`scripts/android-smoke.sh`).
const PAD: f32 = 16.;
const TITLE_H: f32 = 40.;
const GAP: f32 = 12.;
const BUTTON_H: f32 = 56.;
const INPUT_H: f32 = 48.;

struct Demo {
    taps: usize,
    input: Entity<InputState>,
    submitted: Vec<String>,
    scroll: ScrollHandle,
    last_logged_offset: f32,
    last_logged_top: Option<Pixels>,
}

impl Demo {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Type here"));
        cx.subscribe_in(
            &input,
            window,
            |this, input, event: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = event {
                    let text = input.read(cx).value().to_string();
                    log::info!(target: "android-demo", "submitted text={text:?}");
                    this.submitted.push(text);
                    input.update(cx, |input, cx| input.set_value("", window, cx));
                    cx.notify();
                }
            },
        )
        .detach();
        Self {
            taps: 0,
            input,
            submitted: Vec::new(),
            scroll: ScrollHandle::new(),
            last_logged_offset: 0.,
            last_logged_top: None,
        }
    }
}

impl Render for Demo {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let offset = -f32::from(self.scroll.offset().y);
        if (offset - self.last_logged_offset).abs() >= 200. {
            self.last_logged_offset = offset;
            log::info!(target: "android-demo", "scrolled offset={offset:.0}");
        }
        // Keep clear of the system bars and the soft keyboard.
        let visible = window.fully_visible_bounds();
        let viewport = window.viewport_size();
        let (top, left) = (visible.origin.y, visible.origin.x);
        let bottom = viewport.height - visible.bottom();
        let right = viewport.width - visible.right();
        if self.last_logged_top != Some(top) {
            self.last_logged_top = Some(top);
            let button_y = top + px(PAD + TITLE_H + GAP + BUTTON_H / 2.);
            let input_y = top + px(PAD + TITLE_H + GAP + BUTTON_H + GAP + INPUT_H / 2.);
            log::info!(
                target: "android-demo",
                "layout top={} button_y={} input_y={} width={}",
                f32::from(top),
                f32::from(button_y),
                f32::from(input_y),
                f32::from(viewport.width)
            );
        }
        let theme = cx.theme();
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.background)
            .text_color(theme.foreground)
            .pt(top)
            .pb(bottom)
            .pl(left)
            .pr(right)
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(GAP))
                    .p(px(PAD))
                    .border_b_1()
                    .border_color(theme.border)
                    .child(
                        div()
                            .h(px(TITLE_H))
                            .flex()
                            .items_center()
                            .justify_between()
                            .child(
                                div()
                                    .text_xl()
                                    .font_weight(FontWeight::BOLD)
                                    .child("GPUI on Android"),
                            )
                            .child(format!("Taps: {}", self.taps)),
                    )
                    .child(
                        Button::new("tap")
                            .primary()
                            .large()
                            .w_full()
                            .h(px(BUTTON_H))
                            .label("Tap me")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.taps += 1;
                                log::info!(target: "android-demo", "tapped count={}", this.taps);
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .h(px(INPUT_H))
                            .flex()
                            .items_center()
                            .child(Input::new(&self.input).large()),
                    )
                    .children(
                        self.submitted
                            .last()
                            .map(|text| div().text_sm().child(format!("Last submitted: {text}"))),
                    ),
            )
            .child(
                div()
                    .id("rows")
                    .flex_1()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .children((1..=ROWS).map(|i| {
                        div()
                            .h(px(56.))
                            .px_4()
                            .flex()
                            .items_center()
                            .border_b_1()
                            .border_color(theme.border)
                            .child(format!("Row {i}"))
                    })),
            )
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(target_os = "android")]
    android_logger::init_once(
        android_logger::Config::default()
            .with_max_level(log::LevelFilter::Info)
            .with_tag("android-demo"),
    );
    tauri::Builder::default()
        .setup(|app| {
            tauri_plugin_gpui::init_with(app, GpuiConfig::new().on_launch(gpui_kit::init))?;
            let window = tauri::WindowBuilder::new(app, "main")
                .title("GPUI on Android")
                .inner_size(400., 800.)
                .build()?;
            window.attach_gpui_view(GpuiOptions::default(), |window, cx| {
                let view = cx.new(|cx| Demo::new(window, cx));
                cx.new(|cx| base::Root::new(view, window, cx))
            })?;
            log::info!(target: "android-demo", "attached");
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
