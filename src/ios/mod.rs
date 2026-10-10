//! iOS platform layer.
//!
//! TAO already gives each Tauri window a `UIView`: wgpu renders into a
//! `CAMetalLayer` sublayer of it, and TAO reports its touches, resizes and
//! focus as window events. What TAO lacks is provided by `GpuiInputView`, a
//! transparent subview laid over TAO's view that never takes touches. It is
//! the first responder for the software and hardware keyboards
//! (`UIKeyInput`, `pressesBegan:`), and it observes safe-area, keyboard frame,
//! trait (dark mode) and application lifecycle changes.
//!
//! UIKit calls back synchronously, sometimes from inside a GPUI update (showing
//! the keyboard posts its frame notification right away), so every callback
//! becomes a [`ViewEvent`] queued for [`crate::runtime::Runtime::drain`].

pub(crate) mod a11y;
mod cgl;
pub(crate) mod keys;

use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    sync::Arc,
};

use gpui::AppLifecyclePhase;
use objc2::{
    MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::Retained,
    runtime::{AnyObject, NSObjectProtocol, Sel},
    sel,
};
use objc2_core_foundation::{CGPoint, CGRect};
use objc2_foundation::{NSNotification, NSNotificationCenter, NSNotificationName, NSSet, NSString};
use objc2_ui_kit::{
    UIApplicationDidBecomeActiveNotification, UIApplicationDidEnterBackgroundNotification,
    UIApplicationWillEnterForegroundNotification, UIApplicationWillResignActiveNotification,
    UIColor, UIEdgeInsets, UIEvent, UIKeyInput, UIKeyboardFrameEndUserInfoKey,
    UIKeyboardWillChangeFrameNotification, UIKeyboardWillHideNotification, UIPasteboard, UIPress,
    UIPressesEvent, UIResponder, UITextAutocapitalizationType, UITextAutocorrectionType,
    UITextInputTraits, UITextSmartDashesType, UITextSmartQuotesType, UITextSpellCheckingType,
    UITraitCollection, UITraitEnvironment, UIUserInterfaceStyle, UIView, UIViewAutoresizing,
};

use crate::platform::dispatcher::LoopWaker;

/// Queued UIKit callbacks. Geometry is in points (GPUI logical pixels).
pub(crate) enum ViewEvent {
    /// Text typed on the software keyboard, or a hardware key that produces
    /// text. A lone `"\n"` is the return key.
    InsertText(String),
    DeleteBackward,
    /// A hardware key UIKit does not turn into text (arrows, escape, ...) or
    /// any key pressed with command or control.
    Key {
        down: bool,
        key: keys::HardwareKey,
    },
    SafeArea(UIEdgeInsets),
    /// How far the keyboard overlaps the bottom of the view.
    KeyboardBottom(f64),
    Appearance {
        dark: bool,
    },
    Lifecycle(AppLifecyclePhase),
    /// VoiceOver asked to scroll by a page around a node.
    A11yScroll {
        action: gpui::accesskit::Action,
        /// The node's bounds, in physical pixels.
        bounds: gpui::accesskit::Rect,
    },
}

#[derive(Default)]
struct Bridge {
    events: VecDeque<ViewEvent>,
    waker: Option<Arc<LoopWaker>>,
}

thread_local! {
    // UIKit, TAO and GPUI all run on the main thread.
    static BRIDGE: RefCell<Bridge> = RefCell::default();
    static VIEW: RefCell<Option<Retained<InputView>>> = const { RefCell::new(None) };
    static DARK: Cell<bool> = const { Cell::new(false) };
}

fn push(event: ViewEvent) {
    BRIDGE.with(|bridge| {
        let mut bridge = bridge.borrow_mut();
        bridge.events.push_back(event);
        if let Some(waker) = &bridge.waker {
            waker.wake();
        }
    });
}

/// Connects the queue to the event loop once the GPUI runtime exists.
pub(crate) fn set_waker(waker: Arc<LoopWaker>) {
    BRIDGE.with(|bridge| bridge.borrow_mut().waker = Some(waker));
}

pub(crate) fn take_events() -> VecDeque<ViewEvent> {
    BRIDGE.with(|bridge| std::mem::take(&mut bridge.borrow_mut().events))
}

pub(crate) fn is_dark() -> bool {
    DARK.with(Cell::get)
}

fn style_is_dark(traits: &UITraitCollection) -> bool {
    // SAFETY: a plain property read.
    (unsafe { traits.userInterfaceStyle() }) == UIUserInterfaceStyle::Dark
}

