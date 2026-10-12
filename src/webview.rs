//! Tauri child webviews embedded in a GPUI-backed window.
//!
//! [`webview_frame`] is a GPUI element that reserves a rectangle in the
//! layout and keeps a Tauri child webview (`tauri::Window::add_child`) on
//! top of it. The webview is shown while its frame is painted and hidden in
//! any frame that leaves it out, so switching GPUI screens needs no extra
//! bookkeeping.
//!
//! The webview is a native view above the GPUI surface: GPUI content that
//! overlaps the frame (popovers, menus, tooltips) is drawn underneath it,
//! and clipping by scrolling ancestors is rectangular.

use gpui::{
    App, Bounds, Element, ElementId, GlobalElementId, Hitbox, HitboxBehavior, InspectorElementId,
    IntoElement, LayoutId, Pixels, Refineable as _, Style, StyleRefinement, Styled, Window,
};
use tauri::{LogicalPosition, LogicalSize, Rect, Webview, Wry};

use crate::{platform::window::WindowInner, runtime};

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
mod linux;

/// Native state of one embedded webview, owned by its GPUI window.
pub(crate) struct EmbeddedWebview {
    webview: Webview<Wry>,
    /// Where the current frame painted it; `None` when it was left out.
    painted: Option<Bounds<Pixels>>,
    /// What was last applied to the native webview; `None` before the first
    /// frame, `Some(None)` while hidden.
    applied: Option<Option<Bounds<Pixels>>>,
}

/// Creates a GPUI element that hosts `webview`, a child webview of the
/// GPUI-backed Tauri window this element renders in.
///
/// The element takes the full size of its parent by default; size it like
/// any `div`. Create the webview hidden (call `hide()` right after
/// `add_child`) so it does not flash at its initial bounds.
pub fn webview_frame(webview: &Webview<Wry>) -> WebviewFrame {
    WebviewFrame {
        webview: webview.clone(),
        style: StyleRefinement::default(),
    }
}

/// The element returned by [`webview_frame`].
pub struct WebviewFrame {
    webview: Webview<Wry>,
    style: StyleRefinement,
}

impl Styled for WebviewFrame {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl IntoElement for WebviewFrame {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for WebviewFrame {
    type RequestLayoutState = ();
    type PrepaintState = Option<Hitbox>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style {
            size: gpui::Size::full(),
            ..Style::default()
        };
        style.refine(&self.style);
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        _: &mut App,
    ) -> Self::PrepaintState {
        let visible = bounds.intersect(&window.content_mask().bounds);
        if visible.size.width <= Pixels::ZERO || visible.size.height <= Pixels::ZERO {
            return None;
        }
        let window_label = self.webview.window().label().to_string();
        let registered = runtime::with(|runtime| Ok(runtime.surface(&window_label)))
            .ok()
            .flatten()
            .map(|inner| {
                let mut webviews = inner.webviews.borrow_mut();
                let entry = webviews
                    .entry(self.webview.label().to_string())
                    .or_insert_with(|| EmbeddedWebview {
                        webview: self.webview.clone(),
                        painted: None,
                        applied: None,
                    });
                entry.painted = Some(visible);
            });
        if registered.is_none() {
            log::warn!(
                "webview_frame: `{}` is not a child of a GPUI-backed window",
                self.webview.label()
            );
            return None;
        }
        // The native view receives input over this area; keep GPUI content
        // underneath from reacting to it too.
        Some(window.insert_hitbox(visible, HitboxBehavior::BlockMouse))
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        hitbox: &mut Self::PrepaintState,
        window: &mut Window,
        _: &mut App,
    ) {
        if hitbox.is_none() {
            return;
        }
        // Clicks on the webview go to the native view, so any click GPUI
        // sees is outside it and takes keyboard focus back.
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        {
            let webview = self.webview.clone();
            window.on_mouse_event(move |_: &gpui::MouseDownEvent, phase, _, _| {
                if phase == gpui::DispatchPhase::Bubble {
                    linux::focus_parent(&webview);
                }
            });
        }
        #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
        let _ = window;
    }
}

/// Applies the frame GPUI just drew to the native webviews: moves painted
/// ones to their new bounds and hides the ones the frame left out.
///
/// Runs from `PlatformWindow::draw`, inside a GPUI update, so the Tauri calls
/// are deferred until the update returns.
pub(crate) fn sync(inner: &WindowInner) {
    let mut changes = Vec::new();
    for embedded in inner.webviews.borrow_mut().values_mut() {
        let painted = embedded.painted.take();
        if embedded.applied != Some(painted) {
            let was_shown = matches!(embedded.applied, Some(Some(_)));
            embedded.applied = Some(painted);
            changes.push((embedded.webview.clone(), painted, was_shown));
        }
    }
    if changes.is_empty() {
        return;
    }
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    let x11_parent = linux::x11_parent(inner.raw.get().window);
    (inner.defer)(Box::new(move || {
        for (webview, painted, was_shown) in changes {
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            if let Some(parent) = x11_parent {
                linux::apply(&webview, parent, painted);
                continue;
            }
            let result = match painted {
                Some(bounds) => webview
                    .set_bounds(rect(bounds))
                    .and_then(|()| if was_shown { Ok(()) } else { webview.show() }),
                None => webview.hide(),
            };
            if let Err(error) = result {
                log::warn!("failed to update webview `{}`: {error}", webview.label());
            }
        }
    }));
}

fn rect(bounds: Bounds<Pixels>) -> Rect {
    Rect {
        position: LogicalPosition::new(f32::from(bounds.origin.x), f32::from(bounds.origin.y))
            .into(),
        size: LogicalSize::new(f32::from(bounds.size.width), f32::from(bounds.size.height)).into(),
    }
}
