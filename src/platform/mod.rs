//! Minimal GPUI `Platform` adapter hosted by Tauri.
//!
//! Only what rendering and interaction inside Tauri-owned windows needs is
//! implemented. Everything else is either delegated to Tauri (window
//! management, quitting) or explicitly reported as unsupported.

pub(crate) mod clipboard;
pub(crate) mod dispatcher;
pub(crate) mod window;

use std::{
    cell::{Cell, RefCell},
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};

use anyhow::{Result, anyhow};
use futures::channel::oneshot;
use gpui::{
    Action, ActivityGuard, AnyWindowHandle, BackgroundExecutor, Bounds, ClipboardItem, CursorStyle,
    DisplayId, DummyKeyboardMapper, ForegroundExecutor, Keymap, Menu, MenuItem, PathPromptOptions,
    Pixels, Platform, PlatformDisplay, PlatformKeyboardLayout, PlatformKeyboardMapper,
    PlatformTextSystem, PlatformWindow, Task, ThermalState, WindowAppearance, WindowKind,
    WindowParams, point, px, size,
};
use gpui_wgpu::GpuContext;
use tauri::{AppHandle, Wry};

use crate::GpuiError;
use dispatcher::TauriDispatcher;
use window::{TauriGpuiWindow, WindowInner};

thread_local! {
    /// The surface `Platform::open_window` hands to GPUI. Set only for the
    /// duration of an `attach_gpui` mount; any other `open_window` call is a
    /// request for a GPUI-owned native window, which is unsupported.
    pub(crate) static PENDING_SURFACE: RefCell<Option<Rc<WindowInner>>> = const { RefCell::new(None) };
}

#[derive(Debug)]
pub(crate) struct TauriDisplay {
    id: DisplayId,
    bounds: Bounds<Pixels>,
}

impl TauriDisplay {
    pub(crate) fn primary(app: &AppHandle<Wry>) -> Self {
        let bounds = app
            .primary_monitor()
            .ok()
            .flatten()
            .map(|monitor| {
                let scale = monitor.scale_factor();
                let position = monitor.position().to_logical::<f64>(scale);
                let monitor_size = monitor.size().to_logical::<f64>(scale);
                Bounds::new(
                    point(px(position.x as f32), px(position.y as f32)),
                    size(
                        px(monitor_size.width as f32),
                        px(monitor_size.height as f32),
                    ),
                )
            })
            .unwrap_or_else(|| Bounds::new(point(px(0.), px(0.)), size(px(1920.), px(1080.))));
        Self {
            id: DisplayId::new(1),
            bounds,
        }
    }
}

impl PlatformDisplay for TauriDisplay {
    fn id(&self) -> DisplayId {
        self.id
    }

    fn uuid(&self) -> Result<uuid::Uuid> {
        Err(anyhow!(GpuiError::UnsupportedOperation {
            operation: "PlatformDisplay::uuid"
        }))
    }

    fn bounds(&self) -> Bounds<Pixels> {
        self.bounds
    }
}

struct UsKeyboardLayout;

impl PlatformKeyboardLayout for UsKeyboardLayout {
    fn id(&self) -> &str {
        "us"
    }

    fn name(&self) -> &str {
        "US"
    }
}

#[derive(Default)]
struct PlatformCallbacks {
    quit: Option<Box<dyn FnMut() -> bool>>,
    reopen: Option<Box<dyn FnMut()>>,
    open_urls: Option<Box<dyn FnMut(Vec<String>)>>,
    app_menu_action: Option<Box<dyn FnMut(&dyn Action)>>,
    will_open_app_menu: Option<Box<dyn FnMut()>>,
    validate_app_menu_command: Option<Box<dyn FnMut(&dyn Action) -> bool>>,
    keyboard_layout_change: Option<Box<dyn FnMut()>>,
    thermal_state_change: Option<Box<dyn FnMut()>>,
}

pub(crate) struct TauriPlatform {
    app: AppHandle<Wry>,
    background_executor: BackgroundExecutor,
    foreground_executor: ForegroundExecutor,
    text_system: Arc<dyn PlatformTextSystem>,
    pub(crate) gpu_context: GpuContext,
    pub(crate) display: Rc<dyn PlatformDisplay>,
    pub(crate) active_window: Cell<Option<AnyWindowHandle>>,
    /// Label of the attached window under the pointer; cursor styles apply to it.
    pub(crate) hovered_window: RefCell<Option<tauri::Window<Wry>>>,
    cursor_style: Cell<Option<CursorStyle>>,
    callbacks: RefCell<PlatformCallbacks>,
    clipboard: clipboard::Clipboard,
}

impl TauriPlatform {
    pub(crate) fn new(
        app: AppHandle<Wry>,
        dispatcher: Arc<TauriDispatcher>,
        text_system: Arc<dyn PlatformTextSystem>,
    ) -> Self {
        let display: Rc<dyn PlatformDisplay> = Rc::new(TauriDisplay::primary(&app));
        Self {
            background_executor: BackgroundExecutor::new(dispatcher.clone()),
            foreground_executor: ForegroundExecutor::new(dispatcher),
            text_system,
            gpu_context: GpuContext::default(),
            display,
            active_window: Cell::new(None),
            hovered_window: RefCell::new(None),
            cursor_style: Cell::new(None),
            callbacks: RefCell::default(),
            clipboard: clipboard::Clipboard::default(),
            app,
        }
    }
}

