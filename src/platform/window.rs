//! `PlatformWindow` implementation that renders into an existing Tauri window.

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use gpui::{
    AnyWindowHandle, Bounds, Capslock, DevicePixels, DispatchEventResult, GpuSpecs, Modifiers,
    MouseButton, Pixels, PlatformAtlas, PlatformDisplay, PlatformInput, PlatformInputHandler,
    PlatformWindow, Point, PromptButton, PromptLevel, RequestFrameOptions, Scene, Size,
    WindowAppearance, WindowBackgroundAppearance, WindowBounds, WindowControlArea,
    WindowVisibility, px, size,
};
use gpui_wgpu::WgpuRenderer;
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle,
    RawWindowHandle, WindowHandle,
};
use tauri::Wry;

use super::{dispatcher::LoopWaker, typed_text::TypedText};

/// Native handles of a Tauri window, captured once at attach time so the
/// renderer never has to round-trip through the Tauri runtime for them.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RawWindow {
    pub window: RawWindowHandle,
    pub display: RawDisplayHandle,
}

// SAFETY: the handles are plain identifiers; they are only used on the main
// thread and the renderer that holds them is dropped when Tauri destroys the
// window.
unsafe impl Send for RawWindow {}
unsafe impl Sync for RawWindow {}

impl HasWindowHandle for RawWindow {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        // SAFETY: see above.
        Ok(unsafe { WindowHandle::borrow_raw(self.window) })
    }
}

impl HasDisplayHandle for RawWindow {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        // SAFETY: see above.
        Ok(unsafe { DisplayHandle::borrow_raw(self.display) })
    }
}

#[derive(Default)]
pub(crate) struct Callbacks {
    pub request_frame: Option<Box<dyn FnMut(RequestFrameOptions)>>,
    pub input: Option<Box<dyn FnMut(PlatformInput) -> DispatchEventResult>>,
    pub active_status_change: Option<Box<dyn FnMut(bool)>>,
    pub visibility_change: Option<Box<dyn FnMut(WindowVisibility)>>,
    pub hover_status_change: Option<Box<dyn FnMut(bool)>>,
    pub resize: Option<Box<dyn FnMut(Size<Pixels>, f32)>>,
    pub moved: Option<Box<dyn FnMut()>>,
    pub should_close: Option<Box<dyn FnMut() -> bool>>,
    pub close: Option<Box<dyn FnOnce()>>,
    pub hit_test_window_control: Option<Box<dyn FnMut() -> Option<WindowControlArea>>>,
    pub appearance_changed: Option<Box<dyn FnMut()>>,
    #[cfg(gpui_mobile)]
    pub insets_changed: Option<Box<dyn FnMut(gpui::WindowInsets)>>,
    #[cfg(gpui_android)]
    pub back: Option<Box<dyn FnMut()>>,
}

#[derive(Default)]
pub(crate) struct ClickState {
    pub button: Option<MouseButton>,
    pub at: Option<Instant>,
    pub position: Point<Pixels>,
    pub count: usize,
}

/// What GPUI has told the window about its focused text input; the Android
/// runtime reads it after the frame (see `runtime::android::input`).
#[cfg(gpui_android)]
#[derive(Default)]
pub(crate) struct TextInputSignals {
    pub focused: bool,
    /// The input's selection or content may have changed since the last look.
    pub dirty: bool,
}

pub(crate) struct WindowState {
    pub renderer: Option<WgpuRenderer>,
    pub physical_size: Size<DevicePixels>,
    pub scale_factor: f32,
    pub origin: Point<Pixels>,
    pub mouse_position: Point<Pixels>,
    pub modifiers: Modifiers,
    pub capslock: Capslock,
    pub pressed_button: Option<MouseButton>,
    pub click: ClickState,
    pub input_handler: Option<PlatformInputHandler>,
    pub active: bool,
    pub hovered: bool,
    pub visible: bool,
    pub appearance: WindowAppearance,
    pub fullscreen: bool,
    pub maximized: bool,
    pub typed_text: TypedText,
    /// System bar and soft keyboard regions (mobile only).
    #[cfg(gpui_mobile)]
    pub insets: gpui::WindowInsets,
    #[cfg(gpui_android)]
    pub text_input: TextInputSignals,
}

