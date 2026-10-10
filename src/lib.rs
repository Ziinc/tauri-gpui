//! # tauri-plugin-gpui
//!
//! Render [GPUI](https://www.gpui.rs) views inside ordinary Tauri windows.
//!
//! Tauri/TAO stays authoritative for the application lifecycle, the event
//! loop and native windows. This plugin permanently attaches GPUI as the
//! content renderer of an existing Tauri window; all attached windows share
//! one GPUI [`App`](gpui::App) running on Tauri's event-loop thread.
//!
//! ```ignore
//! use tauri_plugin_gpui::{GpuiWindowExt, gpui::{self, prelude::*}};
//!
//! struct Hello;
//!
//! impl gpui::Render for Hello {
//!     fn render(&mut self, _: &mut gpui::Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
//!         gpui::div().size_full().child("Hello from GPUI")
//!     }
//! }
//!
//! tauri::Builder::default()
//!     .setup(|app| {
//!         tauri_plugin_gpui::init(app)?;
//!         let window = tauri::WindowBuilder::new(app, "main").title("Example").build()?;
//!         window.attach_gpui(|cx| cx.new(|_| Hello))?;
//!         Ok(())
//!     })
//!     .run(tauri::generate_context!())
//!     .expect("error while running tauri application");
//! ```

// GPUI's platform callback signatures are inherently nested closure types.
#![allow(clippy::type_complexity)]

mod error;

#[cfg(gpui_backend)]
mod events;
#[cfg(gpui_backend)]
mod platform;
#[cfg(gpui_backend)]
mod runtime;

#[cfg(gpui_android)]
mod android;
#[cfg(gpui_ios)]
mod ios;
// The hardware key mapping is plain Rust: test it on every host.
#[cfg(all(test, not(gpui_ios)))]
#[path = "ios/keys.rs"]
#[allow(dead_code)]
mod ios_keys;

/// The active mobile platform layer (keyboard, clipboard, appearance).
#[cfg(gpui_android)]
use android as mobile;
#[cfg(gpui_ios)]
use ios as mobile;

pub use error::GpuiError;
pub use gpui;

use tauri::Wry;

/// Application-wide configuration for the shared GPUI runtime.
#[derive(Default)]
pub struct GpuiConfig {
    font_fallback: Option<String>,
    on_launch: Option<Box<dyn FnOnce(&mut gpui::App)>>,
    configure: Option<Box<dyn FnOnce(gpui::Application) -> gpui::Application>>,
}

impl GpuiConfig {
    /// Creates a config with default settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Font family used when a requested font is unavailable.
    pub fn font_fallback(mut self, family: impl Into<String>) -> Self {
        self.font_fallback = Some(family.into());
        self
    }

    /// Runs once with the shared GPUI `App` right after it is created. Use it
    /// for app-level GPUI setup: key bindings, globals, fonts.
    pub fn on_launch(mut self, f: impl FnOnce(&mut gpui::App) + 'static) -> Self {
        self.on_launch = Some(Box::new(f));
        self
    }

    /// Sets the GPUI asset source (SVG icons, images).
    pub fn assets(mut self, assets: impl gpui::AssetSource) -> Self {
        self.configure = Some(Box::new(move |app| app.with_assets(assets)));
        self
    }
}

/// Per-window attachment options.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct GpuiOptions {
    /// Focus the window once GPUI is attached.
    pub focus: bool,
}

impl Default for GpuiOptions {
    fn default() -> Self {
        Self { focus: true }
    }
}

impl GpuiOptions {
    /// Sets whether the window takes focus once GPUI is attached (default `true`).
    pub fn focus(mut self, focus: bool) -> Self {
        self.focus = focus;
        self
    }
}

/// Initializes the shared GPUI runtime and hooks it into Tauri's event loop.
///
/// Call once from [`tauri::Builder::setup`]. The plugin needs raw TAO events,
/// which Tauri only exposes through [`tauri::App::wry_plugin`], so it is
/// registered here rather than through `Builder::plugin`.
pub fn init(app: &mut tauri::App<Wry>) -> Result<(), GpuiError> {
    init_with(app, GpuiConfig::default())
}

