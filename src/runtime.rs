//! The shared GPUI runtime: one GPUI `App` per Tauri application, living on
//! the Tauri/TAO event-loop thread, plus the registry of attached windows.

use std::{
    cell::{Cell, RefCell},
    collections::{HashMap, VecDeque},
    rc::Rc,
    sync::{Arc, Mutex},
    thread::{self, ThreadId},
    time::{Duration, Instant},
};

use gpui::{
    AnyWindowHandle, App, Application, ApplicationHandle, AsyncApp, Bounds, QuitMode, WindowBounds,
    WindowOptions, point, px,
};
use gpui_wgpu::{CosmicTextSystem, WgpuRenderer, WgpuSurfaceConfig};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use tauri::{AppHandle, EventLoopMessage, Wry};
use tauri_runtime_wry::{
    EventLoopIterationContext, Message, TaoWindowId, WindowMessage,
    tao::{
        event::{Event, WindowEvent},
        event_loop::EventLoopProxy,
    },
};

use crate::{
    GpuiConfig, GpuiError, GpuiOptions, events,
    platform::{
        PENDING_SURFACE, TauriPlatform,
        dispatcher::{LoopWaker, TauriDispatcher},
        window::{RawWindow, WindowInner, default_window_state, logical},
    },
};

/// Upper bound on how long one drain may spend running GPUI tasks before
/// yielding back to TAO so input and redraws stay responsive.
const TASK_BUDGET: Duration = Duration::from_millis(8);

type OpenWindow = Box<dyn FnOnce(&mut App, WindowOptions) -> anyhow::Result<AnyWindowHandle>>;

struct Mount {
    inner: Rc<WindowInner>,
    options: WindowOptions,
    open: OpenWindow,
}

pub(crate) struct Runtime {
    main_thread: ThreadId,
    /// Keeps the shared App alive; all access goes through `cx`.
    _app: ApplicationHandle,
    /// Unlike `ApplicationHandle::update`, `AsyncApp::update` runs a full
    /// GPUI update cycle, flushing the effects (`defer`, `notify`, `emit`,
    /// observers) the closure queued.
    cx: AsyncApp,
    platform: Rc<TauriPlatform>,
    dispatcher: Arc<TauriDispatcher>,
    waker: Arc<LoopWaker>,
    surfaces: RefCell<HashMap<String, Rc<WindowInner>>>,
    tao_labels: RefCell<HashMap<TaoWindowId, String>>,
    mounts: RefCell<VecDeque<Mount>>,
    deferred: Rc<RefCell<Vec<Box<dyn FnOnce()>>>>,
    depth: Cell<usize>,
    proxy_installed: Cell<bool>,
}

thread_local! {
    /// Leaked on purpose: the shared GPUI App lives until the process exits,
    /// and dropping GPU resources from TLS destructors (after other thread
    /// locals are gone) panics. Surfaces are torn down on `LoopDestroyed`.
    static RUNTIME: Cell<Option<&'static Runtime>> = const { Cell::new(None) };
}

/// Runs `f` with the runtime of the current (main) thread.
pub(crate) fn with<R>(
    f: impl FnOnce(&'static Runtime) -> Result<R, GpuiError>,
) -> Result<R, GpuiError> {
    let runtime = RUNTIME.try_with(Cell::get).ok().flatten();
    match runtime {
        Some(runtime) => f(runtime),
        None if INITIALIZED.lock().unwrap().is_some() => Err(GpuiError::NotMainThread),
        None => Err(GpuiError::NotInitialized),
    }
}

/// Thread the runtime was created on, for diagnosing off-thread calls.
static INITIALIZED: Mutex<Option<ThreadId>> = Mutex::new(None);

struct DepthGuard<'a>(&'a Cell<usize>);

impl Drop for DepthGuard<'_> {
    fn drop(&mut self) {
        self.0.set(self.0.get() - 1);
    }
}

