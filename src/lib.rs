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

#[cfg(not(any(target_os = "ios", target_os = "android")))]
mod events;
#[cfg(not(any(target_os = "ios", target_os = "android")))]
mod platform;
#[cfg(not(any(target_os = "ios", target_os = "android")))]
mod runtime;

#[cfg(feature = "mobile")]
pub mod mobile;

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
    #[cfg(not(any(target_os = "ios", target_os = "android")))]
    {
        runtime::initialize(app.handle().clone(), config)?;
        app.wry_plugin(hook::GpuiHookBuilder);
        Ok(())
    }
    #[cfg(any(target_os = "ios", target_os = "android"))]
    {
        let _ = (app, config);
        Err(GpuiError::UnsupportedOperation {
            operation: "tauri-plugin-gpui on mobile (see the `mobile` feature docs)",
        })
    }
}

/// Runs `f` with the shared GPUI `App`.
///
/// Must be called on the main thread and not from inside GPUI code (which
/// already has a `&mut App`); returns [`GpuiError::Reentrant`] in that case.
pub fn with_app<R>(f: impl FnOnce(&mut gpui::App) -> R) -> Result<R, GpuiError> {
    #[cfg(not(any(target_os = "ios", target_os = "android")))]
    {
        runtime::with(|runtime| runtime.update(f))
    }
    #[cfg(any(target_os = "ios", target_os = "android"))]
    {
        let _ = f;
        Err(GpuiError::NotInitialized)
    }
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
    /// `|window, cx| cx.new(|cx| gpui_kit::base::Root::new(view, window, cx))`.
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
        #[cfg(not(any(target_os = "ios", target_os = "android")))]
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
        #[cfg(any(target_os = "ios", target_os = "android"))]
        {
            let _ = (options, root);
            Err(GpuiError::UnsupportedOperation {
                operation: "attach_gpui on mobile",
            })
        }
    }

    fn is_gpui_attached(&self) -> bool {
        #[cfg(not(any(target_os = "ios", target_os = "android")))]
        {
            runtime::with(|runtime| Ok(runtime.is_attached(self.label()))).unwrap_or(false)
        }
        #[cfg(any(target_os = "ios", target_os = "android"))]
        {
            false
        }
    }
}

#[cfg(not(any(target_os = "ios", target_os = "android")))]
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
            let _ = crate::runtime::with(|runtime| {
                runtime.handle_event(event, proxy, &context);
                Ok(())
            });
            // Never swallow events: Tauri still handles every window event.
            false
        }
    }
}