/// [`init`] with explicit configuration.
pub fn init_with(app: &mut tauri::App<Wry>, config: GpuiConfig) -> Result<(), GpuiError> {
    #[cfg(gpui_backend)]
    {
        runtime::initialize(app.handle().clone(), config)?;
        app.wry_plugin(hook::GpuiHookBuilder);
        #[cfg(gpui_android)]
        app.handle().plugin(android_plugin())?;
        Ok(())
    }
    #[cfg(not(gpui_backend))]
    {
        let _ = (app, config);
        Err(GpuiError::UnsupportedOperation {
            operation: "tauri-plugin-gpui on mobile without the `mobile` feature",
        })
    }
}

/// Registers the `GpuiView` natives, then lets Tauri instantiate the Kotlin
/// `GpuiPlugin`, which installs the view as the activity's content.
#[cfg(gpui_android)]
fn android_plugin() -> tauri::plugin::TauriPlugin<Wry> {
    tauri::plugin::Builder::new("gpui")
        .setup(|app, api| {
            let (tx, rx) = std::sync::mpsc::channel();
            let _ = app;
            tauri_runtime_wry::wry::prelude::dispatch(move |env, activity, _webview| {
                tx.send(android::register_natives(env, activity)).ok();
            });
            rx.recv()
                .map_err(|e| e.to_string())?
                .map_err(|e| format!("registering GpuiView natives: {e}"))?;
            api.register_android_plugin("app.tauri.gpui", "GpuiPlugin")?;
            Ok(())
        })
        .build()
}

/// Runs `f` with the shared GPUI `App`.
///
/// Must be called on the main thread and not from inside GPUI code (which
/// already has a `&mut App`); returns [`GpuiError::Reentrant`] in that case.
pub fn with_app<R>(f: impl FnOnce(&mut gpui::App) -> R) -> Result<R, GpuiError> {
    #[cfg(gpui_backend)]
    {
        runtime::with(|runtime| runtime.update(f))
    }
    #[cfg(not(gpui_backend))]
    {
        let _ = f;
        Err(GpuiError::NotInitialized)
    }
}

thread_local! {
    static BACK_HANDLER: std::cell::RefCell<Option<std::rc::Rc<dyn Fn(&mut gpui::App)>>> =
        const { std::cell::RefCell::new(None) };
}

/// Sets the handler for the system back action (the Android back button
/// or gesture). It only runs while [`set_back_enabled`] is `true`; otherwise
/// back leaves the app as usual. Never called on desktop.
///
/// Call on the main thread (for example from GPUI code).
pub fn on_back(handler: impl Fn(&mut gpui::App) + 'static) {
    BACK_HANDLER.with(|slot| *slot.borrow_mut() = Some(std::rc::Rc::new(handler)));
}

/// Declares whether the app currently handles the back action, typically
/// whether its navigation stack is deeper than its root. No-op on desktop.
pub fn set_back_enabled(enabled: bool) {
    #[cfg(gpui_android)]
    android::set_back_enabled(enabled);
    #[cfg(not(gpui_android))]
    let _ = enabled;
}

#[cfg_attr(not(gpui_android), allow(dead_code))]
pub(crate) fn back_handler() -> Option<std::rc::Rc<dyn Fn(&mut gpui::App)>> {
    BACK_HANDLER.with(|slot| slot.borrow().clone())
}

/// Attaches GPUI as the content renderer of a Tauri window.
pub trait GpuiWindowExt {
    /// Permanently attaches GPUI to this window and mounts `root` as its root
    /// view. There is no detach: the GPUI root and its rendering surface are
    /// destroyed together with the Tauri window.
    ///
    /// Must be called on the main thread for a window without a WebView.
    /// When called from inside GPUI code the mount completes right after the
    /// current GPUI update returns.
    fn attach_gpui<F, V>(&self, root: F) -> Result<(), GpuiError>
    where
        F: FnOnce(&mut gpui::App) -> gpui::Entity<V> + 'static,
        V: gpui::Render + 'static,
    {
        self.attach_gpui_with(GpuiOptions::default(), root)
    }