define_class!(
    // SAFETY: UIView has no subclassing requirements; `Drop` is not
    // implemented.
    #[unsafe(super(UIView, UIResponder, objc2_foundation::NSObject))]
    #[name = "TauriGpuiInputView"]
    #[thread_kind = MainThreadOnly]
    struct InputView;

    impl InputView {
        #[unsafe(method(canBecomeFirstResponder))]
        fn can_become_first_responder(&self) -> bool {
            true
        }

        /// Touches fall through to TAO's view underneath.
        #[unsafe(method(hitTest:withEvent:))]
        fn hit_test(&self, _point: CGPoint, _event: Option<&UIEvent>) -> *mut AnyObject {
            std::ptr::null_mut()
        }

        #[unsafe(method(safeAreaInsetsDidChange))]
        fn safe_area_insets_did_change(&self) {
            let _: () = unsafe { msg_send![super(self), safeAreaInsetsDidChange] };
            push(ViewEvent::SafeArea(self.safeAreaInsets()));
        }

        #[unsafe(method(traitCollectionDidChange:))]
        fn trait_collection_did_change(&self, previous: Option<&UITraitCollection>) {
            let _: () = unsafe { msg_send![super(self), traitCollectionDidChange: previous] };
            self.check_appearance();
        }

        /// VoiceOver: the view is a container for GPUI's AccessKit nodes,
        /// see `a11y`.
        #[unsafe(method(isAccessibilityElement))]
        fn is_accessibility_element(&self) -> bool {
            a11y::with_adapter(|adapter| adapter.is_accessibility_element()).unwrap_or(false)
        }

        #[unsafe(method(accessibilityElements))]
        fn accessibility_elements(&self) -> *mut AnyObject {
            a11y::with_adapter(|adapter| adapter.accessibility_elements().cast())
                .unwrap_or(std::ptr::null_mut())
        }

        #[unsafe(method(accessibilityHitTest:))]
        fn accessibility_hit_test(&self, point: CGPoint) -> *mut AnyObject {
            let point = accesskit_ios::CGPoint {
                x: point.x,
                y: point.y,
            };
            a11y::with_adapter(|adapter| adapter.hit_test(point).cast())
                .unwrap_or(std::ptr::null_mut())
        }

        /// Target of the iOS 17+ trait registration, see `install`.
        #[unsafe(method(gpuiTraitsDidChange))]
        fn traits_did_change(&self) {
            self.check_appearance();
        }

        #[unsafe(method(pressesBegan:withEvent:))]
        fn presses_began(&self, presses: &NSSet<UIPress>, event: Option<&UIPressesEvent>) {
            if !self.forward_presses(presses, true) {
                let _: () = unsafe { msg_send![super(self), pressesBegan: presses, withEvent: event] };
            }
        }

        #[unsafe(method(pressesEnded:withEvent:))]
        fn presses_ended(&self, presses: &NSSet<UIPress>, event: Option<&UIPressesEvent>) {
            if !self.forward_presses(presses, false) {
                let _: () = unsafe { msg_send![super(self), pressesEnded: presses, withEvent: event] };
            }
        }

        #[unsafe(method(pressesCancelled:withEvent:))]
        fn presses_cancelled(&self, presses: &NSSet<UIPress>, event: Option<&UIPressesEvent>) {
            if !self.forward_presses(presses, false) {
                let _: () =
                    unsafe { msg_send![super(self), pressesCancelled: presses, withEvent: event] };
            }
        }

        #[unsafe(method(gpuiKeyboardWillChangeFrame:))]
        fn keyboard_will_change_frame(&self, notification: &NSNotification) {
            push(ViewEvent::KeyboardBottom(self.keyboard_overlap(notification)));
        }

        #[unsafe(method(gpuiKeyboardWillHide:))]
        fn keyboard_will_hide(&self, _notification: &NSNotification) {
            push(ViewEvent::KeyboardBottom(0.));
        }

        #[unsafe(method(gpuiWillResignActive:))]
        fn will_resign_active(&self, _notification: &NSNotification) {
            push(ViewEvent::Lifecycle(AppLifecyclePhase::Inactive));
        }

        #[unsafe(method(gpuiDidEnterBackground:))]
        fn did_enter_background(&self, _notification: &NSNotification) {
            push(ViewEvent::Lifecycle(AppLifecyclePhase::Background));
        }

        #[unsafe(method(gpuiWillEnterForeground:))]
        fn will_enter_foreground(&self, _notification: &NSNotification) {
            push(ViewEvent::Lifecycle(AppLifecyclePhase::Foreground));
        }

        #[unsafe(method(gpuiDidBecomeActive:))]
        fn did_become_active(&self, _notification: &NSNotification) {
            push(ViewEvent::Lifecycle(AppLifecyclePhase::Active));
        }
    }

    unsafe impl NSObjectProtocol for InputView {}

    unsafe impl UITextInputTraits for InputView {
        // GPUI owns the text, and UIKeyInput gives the keyboard no context
        // to correct or capitalize against.
        #[unsafe(method(autocapitalizationType))]
        fn autocapitalization_type(&self) -> UITextAutocapitalizationType {
            UITextAutocapitalizationType::None
        }

        #[unsafe(method(autocorrectionType))]
        fn autocorrection_type(&self) -> UITextAutocorrectionType {
            UITextAutocorrectionType::No
        }

        #[unsafe(method(spellCheckingType))]
        fn spell_checking_type(&self) -> UITextSpellCheckingType {
            UITextSpellCheckingType::No
        }

        #[unsafe(method(smartQuotesType))]
        fn smart_quotes_type(&self) -> UITextSmartQuotesType {
            UITextSmartQuotesType::No
        }

        #[unsafe(method(smartDashesType))]
        fn smart_dashes_type(&self) -> UITextSmartDashesType {
            UITextSmartDashesType::No
        }
    }

    unsafe impl UIKeyInput for InputView {
        /// Always `true`, so backspace reaches GPUI even when UIKit thinks
        /// nothing was typed.
        #[unsafe(method(hasText))]
        fn has_text(&self) -> bool {
            true
        }

        #[unsafe(method(insertText:))]
        fn insert_text(&self, text: &NSString) {
            push(ViewEvent::InsertText(text.to_string()));
        }

        #[unsafe(method(deleteBackward))]
        fn delete_backward(&self) {
            push(ViewEvent::DeleteBackward);
        }
    }
);