pub(crate) fn initialize(app: AppHandle<Wry>, config: GpuiConfig) -> Result<(), GpuiError> {
    {
        let mut initialized = INITIALIZED.lock().unwrap();
        if initialized.is_some() {
            return Err(GpuiError::AlreadyInitialized);
        }
        *initialized = Some(thread::current().id());
    }

    // Until the TAO proxy is captured on the first event, wake the loop by
    // posting an empty task through Tauri.
    let waker = Arc::new(LoopWaker::new(Box::new({
        let app = app.clone();
        move || {
            let _ = app.run_on_main_thread(|| {});
        }
    })));
    let dispatcher = Arc::new(TauriDispatcher::new(waker.clone()));
    let text_system = Arc::new(CosmicTextSystem::new(
        config.font_fallback.as_deref().unwrap_or(default_font()),
    ));
    let platform = Rc::new(TauriPlatform::new(app, dispatcher.clone(), text_system));

    let mut application =
        Application::with_platform(platform.clone()).with_quit_mode(QuitMode::Explicit);
    if let Some(configure) = config.configure {
        application = configure(application);
    }

    let app = application.run_embedded(|_| {});
    let runtime = Box::leak(Box::new(Runtime {
        main_thread: thread::current().id(),
        cx: app.to_async(),
        _app: app,
        platform,
        dispatcher,
        waker,
        surfaces: RefCell::default(),
        tao_labels: RefCell::default(),
        mounts: RefCell::default(),
        deferred: Rc::default(),
        depth: Cell::new(0),
        proxy_installed: Cell::new(false),
    }));
    if let Some(on_launch) = config.on_launch {
        runtime.update(on_launch)?;
    }
    RUNTIME.with(|slot| slot.set(Some(runtime)));
    Ok(())
}

fn is_dummy(window_id: &TaoWindowId) -> bool {
    // SAFETY: the dummy id is only compared, never used to address a window.
    *window_id == unsafe { TaoWindowId::dummy() }
}

fn default_font() -> &'static str {
    if cfg!(target_os = "macos") {
        "Helvetica Neue"
    } else if cfg!(target_os = "windows") {
        "Segoe UI"
    } else {
        "DejaVu Sans"
    }
}