/// State shared between the GPUI-owned `PlatformWindow` and the plugin
/// runtime that feeds it TAO events.
pub(crate) struct WindowInner {
    pub label: String,
    pub tauri_window: tauri::Window<Wry>,
    /// Replaced when Android recreates the surface.
    pub raw: Cell<RawWindow>,
    pub state: RefCell<WindowState>,
    pub callbacks: RefCell<Callbacks>,
    pub frame_pending: Cell<bool>,
    pub force_present: Cell<bool>,
    /// Maximized/fullscreen/visible may have changed; TAO has no events for
    /// them, so they are re-read (lazily, at most once per change) from Tauri.
    pub mode_dirty: Cell<bool>,
    /// The window may be on another monitor.
    pub display_dirty: Cell<bool>,
    /// Visibility as last reported to GPUI.
    pub reported_visible: Cell<bool>,
    /// Kept outside the renderer so GPUI can still ask for it after the
    /// renderer is torn down.
    pub sprite_atlas: Arc<dyn PlatformAtlas>,
    pub closed: Cell<bool>,
    pub gpui_handle: Cell<Option<AnyWindowHandle>>,
    /// The monitor the window is on; refreshed with the window mode.
    pub display: RefCell<Option<Rc<dyn PlatformDisplay>>>,
    pub waker: Arc<LoopWaker>,
    /// Actions that must run outside of any GPUI update (see `Runtime::defer`).
    pub defer: Rc<dyn Fn(Box<dyn FnOnce()>)>,
    /// Child webviews placed by `webview_frame`, keyed by webview label.
    #[cfg(not(gpui_mobile))]
    pub webviews: RefCell<std::collections::HashMap<String, crate::webview::EmbeddedWebview>>,
    /// Files being dragged onto the window from other apps.
    #[cfg(not(gpui_mobile))]
    pub file_drop: RefCell<super::file_drop::FileDrop>,
    /// The system input method; `None` when GTK could not provide one.
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    pub ime: Option<super::ime::linux::Ime>,
}

const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(400);
const DOUBLE_CLICK_DISTANCE: f32 = 4.0;

impl WindowInner {
    pub fn logical_size(&self) -> Size<Pixels> {
        let state = self.state.borrow();
        logical(state.physical_size, state.scale_factor)
    }

    /// Asks the runtime to deliver a frame request on the next drain.
    pub fn schedule_frame(&self) {
        self.frame_pending.set(true);
        self.waker.wake();
    }

    /// Invokes GPUI's frame callback (layout → paint → present when dirty).
    pub fn request_frame(&self) {
        let force = self.force_present.take();
        let callback = self.callbacks.borrow_mut().request_frame.take();
        if let Some(mut callback) = callback {
            callback(RequestFrameOptions {
                require_presentation: force,
                force_render: force,
                ..Default::default()
            });
            restore(&mut self.callbacks.borrow_mut().request_frame, callback);
        }
    }

    pub fn handle_input(&self, input: PlatformInput) -> bool {
        let key_char = match &input {
            PlatformInput::KeyDown(event) => {
                let key_char = event.keystroke.key_char.clone();
                let earlier = self
                    .state
                    .borrow_mut()
                    .typed_text
                    .key_down(key_char.as_deref());
                if let Some(text) = earlier {
                    self.insert_text(&text);
                }
                Some(key_char)
            }
            _ => None,
        };
        let callback = self.callbacks.borrow_mut().input.take();
        let mut handled = false;
        if let Some(mut callback) = callback {
            let result = callback(input);
            restore(&mut self.callbacks.borrow_mut().input, callback);
            handled = !result.propagate;
        }
        if let Some(key_char) = key_char {
            // `key_char` is only set for keys that type text (see `keystroke`).
            if !handled && let Some(text) = &key_char {
                self.insert_text(text);
            }
            // Either we inserted it or GPUI consumed the key: in both cases a
            // duplicate IME commit of the same text must be ignored.
            self.state.borrow_mut().typed_text.key_dispatched(key_char);
        }
        handled
    }

