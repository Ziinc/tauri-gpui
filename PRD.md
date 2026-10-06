tauri-plugin-gpui PRD
Status
Draft — architecture decisions captured to date.
Summary
tauri-plugin-gpui is a Tauri plugin that allows selected Tauri windows to use GPUI as their native content renderer instead of a WebView.
The plugin is a rendering integration, not an alternative Tauri runtime. Tauri/TAO remains authoritative for application lifecycle, native windows, event dispatch, and window management. GPUI is permanently attached as the content renderer of an existing Tauri window.
Prior art includes tauri-plugin-egui. Cross-platform/mobile GPUI support will use gpui-mobile, gated behind an optional Cargo feature to avoid increasing desktop-only compile times.
Goals
Render GPUI applications inside ordinary Tauri-owned native windows.
Preserve Tauri's application, window, and event-loop abstractions.
Avoid a Tauri fork or GPUI fork.
Allow multiple GPUI-backed Tauri windows to share one GPUI application context.
Allow GPUI-backed and ordinary WebView-backed Tauri windows to coexist in one application.
Keep the initial GPUI platform adapter deliberately small.
Support mobile through gpui-mobile behind an opt-in feature.
Non-goals
The initial version will not:
Replace Tauri/TAO's event loop.
Introduce a separate top-level GPUI window abstraction.
Allow GPUI to create native windows.
Support GPUI::open_window.
Dynamically attach/detach GPUI or swap renderers after attachment.
Attempt to emulate the complete GPUI Platform or PlatformWindow API.
Composite GPUI and a WebView within the same Tauri window.
Architectural invariants
Tauri owns the application lifecycle.
Tauri/TAO exclusively owns the native event loop.
Tauri exclusively creates and destroys native top-level windows.
A GPUI-backed window is still a normal Tauri window.
GPUI replaces only the internal content rendering of an attached window.
GPUI runs on the same thread as Tauri/TAO's event loop.
There is exactly one shared GPUI App per Tauri application.
GPUI attachment is permanent for the lifetime of a window.
GPUI native window creation, including open_window, is unsupported.
The GPUI platform adapter implements only the minimum functionality required for rendering and interaction.
Architecture
Tauri Application
│
├── Tauri / TAO
│   ├── Application lifecycle
│   ├── Event loop
│   ├── Native window creation/destruction
│   ├── Input/window events
│   └── Existing Tauri facilities/plugins
│
├── Tauri Window A ───── attach_gpui ────┐
├── Tauri Window B ───── attach_gpui ────┤
│                                        │
├── WebView Window C                     │
│   └── normal Tauri/Wry                 │
│                                        ▼
└──────────────────────────── tauri-plugin-gpui
                                         │
                              ┌──────────┴──────────┐
                              │                     │
                           Surface A             Surface B
                              │                     │
                              └──────────┬──────────┘
                                         ▼
                                  Shared GPUI App
GPUI-backed windows are not independently owned "GPUI windows" from the application's perspective. They are Tauri windows with a GPUI rendering surface attached.
Public API direction
The canonical API uses explicit attachment to an already-created Tauri window.
let window = tauri::WindowBuilder::new(app, "main")
    .title("Example")
    .build()?;