fn unsupported<T>(operation: &'static str) -> Result<T> {
    Err(anyhow!(GpuiError::UnsupportedOperation { operation }))
}

fn unsupported_receiver<T>(operation: &'static str) -> oneshot::Receiver<Result<T>> {
    let (tx, rx) = oneshot::channel();
    tx.send(unsupported(operation)).ok();
    rx
}

fn log_unsupported(operation: &'static str) {
    log::debug!("tauri-plugin-gpui: `{operation}` is not supported by the minimal GPUI adapter");
}

fn cursor_icon(style: CursorStyle) -> tauri::CursorIcon {
    use tauri::CursorIcon as C;
    match style {
        CursorStyle::Arrow => C::Default,
        CursorStyle::IBeam => C::Text,
        CursorStyle::IBeamCursorForVerticalLayout => C::VerticalText,
        CursorStyle::Crosshair => C::Crosshair,
        CursorStyle::ClosedHand => C::Grabbing,
        CursorStyle::OpenHand => C::Grab,
        CursorStyle::PointingHand => C::Hand,
        CursorStyle::ResizeLeft => C::WResize,
        CursorStyle::ResizeRight => C::EResize,
        CursorStyle::ResizeLeftRight => C::EwResize,
        CursorStyle::ResizeUp => C::NResize,
        CursorStyle::ResizeDown => C::SResize,
        CursorStyle::ResizeUpDown => C::NsResize,
        CursorStyle::ResizeUpLeftDownRight => C::NwseResize,
        CursorStyle::ResizeUpRightDownLeft => C::NeswResize,
        CursorStyle::ResizeColumn => C::ColResize,
        CursorStyle::ResizeRow => C::RowResize,
        CursorStyle::OperationNotAllowed => C::NotAllowed,
        CursorStyle::DragLink => C::Alias,
        CursorStyle::DragCopy => C::Copy,
        CursorStyle::ContextualMenu => C::ContextMenu,
        #[allow(unreachable_patterns)]
        _ => C::Default,
    }
}

impl Platform for TauriPlatform {
    fn background_executor(&self) -> BackgroundExecutor {
        self.background_executor.clone()
    }

    fn foreground_executor(&self) -> ForegroundExecutor {
        self.foreground_executor.clone()
    }

    fn text_system(&self) -> Arc<dyn PlatformTextSystem> {
        self.text_system.clone()
    }

    /// Tauri already owns the running event loop: finish launching
    /// immediately and return (see `Application::run_embedded`).
    fn run(&self, on_finish_launching: Box<dyn 'static + FnOnce()>) {
        on_finish_launching();
    }

    fn quit(&self) {
        // Tauri owns the application lifecycle; route the request through it.
        self.app.exit(0);
    }

    fn restart(&self, _binary_path: Option<PathBuf>, _arguments: Vec<std::ffi::OsString>) {
        self.app.restart();
    }

    fn activate(&self, _ignoring_other_apps: bool) {}

    fn hide(&self) {
        log_unsupported("hide");
    }

    fn hide_other_apps(&self) {
        log_unsupported("hide_other_apps");
    }

    fn unhide_other_apps(&self) {
        log_unsupported("unhide_other_apps");
    }

    fn displays(&self) -> Vec<Rc<dyn PlatformDisplay>> {
        vec![self.display.clone()]
    }

