//! Pure logic behind the native selection toolbar, selection handles and
//! Autofill: converting GPUI bounds to view pixels and deciding when the
//! toolbar and the handles appear and where a dragged handle puts the caret.

use std::ops::Range;

use gpui::{Bounds, Pixels, Point, px};

/// A rectangle in view pixels, as `android.graphics.Rect` takes it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ViewRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl ViewRect {
    /// Converts logical `bounds` to view pixels, rounding outwards so the
    /// rectangle always covers the original.
    pub(crate) fn from_bounds(bounds: Bounds<Pixels>, scale: f32) -> Self {
        let px = |v: Pixels, round: fn(f32) -> f32| round(f32::from(v) * scale) as i32;
        Self {
            left: px(bounds.left(), f32::floor),
            top: px(bounds.top(), f32::floor),
            right: px(bounds.right(), f32::ceil),
            bottom: px(bounds.bottom(), f32::ceil),
        }
    }
}

/// Whether a long press at `point` (logical pixels) is on the input whose
/// selection or caret occupies `anchor`. The input's own bounds are not
/// known, so this accepts the anchor's row plus one anchor height above and
/// below, at any x: that covers a field's padding and a long press that
/// lands between lines.
pub(crate) fn long_press_hits(point: Point<Pixels>, anchor: Bounds<Pixels>) -> bool {
    let slack = anchor.size.height.max(px(8.));
    point.y >= anchor.top() - slack && point.y <= anchor.bottom() + slack
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ToolbarChange {
    Show { has_selection: bool },
    Hide,
    Keep,
}

/// What the toolbar does when the focused input's selection goes from `prev`
/// (`None` right after focus) to `now`. A long press always shows it, even on
/// a caret, so Paste and Autofill are reachable. Otherwise it follows the
/// selection: shown when text is selected, hidden when the selection
/// collapses. A toolbar opened on a caret stays until the user touches or
/// types, which the view handles itself.
pub(crate) fn toolbar_change(
    prev: Option<&Range<usize>>,
    now: &Range<usize>,
    long_press: bool,
) -> ToolbarChange {
    let had_selection = prev.is_some_and(|prev| !prev.is_empty());
    let has_selection = !now.is_empty();
    if long_press || (has_selection && prev != Some(now)) {
        ToolbarChange::Show { has_selection }
    } else if had_selection && !has_selection {
        ToolbarChange::Hide
    } else {
        ToolbarChange::Keep
    }
}

/// A native selection handle, numbered as `GpuiView.HANDLE_*` in Kotlin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Handle {
    /// The left handle, at the start of the selection.
    Start,
    /// The right handle, at the end of the selection.
    End,
    /// The drop under a collapsed caret.
    Insertion,
}

impl Handle {
    pub(crate) fn from_code(code: i32) -> Option<Self> {
        match code {
            0 => Some(Self::Start),
            1 => Some(Self::End),
            2 => Some(Self::Insertion),
            _ => None,
        }
    }
}

/// Where a handle drag is, numbered as `GpuiView.HANDLE_DRAG_*` in Kotlin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DragPhase {
    Started,
    Moved,
    /// Lifted or cancelled.
    Ended,
}

impl DragPhase {
    pub(crate) fn from_code(code: i32) -> Option<Self> {
        match code {
            0 => Some(Self::Started),
            1 => Some(Self::Moved),
            2 | 3 => Some(Self::Ended),
            _ => None,
        }
    }
}

/// The selection after `handle` is dragged to the character at `index`
/// (UTF-16, from `character_index_for_point`). Handles never cross: the start
/// handle stops at the end of the selection and the end handle at its start,
/// as on Android. `len` is the text length, when the input reports it.
pub(crate) fn drag_range(
    handle: Handle,
    index: usize,
    current: &Range<usize>,
    len: Option<usize>,
) -> Range<usize> {
    let index = len.map_or(index, |len| index.min(len));
    match handle {
        Handle::Start => index.min(current.end)..current.end,
        Handle::End => current.start..index.max(current.start),
        Handle::Insertion => index..index,
    }
}

/// Where the handles hang, in view pixels: the bottom-left of the selection
/// start's caret and the bottom of the end's, plus the line height `GpuiView`
/// uses to aim a drag at the text line above a handle.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct HandleAnchors {
    pub start_x: f32,
    pub start_y: f32,
    pub end_x: f32,
    pub end_y: f32,
    pub line_height: f32,
}