    pub fn handle_ime_commit(&self, text: &str) {
        let earlier = self.state.borrow_mut().typed_text.commit(text);
        if let Some(text) = earlier {
            self.insert_text(&text);
        }
        if self.state.borrow().typed_text.has_pending() {
            // Inserted on the next drain unless a key press claims it first.
            self.waker.wake();
        }
    }

    /// Hands GPUI the file drag steps gathered since the last drain.
    #[cfg(not(gpui_mobile))]
    pub fn flush_file_drop(&self) {
        use super::file_drop::DropStep;
        use gpui::{ExternalPaths, FileDropEvent};

        let (tao_steps, queued) = {
            let mut file_drop = self.file_drop.borrow_mut();
            let tao_steps = file_drop.has_tao_events().then(|| file_drop.take_tao());
            (tao_steps, file_drop.take_queued())
        };
        let mut steps: Vec<_> = queued.into_iter().collect();
        if let Some(tao_steps) = tao_steps {
            // TAO's drag events carry no position: use the pointer's.
            let position = self.cursor_position();
            steps.extend(tao_steps.into_iter().map(|step| (step, position)));
        }
        for (step, position) in steps {
            self.state.borrow_mut().mouse_position = position;
            let event = match step {
                DropStep::Entered(paths) => FileDropEvent::Entered {
                    position,
                    paths: ExternalPaths(paths.into()),
                },
                DropStep::Pending => FileDropEvent::Pending { position },
                DropStep::Submit => FileDropEvent::Submit { position },
                DropStep::Exited => FileDropEvent::Exited,
            };
            self.handle_input(PlatformInput::FileDrop(event));
        }
    }

    /// The pointer in window coordinates, asked of the OS (it moves without
    /// window events while another app drags files over the window).
    #[cfg(not(gpui_mobile))]
    fn cursor_position(&self) -> Point<Pixels> {
        let window = &self.tauri_window;
        let queried = (|| {
            let cursor = window.cursor_position().ok()?;
            let origin = window.inner_position().ok()?;
            let scale = self.state.borrow().scale_factor as f64;
            Some(gpui::point(
                px(((cursor.x - origin.x as f64) / scale) as f32),
                px(((cursor.y - origin.y as f64) / scale) as f32),
            ))
        })();
        queried.unwrap_or_else(|| self.state.borrow().mouse_position)
    }

    /// Inserts an IME commit no key press claimed.
    pub fn flush_ime_commit(&self) {
        let text = self.state.borrow_mut().typed_text.flush();
        if let Some(text) = text {
            self.insert_text(&text);
        }
    }

    pub fn insert_text(&self, text: &str) {
        let handler = self.state.borrow_mut().input_handler.take();
        if let Some(mut handler) = handler {
            handler.replace_text_in_range(None, text);
            let mut state = self.state.borrow_mut();
            if state.input_handler.is_none() {
                state.input_handler = Some(handler);
            }
        }
    }

    /// Runs `f` with GPUI's input handler for the focused text input, if any.
    #[cfg(any(gpui_mobile, test))]
    #[cfg_attr(not(gpui_mobile), allow(dead_code))]
    pub fn with_input_handler(&self, f: impl FnOnce(&mut PlatformInputHandler)) {
        self.input_handler(f);
    }

