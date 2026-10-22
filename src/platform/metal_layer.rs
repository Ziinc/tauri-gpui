//! The `CAMetalLayer` wgpu draws a macOS window into.
//!
//! wgpu's Metal surfaces offer only opaque and post-multiplied alpha, and
//! GPUI asks for pre-multiplied, so a transparent window gets an opaque
//! layer and its unpainted pixels show black. Each surface configure sets the
//! layer opaque again, so [`sync`] clears it before every frame.
//!
//! wgpu also skips frames while the window is not on screen (a hidden popup
//! being resized before it is shown), leaving the layer with the previous
//! frame, which Core Animation stretches to the new size. [`is_on_screen`]
//! lets the runtime redraw once the window shows.

use objc2::{
    msg_send,
    rc::Retained,
    runtime::{AnyClass, AnyObject},
};
use raw_window_handle::RawWindowHandle;

use super::window::RawWindow;

/// `NSWindowOcclusionStateVisible`.
const OCCLUSION_STATE_VISIBLE: usize = 1 << 1;

/// The view `raw` draws into.
fn ns_view(raw: &RawWindow) -> Option<&AnyObject> {
    let RawWindowHandle::AppKit(handle) = raw.window else {
        return None;
    };
    // SAFETY: the handle points at the window's live `NSView`.
    Some(unsafe { handle.ns_view.cast::<AnyObject>().as_ref() })
}

/// Makes the Metal layers under `raw`'s view translucent.
pub(crate) fn sync(raw: &RawWindow) {
    let Some(view) = ns_view(raw) else {
        return;
    };
    // SAFETY: a plain `NSView` message on the main thread.
    let root: Option<Retained<AnyObject>> = unsafe { msg_send![view, layer] };
    if let Some(root) = root {
        make_translucent(&root);
    }
}

/// Clears `opaque` on the `CAMetalLayer`s among `root`'s sublayers, where
/// wgpu adds its own.
fn make_translucent(root: &AnyObject) {
    let Some(metal) = AnyClass::get(c"CAMetalLayer") else {
        return;
    };
    // SAFETY: plain `CALayer`/`NSArray` messages.
    unsafe {
        let sublayers: Option<Retained<AnyObject>> = msg_send![root, sublayers];
        let Some(sublayers) = sublayers else { return };
        let count: usize = msg_send![&*sublayers, count];
        for index in 0..count {
            let layer: Retained<AnyObject> = msg_send![&*sublayers, objectAtIndex: index];
            let is_metal: bool = msg_send![&*layer, isKindOfClass: metal];
            let opaque: bool = msg_send![&*layer, isOpaque];
            if is_metal && opaque {
                let _: () = msg_send![&*layer, setOpaque: false];
            }
        }
    }
}

/// Whether `raw`'s window is on screen, as wgpu checks before it
/// draws.
pub(crate) fn is_on_screen(raw: &RawWindow) -> bool {
    let Some(view) = ns_view(raw) else {
        return false;
    };
    // SAFETY: plain `NSView`/`NSWindow` messages on the main thread.
    unsafe {
        let window: Option<Retained<AnyObject>> = msg_send![view, window];
        let Some(window) = window else { return false };
        let state: usize = msg_send![&*window, occlusionState];
        state & OCCLUSION_STATE_VISIBLE != 0
    }
}

#[cfg(test)]
mod tests {
    use objc2::{
        msg_send,
        rc::Retained,
        runtime::{AnyClass, AnyObject},
    };

    fn layer(class: &std::ffi::CStr) -> Retained<AnyObject> {
        let class = AnyClass::get(class).expect("a CALayer class");
        // SAFETY: `+new` on a CALayer subclass returns a retained instance.
        unsafe { msg_send![class, new] }
    }

    fn is_opaque(layer: &AnyObject) -> bool {
        // SAFETY: a plain `CALayer` message.
        unsafe { msg_send![layer, isOpaque] }
    }

    #[test]
    fn makes_only_metal_sublayers_translucent() {
        let root = layer(c"CALayer");
        let metal = layer(c"CAMetalLayer");
        let other = layer(c"CALayer");
        // SAFETY: plain `CALayer` messages on live layers.
        unsafe {
            for sublayer in [&metal, &other] {
                let _: () = msg_send![&**sublayer, setOpaque: true];
                let _: () = msg_send![&*root, addSublayer: &**sublayer];
            }
        }
        super::make_translucent(&root);
        assert!(!is_opaque(&metal));
        assert!(is_opaque(&other));
    }
}
