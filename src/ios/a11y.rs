//! VoiceOver support: GPUI's AccessKit tree, exposed through `GpuiInputView`.
//!
//! GPUI builds an AccessKit [`TreeUpdate`] every frame while assistive
//! technology is active. `accesskit_ios`'s low-level [`Adapter`] turns it into
//! `UIAccessibilityElement`s; the input view forwards `accessibilityElements`
//! and `accessibilityHitTest:` to it (see `crate::ios`). The adapter activates
//! itself when VoiceOver (or Switch Control, Speak Screen) first queries the
//! view, so GPUI does no accessibility work otherwise.
//!
//! GPUI does not report which nodes scroll, so VoiceOver's three-finger scroll
//! would find nothing to scroll. Every node advertises the four scroll actions
//! instead, and a request becomes a page-sized `ScrollWheel` at that node,
//! which GPUI routes to the nearest scrollable ancestor like any wheel event.

use std::{cell::RefCell, collections::HashMap};

use accesskit_ios::Adapter;
use gpui::{
    A11yCallbacks,
    accesskit::{
        Action, ActionHandler, ActionRequest, ActivationHandler, DeactivationHandler, NodeId, Rect,
        TreeUpdate,
    },
};
use objc2::rc::Retained;

use super::{InputView, ViewEvent, push};

/// The scroll actions GPUI does not advertise itself.
const SCROLL_ACTIONS: [Action; 4] = [
    Action::ScrollUp,
    Action::ScrollDown,
    Action::ScrollLeft,
    Action::ScrollRight,
];

thread_local! {
    static ADAPTER: RefCell<Option<Adapter>> = const { RefCell::new(None) };
    /// Bounds of every node in the last tree, in physical pixels.
    static BOUNDS: RefCell<HashMap<NodeId, Rect>> = RefCell::default();
}

struct Activation(Box<dyn Fn() -> Option<TreeUpdate> + Send>);

impl ActivationHandler for Activation {
    fn request_initial_tree(&mut self) -> Option<TreeUpdate> {
        (self.0)()
    }
}

struct Deactivation(Box<dyn Fn() + Send>);

impl DeactivationHandler for Deactivation {
    fn deactivate_accessibility(&mut self) {
        (self.0)()
    }
}

struct Actions(Box<dyn Fn(ActionRequest) + Send>);

impl ActionHandler for Actions {
    fn do_action(&mut self, request: ActionRequest) {
        if !SCROLL_ACTIONS.contains(&request.action) {
            // GPUI only queues the request, so this is safe even when UIKit
            // calls from inside a GPUI update.
            (self.0)(request);
            return;
        }
        let bounds = BOUNDS.with(|bounds| bounds.borrow().get(&request.target_node).copied());
        if let Some(bounds) = bounds {
            push(ViewEvent::A11yScroll {
                action: request.action,
                bounds,
            });
        }
    }
}

/// Creates the adapter for `view`, replacing any previous one.
pub(super) fn init(view: &Retained<InputView>, callbacks: A11yCallbacks) {
    // SAFETY: `view` is a live UIView, retained by the adapter.
    let adapter = unsafe {
        Adapter::new(
            Retained::as_ptr(view) as *mut std::ffi::c_void,
            Activation(callbacks.activation),
            Actions(callbacks.action),
            Deactivation(callbacks.deactivation),
        )
    };
    // The view is already in its window, so UIKit will not call
    // `didMoveToWindow`: activate now if VoiceOver is running.
    let events = view.window().and_then(|_| adapter.view_did_appear());
    ADAPTER.with(|slot| *slot.borrow_mut() = Some(adapter));
    if let Some(events) = events {
        events.raise();
    }
}

/// Drops the adapter (the GPUI window is gone).
pub(super) fn reset() {
    let adapter = ADAPTER.with(|slot| slot.borrow_mut().take());
    drop(adapter);
    BOUNDS.with(|bounds| bounds.borrow_mut().clear());
}

/// Hands GPUI's latest tree to the adapter.
pub(crate) fn tree_update(mut update: TreeUpdate) {
    BOUNDS.with(|bounds| {
        let mut bounds = bounds.borrow_mut();
        // GPUI sends the whole tree every frame.
        if update.tree.is_some() {
            bounds.clear();
        }
        for (id, node) in &mut update.nodes {
            for action in SCROLL_ACTIONS {
                node.add_action(action);
            }
            if let Some(rect) = node.bounds() {
                bounds.insert(*id, rect);
            }
        }
    });
    let events = ADAPTER.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(|adapter| adapter.update_if_active(|| update))
    });
    if let Some(events) = events {
        events.raise();
    }
}

/// Runs `f` with the adapter, if GPUI has initialized one.
pub(super) fn with_adapter<T>(f: impl FnOnce(&Adapter) -> T) -> Option<T> {
    ADAPTER.with(|slot| slot.borrow().as_ref().map(f))
}