impl Runtime {
    fn enter(&self) -> DepthGuard<'_> {
        self.depth.set(self.depth.get() + 1);
        DepthGuard(&self.depth)
    }

    fn is_main_thread(&self) -> bool {
        thread::current().id() == self.main_thread
    }

    /// Borrows the shared GPUI `App`. Fails instead of panicking when the App
    /// is already borrowed further up the stack.
    pub(crate) fn update<R>(&self, f: impl FnOnce(&mut App) -> R) -> Result<R, GpuiError> {
        if !self.is_main_thread() {
            return Err(GpuiError::NotMainThread);
        }
        if self.depth.get() > 0 {
            return Err(GpuiError::Reentrant);
        }
        let _guard = self.enter();
        Ok(self.cx.update(f))
    }

    pub(crate) fn attach(
        &self,
        window: &tauri::Window<Wry>,
        options: GpuiOptions,
        open: OpenWindow,
    ) -> Result<(), GpuiError> {
        if !self.is_main_thread() {
            return Err(GpuiError::NotMainThread);
        }
        let label = window.label().to_string();
        if self.surfaces.borrow().contains_key(&label) {
            return Err(GpuiError::AlreadyAttached { label });
        }
        if !window.webviews().is_empty() {
            return Err(GpuiError::NotEligible {
                label,
                reason: "the window hosts a WebView; GPUI and WebView content cannot share a window",
            });
        }

        let scale_factor = window.scale_factor()? as f32;
        let physical = window.inner_size()?;
        let physical_size = events::physical_size(physical.width.max(1), physical.height.max(1));
        let origin = window
            .outer_position()
            .map(|p| {
                let p = p.to_logical::<f32>(scale_factor as f64);
                point(px(p.x), px(p.y))
            })
            .unwrap_or_default();
        let appearance = window
            .theme()
            .map(|theme| match theme {
                tauri::Theme::Dark => gpui::WindowAppearance::Dark,
                _ => gpui::WindowAppearance::Light,
            })
            .unwrap_or(gpui::WindowAppearance::Light);

        let raw = RawWindow {
            window: window
                .window_handle()
                .map_err(|e| GpuiError::PlatformInitialization(format!("window handle: {e}")))?
                .as_raw(),
            display: window
                .display_handle()
                .map_err(|e| GpuiError::PlatformInitialization(format!("display handle: {e}")))?
                .as_raw(),
        };
        let renderer = WgpuRenderer::new(
            self.platform.gpu_context.clone(),
            &raw,
            WgpuSurfaceConfig {
                size: physical_size,
                transparent: false,
                preferred_present_mode: None,
            },
            None,
        )
        .map_err(|e| GpuiError::RendererInitialization(format!("{e:#}")))?;

        let defer: Rc<dyn Fn(Box<dyn FnOnce()>)> = Rc::new({
            let deferred = self.deferred.clone();
            let waker = self.waker.clone();
            move |action| {
                deferred.borrow_mut().push(action);
                waker.wake();
            }
        });
        let inner = Rc::new(WindowInner {
            label: label.clone(),
            tauri_window: window.clone(),
            raw,
            state: RefCell::new(default_window_state(
                physical_size,
                scale_factor,
                origin,
                appearance,
                renderer,
            )),
            callbacks: RefCell::default(),
            frame_pending: Cell::new(false),
            force_present: Cell::new(false),
            closed: Cell::new(false),
            gpui_handle: Cell::new(None),
            display: self.platform.display.clone(),
            waker: self.waker.clone(),
            defer,
        });
        // The window may have been built maximized or fullscreen.
        inner.sync_window_mode();
        self.surfaces.borrow_mut().insert(label, inner.clone());

        let window_options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                origin,
                logical(physical_size, scale_factor),
            ))),
            titlebar: None,
            focus: options.focus,
            show: false,
            ..Default::default()
        };
        let mount = Mount {
            inner,
            options: window_options,
            open,
        };
        if self.depth.get() == 0 {
            self.mount(mount)
        } else {
            // Called from inside GPUI (e.g. a click handler that creates a
            // window): finish once the current GPUI update has unwound.
            self.mounts.borrow_mut().push_back(mount);
            self.waker.wake();
            Ok(())
        }
    }

    fn mount(&self, mount: Mount) -> Result<(), GpuiError> {
        let Mount {
            inner,
            options,
            open,
        } = mount;
        // A deferred mount can outlive its window: Tauri may have destroyed it
        // (or already processed `Destroyed`) since `attach` queued the mount.
        // Getters fail once Tauri has dropped the native window.
        if inner.closed.get() || inner.tauri_window.inner_size().is_err() {
            self.surfaces.borrow_mut().remove(&inner.label);
            inner.destroy();
            return Err(GpuiError::NotEligible {
                label: inner.label.clone(),
                reason: "the window was destroyed before GPUI could be mounted",
            });
        }
        PENDING_SURFACE.with(|pending| *pending.borrow_mut() = Some(inner.clone()));
        let result = self.update(|cx| open(cx, options));
        PENDING_SURFACE.with(|pending| pending.borrow_mut().take());

        let error = match result {
            Ok(Ok(handle)) => {
                inner.gpui_handle.set(Some(handle));
                inner.force_present.set(true);
                inner.schedule_frame();
                return Ok(());
            }
            Ok(Err(error)) => GpuiError::PlatformInitialization(format!("{error:#}")),
            Err(error) => error,
        };
        self.surfaces.borrow_mut().remove(&inner.label);
        inner.destroy();
        Err(error)
    }

    fn surface_for(
        &self,
        window_id: TaoWindowId,
        context: &EventLoopIterationContext<'_, EventLoopMessage>,
    ) -> Option<Rc<WindowInner>> {
        if let Some(label) = self.tao_labels.borrow().get(&window_id) {
            return self.surfaces.borrow().get(label).cloned();
        }
        let id = context.window_id_map.get(&window_id)?;
        let windows = context.windows.0.try_borrow().ok()?;
        let label = windows.get(&id)?.label().to_string();
        drop(windows);
        let inner = self.surfaces.borrow().get(&label).cloned()?;
        self.tao_labels.borrow_mut().insert(window_id, label);
        Some(inner)
    }

    /// Feeds one TAO event to GPUI. Returns `true` when Tauri must not handle
    /// the event (GPUI vetoed a close request).
    pub(crate) fn handle_event(
        &self,
        event: &Event<'_, Message<EventLoopMessage>>,
        proxy: &EventLoopProxy<Message<EventLoopMessage>>,
        context: &EventLoopIterationContext<'_, EventLoopMessage>,
    ) -> bool {
        if !self.proxy_installed.replace(true) {
            let proxy = Mutex::new(proxy.clone());
            self.waker.set(Box::new(move || {
                let _ = proxy
                    .lock()
                    .unwrap()
                    .send_event(Message::Task(Box::new(|| {})));
            }));
        }
        if self.depth.get() > 0 {
            // TAO never re-enters its callback, but be defensive.
            return false;
        }

        match event {
            // GTK themes are application-wide: TAO reports the change once,
            // with a dummy window id.
            Event::WindowEvent {
                window_id,
                event: event @ WindowEvent::ThemeChanged(_),
                ..
            } if is_dummy(window_id) => {
                let surfaces: Vec<_> = self.surfaces.borrow().values().cloned().collect();
                let _guard = self.enter();
                for surface in surfaces {
                    events::dispatch(&surface, event);
                }
            }
            // Closing through the window manager.
            Event::WindowEvent {
                window_id,
                event: WindowEvent::CloseRequested,
                ..
            } => {
                if let Some(inner) = self.surface_for(*window_id, context) {
                    return self.veto_close(&inner);
                }
            }
            // `tauri::Window::close()`.
            Event::UserEvent(Message::Window(id, WindowMessage::Close)) => {
                let label = context
                    .windows
                    .0
                    .try_borrow()
                    .ok()
                    .and_then(|windows| Some(windows.get(id)?.label().to_string()));
                let inner = label.and_then(|label| self.surfaces.borrow().get(&label).cloned());
                if let Some(inner) = inner {
                    return self.veto_close(&inner);
                }
            }
            Event::WindowEvent {
                window_id, event, ..
            } => {
                let Some(inner) = self.surface_for(*window_id, context) else {
                    return false;
                };
                self.track_window_state(&inner, event);
                let destroyed = {
                    let _guard = self.enter();
                    events::dispatch(&inner, event)
                };
                if destroyed {
                    self.surfaces.borrow_mut().remove(&inner.label);
                    self.tao_labels.borrow_mut().remove(window_id);
                    self.forget_platform_refs(&inner);
                    let _guard = self.enter();
                    inner.destroy();
                }
            }
            Event::RedrawRequested(window_id) => {
                // The OS asked for a repaint (expose, resize, ...): present
                // even if GPUI has nothing new to render.
                if let Some(inner) = self.surface_for(*window_id, context) {
                    inner.force_present.set(true);
                    inner.schedule_frame();
                }
            }
            Event::MainEventsCleared | Event::RedrawEventsCleared => self.drain(),
            Event::LoopDestroyed => self.shutdown(),
            _ => {}
        }
        false
    }

    /// Consults GPUI's `on_window_should_close` before Tauri closes the window.
    fn veto_close(&self, inner: &WindowInner) -> bool {
        if inner.closed.get() {
            return false;
        }
        let _guard = self.enter();
        !inner.should_close()
    }

    fn track_window_state(&self, inner: &Rc<WindowInner>, event: &WindowEvent<'_>) {
        match event {
            WindowEvent::CursorEntered { .. } => {
                *self.platform.hovered_window.borrow_mut() = Some(inner.tauri_window.clone());
                // Cursor icons are per native window: the cached style
                // belongs to the previously hovered one.
                self.platform.cursor.reset();
            }
            WindowEvent::CursorLeft { .. } => {
                let mut hovered = self.platform.hovered_window.borrow_mut();
                if hovered.as_ref().is_some_and(|w| w.label() == inner.label) {
                    *hovered = None;
                }
            }
            WindowEvent::Focused(true) => self.platform.active_window.set(inner.gpui_handle.get()),
            WindowEvent::Focused(false)
                if self.platform.active_window.get() == inner.gpui_handle.get() =>
            {
                self.platform.active_window.set(None);
            }
            _ => {}
        }
    }

    fn forget_platform_refs(&self, inner: &WindowInner) {
        let mut hovered = self.platform.hovered_window.borrow_mut();
        if hovered.as_ref().is_some_and(|w| w.label() == inner.label) {
            *hovered = None;
        }
        if self.platform.active_window.get() == inner.gpui_handle.get() {
            self.platform.active_window.set(None);
        }
    }

    /// Runs queued GPUI work, finishes deferred attachments and delivers
    /// pending frame requests. Called once or twice per TAO iteration.
    fn drain(&self) {
        if self.depth.get() > 0 {
            return;
        }
        self.waker.clear();

        let start = Instant::now();
        {
            let _guard = self.enter();
            while let Some(runnable) = self.dispatcher.pop_main() {
                runnable.run();
                if start.elapsed() > TASK_BUDGET {
                    break;
                }
            }
        }

        loop {
            let mount = self.mounts.borrow_mut().pop_front();
            let Some(mount) = mount else { break };
            if let Err(error) = self.mount(mount) {
                log::error!("tauri-plugin-gpui: deferred GPUI attachment failed: {error}");
            }
        }

        let actions = std::mem::take(&mut *self.deferred.borrow_mut());
        for action in actions {
            action();
        }

        let surfaces: Vec<_> = self.surfaces.borrow().values().cloned().collect();
        {
            let _guard = self.enter();
            for surface in &surfaces {
                if surface.frame_pending.take() && !surface.closed.get() {
                    surface.request_frame();
                }
            }
        }

        let more_work = self.dispatcher.has_main_work()
            || !self.mounts.borrow().is_empty()
            || !self.deferred.borrow().is_empty()
            || surfaces.iter().any(|s| s.frame_pending.get());
        if more_work {
            self.waker.wake();
        }
    }

    /// Releases every surface while thread locals are still alive.
    fn shutdown(&self) {
        let surfaces: Vec<_> = self.surfaces.borrow_mut().drain().map(|(_, s)| s).collect();
        self.tao_labels.borrow_mut().clear();
        let _guard = self.enter();
        for surface in surfaces {
            self.forget_platform_refs(&surface);
            surface.destroy();
        }
    }

    pub(crate) fn is_attached(&self, label: &str) -> bool {
        self.surfaces.borrow().contains_key(label)
    }
}