    /// Like [`Self::with_input_handler`], returning `f`'s result, or `None`
    /// when no text input has focus.
    #[cfg(any(gpui_mobile, test, target_os = "linux", target_os = "freebsd"))]
    #[cfg_attr(
        not(any(gpui_mobile, target_os = "linux", target_os = "freebsd")),
        allow(dead_code)
    )]
    pub fn input_handler<R>(&self, f: impl FnOnce(&mut PlatformInputHandler) -> R) -> Option<R> {
        let mut handler = self.state.borrow_mut().input_handler.take()?;
        let result = f(&mut handler);
        let mut state = self.state.borrow_mut();
        if state.input_handler.is_none() {
            state.input_handler = Some(handler);
        }
        Some(result)
    }

    /// Notes that the text input's focus (`Some`) or selection and content
    /// changed, and wakes the loop so the Android runtime looks at it after
    /// the frame. GPUI may hold the input handler right now, so nothing is
    /// read here.
    #[cfg(gpui_android)]
    pub fn text_input_changed(&self, focused: Option<bool>) {
        {
            let mut state = self.state.borrow_mut();
            if let Some(focused) = focused {
                state.text_input.focused = focused;
            }
            state.text_input.dirty = true;
        }
        self.waker.wake();
    }

    /// Records a visibility change (mobile apps moving to and from the
    /// background) and tells GPUI.
    #[cfg(gpui_mobile)]
    pub fn set_visible(&self, visible: bool) {
        self.state.borrow_mut().visible = visible;
        self.report_visibility();
    }

    /// Records new system bar and keyboard insets and tells GPUI.
    #[cfg(gpui_mobile)]
    pub fn set_insets(&self, insets: gpui::WindowInsets) {
        self.state.borrow_mut().insets = insets.clone();
        let callback = self.callbacks.borrow_mut().insets_changed.take();
        if let Some(mut callback) = callback {
            callback(insets);
            restore(&mut self.callbacks.borrow_mut().insets_changed, callback);
        }
        self.schedule_frame();
    }

    /// Returns the click count for a mouse-down at `position`.
    pub fn register_click(&self, button: MouseButton, position: Point<Pixels>) -> usize {
        let mut state = self.state.borrow_mut();
        let now = Instant::now();
        let click = &mut state.click;
        let close = (click.position.x - position.x).abs() <= px(DOUBLE_CLICK_DISTANCE)
            && (click.position.y - position.y).abs() <= px(DOUBLE_CLICK_DISTANCE);
        let recent = click
            .at
            .is_some_and(|at| now.duration_since(at) <= DOUBLE_CLICK_INTERVAL);
        click.count = if click.button == Some(button) && close && recent {
            click.count + 1
        } else {
            1
        };
        click.button = Some(button);
        click.at = Some(now);
        click.position = position;
        click.count
    }

    /// Marks maximized/fullscreen/visibility and the monitor for a re-read.
    pub fn window_mode_changed(&self) {
        self.mode_dirty.set(true);
        self.window_moved();
    }

    /// Marks the monitor for a re-read: moving changes nothing else.
    pub fn window_moved(&self) {
        self.display_dirty.set(true);
        self.waker.wake();
    }

    /// Re-reads window modes TAO reports no dedicated event for, if an event
    /// that can change them arrived since the last read. Each read is a
    /// synchronous round trip through the Tauri runtime, so resize and move
    /// storms are coalesced into one read.
    pub fn sync_window_mode(&self) {
        let window = &self.tauri_window;
        if self.display_dirty.take() {
            let display = window
                .current_monitor()
                .ok()
                .flatten()
                .map(|monitor| super::TauriDisplay::from_monitor(&monitor));
            *self.display.borrow_mut() = display;
        }
        if !self.mode_dirty.take() {
            return;
        }
        let maximized = window.is_maximized().unwrap_or(false);
        let fullscreen = window.is_fullscreen().unwrap_or(false);
        let mut state = self.state.borrow_mut();
        state.maximized = maximized;
        state.fullscreen = fullscreen;
        // Hidden (`hide()`) windows are not tracked: TAO reports nothing when
        // they are shown again. Minimizing and restoring always do. Mobile
        // visibility follows the app lifecycle instead (`set_visible`).
        #[cfg(not(gpui_mobile))]
        {
            state.visible = !window.is_minimized().unwrap_or(false);
        }
    }

    /// Applies a pending window-mode change and tells GPUI when visibility
    /// changed since it was last told.
    pub fn refresh_window_mode(&self) {
        let display_id = |inner: &Self| inner.display.borrow().as_ref().map(|d| d.id());
        let old_display = display_id(self);
        self.sync_window_mode();
        if display_id(self) != old_display {
            // GPUI re-reads the window's display on bounds changes.
            self.call_unit(|c| &mut c.moved);
        }
        self.report_visibility();
    }

    /// Tells GPUI when visibility changed since it was last told.
    fn report_visibility(&self) {
        let visible = self.state.borrow().visible;
        if self.reported_visible.replace(visible) != visible {
            let visibility = if visible {
                WindowVisibility::Visible
            } else {
                WindowVisibility::Hidden
            };
            let callback = self.callbacks.borrow_mut().visibility_change.take();
            if let Some(mut callback) = callback {
                callback(visibility);
                restore(&mut self.callbacks.borrow_mut().visibility_change, callback);
            }
            if visibility == WindowVisibility::Visible {
                self.force_present.set(true);
                self.schedule_frame();
            }
        }
    }

    pub fn resized(&self, physical: Size<DevicePixels>, scale_factor: f32) {
        {
            let mut state = self.state.borrow_mut();
            if state.physical_size == physical && state.scale_factor == scale_factor {
                return;
            }
            state.physical_size = physical;
            state.scale_factor = scale_factor;
            if let Some(renderer) = state.renderer.as_mut() {
                renderer.update_drawable_size(physical);
            }
        }
        let callback = self.callbacks.borrow_mut().resize.take();
        if let Some(mut callback) = callback {
            callback(logical(physical, scale_factor), scale_factor);
            restore(&mut self.callbacks.borrow_mut().resize, callback);
        }
        self.force_present.set(true);
        self.schedule_frame();
    }

    /// Asks GPUI whether the window may close (`Window::on_window_should_close`).
    pub fn should_close(&self) -> bool {
        let callback = self.callbacks.borrow_mut().should_close.take();
        let Some(mut callback) = callback else {
            return true;
        };
        let allowed = callback();
        restore(&mut self.callbacks.borrow_mut().should_close, callback);
        allowed
    }

    pub fn call_bool(
        &self,
        which: fn(&mut Callbacks) -> &mut Option<Box<dyn FnMut(bool)>>,
        value: bool,
    ) {
        let callback = which(&mut self.callbacks.borrow_mut()).take();
        if let Some(mut callback) = callback {
            callback(value);
            restore(which(&mut self.callbacks.borrow_mut()), callback);
        }
    }

    pub fn call_unit(&self, which: fn(&mut Callbacks) -> &mut Option<Box<dyn FnMut()>>) {
        let callback = which(&mut self.callbacks.borrow_mut()).take();
        if let Some(mut callback) = callback {
            callback();
            restore(which(&mut self.callbacks.borrow_mut()), callback);
        }
    }

    /// Tears down the GPUI side after Tauri destroyed the native window.
    pub fn destroy(&self) {
        if self.closed.replace(true) {
            return;
        }
        let close = self.callbacks.borrow_mut().close.take();
        if let Some(close) = close {
            close();
        }
        if let Some(mut renderer) = self.state.borrow_mut().renderer.take() {
            renderer.destroy();
        }
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        if let Some(ime) = &self.ime {
            ime.shutdown();
        }
        // Break reference cycles between GPUI closures and this window.
        let callbacks = std::mem::take(&mut *self.callbacks.borrow_mut());
        let handler = self.state.borrow_mut().input_handler.take();
        drop((callbacks, handler));
    }
}