impl HandleAnchors {
    /// `start` and `end` are the bounds of the empty ranges at the selection's
    /// two ends, in logical pixels.
    pub(crate) fn from_bounds(start: Bounds<Pixels>, end: Bounds<Pixels>, scale: f32) -> Self {
        let px = |v: Pixels| f32::from(v) * scale;
        Self {
            start_x: px(start.left()),
            start_y: px(start.bottom()),
            end_x: px(end.right()),
            end_y: px(end.bottom()),
            line_height: px(start.size.height.max(end.size.height)),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum HandlesChange {
    /// Show or move the handles; `collapsed` is the insertion handle alone.
    Show {
        collapsed: bool,
    },
    Hide,
    Keep,
}

/// What the handles do when the focused input's selection goes from `prev`
/// (`None` right after focus) to `now`. They show whenever text is selected.
/// On a caret only a long press opens them (the insertion handle), and they
/// then follow the caret as the content changes and go away when it moves,
/// which is a tap or typing. While `dragging`, they always follow the
/// selection: dragging a selection handle onto the other one keeps both,
/// since the finger is still on one of them.
pub(crate) fn handles_change(
    prev: Option<&Range<usize>>,
    now: &Range<usize>,
    long_press: bool,
    dragging: Option<Handle>,
    shown: bool,
) -> HandlesChange {
    if !now.is_empty() {
        return HandlesChange::Show { collapsed: false };
    }
    if dragging.is_some() || long_press {
        let selecting = matches!(dragging, Some(Handle::Start | Handle::End));
        HandlesChange::Show {
            collapsed: !selecting,
        }
    } else if shown && prev == Some(now) {
        HandlesChange::Show { collapsed: true }
    } else if shown {
        HandlesChange::Hide
    } else {
        HandlesChange::Keep
    }
}

/// `change` adjusted for a handle drag: the toolbar stays away while a handle
/// is held and comes back where the selection ended up when it is released.
pub(crate) fn toolbar_for_drag(
    dragging: bool,
    ended: bool,
    now: &Range<usize>,
    change: ToolbarChange,
) -> ToolbarChange {
    if dragging {
        ToolbarChange::Keep
    } else if ended {
        ToolbarChange::Show {
            has_selection: !now.is_empty(),
        }
    } else {
        change
    }
}

#[cfg(test)]
mod tests {
    use gpui::{point, size};

    use super::*;

    fn bounds(x: f32, y: f32, w: f32, h: f32) -> Bounds<Pixels> {
        Bounds::new(point(px(x), px(y)), size(px(w), px(h)))
    }

    #[test]
    fn bounds_scale_to_view_pixels_rounding_outwards() {
        let rect = ViewRect::from_bounds(bounds(10.2, 20., 30., 10.), 2.5);
        assert_eq!(
            rect,
            ViewRect {
                left: 25,
                top: 50,
                right: 101,
                bottom: 75
            }
        );
    }

    #[test]
    fn long_press_must_be_near_the_caret_row() {
        let caret = bounds(50., 100., 0., 20.);
        assert!(long_press_hits(point(px(5.), px(110.)), caret));
        assert!(long_press_hits(point(px(300.), px(85.)), caret));
        assert!(!long_press_hits(point(px(50.), px(10.)), caret));
        assert!(!long_press_hits(point(px(50.), px(400.)), caret));
    }

    #[test]
    fn toolbar_follows_the_selection() {
        let caret = 3..3;
        let word = 0..5;
        let show = |has_selection| ToolbarChange::Show { has_selection };
        assert_eq!(toolbar_change(Some(&caret), &word, false), show(true));
        assert_eq!(toolbar_change(None, &word, false), show(true));
        assert_eq!(toolbar_change(Some(&word), &(0..7), false), show(true));
        assert_eq!(
            toolbar_change(Some(&word), &word, false),
            ToolbarChange::Keep
        );
        assert_eq!(
            toolbar_change(Some(&word), &caret, false),
            ToolbarChange::Hide
        );
        assert_eq!(
            toolbar_change(Some(&caret), &(4..4), false),
            ToolbarChange::Keep
        );
        assert_eq!(toolbar_change(None, &caret, false), ToolbarChange::Keep);
    }

    #[test]
    fn long_press_shows_the_toolbar_on_a_caret() {
        let caret = 3..3;
        assert_eq!(
            toolbar_change(Some(&caret), &caret, true),
            ToolbarChange::Show {
                has_selection: false
            }
        );
        assert_eq!(
            toolbar_change(Some(&(0..2)), &(0..2), true),
            ToolbarChange::Show {
                has_selection: true
            }
        );
    }

    #[test]
    fn handle_and_phase_codes() {
        assert_eq!(Handle::from_code(0), Some(Handle::Start));
        assert_eq!(Handle::from_code(1), Some(Handle::End));
        assert_eq!(Handle::from_code(2), Some(Handle::Insertion));
        assert_eq!(Handle::from_code(3), None);
        assert_eq!(DragPhase::from_code(0), Some(DragPhase::Started));
        assert_eq!(DragPhase::from_code(1), Some(DragPhase::Moved));
        assert_eq!(DragPhase::from_code(2), Some(DragPhase::Ended));
        assert_eq!(DragPhase::from_code(3), Some(DragPhase::Ended));
        assert_eq!(DragPhase::from_code(4), None);
    }

    #[test]
    fn dragging_a_handle_moves_its_end_of_the_selection() {
        let sel = 4..9;
        assert_eq!(drag_range(Handle::Start, 2, &sel, None), 2..9);
        assert_eq!(drag_range(Handle::Start, 6, &sel, None), 6..9);
        assert_eq!(drag_range(Handle::End, 12, &sel, None), 4..12);
        assert_eq!(drag_range(Handle::End, 5, &sel, None), 4..5);
        assert_eq!(drag_range(Handle::Insertion, 7, &(3..3), None), 7..7);
    }

    #[test]
    fn dragged_handles_do_not_cross() {
        let sel = 4..9;
        assert_eq!(drag_range(Handle::Start, 20, &sel, None), 9..9);
        assert_eq!(drag_range(Handle::End, 0, &sel, None), 4..4);
        // Stopped at the other handle, then moved back.
        assert_eq!(drag_range(Handle::Start, 6, &(9..9), None), 6..9);
    }

    #[test]
    fn dragged_handles_stay_inside_the_text() {
        assert_eq!(drag_range(Handle::End, 50, &(1..2), Some(10)), 1..10);
        assert_eq!(drag_range(Handle::Insertion, 50, &(1..1), Some(10)), 10..10);
        assert_eq!(drag_range(Handle::Start, 50, &(1..2), Some(10)), 2..2);
    }

    #[test]
    fn handle_anchors_are_the_caret_bottoms_in_view_pixels() {
        let anchors =
            HandleAnchors::from_bounds(bounds(10., 20., 0., 18.), bounds(50., 20., 0., 18.), 2.);
        assert_eq!(
            anchors,
            HandleAnchors {
                start_x: 20.,
                start_y: 76.,
                end_x: 100.,
                end_y: 76.,
                line_height: 36.,
            }
        );
        // A selection across lines: each end keeps its own line.
        let anchors =
            HandleAnchors::from_bounds(bounds(10., 20., 0., 18.), bounds(30., 38., 0., 20.), 1.);
        assert_eq!((anchors.start_y, anchors.end_y), (38., 58.));
        assert_eq!(anchors.line_height, 20.);
    }

    #[test]
    fn handles_follow_a_selection() {
        let show = |collapsed| HandlesChange::Show { collapsed };
        let caret = 3..3;
        assert_eq!(
            handles_change(Some(&caret), &(0..5), false, None, false),
            show(false)
        );
        assert_eq!(
            handles_change(None, &(0..5), false, None, false),
            show(false)
        );
        // Content changed under an unchanged selection: reposition.
        assert_eq!(
            handles_change(Some(&(0..5)), &(0..5), false, None, true),
            show(false)
        );
    }

    #[test]
    fn handles_open_on_a_caret_only_after_a_long_press() {
        let caret = 3..3;
        assert_eq!(
            handles_change(Some(&caret), &caret, true, None, false),
            HandlesChange::Show { collapsed: true }
        );
        assert_eq!(
            handles_change(None, &caret, false, None, false),
            HandlesChange::Keep
        );
        assert_eq!(
            handles_change(Some(&caret), &(4..4), false, None, false),
            HandlesChange::Keep
        );
    }

    #[test]
    fn handles_follow_or_leave_a_caret() {
        let caret = 3..3;
        // Same caret, new content: stay and reposition.
        assert_eq!(
            handles_change(Some(&caret), &caret, false, None, true),
            HandlesChange::Show { collapsed: true }
        );
        // Typing or a tap moved the caret, or collapsed a selection.
        assert_eq!(
            handles_change(Some(&caret), &(4..4), false, None, true),
            HandlesChange::Hide
        );
        assert_eq!(
            handles_change(Some(&(0..5)), &caret, false, None, true),
            HandlesChange::Hide
        );
    }

    #[test]
    fn handles_follow_a_drag_onto_a_caret() {
        let caret = 3..3;
        // The insertion handle follows the caret it drags.
        assert_eq!(
            handles_change(Some(&(2..2)), &caret, false, Some(Handle::Insertion), true),
            HandlesChange::Show { collapsed: true }
        );
        // A selection handle dragged onto the other one keeps both.
        assert_eq!(
            handles_change(Some(&(0..3)), &caret, false, Some(Handle::Start), true),
            HandlesChange::Show { collapsed: false }
        );
        assert_eq!(
            handles_change(Some(&(3..8)), &caret, false, Some(Handle::End), true),
            HandlesChange::Show { collapsed: false }
        );
    }

    #[test]
    fn toolbar_waits_for_the_end_of_a_drag() {
        let show = |has_selection| ToolbarChange::Show { has_selection };
        assert_eq!(
            toolbar_for_drag(true, false, &(0..4), show(true)),
            ToolbarChange::Keep
        );
        assert_eq!(
            toolbar_for_drag(false, true, &(0..4), ToolbarChange::Keep),
            show(true)
        );
        assert_eq!(
            toolbar_for_drag(false, true, &(4..4), ToolbarChange::Hide),
            show(false)
        );
        assert_eq!(
            toolbar_for_drag(false, false, &(4..4), ToolbarChange::Hide),
            ToolbarChange::Hide
        );
    }
}
