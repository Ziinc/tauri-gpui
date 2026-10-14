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

use std::rc::{Rc, Weak};

use gpui::{
    App, Bounds, Element, ElementId, GlobalElementId, Hitbox, HitboxBehavior, InspectorElementId,
    IntoElement, LayoutId, Pixels, Refineable as _, Style, StyleRefinement, Styled, Window,
};
use tauri::{LogicalPosition, LogicalSize, Manager as _, Rect, Webview, Wry};

use crate::{platform::window::WindowInner, runtime};

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
mod linux;

/// Native state of one embedded webview, owned by its GPUI window.
pub(crate) struct EmbeddedWebview {
    webview: Webview<Wry>,
    /// Where the latest frame places it; `None` once no frame shows it.
    desired: Option<Bounds<Pixels>>,
    /// What was last applied to the native webview; `None` before the first
    /// frame, `Some(None)` while hidden.
    applied: Option<Option<Bounds<Pixels>>>,
    /// Live `webview_frame` element states for this webview.
    frames: usize,
}

#[cfg(any(target_os = "linux", target_os = "freebsd"))]
impl Drop for EmbeddedWebview {
    fn drop(&mut self) {
        linux::forget(self.webview.label());
    }
}

/// GPUI element state of one `webview_frame`. GPUI keeps it while the
/// element is in the rendered frame, including frames that reuse a cached
/// view without running `prepaint`, and drops it in the first frame that
/// leaves the element out: that hides the webview.
struct FrameState {
    window: Weak<WindowInner>,
    label: String,
}

impl FrameState {
    fn new(inner: &Rc<WindowInner>, webview: &Webview<Wry>) -> Self {
        let label = webview.label().to_string();
        inner
            .webviews
            .borrow_mut()
            .entry(label.clone())
            .or_insert_with(|| EmbeddedWebview {
                webview: webview.clone(),
                desired: None,
                applied: None,
                frames: 0,
            })
            .frames += 1;
        Self {
            window: Rc::downgrade(inner),
            label,
        }
    }

    fn place(&self, bounds: Option<Bounds<Pixels>>) {
        if let Some(inner) = self.window.upgrade()
            && let Some(embedded) = inner.webviews.borrow_mut().get_mut(&self.label)
        {
            embedded.desired = bounds;
        }
    }
}

impl Drop for FrameState {
    fn drop(&mut self) {
        let Some(inner) = self.window.upgrade() else {
            return;
        };
        // Dropped from inside `Window::draw`, which holds no borrow of it.
        let Ok(mut webviews) = inner.webviews.try_borrow_mut() else {
            return;
        };
        if let Some(embedded) = webviews.get_mut(&self.label) {
            embedded.frames -= 1;
            if embedded.frames == 0 {
                embedded.desired = None;
            }
        }
    }
}

/// Creates a GPUI element that hosts `webview`, a child webview of the
/// GPUI-backed Tauri window this element renders in.
///
/// The element takes the full size of its parent by default; size it like
/// any `div`. Create the webview hidden (call `hide()` right after
/// `add_child`) so it does not flash at its initial bounds, and render each
/// webview in at most one frame at a time. A webview created to replace a
/// closed one needs a label of its own: frames track webviews by label.
pub fn webview_frame(webview: &Webview<Wry>) -> WebviewFrame {
    WebviewFrame {
        id: ElementId::Name(format!("webview-frame:{}", webview.label()).into()),
        webview: webview.clone(),
        style: StyleRefinement::default(),
    }
}

/// The element returned by [`webview_frame`].
pub struct WebviewFrame {
    id: ElementId,
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
        Some(self.id.clone())
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
        id: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        _: &mut App,
    ) -> Self::PrepaintState {
        let visible = bounds.intersect(&window.content_mask().bounds);
        let visible = (visible.size.width > Pixels::ZERO && visible.size.height > Pixels::ZERO)
            .then_some(visible);
        let id = id.expect("webview_frame has an element id");
        let placed = window.with_element_state::<Option<FrameState>, _>(id, |state, _| {
            let state = state.flatten().or_else(|| {
                // Looked up once per element; later frames reuse the state.
                let window = self.webview.window();
                match runtime::with(|runtime| Ok(runtime.surface(window.label()))) {
                    Ok(Some(inner)) => Some(FrameState::new(&inner, &self.webview)),
                    _ => {
                        log::warn!(
                            "webview_frame: `{}` is not a child of a GPUI-backed window",
                            self.webview.label()
                        );
                        None
                    }
                }
            });
            if let Some(state) = &state {
                state.place(visible);
            }
            (state.is_some(), state)
        });
        // The native view receives input over this area; keep GPUI content
        // underneath from reacting to it too.
        visible
            .filter(|_| placed)
            .map(|visible| window.insert_hitbox(visible, HitboxBehavior::BlockMouse))
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
            let label = self.webview.label().to_string();
            window.on_mouse_event(move |_: &gpui::MouseDownEvent, phase, _, _| {
                if phase == gpui::DispatchPhase::Bubble {
                    linux::focus_parent(&label);
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
    let mut webviews = inner.webviews.borrow_mut();
    for embedded in webviews.values_mut() {
        let painted = embedded.desired;
        if embedded.applied != Some(painted) {
            let was_shown = matches!(embedded.applied, Some(Some(_)));
            embedded.applied = Some(painted);
            changes.push((embedded.webview.clone(), painted, was_shown));
        }
    }
    if changes.is_empty() {
        return;
    }
    // Forget hidden webviews that no frame shows and Tauri has closed.
    webviews.retain(|_, embedded| {
        embedded.frames > 0
            || embedded.applied != Some(None)
            || embedded
                .webview
                .get_webview(embedded.webview.label())
                .is_some()
    });
    drop(webviews);
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    let x11_parent = crate::platform::x11::xid(inner.raw.get().window);
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    let toplevel = inner.x11.as_ref().map(|x11| x11.toplevel_xid);
    (inner.defer)(Box::new(move || {
        for (webview, painted, was_shown) in changes {
            #[cfg(any(target_os = "linux", target_os = "freebsd"))]
            if let Some(parent) = x11_parent {
                linux::apply(&webview, parent, toplevel, painted);
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

/// Makes the next frame re-apply every webview, e.g. after GPUI's native
/// surface changed.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub(crate) fn invalidate(inner: &WindowInner) {
    for embedded in inner.webviews.borrow_mut().values_mut() {
        embedded.applied = None;
    }
}

fn rect(bounds: Bounds<Pixels>) -> Rect {
    Rect {
        position: LogicalPosition::new(f32::from(bounds.origin.x), f32::from(bounds.origin.y))
            .into(),
        size: LogicalSize::new(f32::from(bounds.size.width), f32::from(bounds.size.height)).into(),
    }
}