fn restore<T>(slot: &mut Option<T>, value: T) {
    if slot.is_none() {
        *slot = Some(value);
    }
}

pub(crate) fn logical(physical: Size<DevicePixels>, scale_factor: f32) -> Size<Pixels> {
    size(
        px(physical.width.0 as f32 / scale_factor),
        px(physical.height.0 as f32 / scale_factor),
    )
}

/// The `PlatformWindow` GPUI owns. Dropped by GPUI when its window is removed.
pub(crate) struct TauriGpuiWindow {
    pub inner: Rc<WindowInner>,
}

impl Drop for TauriGpuiWindow {
    fn drop(&mut self) {
        // GPUI removed the window (e.g. `window.remove_window()`). Tauri owns
        // native windows, so ask Tauri to destroy it rather than leaving a
        // window with no content renderer. Not `close()`: a Tauri
        // `prevent_close` handler would keep a blank window alive.
        if !self.inner.closed.get() {
            let window = self.inner.tauri_window.clone();
            (self.inner.defer)(Box::new(move || {
                if let Err(error) = window.destroy() {
                    log::warn!("failed to destroy Tauri window after GPUI removed it: {error}");
                }
            }));
        }
    }
}

impl HasWindowHandle for TauriGpuiWindow {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        // SAFETY: valid while the Tauri window is alive, which outlives this
        // object in every path except teardown (where GPUI no longer draws).
        Ok(unsafe { WindowHandle::borrow_raw(self.inner.raw.get().window) })
    }
}

