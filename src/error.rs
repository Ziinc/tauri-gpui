/// Errors returned by the plugin's public API.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum GpuiError {
    /// [`crate::init`] has not been called (or failed) for this application.
    #[error("tauri-plugin-gpui is not initialized; call `tauri_plugin_gpui::init(app)` in `setup`")]
    NotInitialized,

    /// [`crate::init`] was called more than once.
    #[error("tauri-plugin-gpui is already initialized")]
    AlreadyInitialized,

    /// GPUI is already attached to this window. Attachment is permanent.
    #[error("GPUI is already attached to window `{label}`")]
    AlreadyAttached { label: String },

    /// The window cannot host a GPUI surface (for example it already hosts a WebView).
    #[error("window `{label}` is not eligible for GPUI attachment: {reason}")]
    NotEligible { label: String, reason: &'static str },

    /// The call must happen on the Tauri/TAO event-loop thread.
    #[error("must be called on the Tauri event-loop (main) thread")]
    NotMainThread,

    /// The shared GPUI `App` is already borrowed further up the stack.
    #[error("the shared GPUI App is already in use further up the call stack")]
    Reentrant,

    /// The operation is outside the minimal GPUI platform adapter.
    #[error("unsupported operation: {operation}")]
    UnsupportedOperation { operation: &'static str },

    /// The GPUI platform adapter could not be created.
    #[error("GPUI platform initialization failed: {0}")]
    PlatformInitialization(String),

    /// The GPU renderer for a window could not be created.
    #[error("GPUI renderer initialization failed: {0}")]
    RendererInitialization(String),

    /// An error reported by Tauri while querying the window.
    #[error(transparent)]
    Tauri(#[from] tauri::Error),
}