window.attach_gpui(|cx| {
    cx.new(|cx| AppView::new(cx))
})?;
The API should be provided through an extension trait similar to:
pub trait GpuiWindowExt {
    fn attach_gpui<F, V>(&self, root: F) -> Result<(), GpuiError>
    where
        F: FnOnce(&mut gpui::App) -> gpui::Entity<V> + 'static,
        V: gpui::Render + 'static;
}
A future options variant may be provided:
window.attach_gpui_with(
    GpuiOptions::default(),
    |cx| cx.new(AppView::new),
)?;
A separate GpuiWindowBuilder is not part of the initial design because explicit attachment reinforces the intended ownership boundary.
Attachment semantics
attach_gpui() is a one-way operation for the lifetime of the Tauri window.
It should conceptually:
Verify that the window is eligible for GPUI attachment.
Reject a second attachment attempt.
Acquire/use the existing Tauri native window handle.
Create the minimum GPUI rendering/platform state required for that window.
Register the window with the plugin's shared GPUI runtime.
Mount the supplied GPUI root entity.
Register event translation for the window.
Request the initial redraw.
There is intentionally no detach_gpui, set_renderer, or runtime renderer replacement API.
Destroying the Tauri window destroys its GPUI root, rendering surface, and associated resources.
Shared GPUI application
A Tauri application has one GPUI App, shared by all attached windows.
This preserves GPUI application-level semantics such as shared entities, globals, subscriptions, actions, services, and asynchronous tasks without allowing GPUI to become the owner of native windows.
Conceptually:
struct GpuiRuntime {
    app: gpui::App,
    windows: HashMap<WindowId, GpuiSurface>,
}
The GPUI App is created when the plugin runtime initializes and lives until the Tauri application exits.
Closing the last GPUI-backed window does not destroy the shared GPUI App. A later Tauri window may be created and attached to the existing GPUI application context.
Threading model
GPUI executes on Tauri/TAO's event-loop thread.
The plugin must not create a competing GPUI UI/event-loop thread.
TAO event
   │
   ▼
Identify attached Tauri window
   │
   ▼
Translate required event into GPUI
   │
   ▼
Update shared GPUI App/window state
   │
   ▼
GPUI requests redraw when necessary
   │
   ▼
Tauri/TAO request_redraw()
   │
   ▼
RedrawRequested
   │
   ▼
GPUI layout → paint → render → present
Rendering should be demand-driven rather than running a permanent fixed-rate render loop.
Window creation
All native windows must be created through Tauri.
To create another GPUI-backed window:
let inspector = tauri::WindowBuilder::new(app, "inspector")
    .title("Inspector")
    .build()?;

inspector.attach_gpui(|cx| {
    cx.new(|cx| InspectorView::new(cx))
})?;
GPUI's native window-creation API is unsupported:
cx.open_window(...); // unsupported
The adapter should fail clearly for unsupported operations rather than silently creating a separate GPUI-owned window or pretending the operation succeeded.
Application code that needs another window must use Tauri APIs and subsequently call attach_gpui().
GPUI platform adapter scope
The first implementation deliberately uses a minimal adapter rather than attempting to map the full GPUI Platform/PlatformWindow contract onto Tauri.
Only functionality necessary to:
host GPUI against an existing native Tauri window,
deliver the minimum required input/window events,
schedule redraws,
perform layout/paint/render/presentation,
handle required display scaling/geometry,
and correctly release resources
should be implemented initially.
Operations outside this minimum surface should explicitly return an unsupported-operation error or equivalent behavior appropriate to the GPUI API.
This is intended to minimize coupling to GPUI internals and reduce maintenance burden as GPUI evolves.
Event integration
Tauri/TAO remains the source of native events. The plugin translates only the subset required by GPUI.
Expected initial categories include:
redraw requests,
resize,
scale-factor/DPI changes,
cursor movement,
mouse buttons,
mouse wheel/scroll,
keyboard input,
modifier state,
text/IME input,
focus changes,
and window destruction/lifecycle events required for cleanup.
The exact minimum event set should be validated during the architectural prototype against the GPUI version targeted by the crate.
Redraw coordination
GPUI must not establish its own top-level event loop.
The intended flow is:
GPUI state invalidated
        ↓
GPUI/plugin requests frame
        ↓
Tauri/TAO request_redraw()
        ↓
RedrawRequested(window_id)
        ↓
plugin resolves GpuiSurface
        ↓
GPUI layout + paint
        ↓
present into existing native window
Desktop and mobile features
Desktop support is the default build configuration.
Mobile support is opt-in:
[features]
default = []
mobile = ["dep:gpui-mobile"]
Conceptually:
[dependencies]
tauri-plugin-gpui = "..."

