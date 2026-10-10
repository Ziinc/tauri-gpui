//! iOS platform layer.
//!
//! TAO already gives each Tauri window a `UIView`: wgpu renders into a
//! `CAMetalLayer` sublayer of it, and TAO reports its touches, resizes and
//! focus as window events. What TAO lacks is provided by `GpuiInputView`, a
//! transparent subview laid over TAO's view that never takes touches. It is
//! the first responder for the software and hardware keyboards
//! (`UITextInput`, including IME composition, and `pressesBegan:`), and it
//! observes safe-area, keyboard frame, trait (dark mode) and application
//! lifecycle changes.
//!
//! The keyboard notifications only carry the end frame plus a duration and a
//! curve (usually UIKit's private keyboard curve), so the inset is animated by
//! UIKit itself: a zero-width shadow view's height follows the same animation,
//! and a `CADisplayLink` samples its presentation layer every frame while it
//! runs. GPUI therefore sees the keyboard inset move in step with the
//! keyboard.
//!
//! UIKit calls back synchronously, sometimes from inside a GPUI update (showing
//! the keyboard posts its frame notification right away), so every callback
//! becomes a [`ViewEvent`] queued for [`crate::runtime::Runtime::drain`].
//! Keyboard edits and text queries then drain the queue on the spot when GPUI
//! is free, because UIKit reads the document back right after editing it (see
//! `document`).

mod cgl;
mod document;
pub(crate) mod keys;
pub(crate) mod text;

use std::{
    cell::{Cell, OnceCell, RefCell},
    collections::VecDeque,
    sync::Arc,
};