    /// [`attach_gpui`](Self::attach_gpui) with explicit options.
    fn attach_gpui_with<F, V>(&self, options: GpuiOptions, root: F) -> Result<(), GpuiError>
    where
        F: FnOnce(&mut gpui::App) -> gpui::Entity<V> + 'static,
        V: gpui::Render + 'static,
    {
        self.attach_gpui_view(options, move |_, cx| root(cx))
    }

    /// Like [`attach_gpui_with`](Self::attach_gpui_with), but the root
    /// builder also receives the GPUI [`Window`](gpui::Window). Component
    /// libraries need this to wrap content in their root view, e.g.
    /// `|window, cx| cx.new(|cx| Root::new(view, window, cx))`.
    fn attach_gpui_view<F, V>(&self, options: GpuiOptions, root: F) -> Result<(), GpuiError>
    where
        F: FnOnce(&mut gpui::Window, &mut gpui::App) -> gpui::Entity<V> + 'static,
        V: gpui::Render + 'static;

    /// Whether GPUI is attached to this window.
    fn is_gpui_attached(&self) -> bool;
}

impl GpuiWindowExt for tauri::Window<Wry> {
    fn attach_gpui_view<F, V>(&self, options: GpuiOptions, root: F) -> Result<(), GpuiError>
    where
        F: FnOnce(&mut gpui::Window, &mut gpui::App) -> gpui::Entity<V> + 'static,
        V: gpui::Render + 'static,
    {
        #[cfg(gpui_backend)]
        {
            runtime::with(|runtime| {
                runtime.attach(
                    self,
                    options,
                    Box::new(move |cx, window_options| {
                        cx.open_window(window_options, root).map(Into::into)
                    }),
                )
            })
        }
        #[cfg(not(gpui_backend))]
        {
            let _ = (options, root);
            Err(GpuiError::UnsupportedOperation {
                operation: "attach_gpui on mobile",
            })
        }
    }

    fn is_gpui_attached(&self) -> bool {
        #[cfg(gpui_backend)]
        {
            runtime::with(|runtime| Ok(runtime.is_attached(self.label()))).unwrap_or(false)
        }
        #[cfg(not(gpui_backend))]
        {
            false
        }
    }
}

#[cfg(gpui_backend)]
mod hook {
    use tauri::EventLoopMessage;
    use tauri_runtime_wry::{
        Context, EventLoopIterationContext, Message, Plugin, PluginBuilder, WebContextStore,
        tao::{
            event::Event,
            event_loop::{ControlFlow, EventLoopProxy, EventLoopWindowTarget},
        },
    };

    /// Taps Tauri's TAO event loop. All state lives in the main-thread
    /// runtime, so the hook itself is stateless.
    pub(crate) struct GpuiHookBuilder;
    pub(crate) struct GpuiHook;

    impl PluginBuilder<EventLoopMessage> for GpuiHookBuilder {
        type Plugin = GpuiHook;

        fn build(self, _context: Context<EventLoopMessage>) -> Self::Plugin {
            GpuiHook
        }
    }

    impl Plugin<EventLoopMessage> for GpuiHook {
        fn on_event(
            &mut self,
            event: &Event<Message<EventLoopMessage>>,
            _event_loop: &EventLoopWindowTarget<Message<EventLoopMessage>>,
            proxy: &EventLoopProxy<Message<EventLoopMessage>>,
            _control_flow: &mut ControlFlow,
            context: EventLoopIterationContext<'_, EventLoopMessage>,
            _web_context: &WebContextStore,
        ) -> bool {
            // Events are only swallowed when GPUI vetoes a close request;
            // Tauri handles every other event as usual.
            crate::runtime::with(|runtime| Ok(runtime.handle_event(event, proxy, &context)))
                .unwrap_or(false)
        }
    }
}
