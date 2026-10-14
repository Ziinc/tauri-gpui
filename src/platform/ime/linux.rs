//! Linux: the system input method (IBus, Fcitx, XIM, GTK's compose tables)
//! through a `GtkIMMulticontext` on the Tauri window's GTK toplevel.
//!
//! TAO feeds keys only to a `GtkIMContextSimple`, which never talks to the
//! system input method and reports nothing but commits. While a GPUI text
//! input has focus, key events are offered to this context first, from the
//! window's `event` signal (GTK emits it before `key-press-event`, where TAO
//! listens). Keys the context takes never reach TAO; its preedit becomes
//! GPUI marked text and its commits are inserted. Keys it does not want go
//! on to TAO as before, so GPUI key bindings keep working.
//!
//! GTK emits these signals from its own main-loop iteration, outside TAO's
//! event callback, so GPUI is normally idle and they are applied right away.
//! What arrives while GPUI is mid-update is queued for the next drain.

use std::{
    cell::{Cell, RefCell},
    rc::Weak,
};

use gpui::{Bounds, Pixels};
use gtk::{gdk, glib, prelude::*};
use tauri::Wry;

use super::{ImeEvent, KeyRoute, pixel_rect, route_key, utf16_offset};
use crate::{platform::window::WindowInner, runtime};

pub(crate) struct Ime {
    /// The window state that owns this context, for deferred focus changes.
    window: Weak<WindowInner>,
    context: gtk::IMMulticontext,
    /// GPUI has a focused text input.
    text_input: Cell<bool>,
    /// The Tauri window has keyboard focus.
    active: Cell<bool>,
    /// Whether the context was last told it has focus; keys go through it
    /// only then.
    focused: Cell<bool>,
    /// GPUI holds marked text from this context.
    composing: Cell<bool>,
    /// Set while the context looks at a key: what it emits is collected for
    /// `route_key` instead of applied.
    filtering: Cell<bool>,
    captured: RefCell<Vec<ImeEvent>>,
    /// Events that arrived while GPUI was mid-update.
    queued: RefCell<Vec<ImeEvent>>,
    /// Hardware keycodes of presses the context took; their releases are
    /// kept from TAO too.
    claimed_keys: RefCell<Vec<u16>>,
}

impl Ime {
    /// Creates the context for `window`, whose plugin state is `inner`.
    /// `None` when the window has no realized GTK toplevel.
    pub fn new(window: &tauri::Window<Wry>, inner: Weak<WindowInner>) -> Option<Self> {
        let widget = window
            .gtk_window()
            .inspect_err(|error| log::warn!("IME: no GTK window: {error}"))
            .ok()?;
        let Some(client) = widget.window() else {
            log::warn!("IME: the GTK window is not realized");
            return None;
        };
        let context = gtk::IMMulticontext::new();
        context.set_client_window(Some(&client));
        context.set_use_preedit(true);
        context.connect_commit({
            let inner = inner.clone();
            move |_, text| emit(&inner, ImeEvent::Commit(text.to_owned()))
        });
        context.connect_preedit_changed({
            let inner = inner.clone();
            move |context| {
                let (text, _, cursor) = context.preedit_string();
                let cursor = utf16_offset(&text, cursor.max(0) as usize);
                emit(
                    &inner,
                    ImeEvent::Preedit {
                        text: text.into(),
                        cursor,
                    },
                );
            }
        });
        context.connect_preedit_end({
            let inner = inner.clone();
            move |_| emit(&inner, ImeEvent::End)
        });
        // Not disconnected on teardown: GTK drops the handler with the
        // widget, which may already be gone, and it lets every key through
        // once the context lost focus.
        let _ = widget.connect_event({
            let inner = inner.clone();
            move |_, event| key_event(&inner, event)
        });
        Some(Self {
            window: inner,
            context,
            text_input: Cell::new(false),
            // TAO reports only focus changes after this.
            active: Cell::new(window.is_focused().unwrap_or(false)),
            focused: Cell::new(false),
            composing: Cell::new(false),
            filtering: Cell::new(false),
            captured: RefCell::default(),
            queued: RefCell::default(),
            claimed_keys: RefCell::default(),
        })
    }

    /// GPUI's text input gained (`true`) or lost focus.
    pub fn set_text_input(&self, focused: bool) {
        self.text_input.set(focused);
        if !focused {
            // Whatever the context still sends belongs to an input GPUI no
            // longer focuses.
            self.composing.set(false);
            self.queued.borrow_mut().clear();
        }
        self.focus_later();
    }

    /// The Tauri window gained (`true`) or lost keyboard focus.
    pub fn set_active(&self, active: bool) {
        self.active.set(active);
        self.focus_later();
    }

    /// Tells the context about focus once GPUI is idle: focus changes can
    /// make it emit (an input method may commit its composition on focus
    /// out), and they are reported from inside GPUI updates.
    fn focus_later(&self) {
        let Some(inner) = self.window.upgrade() else {
            return;
        };
        let window = self.window.clone();
        (inner.defer)(Box::new(move || {
            if let Some(inner) = window.upgrade()
                && let Some(ime) = &inner.ime
            {
                ime.sync_focus();
            }
        }));
    }

