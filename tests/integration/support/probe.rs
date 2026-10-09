//! The GPUI view mounted in every test window. It records what reaches
//! GPUI so tests can assert on it from the driver thread.

use std::{collections::HashMap, ops::Range};

use tauri::Wry;
use tauri_plugin_gpui::{
    GpuiError, GpuiOptions, GpuiWindowExt,
    gpui::{
        self, AnyWindowHandle, App, AppContext as _, Bounds, Context, ElementInputHandler, Entity,
        EntityInputHandler, FocusHandle, Global, InteractiveElement as _, IntoElement,
        KeyDownEvent, MouseButton, ParentElement as _, Pixels, Render, Styled as _, UTF16Selection,
        WeakEntity, Window, WindowAppearance, canvas, div,
    },
};

pub struct Probe {
    focus: FocusHandle,
    seen: Seen,
}

/// Everything a probe has observed, readable off the main thread.
#[derive(Clone, Debug, Default)]
pub struct Seen {
    pub renders: usize,
    pub viewport: (f32, f32),
    pub maximized: bool,
    pub fullscreen: bool,
    pub active: bool,
    pub appearance: Option<WindowAppearance>,
    pub mouse_downs: usize,
    pub mouse_ups: usize,
    pub scrolls: usize,
    pub keys: Vec<String>,
    pub text: String,
}

impl Probe {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        Self {
            focus,
            seen: Seen::default(),
        }
    }

    pub fn seen(&self) -> Seen {
        self.seen.clone()
    }

    pub fn push_text(&mut self, text: &str) {
        self.seen.text.push_str(text);
    }
}

impl Render for Probe {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let seen = &mut self.seen;
        seen.renders += 1;
        let viewport = window.viewport_size();
        seen.viewport = (f32::from(viewport.width), f32::from(viewport.height));
        seen.maximized = window.is_maximized();
        seen.fullscreen = window.is_fullscreen();
        seen.active = window.is_window_active();
        seen.appearance = Some(window.appearance());
        let entity = cx.entity();
        let focus = self.focus.clone();
        div()
            .id("probe")
            .size_full()
            .track_focus(&self.focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.seen.mouse_downs += 1),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.seen.mouse_ups += 1),
            )
            .on_scroll_wheel(cx.listener(|this, _, _, _| this.seen.scrolls += 1))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, _| {
                this.seen.keys.push(event.keystroke.key.clone());
            }))
            .child(format!("renders: {} text: {}", seen.renders, seen.text))
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, _, window, cx| {
                        window.handle_input(&focus, ElementInputHandler::new(bounds, entity), cx)
                    },
                )
                .size_full(),
            )
    }
}

/// Minimal text sink so typed text travels the plugin's text-input path.
impl EntityInputHandler for Probe {
    fn text_for_range(
        &mut self,
        _: Range<usize>,
        _: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        None
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let end = self.seen.text.encode_utf16().count();
        Some(UTF16Selection {
            range: end..end,
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        None
    }

    fn unmark_text(&mut self, _: &mut Window, _: &mut Context<Self>) {}

    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.push_text(text);
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        _: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) {
    }

    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        None
    }

    fn character_index_for_point(
        &mut self,
        _: gpui::Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

/// Probes and GPUI window handles by Tauri window label. Probes are weak so
/// tests can verify GPUI roots are released with their Tauri window.
#[derive(Default)]
pub struct Registry {
    probes: HashMap<String, WeakEntity<Probe>>,
    windows: HashMap<String, AnyWindowHandle>,
}
impl Global for Registry {}

impl Registry {
    /// Labels whose probe is still alive.
    pub fn live(cx: &App) -> Vec<String> {
        cx.try_global::<Self>()
            .map(|r| {
                r.probes
                    .iter()
                    .filter(|(_, p)| p.upgrade().is_some())
                    .map(|(l, _)| l.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn clear(cx: &mut App) {
        cx.set_global(Self::default());
    }
}

pub fn attach_probe(window: &tauri::Window<Wry>) -> Result<(), GpuiError> {
    let label = window.label().to_string();
    window.attach_gpui_view(GpuiOptions::default(), move |window, cx| {
        let probe = cx.new(|cx| Probe::new(window, cx));
        let registry = cx.default_global::<Registry>();
        registry.probes.insert(label.clone(), probe.downgrade());
        registry.windows.insert(label, window.window_handle());
        probe
    })
}

pub fn probe(cx: &App, label: &str) -> Option<Entity<Probe>> {
    cx.try_global::<Registry>()?.probes.get(label)?.upgrade()
}

/// Runs `f` with the GPUI `Window` attached to the Tauri window `label`.
pub fn with_gpui_window<R>(
    cx: &mut App,
    label: &str,
    f: impl FnOnce(&mut Window, &mut App) -> R,
) -> Option<R> {
    let handle = *cx.try_global::<Registry>()?.windows.get(label)?;
    handle.update(cx, |_, window, cx| f(window, cx)).ok()
}
