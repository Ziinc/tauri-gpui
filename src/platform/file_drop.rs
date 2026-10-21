//! Files dragged onto a window from other apps.
//!
//! GPUI expects one `Entered` with every path, `Pending` moves, then `Submit`
//! or `Exited`. TAO reports a drag per file instead (`HoveredFile` for each
//! path, then `DroppedFile` for each) on macOS and Windows, with no position,
//! and nothing at all on Linux, where the window is a GTK drag destination
//! (see [`gtk`]). Both feed [`FileDrop`], which the runtime flushes into GPUI
//! once per event-loop iteration.

use std::{collections::VecDeque, path::PathBuf};

use gpui::{Pixels, Point};

/// One step of a drag, as GPUI sees it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum DropStep {
    Entered(Vec<PathBuf>),
    /// Only GTK reports moves while files hover.
    #[cfg_attr(not(any(target_os = "linux", target_os = "freebsd")), allow(dead_code))]
    Pending,
    Submit,
    Exited,
}

#[derive(Default)]
pub(crate) struct FileDrop {
    /// TAO's per-file events since the last flush.
    hovered: Vec<PathBuf>,
    dropped: Vec<PathBuf>,
    cancelled: bool,
    /// GPUI was sent `Entered` for the current drag.
    entered: bool,
    /// Steps with a known position (GTK).
    #[cfg_attr(not(any(target_os = "linux", target_os = "freebsd")), allow(dead_code))]
    queued: VecDeque<(DropStep, Point<Pixels>)>,
}

impl FileDrop {
    pub(crate) fn hovered(&mut self, path: PathBuf) {
        self.hovered.push(path);
    }

    pub(crate) fn dropped(&mut self, path: PathBuf) {
        self.dropped.push(path);
    }

    pub(crate) fn cancelled(&mut self) {
        self.cancelled = true;
    }

    #[cfg_attr(not(any(target_os = "linux", target_os = "freebsd")), allow(dead_code))]
    pub(crate) fn queue(&mut self, step: DropStep, position: Point<Pixels>) {
        self.queued.push_back((step, position));
    }

    /// Whether TAO reported anything since the last [`Self::take_tao`].
    pub(crate) fn has_tao_events(&self) -> bool {
        !self.hovered.is_empty() || !self.dropped.is_empty() || self.cancelled
    }

    /// GPUI's steps for TAO's events since the last call. All `HoveredFile`
    /// events of a drag arrive in one event-loop iteration, so they make one
    /// `Entered`.
    pub(crate) fn take_tao(&mut self) -> Vec<DropStep> {
        let mut steps = Vec::new();
        if !self.dropped.is_empty() {
            if !self.entered {
                let mut paths = std::mem::take(&mut self.hovered);
                for path in self.dropped.drain(..) {
                    if !paths.contains(&path) {
                        paths.push(path);
                    }
                }
                steps.push(DropStep::Entered(paths));
            }
            // GPUI drops on the mouse-up `Submit` turns into; `Exited` then
            // clears any drag a drop target left behind, as on macOS.
            steps.extend([DropStep::Submit, DropStep::Exited]);
            self.reset();
        } else if self.cancelled {
            if self.entered {
                steps.push(DropStep::Exited);
            }
            self.reset();
        } else if !self.hovered.is_empty() {
            let paths = std::mem::take(&mut self.hovered);
            if !self.entered {
                self.entered = true;
                steps.push(DropStep::Entered(paths));
            }
        }
        steps
    }

    pub(crate) fn take_queued(&mut self) -> VecDeque<(DropStep, Point<Pixels>)> {
        std::mem::take(&mut self.queued)
    }

    fn reset(&mut self) {
        self.hovered.clear();
        self.dropped.clear();
        self.cancelled = false;
        self.entered = false;
    }
}

/// The window as a GTK drag destination for `text/uri-list`.
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
pub(crate) mod gtk {
    use std::{cell::RefCell, rc::Weak};

    use gpui::{point, px};
    use gtk::{
        DestDefaults, TargetEntry, TargetFlags,
        gdk::{Atom, DragAction, DragContext},
        prelude::*,
    };

    use super::DropStep;
    use crate::platform::window::WindowInner;

    const URI_LIST: &str = "text/uri-list";

    /// Where the current drag is. GTK sends `drag-leave` right before
    /// `drag-drop`, so a leave only becomes `Exited` if no drop follows it
    /// before the event loop next runs.
    #[derive(Default)]
    struct Drag {
        requested: bool,
        paths: Option<Vec<std::path::PathBuf>>,
        entered: bool,
        dropping: bool,
        left: bool,
    }