    fn sync_focus(&self) {
        let focused = self.text_input.get() && self.active.get();
        if self.focused.replace(focused) == focused {
            return;
        }
        if focused {
            self.context.focus_in();
        } else {
            self.context.focus_out();
            self.claimed_keys.borrow_mut().clear();
        }
    }

    /// Places the candidate window at `bounds` (logical, window-relative,
    /// which is GDK's coordinate space for the client window).
    pub fn set_cursor(&self, bounds: Bounds<Pixels>) {
        let [left, top, right, bottom] = pixel_rect(bounds, 1.);
        self.context.set_cursor_location(&gdk::Rectangle::new(
            left,
            top,
            right - left,
            bottom - top,
        ));
    }

    /// Applies events that arrived while GPUI was busy. Call with GPUI idle.
    pub fn flush(&self, inner: &WindowInner) {
        let events = std::mem::take(&mut *self.queued.borrow_mut());
        self.apply(inner, events);
    }

    /// Detaches from the GTK window when the GPUI window goes away.
    pub fn shutdown(&self) {
        if self.focused.replace(false) {
            self.context.focus_out();
        }
        self.text_input.set(false);
        self.queued.borrow_mut().clear();
        self.context.set_client_window(None::<&gdk::Window>);
    }

    /// Offers a key event to the context. Call with GPUI idle.
    fn key(&self, inner: &WindowInner, key: &gdk::EventKey, pressed: bool) -> glib::Propagation {
        let composing = self.composing.get();
        self.filtering.set(true);
        let consumed = self.context.filter_keypress(key);
        self.filtering.set(false);
        let events = std::mem::take(&mut *self.captured.borrow_mut());
        let code = key.hardware_keycode();
        // TAO turns modifier keys into `ModifiersChanged`; GPUI must always
        // see those, whatever the input method makes of them.
        let stop = if key.is_modifier() {
            self.apply(inner, events);
            false
        } else if !pressed {
            let mut claimed = self.claimed_keys.borrow_mut();
            let position = claimed.iter().position(|&c| c == code);
            if let Some(position) = position {
                claimed.swap_remove(position);
            }
            drop(claimed);
            self.apply(inner, events);
            position.is_some()
        } else {
            match route_key(consumed, &events, key.keyval().to_unicode(), composing) {
                KeyRoute::Key => {
                    self.apply(inner, events);
                    false
                }
                KeyRoute::KeyInsteadOfCommit => false,
                KeyRoute::Ime => {
                    self.claimed_keys.borrow_mut().push(code);
                    self.apply(inner, events);
                    true
                }
            }
        };
        if stop {
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    }

    /// Hands events to GPUI's focused input handler.
    fn apply(&self, inner: &WindowInner, events: Vec<ImeEvent>) {
        if events.is_empty() || !self.text_input.get() {
            return;
        }
        let mut cursor = None;
        for event in events {
            match event {
                ImeEvent::Commit(text) => {
                    self.composing.set(false);
                    inner.insert_text(&text);
                }
                ImeEvent::Preedit { text, cursor: at } if !text.is_empty() => {
                    self.composing.set(true);
                    cursor = inner
                        .input_handler(|handler| {
                            handler.replace_and_mark_text_in_range(None, &text, Some(at..at));
                            handler.ime_candidate_bounds()
                        })
                        .flatten();
                }
                ImeEvent::Preedit { .. } | ImeEvent::End => {
                    // An empty preedit removes the marked text; `End`
                    // keeps whatever a commit left.
                    let remove = matches!(event, ImeEvent::Preedit { .. });
                    self.composing.set(false);
                    inner.input_handler(|handler| {
                        if remove && let Some(marked) = handler.marked_text_range() {
                            handler.replace_text_in_range(Some(marked), "");
                        }
                        handler.unmark_text();
                    });
                }
            }
        }
        if let Some(bounds) = cursor {
            self.set_cursor(bounds);
        }
    }
}

/// Records something the context emitted.
fn emit(inner: &Weak<WindowInner>, event: ImeEvent) {
    let Some(inner) = inner.upgrade() else { return };
    let Some(ime) = &inner.ime else { return };
    if ime.filtering.get() {
        ime.captured.borrow_mut().push(event);
        return;
    }
    if !ime.text_input.get() {
        return;
    }
    ime.queued.borrow_mut().push(event);
    if runtime::input(|| ime.flush(&inner)).is_none() {
        // GPUI is mid-update: the drain applies it.
        inner.waker.wake();
    }
}

/// The toplevel's `event` signal: offers key events to the context while a
/// GPUI text input has focus.
fn key_event(inner: &Weak<WindowInner>, event: &gdk::Event) -> glib::Propagation {
    let pressed = match event.event_type() {
        gdk::EventType::KeyPress => true,
        gdk::EventType::KeyRelease => false,
        _ => return glib::Propagation::Proceed,
    };
    let Some(inner) = inner.upgrade() else {
        return glib::Propagation::Proceed;
    };
    let (Some(ime), Some(key)) = (&inner.ime, event.downcast_ref::<gdk::EventKey>()) else {
        return glib::Propagation::Proceed;
    };
    if !ime.focused.get() {
        return glib::Propagation::Proceed;
    }
    // GPUI must be idle to take what the context makes of the key; if it is
    // not, TAO delivers the key as it would without an input method.
    runtime::input(|| ime.key(&inner, key, pressed)).unwrap_or(glib::Propagation::Proceed)
}