impl HasDisplayHandle for TauriGpuiWindow {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        // SAFETY: as above.
        Ok(unsafe { DisplayHandle::borrow_raw(self.inner.raw.get().display) })
    }
}

impl PlatformWindow for TauriGpuiWindow {
    fn bounds(&self) -> Bounds<Pixels> {
        Bounds::new(self.inner.state.borrow().origin, self.inner.logical_size())
    }

    fn is_maximized(&self) -> bool {
        self.inner.sync_window_mode();
        self.inner.state.borrow().maximized
    }

    fn window_bounds(&self) -> WindowBounds {
        self.inner.sync_window_mode();
        let state = self.inner.state.borrow();
        let bounds = Bounds::new(
            state.origin,
            logical(state.physical_size, state.scale_factor),
        );
        if state.fullscreen {
            WindowBounds::Fullscreen(bounds)
        } else if state.maximized {
            WindowBounds::Maximized(bounds)
        } else {
            WindowBounds::Windowed(bounds)
        }
    }

    fn content_size(&self) -> Size<Pixels> {
        self.inner.logical_size()
    }

    fn resize(&mut self, size: Size<Pixels>) {
        let size =
            tauri::LogicalSize::new(f32::from(size.width) as f64, f32::from(size.height) as f64);
        if let Err(error) = self.inner.tauri_window.set_size(size) {
            log::warn!("failed to resize Tauri window: {error}");
        }
    }

    fn scale_factor(&self) -> f32 {
        self.inner.state.borrow().scale_factor
    }

    fn appearance(&self) -> WindowAppearance {
        self.inner.state.borrow().appearance
    }