    pub(crate) fn install(window: &tauri::Window<tauri::Wry>, inner: Weak<WindowInner>) {
        let gtk_window = match window.gtk_window() {
            Ok(window) => window,
            Err(error) => {
                log::warn!("tauri-plugin-gpui: no GTK window for file drops: {error}");
                return;
            }
        };
        let targets = [TargetEntry::new(URI_LIST, TargetFlags::OTHER_APP, 0)];
        gtk_window.drag_dest_set(DestDefaults::empty(), &targets, DragAction::COPY);
        let drag = std::rc::Rc::new(RefCell::new(Drag::default()));

        gtk_window.connect_drag_motion({
            let (inner, drag) = (inner.clone(), drag.clone());
            move |widget, context, x, y, time| {
                crate::ffi::callback("drag-motion", false, || {
                    let mut drag = drag.borrow_mut();
                    drag.left = false;
                    if !drag.requested {
                        drag.requested = true;
                        widget.drag_get_data(context, &Atom::intern(URI_LIST), time);
                    }
                    context.drag_status(DragAction::COPY, time);
                    if drag.entered {
                        send(&inner, DropStep::Pending, widget, x, y);
                    }
                    true
                })
            }
        });

        gtk_window.connect_drag_leave({
            let (inner, drag) = (inner.clone(), drag.clone());
            move |_, _, _| {
                drag.borrow_mut().left = true;
                // Checked once the current GTK dispatch has finished.
                let (inner, drag) = (inner.clone(), drag.clone());
                gtk::glib::idle_add_local_once(move || {
                    let left = {
                        let drag = drag.borrow();
                        drag.left && !drag.dropping
                    };
                    if left {
                        let entered = std::mem::take(&mut *drag.borrow_mut()).entered;
                        if entered && let Some(inner) = inner.upgrade() {
                            queue(&inner, DropStep::Exited, None);
                        }
                    }
                });
            }
        });

        gtk_window.connect_drag_drop({
            let (inner, drag) = (inner.clone(), drag.clone());
            move |widget, context, x, y, time| {
                crate::ffi::callback("drag-drop", false, || {
                    let mut state = drag.borrow_mut();
                    state.left = false;
                    state.dropping = true;
                    if state.paths.is_some() {
                        drop(state);
                        finish(&inner, &drag, widget, context, x, y, time);
                    } else if !state.requested {
                        state.requested = true;
                        widget.drag_get_data(context, &Atom::intern(URI_LIST), time);
                    }
                    true
                })
            }
        });

        gtk_window.connect_drag_data_received({
            let (inner, drag) = (inner.clone(), drag.clone());
            move |widget, context, x, y, data, _, time| {
                crate::ffi::callback("drag-data-received", (), || {
                    let paths: Vec<_> = data
                        .uris()
                        .iter()
                        .filter_map(|uri| gtk::glib::filename_from_uri(uri).ok())
                        .map(|(path, _)| path)
                        .collect();
                    let dropping = {
                        let mut state = drag.borrow_mut();
                        state.paths = Some(paths);
                        state.dropping
                    };
                    if dropping {
                        finish(&inner, &drag, widget, context, x, y, time);
                    } else {
                        enter(&inner, &drag, widget, x, y);
                    }
                })
            }
        });
    }

    fn enter(
        inner: &Weak<WindowInner>,
        drag: &RefCell<Drag>,
        widget: &gtk::ApplicationWindow,
        x: i32,
        y: i32,
    ) {
        let mut state = drag.borrow_mut();
        if state.entered {
            return;
        }
        let Some(paths) = state.paths.clone().filter(|paths| !paths.is_empty()) else {
            return;
        };
        state.entered = true;
        drop(state);
        send(inner, DropStep::Entered(paths), widget, x, y);
    }

    fn finish(
        inner: &Weak<WindowInner>,
        drag: &RefCell<Drag>,
        widget: &gtk::ApplicationWindow,
        context: &DragContext,
        x: i32,
        y: i32,
        time: u32,
    ) {
        enter(inner, drag, widget, x, y);
        let entered = std::mem::take(&mut *drag.borrow_mut()).entered;
        if entered {
            send(inner, DropStep::Submit, widget, x, y);
            send(inner, DropStep::Exited, widget, x, y);
        }
        context.drag_finish(entered, false, time);
    }

    /// Queues `step` at GTK's widget coordinates (logical pixels).
    fn send(
        inner: &Weak<WindowInner>,
        step: DropStep,
        widget: &gtk::ApplicationWindow,
        x: i32,
        y: i32,
    ) {
        let Some(inner) = inner.upgrade() else { return };
        // Toplevel coordinates in logical pixels, like TAO's cursor events.
        let _ = widget;
        let position = point(px(x as f32), px(y as f32)) - inner.toplevel_content_origin();
        queue(&inner, step, Some(position));
    }

    fn queue(inner: &WindowInner, step: DropStep, position: Option<gpui::Point<gpui::Pixels>>) {
        let position = position.unwrap_or_else(|| inner.state.borrow().mouse_position);
        inner.file_drop.borrow_mut().queue(step, position);
        inner.waker.wake();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn tao_hovers_make_one_entered() {
        let mut drop = FileDrop::default();
        drop.hovered("/a".into());
        drop.hovered("/b".into());
        assert_eq!(drop.take_tao(), [DropStep::Entered(paths(&["/a", "/b"]))]);
        // Hovering on (Windows repeats nothing, but be safe) adds nothing.
        drop.hovered("/a".into());
        assert_eq!(drop.take_tao(), []);
    }

    #[test]
    fn tao_drop_submits_once_for_every_file() {
        let mut drop = FileDrop::default();
        drop.hovered("/a".into());
        drop.hovered("/b".into());
        drop.take_tao();
        drop.dropped("/a".into());
        drop.dropped("/b".into());
        assert_eq!(drop.take_tao(), [DropStep::Submit, DropStep::Exited]);
        assert!(!drop.has_tao_events());
    }

    #[test]
    fn tao_drop_without_hover_still_enters() {
        let mut drop = FileDrop::default();
        drop.hovered("/a".into());
        drop.dropped("/a".into());
        drop.dropped("/b".into());
        assert_eq!(
            drop.take_tao(),
            [
                DropStep::Entered(paths(&["/a", "/b"])),
                DropStep::Submit,
                DropStep::Exited
            ]
        );
    }

    #[test]
    fn tao_cancel_exits_only_an_entered_drag() {
        let mut drop = FileDrop::default();
        drop.cancelled();
        assert_eq!(drop.take_tao(), []);
        drop.hovered("/a".into());
        drop.take_tao();
        drop.cancelled();
        assert_eq!(drop.take_tao(), [DropStep::Exited]);
        drop.hovered("/b".into());
        assert_eq!(drop.take_tao(), [DropStep::Entered(paths(&["/b"]))]);
    }
}