impl InputView {
    fn check_appearance(&self) {
        let dark = style_is_dark(&self.traitCollection());
        if DARK.with(|d| d.replace(dark)) != dark {
            push(ViewEvent::Appearance { dark });
        }
    }

    fn new(mtm: MainThreadMarker, frame: CGRect) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    /// Queues the hardware keys GPUI must see as keystrokes. Returns `false`
    /// when none of `presses` is one, so UIKit turns them into text.
    fn forward_presses(&self, presses: &NSSet<UIPress>, down: bool) -> bool {
        let mtm = self.mtm();
        let keys: Vec<_> = presses
            .iter()
            .filter_map(|press| press.key(mtm))
            .filter_map(|key| {
                keys::hardware_key(
                    key.keyCode().0 as u32,
                    key.modifierFlags().bits() as u64,
                    &key.charactersIgnoringModifiers().to_string(),
                )
            })
            .collect();
        if keys.is_empty() {
            return false;
        }
        for key in keys {
            push(ViewEvent::Key { down, key });
        }
        true
    }

    /// Height of the keyboard's end frame over this view, in points.
    fn keyboard_overlap(&self, notification: &NSNotification) -> f64 {
        let Some(info) = notification.userInfo() else {
            return 0.;
        };
        let Some(value) = (unsafe { info.objectForKey(UIKeyboardFrameEndUserInfoKey) }) else {
            return 0.;
        };
        let frame: CGRect = unsafe { msg_send![&*value, CGRectValue] };
        let Some(window) = self.window() else {
            return 0.;
        };
        // The frame is in screen coordinates.
        let space = window.screen().coordinateSpace();
        let frame: CGRect =
            unsafe { msg_send![self, convertRect: frame, fromCoordinateSpace: &*space] };
        let bounds = self.bounds();
        let overlap = (bounds.origin.y + bounds.size.height) - frame.origin.y;
        overlap.clamp(0., bounds.size.height)
    }

    fn observe(&self, selector: Sel, name: &NSNotificationName) {
        let center = NSNotificationCenter::defaultCenter();
        unsafe { center.addObserver_selector_name_object(self, selector, Some(name), None) };
    }
}

