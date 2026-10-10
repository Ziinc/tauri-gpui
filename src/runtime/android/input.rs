//! Keeps the native selection toolbar, selection handles and Android Autofill
//! in step with the focused GPUI text input, and applies handle drags to it.
//!
//! GPUI draws its text inputs, so Android does not know they exist. After
//! each frame this looks at the window's input handler (never from inside
//! `text_input_state_changed`, where GPUI may be holding it): when the
//! selection moved, or focus, content or a long press asked for a look, it
//! reads the text and the caret rectangles and tells `GpuiView`.

use std::{
    ops::Range,
    time::{Duration, Instant},
};

use gpui::{Pixels, Point};

use super::Runtime;
use crate::{
    android::{
        self,
        selection::{
            DragPhase, Handle, HandleAnchors, HandlesChange, ToolbarChange, ViewRect, drag_range,
            handles_change, long_press_hits, toolbar_change, toolbar_for_drag,
        },
    },
    platform::window::WindowInner,
};

/// How long a long press waits for the input it landed on to take focus.
const LONG_PRESS_WINDOW: Duration = Duration::from_secs(1);

#[derive(Default)]
pub(super) struct InputSync {
    /// The view has been told about an input and must be told when it is gone.
    active: bool,
    selection: Option<Range<usize>>,
    /// The text and rectangle last given to Autofill.
    autofill: Option<(String, ViewRect)>,
    /// A long press (logical pixels) not yet matched to a focused input.
    long_press: Option<(Point<Pixels>, Instant)>,
    /// The view is showing selection handles.
    handles_shown: bool,
    /// The handle a finger is on.
    dragging: Option<Handle>,
    /// A drag ended since the toolbar was last updated.
    drag_ended: bool,
}

impl InputSync {
    pub(super) fn long_press(&mut self, position: Point<Pixels>) {
        self.long_press = Some((position, Instant::now()));
    }
}

impl Runtime {
    /// Runs at the end of every drain, after frames.
    pub(in crate::runtime) fn sync_text_input(&self) {
        let Some(inner) = self.attached() else { return };
        let (focused, dirty, scale) = {
            let mut state = inner.state.borrow_mut();
            (
                state.text_input.focused,
                std::mem::take(&mut state.text_input.dirty),
                state.scale_factor,
            )
        };
        let mut sync = self.android.input.borrow_mut();
        if sync
            .long_press
            .is_some_and(|(_, at)| at.elapsed() > LONG_PRESS_WINDOW)
        {
            sync.long_press = None;
        }
        if !focused {
            if sync.active {
                *sync = InputSync {
                    long_press: sync.long_press.take(),
                    ..InputSync::default()
                };
                android::hide_selection_toolbar();
                android::hide_selection_handles();
                android::clear_autofill_input();
            }
            return;
        }

        let _guard = self.enter();
        let mut selection = None;
        inner.with_input_handler(|handler| selection = handler.selected_text_range(true));
        let Some(selection) = selection else { return };
        let range = selection.range;
        // Moving the caret is the cheap common case: only read the text when
        // something may have changed it.
        if !dirty && sync.long_press.is_none() && sync.selection.as_ref() == Some(&range) {
            return;
        }
        let mut sample = None;
        inner.with_input_handler(|handler| {
            let mut adjusted = None;
            // The range is clamped to the content, as for any platform IME.
            let text = handler
                .text_for_range(0..usize::MAX, &mut adjusted)
                .unwrap_or_default();
            let start = handler.bounds_for_range(range.start..range.start);
            let end = handler.bounds_for_range(range.end..range.end);
            sample = handler
                .bounds_for_range(range.clone())
                .map(|bounds| (text, bounds, start, end));
        });
        // No layout yet: try again on the next change.
        let Some((text, bounds, start, end)) = sample else {
            return;
        };

        let long_press = sync
            .long_press
            .take()
            .is_some_and(|(position, _)| long_press_hits(position, bounds));
        let change = toolbar_change(sync.selection.as_ref(), &range, long_press);
        let dragging = sync.dragging;
        let change = toolbar_for_drag(
            dragging.is_some(),
            std::mem::take(&mut sync.drag_ended),
            &range,
            change,
        );
        let handles = handles_change(
            sync.selection.as_ref(),
            &range,
            long_press,
            dragging,
            sync.handles_shown,
        );
        sync.selection = Some(range);
        sync.active = true;

        let rect = ViewRect::from_bounds(bounds, scale);
        let current = (text, rect);
        if sync.autofill.as_ref() != Some(&current) {
            android::set_autofill_input(&current.0, rect);
            sync.autofill = Some(current);
        }
        match change {
            ToolbarChange::Show { has_selection } => {
                android::show_selection_toolbar(rect, has_selection)
            }
            ToolbarChange::Hide => android::hide_selection_toolbar(),
            ToolbarChange::Keep => {}
        }
        match handles {
            HandlesChange::Show { collapsed } => {
                // An input that cannot place its carets gets no handles.
                if let (Some(start), Some(end)) = (start, end) {
                    android::show_selection_handles(
                        HandleAnchors::from_bounds(start, end, scale),
                        collapsed,
                    );
                    sync.handles_shown = true;
                }
            }
            HandlesChange::Hide => {
                android::hide_selection_handles();
                sync.handles_shown = false;
            }
            HandlesChange::Keep => {}
        }
    }

    /// Moves the focused input's selection to where a dragged handle points
    /// (`position`, logical pixels). The view hides the toolbar while a handle
    /// is held; the next sync after the release shows it again.
    pub(in crate::runtime) fn handle_drag(
        &self,
        inner: &WindowInner,
        handle: Handle,
        phase: DragPhase,
        position: Point<Pixels>,
    ) {
        {
            let mut sync = self.android.input.borrow_mut();
            match phase {
                DragPhase::Started => {
                    sync.dragging = Some(handle);
                    sync.drag_ended = false;
                }
                DragPhase::Moved => {}
                DragPhase::Ended => {
                    sync.dragging = None;
                    sync.drag_ended = true;
                }
            }
        }
        if phase == DragPhase::Moved {
            inner.with_input_handler(|input| {
                let Some(current) = input.selected_text_range(true) else {
                    return;
                };
                let Some(index) = input.character_index_for_point(position) else {
                    return;
                };
                let range = drag_range(handle, index, &current.range, input.text_length_utf16());
                if range != current.range {
                    input.set_selected_text_range(range);
                }
            });
        }
        // Look at the selection again after the next frame.
        inner.text_input_changed(None);
    }
}