    fn display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        // Not synced here: GPUI asks on every resize/move, and the drain
        // refreshes it (and re-notifies GPUI) once per event-loop iteration.
        self.inner.display.borrow().clone()
    }

    fn mouse_position(&self) -> Point<Pixels> {
        self.inner.state.borrow().mouse_position
    }

    fn modifiers(&self) -> Modifiers {
        self.inner.state.borrow().modifiers
    }

    fn capslock(&self) -> Capslock {
        self.inner.state.borrow().capslock
    }

    fn set_input_handler(&mut self, input_handler: PlatformInputHandler) {
        self.inner.state.borrow_mut().input_handler = Some(input_handler);
    }

    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
        self.inner.state.borrow_mut().input_handler.take()
    }

    fn prompt(
        &self,
        _level: PromptLevel,
        _msg: &str,
        _detail: Option<&str>,
        _answers: &[PromptButton],
    ) -> Option<futures::channel::oneshot::Receiver<usize>> {
        // `None` makes GPUI fall back to its own in-window prompt renderer.
        None
    }

    fn activate(&self) {
        let window = self.inner.tauri_window.clone();
        (self.inner.defer)(Box::new(move || {
            let _ = window.set_focus();
        }));
    }

    fn is_active(&self) -> bool {
        self.inner.state.borrow().active
    }

    fn visibility(&self) -> WindowVisibility {
        // What GPUI was told, so it never sees a change without its callback.
        if self.inner.reported_visible.get() {
            WindowVisibility::Visible
        } else {
            WindowVisibility::Hidden
        }
    }

    fn is_hovered(&self) -> bool {
        self.inner.state.borrow().hovered
    }

    fn background_appearance(&self) -> WindowBackgroundAppearance {
        WindowBackgroundAppearance::Opaque
    }

    fn set_title(&mut self, title: &str) {
        let window = self.inner.tauri_window.clone();
        let title = title.to_owned();
        (self.inner.defer)(Box::new(move || {
            let _ = window.set_title(&title);
        }));
    }

    fn set_background_appearance(&self, _background_appearance: WindowBackgroundAppearance) {
        super::log_unsupported("set_background_appearance");
    }

    fn minimize(&self) {
        // Mobile windows have no such states.
        #[cfg(not(gpui_mobile))]
        {
            let window = self.inner.tauri_window.clone();
            (self.inner.defer)(Box::new(move || {
                let _ = window.minimize();
            }));
        }
    }

    fn zoom(&self) {
        // Mobile windows have no such states.
        #[cfg(not(gpui_mobile))]
        {
            let window = self.inner.tauri_window.clone();
            (self.inner.defer)(Box::new(move || {
                let _ = if window.is_maximized().unwrap_or(false) {
                    window.unmaximize()
                } else {
                    window.maximize()
                };
            }));
        }
    }

    fn toggle_fullscreen(&self) {
        // Mobile windows have no such states.
        #[cfg(not(gpui_mobile))]
        {
            let window = self.inner.tauri_window.clone();
            (self.inner.defer)(Box::new(move || {
                let fullscreen = window.is_fullscreen().unwrap_or(false);
                let _ = window.set_fullscreen(!fullscreen);
            }));
        }
    }

    fn is_fullscreen(&self) -> bool {
        self.inner.sync_window_mode();
        self.inner.state.borrow().fullscreen
    }

    fn frame_waker(&self) -> Option<Rc<dyn Fn()>> {
        // Weak: the waker lives in GPUI's invalidator, which our callbacks
        // capture; a strong reference would leak the window.
        let inner = Rc::downgrade(&self.inner);
        Some(Rc::new(move || {
            if let Some(inner) = inner.upgrade() {
                inner.schedule_frame();
            }
        }))
    }

    fn schedule_frame(&self) {
        self.inner.schedule_frame();
    }

    fn on_request_frame(&self, callback: Box<dyn FnMut(RequestFrameOptions)>) {
        self.inner.callbacks.borrow_mut().request_frame = Some(callback);
    }

    fn on_input(&self, callback: Box<dyn FnMut(PlatformInput) -> DispatchEventResult>) {
        self.inner.callbacks.borrow_mut().input = Some(callback);
    }

    fn on_active_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.inner.callbacks.borrow_mut().active_status_change = Some(callback);
    }

    fn on_visibility_change(&self, callback: Box<dyn FnMut(WindowVisibility)>) {
        self.inner.callbacks.borrow_mut().visibility_change = Some(callback);
    }

    fn on_hover_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.inner.callbacks.borrow_mut().hover_status_change = Some(callback);
    }

    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) {
        self.inner.callbacks.borrow_mut().resize = Some(callback);
    }

    fn on_moved(&self, callback: Box<dyn FnMut()>) {
        self.inner.callbacks.borrow_mut().moved = Some(callback);
    }

    fn on_should_close(&self, callback: Box<dyn FnMut() -> bool>) {
        self.inner.callbacks.borrow_mut().should_close = Some(callback);
    }

    fn on_hit_test_window_control(&self, callback: Box<dyn FnMut() -> Option<WindowControlArea>>) {
        self.inner.callbacks.borrow_mut().hit_test_window_control = Some(callback);
    }

    fn on_close(&self, callback: Box<dyn FnOnce()>) {
        self.inner.callbacks.borrow_mut().close = Some(callback);
    }

    fn on_appearance_changed(&self, callback: Box<dyn FnMut()>) {
        self.inner.callbacks.borrow_mut().appearance_changed = Some(callback);
    }

    fn draw(&self, scene: &Scene) {
        let mut state = self.inner.state.borrow_mut();
        let Some(renderer) = state.renderer.as_mut() else {
            return;
        };
        renderer.draw(scene);
        #[cfg(not(gpui_mobile))]
        crate::webview::sync(&self.inner);
        if renderer.device_lost() {
            log::warn!(
                "GPU device lost; recovering renderer for `{}`",
                self.inner.label
            );
            if let Err(error) = renderer.recover(&self.inner.raw.get()) {
                log::error!("failed to recover GPUI renderer: {error:#}");
            }
            drop(state);
            self.inner.force_present.set(true);
            self.inner.schedule_frame();
        } else if renderer.needs_redraw() {
            drop(state);
            self.inner.force_present.set(true);
            self.inner.schedule_frame();
        }
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        self.inner.sprite_atlas.clone()
    }

    fn is_subpixel_rendering_supported(&self) -> bool {
        self.inner
            .state
            .borrow()
            .renderer
            .as_ref()
            .is_some_and(|r| r.supports_dual_source_blending())
    }

    fn gpu_specs(&self) -> Option<GpuSpecs> {
        self.inner
            .state
            .borrow()
            .renderer
            .as_ref()
            .and_then(|r| r.gpu_specs())
    }

    fn update_ime_position(&self, bounds: Bounds<Pixels>) {
        #[cfg(any(target_os = "linux", target_os = "freebsd"))]
        if let Some(ime) = &self.inner.ime {
            ime.set_cursor(bounds);
        }
        // Tauri exposes no IME candidate-window positioning API.
        #[cfg(not(any(target_os = "linux", target_os = "freebsd")))]
        {
            let _ = bounds;
            super::log_unsupported("update_ime_position");
        }
    }

    /// Routes keys through the system input method while a text input has
    /// focus (see `ime::linux`).
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    fn text_input_state_changed(&self, change: gpui::TextInputStateChange) {
        let Some(ime) = &self.inner.ime else { return };
        match change {
            gpui::TextInputStateChange::FocusGained => ime.set_text_input(true),
            gpui::TextInputStateChange::FocusLost => ime.set_text_input(false),
            _ => {}
        }
    }

    #[cfg(gpui_mobile)]
    fn insets(&self) -> gpui::WindowInsets {
        self.inner.state.borrow().insets.clone()
    }

    #[cfg(gpui_mobile)]
    fn on_insets_changed(&self, callback: Box<dyn FnMut(gpui::WindowInsets)>) {
        self.inner.callbacks.borrow_mut().insets_changed = Some(callback);
    }

    #[cfg(gpui_android)]
    fn set_back_handler(&self, callback: Box<dyn FnMut()>) {
        self.inner.callbacks.borrow_mut().back = Some(callback);
    }

    #[cfg(gpui_android)]
    fn set_back_enabled(&self, enabled: bool) {
        crate::android::set_back_enabled(enabled);
    }

    #[cfg(gpui_mobile)]
    fn show_soft_keyboard(&self) {
        crate::mobile::show_keyboard();
    }

    #[cfg(gpui_mobile)]
    fn hide_soft_keyboard(&self) {
        crate::mobile::hide_keyboard();
    }

    #[cfg(gpui_ios)]
    fn set_text_input_configuration(&mut self, configuration: gpui::TextInputConfiguration) {
        crate::ios::set_text_input_configuration(configuration);
    }

    #[cfg(gpui_mobile)]
    fn text_input_state_changed(&self, change: gpui::TextInputStateChange) {
        match change {
            gpui::TextInputStateChange::FocusGained => {
                crate::mobile::show_keyboard();
                #[cfg(gpui_android)]
                self.inner.text_input_changed(Some(true));
            }
            gpui::TextInputStateChange::FocusLost => {
                crate::mobile::hide_keyboard();
                #[cfg(gpui_android)]
                self.inner.text_input_changed(Some(false));
            }
            gpui::TextInputStateChange::SelectionChanged
            | gpui::TextInputStateChange::ContentChanged => {
                #[cfg(gpui_android)]
                self.inner.text_input_changed(None);
            }
        }
    }

    #[cfg(target_os = "windows")]
    fn get_raw_handle(&self) -> windows::Win32::Foundation::HWND {
        match self.inner.raw.get().window {
            RawWindowHandle::Win32(handle) => {
                windows::Win32::Foundation::HWND(handle.hwnd.get() as *mut core::ffi::c_void)
            }
            _ => unreachable!("Win32 window handle expected"),
        }
    }
}

pub(crate) fn default_window_state(
    physical_size: Size<DevicePixels>,
    scale_factor: f32,
    origin: Point<Pixels>,
    appearance: WindowAppearance,
    renderer: WgpuRenderer,
) -> WindowState {
    WindowState {
        renderer: Some(renderer),
        physical_size,
        scale_factor,
        origin,
        mouse_position: Point::default(),
        modifiers: Modifiers::default(),
        capslock: Capslock::default(),
        pressed_button: None,
        click: ClickState::default(),
        input_handler: None,
        active: false,
        hovered: false,
        visible: true,
        appearance,
        fullscreen: false,
        maximized: false,
        typed_text: TypedText::default(),
        #[cfg(gpui_mobile)]
        insets: gpui::WindowInsets::default(),
        #[cfg(gpui_android)]
        text_input: TextInputSignals::default(),
    }
}