/// Lays `GpuiInputView` over TAO's view of the attached window.
///
/// # Safety
///
/// `view` must be a live `UIView`.
pub(crate) unsafe fn install(view: *mut std::ffi::c_void) {
    let Some(mtm) = MainThreadMarker::new() else {
        log::error!("tauri-plugin-gpui: the iOS input view must be installed on the main thread");
        return;
    };
    // SAFETY: the caller guarantees `view` is a live UIView.
    let Some(host) = (unsafe { (view as *mut UIView).as_ref() }) else {
        return;
    };
    uninstall();
    let input = InputView::new(mtm, host.bounds());
    input.setAutoresizingMask(
        UIViewAutoresizing::FlexibleWidth | UIViewAutoresizing::FlexibleHeight,
    );
    input.setBackgroundColor(Some(&UIColor::clearColor()));
    input.setOpaque(false);
    host.addSubview(&input);

    unsafe {
        input.observe(
            sel!(gpuiKeyboardWillChangeFrame:),
            UIKeyboardWillChangeFrameNotification,
        );
        input.observe(sel!(gpuiKeyboardWillHide:), UIKeyboardWillHideNotification);
        input.observe(
            sel!(gpuiWillResignActive:),
            UIApplicationWillResignActiveNotification,
        );
        input.observe(
            sel!(gpuiDidEnterBackground:),
            UIApplicationDidEnterBackgroundNotification,
        );
        input.observe(
            sel!(gpuiWillEnterForeground:),
            UIApplicationWillEnterForegroundNotification,
        );
        input.observe(
            sel!(gpuiDidBecomeActive:),
            UIApplicationDidBecomeActiveNotification,
        );
    }

    // iOS 17+ deprecates `traitCollectionDidChange:` and, on iOS 18, no longer
    // reliably calls it; register for the interface style instead.
    if input.respondsToSelector(sel!(registerForTraitChanges:withTarget:action:))
        && let Some(style) = objc2::runtime::AnyClass::get(c"UITraitUserInterfaceStyle")
    {
        let traits = objc2_foundation::NSArray::<AnyObject>::from_slice(&[style.as_ref()]);
        let _: Option<Retained<AnyObject>> = unsafe {
            msg_send![
                &*input,
                registerForTraitChanges: &*traits,
                withTarget: &*input,
                action: sel!(gpuiTraitsDidChange)
            ]
        };
    }
    DARK.with(|dark| dark.set(style_is_dark(&input.traitCollection())));
    push(ViewEvent::SafeArea(input.safeAreaInsets()));
    VIEW.with(|slot| *slot.borrow_mut() = Some(input));
}

/// Removes the input view (the attached window was destroyed).
pub(crate) fn uninstall() {
    a11y::reset();
    if let Some(input) = VIEW.with(|slot| slot.borrow_mut().take()) {
        unsafe { NSNotificationCenter::defaultCenter().removeObserver(&input) };
        input.resignFirstResponder();
        input.removeFromSuperview();
    }
}

/// Connects GPUI's accessibility callbacks to the input view.
pub(crate) fn init_a11y(callbacks: gpui::A11yCallbacks) {
    let view = VIEW.with(|slot| slot.borrow().clone());
    match view {
        Some(view) => a11y::init(&view, callbacks),
        None => log::warn!("tauri-plugin-gpui: no input view to host the accessibility tree"),
    }
}

fn with_view(f: impl FnOnce(&InputView)) {
    let view = VIEW.with(|slot| slot.borrow().clone());
    if let Some(view) = view {
        f(&view);
    }
}

pub(crate) fn show_keyboard() {
    log::debug!("tauri-plugin-gpui: showing the software keyboard");
    with_view(|view| {
        if !view.isFirstResponder() && !view.becomeFirstResponder() {
            log::warn!("tauri-plugin-gpui: the input view could not become first responder");
        }
    });
}

pub(crate) fn hide_keyboard() {
    log::debug!("tauri-plugin-gpui: hiding the software keyboard");
    with_view(|view| {
        view.resignFirstResponder();
    });
}

pub(crate) fn clipboard_text() -> Option<String> {
    // SAFETY: a plain property read.
    unsafe { UIPasteboard::generalPasteboard().string() }.map(|text| text.to_string())
}

pub(crate) fn set_clipboard_text(text: &str) {
    // SAFETY: a plain property write.
    unsafe { UIPasteboard::generalPasteboard().setString(Some(&NSString::from_str(text))) };
}

/// A new, detached `CAMetalLayer`.
pub(crate) fn metal_layer() -> Option<Retained<AnyObject>> {
    let class = objc2::runtime::AnyClass::get(c"CAMetalLayer")?;
    // SAFETY: `+new` on a CALayer subclass returns a retained instance.
    unsafe { msg_send![class, new] }
}