# Enable iOS/Android support when required
tauri-plugin-gpui = { version = "...", features = ["mobile"] }
The goal is that desktop-only consumers do not compile gpui-mobile or its mobile-specific dependency graph.
Mobile architecture
When the mobile feature is enabled, gpui-mobile is used as the basis for GPUI support on iOS and Android.
The same ownership principle applies:
Tauri Mobile
    │
    ├── lifecycle / application ownership
    ├── native window/view ownership
    └── event integration
            │
            ▼
    tauri-plugin-gpui
            │
            ▼
       gpui-mobile
            │
       GPUI rendering
The plugin should reuse gpui-mobile rather than independently reimplementing GPUI's mobile platform support.
Mobile-specific integration details remain to be validated during implementation/prototyping.
Error handling
Unsupported GPUI platform operations should fail explicitly where the GPUI API permits it.
For plugin APIs, errors should use a structured plugin error type, for example:
pub enum GpuiError {
    AlreadyAttached,
    UnsupportedOperation {
        operation: &'static str,
    },
    PlatformInitialization(/* ... */),
    RendererInitialization(/* ... */),
}
The exact representation may change based on GPUI's internal interfaces.
Initial platform targets
Target support:
macOS
Windows
Linux
iOS (mobile feature)
Android (mobile feature)
Desktop should be implemented and stabilized first. Mobile support should reuse the same public attachment model wherever Tauri's mobile window/view model permits it.
Architectural spike / Phase 0
Before committing to the complete implementation, build a minimal desktop prototype answering the central integration question:
Can GPUI's rendering machinery be attached to an existing Tauri/TAO native window while Tauri retains ownership of the window and event loop, without maintaining a fork of GPUI or Tauri?
The spike should prove at least:
Create a normal Tauri window.
Obtain the native window/display handles required by the renderer.
Initialize enough GPUI state to render into that existing window.
Render a basic GPUI view.
Translate basic pointer and keyboard input.
Handle resize and DPI changes.
Coordinate demand-driven redraw through TAO.
Cleanly destroy the rendering state when the Tauri window closes.
Run everything on Tauri's event-loop thread.
If GPUI requires implementation of portions of Platform or PlatformWindow, the spike should identify the smallest viable subset and document all unsupported operations.
Initial success criteria
The architectural approach is viable when a sample application can:
start as an ordinary Tauri application,
create a normal Tauri window,
permanently attach GPUI to it,
render an interactive GPUI view without a WebView,
process normal mouse/keyboard input,
resize and redraw correctly,
share one GPUI App between at least two attached Tauri windows,
coexist with an ordinary WebView-backed Tauri window,
and close/recreate GPUI-backed windows without restarting the shared GPUI application.
Open questions
The following remain intentionally undecided:
Exact supported GPUI version and versioning policy.
Exact minimum Platform/PlatformWindow methods required by current GPUI.
Renderer/backend initialization strategy per desktop platform.
Clipboard support in the first release.
IME completeness required for the MVP.
Accessibility integration.
Cursor/icon integration.
GPUI async executor integration with Tauri/Tokio.
Detailed mobile lifecycle and native-surface integration.
Whether the first published release requires all desktop platforms or may stabilize them incrementally.
Decisions log
Decision
Choice
Overall architecture
Tauri owns window/event abstractions; GPUI replaces internal rendering only
Attachment API
Explicit window.attach_gpui(...)
Attachment lifetime
Permanent until Tauri window destruction
GPUI application ownership
One shared GPUI App per Tauri application
Threading
Same thread as Tauri/TAO event loop
GPUI open_window
Unsupported
GPUI platform compatibility
Minimum required adapter only
Mobile
gpui-mobile, optional mobile Cargo feature
Desktop compile impact from mobile
Mobile dependency disabled by default