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

use super::dispatcher::LoopWaker;

/// Native handles of a Tauri window, captured once at attach time so the
/// renderer never has to round-trip through the Tauri runtime for them.
#[derive(Clone, Debug)]
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
}

#[derive(Default)]
pub(crate) struct ClickState {
    pub button: Option<MouseButton>,
    pub at: Option<Instant>,
    pub position: Point<Pixels>,
    pub count: usize,
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
    /// Text inserted from the last key press, used to drop the duplicate
    /// commit some TAO backends send through `ReceivedImeText`.
    pub last_key_text: Option<String>,
}

/// State shared between the GPUI-owned `PlatformWindow` and the plugin
/// runtime that feeds it TAO events.
pub(crate) struct WindowInner {
    pub label: String,
    pub tauri_window: tauri::Window<Wry>,
    pub raw: RawWindow,
    pub state: RefCell<WindowState>,
    pub callbacks: RefCell<Callbacks>,
    pub frame_pending: Cell<bool>,
    pub force_present: Cell<bool>,
    pub closed: Cell<bool>,
    pub gpui_handle: Cell<Option<AnyWindowHandle>>,
    pub display: Rc<dyn PlatformDisplay>,
    pub waker: Arc<LoopWaker>,
    /// Actions that must run outside of any GPUI update (see `Runtime::defer`).
    pub defer: Rc<dyn Fn(Box<dyn FnOnce()>)>,
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
        let callback = self.callbacks.borrow_mut().input.take();
        let mut handled = false;
        if let Some(mut callback) = callback {
            let result = callback(input.clone());
            restore(&mut self.callbacks.borrow_mut().input, callback);
            handled = !result.propagate;
        }
        if let PlatformInput::KeyDown(event) = &input {
            let key_char = event.keystroke.key_char.clone();
            if !handled
                && event.keystroke.modifiers.is_subset_of(&Modifiers::shift())
                && let Some(text) = &key_char
            {
                self.insert_text(text);
            }
            // Either we inserted it or GPUI consumed the key: in both cases a
            // duplicate IME commit of the same text must be ignored.
            self.state.borrow_mut().last_key_text = key_char;
        }
        handled
    }

    pub fn handle_ime_commit(&self, text: &str) {
        let duplicate = {
            let mut state = self.state.borrow_mut();
            state.last_key_text.take().as_deref() == Some(text)
        };
        if !duplicate {
            self.insert_text(text);
        }
    }

    fn insert_text(&self, text: &str) {
        let handler = self.state.borrow_mut().input_handler.take();
        if let Some(mut handler) = handler {
            handler.replace_text_in_range(None, text);
            let mut state = self.state.borrow_mut();
            if state.input_handler.is_none() {
                state.input_handler = Some(handler);
            }
        }
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

    /// Re-reads window modes TAO reports no dedicated event for. Maximizing
    /// or entering fullscreen always resizes, so this runs on resize/move.
    pub fn sync_window_mode(&self) {
        let maximized = self.tauri_window.is_maximized().unwrap_or(false);
        let fullscreen = self.tauri_window.is_fullscreen().unwrap_or(false);
        let mut state = self.state.borrow_mut();
        state.maximized = maximized;
        state.fullscreen = fullscreen;
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
        // native windows, so ask Tauri to close it rather than leaving a
        // window with no content renderer.
        if !self.inner.closed.get() {
            let window = self.inner.tauri_window.clone();
            (self.inner.defer)(Box::new(move || {
                if let Err(error) = window.close() {
                    log::warn!("failed to close Tauri window after GPUI removed it: {error}");
                }
            }));
        }
    }
}

impl HasWindowHandle for TauriGpuiWindow {
    fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
        // SAFETY: valid while the Tauri window is alive, which outlives this
        // object in every path except teardown (where GPUI no longer draws).
        Ok(unsafe { WindowHandle::borrow_raw(self.inner.raw.window) })
    }
}

impl HasDisplayHandle for TauriGpuiWindow {
    fn display_handle(&self) -> Result<DisplayHandle<'_>, HandleError> {
        // SAFETY: as above.
        Ok(unsafe { DisplayHandle::borrow_raw(self.inner.raw.display) })
    }
}

fn unsupported(operation: &str) {
    log::debug!("tauri-plugin-gpui: `{operation}` is not supported by the minimal GPUI adapter");
}

impl PlatformWindow for TauriGpuiWindow {
    fn bounds(&self) -> Bounds<Pixels> {
        Bounds::new(self.inner.state.borrow().origin, self.inner.logical_size())
    }

    fn is_maximized(&self) -> bool {
        self.inner.state.borrow().maximized
    }

    fn window_bounds(&self) -> WindowBounds {
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
        Some(self.inner.display.clone())
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
        if self.inner.state.borrow().visible {
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
        unsupported("set_background_appearance");
    }

    fn minimize(&self) {
        let window = self.inner.tauri_window.clone();
        (self.inner.defer)(Box::new(move || {
            let _ = window.minimize();
        }));
    }

    fn zoom(&self) {
        let window = self.inner.tauri_window.clone();
        (self.inner.defer)(Box::new(move || {
            let _ = if window.is_maximized().unwrap_or(false) {
                window.unmaximize()
            } else {
                window.maximize()
            };
        }));
    }

    fn toggle_fullscreen(&self) {
        let window = self.inner.tauri_window.clone();
        (self.inner.defer)(Box::new(move || {
            let fullscreen = window.is_fullscreen().unwrap_or(false);
            let _ = window.set_fullscreen(!fullscreen);
        }));
    }

    fn is_fullscreen(&self) -> bool {
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
        if renderer.device_lost() {
            log::warn!(
                "GPU device lost; recovering renderer for `{}`",
                self.inner.label
            );
            if let Err(error) = renderer.recover(&self.inner.raw) {
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
        let state = self.inner.state.borrow();
        let renderer = state
            .renderer
            .as_ref()
            .expect("sprite atlas requested after the window was destroyed");
        renderer.sprite_atlas().clone()
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

    fn update_ime_position(&self, _bounds: Bounds<Pixels>) {
        // Tauri exposes no IME candidate-window positioning API.
        unsupported("update_ime_position");
    }

    #[cfg(target_os = "windows")]
    fn get_raw_handle(&self) -> windows::Win32::Foundation::HWND {
        match self.inner.raw.window {
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
        last_key_text: None,
    }
}
