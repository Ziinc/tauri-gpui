//! Pure logic behind the native selection toolbar and Autofill: converting
//! GPUI bounds to view pixels and deciding when the toolbar appears.

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
}