use gpui::{AppLifecyclePhase, Autocapitalize, TextInputAction};
use objc2::{
    DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send,
    rc::{Retained, Weak},
    runtime::{AnyObject, NSObjectProtocol, ProtocolObject, Sel},
    sel,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{
    NSArray, NSAttributedStringKey, NSComparisonResult, NSDictionary, NSInteger, NSNotification,
    NSNotificationCenter, NSNotificationName, NSRange, NSRunLoop, NSRunLoopCommonModes, NSSet,
    NSString,
};
use objc2_quartz_core::{CADisplayLink, CAFrameRateRange};
use objc2_ui_kit::{
    NSWritingDirection, UIApplicationDidBecomeActiveNotification,
    UIApplicationDidEnterBackgroundNotification, UIApplicationWillEnterForegroundNotification,
    UIApplicationWillResignActiveNotification, UIColor, UIEdgeInsets, UIEvent, UIKeyInput,
    UIKeyboardAnimationCurveUserInfoKey, UIKeyboardAnimationDurationUserInfoKey,
    UIKeyboardFrameEndUserInfoKey, UIKeyboardWillChangeFrameNotification,
    UIKeyboardWillHideNotification, UIPasteboard, UIPress, UIPressesEvent, UIResponder,
    UIReturnKeyType, UITextAutocapitalizationType, UITextAutocorrectionType, UITextInput,
    UITextInputDelegate, UITextInputStringTokenizer, UITextInputTokenizer, UITextInputTraits,
    UITextLayoutDirection, UITextPosition, UITextRange, UITextSelectionRect, UITextSmartDashesType,
    UITextSmartQuotesType, UITextSpellCheckingType, UITextStorageDirection, UITraitCollection,
    UITraitEnvironment, UIUserInterfaceStyle, UIView, UIViewAnimationOptions, UIViewAutoresizing,
};

use crate::platform::dispatcher::LoopWaker;
use text::TextEdit;

/// Queued UIKit callbacks. Geometry is in points (GPUI logical pixels).
pub(crate) enum ViewEvent {
    /// A keyboard edit: typed or committed text (a lone `"\n"` is the return
    /// key), backspace, composition, or a selection change.
    Text(TextEdit),
    /// A hardware key UIKit does not turn into text (arrows, escape, ...) or
    /// any key pressed with command or control.
    Key {
        down: bool,
        key: keys::HardwareKey,
    },
    SafeArea(UIEdgeInsets),
    /// How far the keyboard overlaps the bottom of the view, sampled every
    /// frame while the keyboard animates.
    KeyboardBottom(f64),
    Appearance {
        dark: bool,
    },
    Lifecycle(AppLifecyclePhase),
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
    static KEYBOARD: RefCell<Option<KeyboardTracker>> = const { RefCell::new(None) };
}

/// Follows the keyboard's show, hide and resize animations.
struct KeyboardTracker {
    /// Zero-width subview of the input view whose height UIKit animates
    /// alongside the keyboard.
    shadow: Retained<UIView>,
    /// Runs only while `shadow` animates.
    link: Option<Retained<CADisplayLink>>,
    /// The overlap the current animation ends at.
    target: f64,
    /// The overlap last reported to GPUI.
    reported: f64,
}

impl KeyboardTracker {
    fn report(&mut self, overlap: f64) {
        if (overlap - self.reported).abs() > f64::EPSILON {
            self.reported = overlap;
            push(ViewEvent::KeyboardBottom(overlap));
        }
    }

    fn stop(&mut self) {
        if let Some(link) = self.link.take() {
            // Releases the link's strong reference to the input view.
            link.invalidate();
        }
    }
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

pub(crate) use document::{set_text_input_configuration, sync};

fn style_is_dark(traits: &UITraitCollection) -> bool {
    // SAFETY: a plain property read.
    (unsafe { traits.userInterfaceStyle() }) == UIUserInterfaceStyle::Dark
}

#[derive(Default)]
struct InputViewIvars {
    tokenizer: OnceCell<Retained<UITextInputStringTokenizer>>,
    /// `inputDelegate` is a weak property.
    delegate: RefCell<Option<Weak<ProtocolObject<dyn UITextInputDelegate>>>>,
}

define_class!(
    // SAFETY: UIView has no subclassing requirements; `Drop` is not
    // implemented.
    #[unsafe(super(UIView, UIResponder, objc2_foundation::NSObject))]
    #[name = "TauriGpuiInputView"]
    #[thread_kind = MainThreadOnly]
    #[ivars = InputViewIvars]
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
            self.animate_keyboard(self.keyboard_overlap(notification), notification);
        }

        #[unsafe(method(gpuiKeyboardWillHide:))]
        fn keyboard_will_hide(&self, notification: &NSNotification) {
            self.animate_keyboard(0., notification);
        }

        /// `CADisplayLink` callback while the keyboard animates.
        #[unsafe(method(gpuiKeyboardTick:))]
        fn keyboard_tick(&self, _link: &CADisplayLink) {
            KEYBOARD.with(|keyboard| {
                if let Some(keyboard) = &mut *keyboard.borrow_mut() {
                    let layer = keyboard.shadow.layer();
                    let running = layer.animationKeys().is_some_and(|keys| keys.count() > 0);
                    if running {
                        // SAFETY: a plain property read on the main thread.
                        let presented = unsafe { layer.presentationLayer() };
                        let height = presented.map_or(keyboard.target, |p| p.bounds().size.height);
                        keyboard.report(height);
                    } else {
                        keyboard.stop();
                        let target = keyboard.target;
                        keyboard.report(target);
                    }
                }
            });
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
        // GPUI's default `TextInputConfiguration` turns all of these off;
        // a focused input opts in through `text_input_configuration`.
        #[unsafe(method(autocapitalizationType))]
        fn autocapitalization_type(&self) -> UITextAutocapitalizationType {
            match document::configuration().autocapitalize {
                Autocapitalize::None => UITextAutocapitalizationType::None,
                Autocapitalize::Words => UITextAutocapitalizationType::Words,
                Autocapitalize::Sentences => UITextAutocapitalizationType::Sentences,
                Autocapitalize::Characters => UITextAutocapitalizationType::AllCharacters,
            }
        }

        #[unsafe(method(autocorrectionType))]
        fn autocorrection_type(&self) -> UITextAutocorrectionType {
            if document::configuration().autocorrect {
                UITextAutocorrectionType::Yes
            } else {
                UITextAutocorrectionType::No
            }
        }

        #[unsafe(method(spellCheckingType))]
        fn spell_checking_type(&self) -> UITextSpellCheckingType {
            if document::configuration().suggestions {
                UITextSpellCheckingType::Yes
            } else {
                UITextSpellCheckingType::No
            }
        }

        #[unsafe(method(smartQuotesType))]
        fn smart_quotes_type(&self) -> UITextSmartQuotesType {
            if document::configuration().autocorrect {
                UITextSmartQuotesType::Default
            } else {
                UITextSmartQuotesType::No
            }
        }

        #[unsafe(method(smartDashesType))]
        fn smart_dashes_type(&self) -> UITextSmartDashesType {
            if document::configuration().autocorrect {
                UITextSmartDashesType::Default
            } else {
                UITextSmartDashesType::No
            }
        }

        #[unsafe(method(returnKeyType))]
        fn return_key_type(&self) -> UIReturnKeyType {
            match document::configuration().input_action {
                TextInputAction::Unspecified | TextInputAction::Enter | TextInputAction::Previous => {
                    UIReturnKeyType::Default
                }
                TextInputAction::Done => UIReturnKeyType::Done,
                TextInputAction::Go => UIReturnKeyType::Go,
                TextInputAction::Next => UIReturnKeyType::Next,
                TextInputAction::Search => UIReturnKeyType::Search,
                TextInputAction::Send => UIReturnKeyType::Send,
            }
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
            document::edit(TextEdit::Insert(text.to_string()));
        }

        #[unsafe(method(deleteBackward))]
        fn delete_backward(&self) {
            document::edit(TextEdit::DeleteBackward);
        }
    }

    /// The document is GPUI's focused input, read through its input handler
    /// (see `document`). GPUI draws the text, caret, selection and marked
    /// text itself, so no `UITextInteraction` is installed.
    unsafe impl UITextInput for InputView {
        #[unsafe(method_id(textInRange:))]
        fn text_in_range(&self, range: &UITextRange) -> Option<Retained<NSString>> {
            (|| {
                let text = document::text_in(document::offsets(range)?)?;
                Some(NSString::from_str(&text))
            })()
        }

        #[unsafe(method(replaceRange:withText:))]
        fn replace_range(&self, range: &UITextRange, text: &NSString) {
            if let Some(range) = document::offsets(range) {
                document::edit(TextEdit::Replace {
                    range,
                    text: text.to_string(),
                });
            }
        }

        #[unsafe(method_id(selectedTextRange))]
        fn selected_text_range(&self) -> Option<Retained<UITextRange>> {
            let selection = document::state().map_or(0..0, |state| state.selection);
            Some(document::range(self.mtm(), selection))
        }

        #[unsafe(method(setSelectedTextRange:))]
        fn set_selected_text_range(&self, range: Option<&UITextRange>) {
            if let Some(range) = range.and_then(document::offsets) {
                document::edit(TextEdit::Select(range));
            }
        }

        #[unsafe(method_id(markedTextRange))]
        fn marked_text_range(&self) -> Option<Retained<UITextRange>> {
            (|| {
                let marked = document::state()?.marked?;
                Some(document::range(self.mtm(), marked))
            })()
        }

        /// GPUI styles marked text itself.
        #[unsafe(method_id(markedTextStyle))]
        fn marked_text_style(&self) -> Option<Retained<NSDictionary<NSAttributedStringKey, AnyObject>>> {
            None
        }

        #[unsafe(method(setMarkedTextStyle:))]
        fn set_marked_text_style(&self, _style: Option<&NSDictionary<NSAttributedStringKey, AnyObject>>) {}

        #[unsafe(method(setMarkedText:selectedRange:))]
        fn set_marked_text(&self, text: Option<&NSString>, selected: NSRange) {
            document::edit(TextEdit::Mark {
                text: text.map(|text| text.to_string()).unwrap_or_default(),
                selected: selected.location..selected.location + selected.length,
            });
        }

        #[unsafe(method(unmarkText))]
        fn unmark_text(&self) {
            document::edit(TextEdit::Unmark);
        }

        #[unsafe(method_id(beginningOfDocument))]
        fn beginning_of_document(&self) -> Retained<UITextPosition> {
            let start = document::state().map_or(0, |state| state.document.start);
            document::position(self.mtm(), start)
        }

        #[unsafe(method_id(endOfDocument))]
        fn end_of_document(&self) -> Retained<UITextPosition> {
            let end = document::state().map_or(0, |state| state.document.end);
            document::position(self.mtm(), end)
        }

        #[unsafe(method_id(textRangeFromPosition:toPosition:))]
        fn text_range_from_position(
            &self,
            from: &UITextPosition,
            to: &UITextPosition,
        ) -> Option<Retained<UITextRange>> {
            (|| {
                let (from, to) = (document::offset(from)?, document::offset(to)?);
                Some(document::range(self.mtm(), from.min(to)..from.max(to)))
            })()
        }

        #[unsafe(method_id(positionFromPosition:offset:))]
        fn position_from_position(
            &self,
            position: &UITextPosition,
            offset: NSInteger,
        ) -> Option<Retained<UITextPosition>> {
            self.moved(position, offset)
        }

        #[unsafe(method_id(positionFromPosition:inDirection:offset:))]
        fn position_in_direction(
            &self,
            position: &UITextPosition,
            direction: UITextLayoutDirection,
            offset: NSInteger,
        ) -> Option<Retained<UITextPosition>> {
            // GPUI exposes no line layout: up and down move like left and
            // right.
            let backward = matches!(direction, UITextLayoutDirection::Left | UITextLayoutDirection::Up);
            self.moved(position, if backward { -offset } else { offset })
        }

        #[unsafe(method(comparePosition:toPosition:))]
        fn compare_position(&self, position: &UITextPosition, other: &UITextPosition) -> NSComparisonResult {
            match document::offset(position).cmp(&document::offset(other)) {
                std::cmp::Ordering::Less => NSComparisonResult::Ascending,
                std::cmp::Ordering::Equal => NSComparisonResult::Same,
                std::cmp::Ordering::Greater => NSComparisonResult::Descending,
            }
        }

        #[unsafe(method(offsetFromPosition:toPosition:))]
        fn offset_from_position(&self, from: &UITextPosition, to: &UITextPosition) -> NSInteger {
            let (Some(from), Some(to)) = (document::offset(from), document::offset(to)) else {
                return 0;
            };
            to as NSInteger - from as NSInteger
        }

        #[unsafe(method_id(inputDelegate))]
        fn input_delegate_property(&self) -> Option<Retained<ProtocolObject<dyn UITextInputDelegate>>> {
            self.input_delegate()
        }

        #[unsafe(method(setInputDelegate:))]
        fn set_input_delegate(&self, delegate: Option<&ProtocolObject<dyn UITextInputDelegate>>) {
            *self.ivars().delegate.borrow_mut() = delegate.map(Weak::from);
        }

        #[unsafe(method_id(tokenizer))]
        fn tokenizer(&self) -> Retained<ProtocolObject<dyn UITextInputTokenizer>> {
            let tokenizer = self.ivars().tokenizer.get_or_init(|| {
                // SAFETY: this view implements UITextInput.
                unsafe {
                    UITextInputStringTokenizer::initWithTextInput(
                        UITextInputStringTokenizer::alloc(self.mtm()),
                        self,
                    )
                }
            });
            ProtocolObject::from_retained(tokenizer.clone())
        }

        #[unsafe(method_id(positionWithinRange:farthestInDirection:))]
        fn position_within_range(
            &self,
            range: &UITextRange,
            direction: UITextLayoutDirection,
        ) -> Option<Retained<UITextPosition>> {
            (|| {
                let range = document::offsets(range)?;
                let backward = matches!(direction, UITextLayoutDirection::Left | UITextLayoutDirection::Up);
                Some(document::position(self.mtm(), if backward { range.start } else { range.end }))
            })()
        }

        #[unsafe(method_id(characterRangeByExtendingPosition:inDirection:))]
        fn character_range_by_extending(
            &self,
            position: &UITextPosition,
            direction: UITextLayoutDirection,
        ) -> Option<Retained<UITextRange>> {
            (|| {
                let offset = document::offset(position)?;
                let document = document::state()?.document;
                let backward = matches!(direction, UITextLayoutDirection::Left | UITextLayoutDirection::Up);
                let range = if backward {
                    document.start..offset
                } else {
                    offset..document.end
                };
                Some(document::range(self.mtm(), range))
            })()
        }

        #[unsafe(method(baseWritingDirectionForPosition:inDirection:))]
        fn base_writing_direction(
            &self,
            _position: &UITextPosition,
            _direction: UITextStorageDirection,
        ) -> NSWritingDirection {
            NSWritingDirection::Natural
        }

        #[unsafe(method(setBaseWritingDirection:forRange:))]
        fn set_base_writing_direction(&self, _direction: NSWritingDirection, _range: &UITextRange) {}

        /// Where the keyboard anchors its candidate and correction UI.
        #[unsafe(method(firstRectForRange:))]
        fn first_rect_for_range(&self, range: &UITextRange) -> CGRect {
            document::offsets(range)
                .and_then(document::bounds_for)
                .map_or(CGRect::ZERO, document::cg_rect)
        }

        #[unsafe(method(caretRectForPosition:))]
        fn caret_rect_for_position(&self, position: &UITextPosition) -> CGRect {
            let Some(offset) = document::offset(position) else {
                return CGRect::ZERO;
            };
            document::bounds_for(offset..offset).map_or(CGRect::ZERO, |bounds| {
                let rect = document::cg_rect(bounds);
                CGRect::new(rect.origin, CGSize::new(rect.size.width.max(2.), rect.size.height))
            })
        }

        /// No system selection UI is shown, so there is nothing to outline.
        #[unsafe(method_id(selectionRectsForRange:))]
        fn selection_rects_for_range(&self, _range: &UITextRange) -> Retained<NSArray<UITextSelectionRect>> {
            NSArray::new()
        }

        #[unsafe(method_id(closestPositionToPoint:))]
        fn closest_position_to_point(&self, at: CGPoint) -> Option<Retained<UITextPosition>> {
            (|| {
                let offset = document::index_at(at)
                    .or_else(|| document::state().map(|state| state.selection.end))?;
                Some(document::position(self.mtm(), offset))
            })()
        }

        #[unsafe(method_id(closestPositionToPoint:withinRange:))]
        fn closest_position_within_range(
            &self,
            at: CGPoint,
            range: &UITextRange,
        ) -> Option<Retained<UITextPosition>> {
            (|| {
                let range = document::offsets(range)?;
                let offset = document::index_at(at).unwrap_or(range.start);
                Some(document::position(self.mtm(), offset.clamp(range.start, range.end)))
            })()
        }

        #[unsafe(method_id(characterRangeAtPoint:))]
        fn character_range_at_point(&self, at: CGPoint) -> Option<Retained<UITextRange>> {
            (|| {
                let offset = document::index_at(at)?;
                let end = document::state().map_or(offset, |state| (offset + 1).min(state.document.end));
                Some(document::range(self.mtm(), offset..end))
            })()
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
        let this = Self::alloc(mtm).set_ivars(InputViewIvars::default());
        unsafe { msg_send![super(this), initWithFrame: frame] }
    }

    /// Queues the hardware keys GPUI must see as keystrokes. Returns `false`
    /// when none of `presses` is one, so UIKit turns them into text.
    /// Mid-composition every key goes to the IME, which uses the arrows,
    /// return and backspace itself.
    fn forward_presses(&self, presses: &NSSet<UIPress>, down: bool) -> bool {
        if document::composing() {
            return false;
        }
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

    /// `position` moved by `offset`, or `None` outside the document.
    fn moved(
        &self,
        position: &UITextPosition,
        offset: NSInteger,
    ) -> Option<Retained<UITextPosition>> {
        let document = document::state().map_or(0..0, |state| state.document);
        let offset = document::offset(position)?.checked_add_signed(offset)?;
        (document.start..=document.end)
            .contains(&offset)
            .then(|| document::position(self.mtm(), offset))
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

    /// Moves the reported keyboard overlap to `end`, following the keyboard's
    /// own animation when the notification carries one.
    fn animate_keyboard(&self, end: f64, notification: &NSNotification) {
        let mtm = self.mtm();
        let (duration, curve) = keyboard_animation(notification);
        log::debug!(
            "tauri-plugin-gpui: keyboard overlap -> {end} over {duration}s (curve {curve})"
        );
        KEYBOARD.with(|keyboard| {
            let mut keyboard = keyboard.borrow_mut();
            let keyboard = keyboard.get_or_insert_with(|| {
                let shadow = UIView::initWithFrame(UIView::alloc(mtm), CGRect::ZERO);
                shadow.setUserInteractionEnabled(false);
                self.addSubview(&shadow);
                KeyboardTracker {
                    shadow,
                    link: None,
                    target: 0.,
                    reported: 0.,
                }
            });
            // Hiding posts both a frame change and a hide notification.
            if keyboard.link.is_some() && (keyboard.target - end).abs() <= f64::EPSILON {
                return;
            }
            keyboard.target = end;
            let frame = CGRect::new(CGPoint::ZERO, CGSize::new(0., end));
            if duration <= 0. {
                keyboard.stop();
                keyboard.shadow.layer().removeAllAnimations();
                keyboard.shadow.setFrame(frame);
                keyboard.report(end);
                return;
            }
            // UIKit's keyboard curve (7) is private, but UIView animations
            // accept any curve value shifted into the options' curve bits.
            let options = UIViewAnimationOptions((curve as usize) << 16)
                | UIViewAnimationOptions::BeginFromCurrentState;
            let shadow = keyboard.shadow.clone();
            let animations = block2::RcBlock::new(move || shadow.setFrame(frame));
            UIView::animateWithDuration_delay_options_animations_completion(
                duration,
                0.,
                options,
                &animations,
                None,
                mtm,
            );
            if keyboard.link.is_none() {
                keyboard.link = Some(self.display_link());
            }
        });
    }

    /// A display link calling `gpuiKeyboardTick:` every frame, at up to the
    /// display's maximum rate.
    fn display_link(&self) -> Retained<CADisplayLink> {
        // SAFETY: `gpuiKeyboardTick:` is defined above and takes the link.
        let link =
            unsafe { CADisplayLink::displayLinkWithTarget_selector(self, sel!(gpuiKeyboardTick:)) };
        // iOS 15+. ProMotion devices also need
        // `CADisableMinimumFrameDurationOnPhone` in the app's Info.plist.
        if link.respondsToSelector(sel!(setPreferredFrameRateRange:)) {
            link.setPreferredFrameRateRange(CAFrameRateRange {
                minimum: 60.,
                maximum: 120.,
                preferred: 120.,
            });
        }
        // SAFETY: the main run loop, on the main thread.
        unsafe { link.addToRunLoop_forMode(&NSRunLoop::mainRunLoop(), NSRunLoopCommonModes) };
        link
    }

    fn observe(&self, selector: Sel, name: &NSNotificationName) {
        let center = NSNotificationCenter::defaultCenter();
        unsafe { center.addObserver_selector_name_object(self, selector, Some(name), None) };
    }
}

/// The keyboard animation's duration in seconds and its `UIViewAnimationCurve`.
fn keyboard_animation(notification: &NSNotification) -> (f64, isize) {
    let Some(info) = notification.userInfo() else {
        return (0., 0);
    };
    // SAFETY: both keys hold `NSNumber`s.
    unsafe {
        let duration = info
            .objectForKey(UIKeyboardAnimationDurationUserInfoKey)
            .map_or(0., |value| msg_send![&*value, doubleValue]);
        let curve = info
            .objectForKey(UIKeyboardAnimationCurveUserInfoKey)
            .map_or(0, |value| msg_send![&*value, integerValue]);
        (duration, curve)
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
    // GPUI draws the caret; keep any caret UIKit adds for a UITextInput
    // view invisible.
    let _: () = unsafe { msg_send![&*input, setTintColor: &*UIColor::clearColor()] };
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
    document::reset();
    if let Some(mut keyboard) = KEYBOARD.with(|keyboard| keyboard.borrow_mut().take()) {
        keyboard.stop();
    }
    if let Some(input) = VIEW.with(|slot| slot.borrow_mut().take()) {
        unsafe { NSNotificationCenter::defaultCenter().removeObserver(&input) };
        input.resignFirstResponder();
        input.removeFromSuperview();
    }
}

fn with_view(f: impl FnOnce(&InputView)) {
    let view = VIEW.with(|slot| slot.borrow().clone());
    if let Some(view) = view {
        f(&view);
    }
}

pub(crate) fn show_keyboard() {
    let installed = VIEW.with(|slot| slot.borrow().is_some());
    log::debug!(
        "tauri-plugin-gpui: showing the software keyboard (input view installed: {installed})"
    );
    with_view(|view| {
        // UIKit only shows the keyboard for a responder in the key window,
        // and TAO does not always make its window key.
        if let Some(window) = view.window()
            && !window.isKeyWindow()
        {
            window.makeKeyWindow();
        }
        let was = view.isFirstResponder();
        let became = was || view.becomeFirstResponder();
        let window = view.window();
        log::debug!(
            "tauri-plugin-gpui: first responder was={was} now={} (in window: {}, key: {})",
            view.isFirstResponder(),
            window.is_some(),
            window.as_ref().is_some_and(|window| window.isKeyWindow())
        );
        if !became {
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
