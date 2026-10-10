//! The UIKit side of `GpuiInputView`'s `UITextInput`: text positions and
//! ranges, the bridge to GPUI's focused input handler, and keeping the
//! keyboard in step with edits GPUI makes on its own. See [`super::text`].

use std::{cell::RefCell, ops::Range};

use gpui::{Bounds, Pixels, PlatformInputHandler, TextInputConfiguration, point, px};
use objc2::{
    ClassType, DefinedClass, MainThreadMarker, MainThreadOnly, Message, define_class, msg_send,
    rc::Retained,
    runtime::{NSObjectProtocol, ProtocolObject},
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{NSCopying, NSObject, NSZone};
use objc2_ui_kit::{UITextInputDelegate, UITextPosition, UITextRange};

use super::{
    InputView, VIEW,
    text::{Change, TextEdit, TextState, change, utf16_len},
};

thread_local! {
    /// What the keyboard last saw, for questions UIKit asks while GPUI is
    /// busy. `None` when no text input has focus.
    static STATE: RefCell<Option<TextState>> = const { RefCell::new(None) };
    static CONFIGURATION: RefCell<TextInputConfiguration> =
        RefCell::new(TextInputConfiguration::default());
    /// The configuration changed while the keyboard was up.
    static RELOAD: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// GPUI is mid-update, so its input handler cannot be reached.
pub(crate) struct Busy;

/// Runs `f` on GPUI's focused input handler, after applying queued view
/// events so it sees every earlier edit. `Ok(None)` when nothing has focus.
fn gpui<R>(f: impl FnOnce(&mut PlatformInputHandler) -> R) -> Result<Option<R>, Busy> {
    crate::runtime::with(|runtime| Ok(runtime.ios_input_handler(f)))
        .ok()
        .flatten()
        .ok_or(Busy)
}

fn read_state(handler: &mut PlatformInputHandler) -> Option<TextState> {
    let selection = handler.selected_text_range(false)?.range;
    let marked = handler.marked_text_range();
    let len = handler.text_length_utf16().unwrap_or_else(|| {
        // Handlers clamp the range, as AppKit asks for arbitrary ones too.
        let mut adjusted = None;
        let text = handler.text_for_range(0..usize::MAX, &mut adjusted);
        adjusted
            .map(|range| range.end)
            .or(text.map(|text| utf16_len(&text)))
            .unwrap_or(selection.end)
    });
    let document = handler.text_input_editable_range().unwrap_or(0..len);
    Some(TextState {
        document,
        selection,
        marked,
    })
}

/// The document as GPUI has it now, or as the keyboard last saw it while
/// GPUI is busy.
pub(super) fn state() -> Option<TextState> {
    match gpui(read_state) {
        Ok(state) => {
            let state = state.flatten();
            STATE.with(|slot| *slot.borrow_mut() = state.clone());
            state
        }
        Err(Busy) => STATE.with(|slot| slot.borrow().clone()),
    }
}

/// Whether the keyboard is composing, by what it last saw. Cheap.
pub(super) fn composing() -> bool {
    STATE.with(|slot| slot.borrow().as_ref().is_some_and(|s| s.marked.is_some()))
}

/// Applies a keyboard edit: right away when GPUI is free, else when the
/// event loop next drains, with the remembered state updated meanwhile.
pub(super) fn edit(edit: TextEdit) {
    STATE.with(|slot| {
        if let Some(state) = slot.borrow_mut().as_mut() {
            state.apply(&edit);
        }
    });
    super::push(super::ViewEvent::Text(edit));
    // Pumps the queue, then reads back what GPUI made of the edit.
    state();
}

/// GPUI's answer for `f`, or `None` while it is busy or nothing has focus.
pub(super) fn query<R>(f: impl FnOnce(&mut PlatformInputHandler) -> Option<R>) -> Option<R> {
    gpui(f).ok().flatten().flatten()
}

pub(super) fn text_in(range: Range<usize>) -> Option<String> {
    query(|handler| handler.text_for_range(range, &mut None))
}

pub(super) fn bounds_for(range: Range<usize>) -> Option<Bounds<Pixels>> {
    query(|handler| handler.bounds_for_range(range))
}

pub(super) fn index_at(at: CGPoint) -> Option<usize> {
    query(|handler| handler.character_index_for_point(point(px(at.x as f32), px(at.y as f32))))
}

/// GPUI lays its window out in points from the top left of TAO's view,
/// which the input view covers.
pub(super) fn cg_rect(bounds: Bounds<Pixels>) -> CGRect {
    CGRect::new(
        CGPoint::new(
            f32::from(bounds.origin.x).into(),
            f32::from(bounds.origin.y).into(),
        ),
        CGSize::new(
            f32::from(bounds.size.width).into(),
            f32::from(bounds.size.height).into(),
        ),
    )
}

pub(super) fn configuration() -> TextInputConfiguration {
    CONFIGURATION.with(|c| c.borrow().clone())
}

/// The focused input's keyboard preferences (autocorrect, capitalization,
/// return key). A keyboard already up picks them up on the next sync.
pub(crate) fn set_text_input_configuration(configuration: TextInputConfiguration) {
    CONFIGURATION.with(|c| *c.borrow_mut() = configuration);
    RELOAD.with(|reload| reload.set(true));
}

pub(super) fn reset() {
    STATE.with(|slot| *slot.borrow_mut() = None);
}

/// Tells the keyboard about changes GPUI made on its own: a key binding
/// edited the text, a tap moved the caret, another input took focus, a
/// composition ended. Runs after every drain of the event loop.
pub(crate) fn sync() {
    let Some(view) = VIEW.with(|slot| slot.borrow().clone()) else {
        return;
    };
    if !view.isFirstResponder() {
        reset();
        return;
    }
    if RELOAD.with(|reload| reload.replace(false)) {
        view.reloadInputViews();
    }
    let Ok(new) = gpui(read_state) else { return };
    let new = new.flatten();
    let old = STATE.with(|slot| slot.replace(new.clone()));
    let Some(delegate) = view.input_delegate() else {
        return;
    };
    let input = Some(ProtocolObject::from_ref(&*view));
    match change(old.as_ref(), new.as_ref()) {
        Change::None => {}
        Change::Selection => {
            delegate.selectionWillChange(input);
            delegate.selectionDidChange(input);
        }
        Change::Text => {
            delegate.textWillChange(input);
            delegate.textDidChange(input);
        }
    }
}

define_class!(
    /// An offset into the document, in UTF-16 code units. Immutable.
    // SAFETY: UITextPosition has no subclassing requirements; `Drop` is not
    // implemented.
    #[unsafe(super(UITextPosition, NSObject))]
    #[name = "TauriGpuiTextPosition"]
    #[thread_kind = MainThreadOnly]
    #[ivars = usize]
    pub(super) struct TextPosition;

    unsafe impl NSObjectProtocol for TextPosition {}

    unsafe impl NSCopying for TextPosition {
        #[unsafe(method_id(copyWithZone:))]
        fn copy_with_zone(&self, _zone: *mut NSZone) -> Retained<Self> {
            self.retain()
        }
    }
);

define_class!(
    /// A range of the document. Immutable.
    // SAFETY: UITextRange has no subclassing requirements; `Drop` is not
    // implemented.
    #[unsafe(super(UITextRange, NSObject))]
    #[name = "TauriGpuiTextRange"]
    #[thread_kind = MainThreadOnly]
    #[ivars = Range<usize>]
    pub(super) struct TextRange;

    impl TextRange {
        #[unsafe(method_id(start))]
        fn start(&self) -> Retained<UITextPosition> {
            position(self.mtm(), self.ivars().start)
        }

        #[unsafe(method_id(end))]
        fn end(&self) -> Retained<UITextPosition> {
            position(self.mtm(), self.ivars().end)
        }

        #[unsafe(method(isEmpty))]
        fn is_empty(&self) -> bool {
            self.ivars().is_empty()
        }
    }

    unsafe impl NSObjectProtocol for TextRange {}

    unsafe impl NSCopying for TextRange {
        #[unsafe(method_id(copyWithZone:))]
        fn copy_with_zone(&self, _zone: *mut NSZone) -> Retained<Self> {
            self.retain()
        }
    }
);

pub(super) fn position(mtm: MainThreadMarker, offset: usize) -> Retained<UITextPosition> {
    let this = TextPosition::alloc(mtm).set_ivars(offset);
    let this: Retained<TextPosition> = unsafe { msg_send![super(this), init] };
    Retained::into_super(this)
}

pub(super) fn range(mtm: MainThreadMarker, range: Range<usize>) -> Retained<UITextRange> {
    let this = TextRange::alloc(mtm).set_ivars(range);
    let this: Retained<TextRange> = unsafe { msg_send![super(this), init] };
    Retained::into_super(this)
}

/// The offset of one of our positions. UIKit only hands back positions this
/// view made.
pub(super) fn offset(position: &UITextPosition) -> Option<usize> {
    position
        .isKindOfClass(TextPosition::class())
        // SAFETY: checked to be a `TextPosition` just above.
        .then(|| *unsafe { &*(position as *const UITextPosition as *const TextPosition) }.ivars())
}

pub(super) fn offsets(range: &UITextRange) -> Option<Range<usize>> {
    if range.isKindOfClass(TextRange::class()) {
        // SAFETY: checked to be a `TextRange` just above.
        Some(
            unsafe { &*(range as *const UITextRange as *const TextRange) }
                .ivars()
                .clone(),
        )
    } else {
        Some(offset(&range.start())?..offset(&range.end())?)
    }
}

impl InputView {
    pub(super) fn input_delegate(
        &self,
    ) -> Option<Retained<ProtocolObject<dyn UITextInputDelegate>>> {
        self.ivars()
            .delegate
            .borrow()
            .as_ref()
            .and_then(|delegate| delegate.load())
    }
}