    fn primary_display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        Some(self.display.clone())
    }

    fn active_window(&self) -> Option<AnyWindowHandle> {
        self.active_window.get()
    }

    fn open_window(
        &self,
        _handle: AnyWindowHandle,
        options: WindowParams,
    ) -> Result<Box<dyn PlatformWindow>> {
        let Some(inner) = PENDING_SURFACE.with(|pending| pending.borrow_mut().take()) else {
            return unsupported(
                "open_window (GPUI cannot create native windows; create a Tauri window and call attach_gpui)",
            );
        };
        if !matches!(options.kind, WindowKind::Normal) {
            return unsupported("open_window with a non-normal WindowKind");
        }
        Ok(Box::new(TauriGpuiWindow { inner }))
    }

    fn window_appearance(&self) -> WindowAppearance {
        WindowAppearance::Light
    }

    fn open_url(&self, url: &str) {
        log::warn!("tauri-plugin-gpui: open_url({url}) is unsupported; use tauri-plugin-opener");
    }

    fn on_open_urls(&self, callback: Box<dyn FnMut(Vec<String>)>) {
        self.callbacks.borrow_mut().open_urls = Some(callback);
    }

    fn register_url_scheme(&self, _url: &str) -> Task<Result<()>> {
        Task::ready(unsupported("register_url_scheme"))
    }

    fn prompt_for_paths(
        &self,
        _options: PathPromptOptions,
    ) -> oneshot::Receiver<Result<Option<Vec<PathBuf>>>> {
        unsupported_receiver("prompt_for_paths (use tauri-plugin-dialog)")
    }

    fn prompt_for_new_path(
        &self,
        _directory: &Path,
        _suggested_name: Option<&str>,
    ) -> oneshot::Receiver<Result<Option<PathBuf>>> {
        unsupported_receiver("prompt_for_new_path (use tauri-plugin-dialog)")
    }

    fn can_select_mixed_files_and_dirs(&self) -> bool {
        false
    }

    fn reveal_path(&self, _path: &Path) {
        log_unsupported("reveal_path");
    }

    fn open_with_system(&self, _path: &Path) {
        log_unsupported("open_with_system");
    }

    fn on_quit(&self, callback: Box<dyn FnMut() -> bool>) {
        self.callbacks.borrow_mut().quit = Some(callback);
    }

    fn on_reopen(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().reopen = Some(callback);
    }

    fn on_system_sleep(&self, _callback: Box<dyn FnMut()>) {}

    fn on_system_wake(&self, _callback: Box<dyn FnMut()>) {}

    fn set_menus(&self, _menus: Vec<Menu>, _keymap: &Keymap) {
        log_unsupported("set_menus (use Tauri's menu API)");
    }

    fn set_dock_menu(&self, _menu: Vec<MenuItem>, _keymap: &Keymap) {
        log_unsupported("set_dock_menu");
    }

    fn on_app_menu_action(&self, callback: Box<dyn FnMut(&dyn Action)>) {
        self.callbacks.borrow_mut().app_menu_action = Some(callback);
    }

    fn on_will_open_app_menu(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().will_open_app_menu = Some(callback);
    }

    fn on_validate_app_menu_command(&self, callback: Box<dyn FnMut(&dyn Action) -> bool>) {
        self.callbacks.borrow_mut().validate_app_menu_command = Some(callback);
    }

    fn thermal_state(&self) -> ThermalState {
        ThermalState::Nominal
    }

    fn on_thermal_state_change(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().thermal_state_change = Some(callback);
    }

    fn prevent_idle_sleep(&self, _reason: &str) -> Task<Result<ActivityGuard>> {
        Task::ready(unsupported("prevent_idle_sleep"))
    }

    fn compositor_name(&self) -> &'static str {
        "tauri"
    }

    fn app_path(&self) -> Result<PathBuf> {
        Ok(std::env::current_exe()?)
    }

    fn path_for_auxiliary_executable(&self, _name: &str) -> Result<PathBuf> {
        unsupported("path_for_auxiliary_executable")
    }

    fn set_cursor_style(&self, style: CursorStyle) {
        if self.cursor_style.replace(Some(style)) == Some(style) {
            return;
        }
        if let Some(window) = &*self.hovered_window.borrow() {
            let _ = window.set_cursor_icon(cursor_icon(style));
        }
    }

    fn hide_cursor_until_mouse_moves(&self) {
        log_unsupported("hide_cursor_until_mouse_moves");
    }

    fn is_cursor_visible(&self) -> bool {
        true
    }

    fn should_auto_hide_scrollbars(&self) -> bool {
        false
    }

    fn read_from_clipboard(&self) -> Option<ClipboardItem> {
        self.clipboard.read(clipboard::Kind::Clipboard)
    }

    fn write_to_clipboard(&self, item: ClipboardItem) {
        self.clipboard.write(clipboard::Kind::Clipboard, item);
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    fn read_from_primary(&self) -> Option<ClipboardItem> {
        self.clipboard.read(clipboard::Kind::Primary)
    }

    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    fn write_to_primary(&self, item: ClipboardItem) {
        self.clipboard.write(clipboard::Kind::Primary, item);
    }

    #[cfg(target_os = "macos")]
    fn read_from_find_pasteboard(&self) -> Option<ClipboardItem> {
        None
    }

    #[cfg(target_os = "macos")]
    fn write_to_find_pasteboard(&self, _item: ClipboardItem) {}

    fn write_credentials(&self, _url: &str, _username: &str, _password: &[u8]) -> Task<Result<()>> {
        Task::ready(unsupported("write_credentials"))
    }

    fn read_credentials(&self, _url: &str) -> Task<Result<Option<(String, Vec<u8>)>>> {
        Task::ready(unsupported("read_credentials"))
    }

    fn delete_credentials(&self, _url: &str) -> Task<Result<()>> {
        Task::ready(unsupported("delete_credentials"))
    }

    fn keyboard_layout(&self) -> Box<dyn PlatformKeyboardLayout> {
        Box::new(UsKeyboardLayout)
    }

    fn keyboard_mapper(&self) -> Rc<dyn PlatformKeyboardMapper> {
        Rc::new(DummyKeyboardMapper)
    }

    fn on_keyboard_layout_change(&self, callback: Box<dyn FnMut()>) {
        self.callbacks.borrow_mut().keyboard_layout_change = Some(callback);
    }
}
