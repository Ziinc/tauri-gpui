//! Android platform layer.
//!
//! Tauri's Android activity has no native surface of its own, so the plugin
//! installs `GpuiView` (a `SurfaceView`, see `android/`) as the activity's
//! content and renders into its surface. The view's callbacks run on the
//! Android UI thread, while TAO, and with it GPUI, runs on a thread of its own.
//! Every callback therefore becomes a [`ViewEvent`] queued for the event-loop
//! thread, which drains the queue in [`crate::runtime::Runtime::drain`].

pub(crate) mod keys;
pub(crate) mod selection;

use std::{
    collections::VecDeque,
    ffi::c_void,
    sync::{Arc, Condvar, Mutex, OnceLock},
    time::Duration,
};

use jni::{
    JNIEnv, JavaVM, NativeMethod,
    objects::{GlobalRef, JClass, JObject, JString, JValue},
    sys::{jboolean, jfloat, jint},
};
use ndk::native_window::NativeWindow;

use crate::platform::dispatcher::LoopWaker;

/// How long `surfaceDestroyed` waits for the renderer to release the surface.
const SURFACE_RELEASE_TIMEOUT: Duration = Duration::from_secs(2);

pub(crate) enum ViewEvent {
    SurfaceChanged {
        window: NativeWindow,
        width: u32,
        height: u32,
        density: f32,
    },
    SurfaceDestroyed(Arc<Released>),
    Touch {
        phase: i32,
        id: i32,
        x: f32,
        y: f32,
    },
    Key {
        down: bool,
        key_code: i32,
        unicode: i32,
        meta: i32,
        repeat: bool,
    },
    CommitText(String),
    ComposingText(String),
    FinishComposing,
    DeleteSurrounding {
        before: usize,
        after: usize,
    },
    /// System bar and keyboard regions, in physical pixels.
    Insets {
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
        ime_bottom: i32,
    },
    Back,
    Lifecycle {
        active: bool,
    },
    Appearance {
        dark: bool,
    },
    /// A long press, in view pixels (Android's own detection, so it also
    /// covers inputs GPUI draws itself).
    LongPress {
        x: f32,
        y: f32,
    },
    /// An item chosen in the native selection toolbar.
    EditAction(keys::EditAction),
    /// Text from Android Autofill for the focused input.
    Autofill(String),
}

/// Signalled once the renderer no longer references a destroyed surface.
#[derive(Default)]
pub(crate) struct Released {
    done: Mutex<bool>,
    condvar: Condvar,
}

impl Released {
    pub(crate) fn signal(&self) {
        *self.done.lock().unwrap() = true;
        self.condvar.notify_all();
    }

    fn wait(&self) {
        let done = self.done.lock().unwrap();
        let (_done, timeout) = self
            .condvar
            .wait_timeout_while(done, SURFACE_RELEASE_TIMEOUT, |done| !*done)
            .unwrap();
        if timeout.timed_out() {
            log::warn!(
                "tauri-plugin-gpui: timed out waiting for the renderer to release the surface"
            );
        }
    }
}

#[derive(Default)]
struct Queue {
    events: VecDeque<ViewEvent>,
    waker: Option<Arc<LoopWaker>>,
}

static QUEUE: Mutex<Queue> = Mutex::new(Queue {
    events: VecDeque::new(),
    waker: None,
});
static VM: OnceLock<JavaVM> = OnceLock::new();
static VIEW: Mutex<Option<GlobalRef>> = Mutex::new(None);

/// Display density (physical pixels per logical pixel) last reported.
static DENSITY: Mutex<f32> = Mutex::new(1.0);
static DARK: Mutex<bool> = Mutex::new(false);

fn push(event: ViewEvent) -> bool {
    let mut queue = QUEUE.lock().unwrap();
    queue.events.push_back(event);
    match &queue.waker {
        Some(waker) => {
            waker.wake();
            true
        }
        None => false,
    }
}

/// Connects the queue to the event loop once the GPUI runtime exists.
pub(crate) fn set_waker(waker: Arc<LoopWaker>) {
    let mut queue = QUEUE.lock().unwrap();
    if !queue.events.is_empty() {
        waker.wake();
    }
    queue.waker = Some(waker);
}

pub(crate) fn take_events() -> VecDeque<ViewEvent> {
    std::mem::take(&mut QUEUE.lock().unwrap().events)
}

pub(crate) fn density() -> f32 {
    *DENSITY.lock().unwrap()
}

pub(crate) fn is_dark() -> bool {
    *DARK.lock().unwrap()
}

// Calls into the view, from the event-loop thread.

fn with_view(name: &str, f: impl FnOnce(&mut JNIEnv, &JObject) -> jni::errors::Result<()>) {
    let Some(vm) = VM.get() else { return };
    let Some(view) = VIEW.lock().unwrap().clone() else {
        return;
    };
    let mut env = match vm.attach_current_thread_as_daemon() {
        Ok(env) => env,
        Err(error) => {
            log::warn!("tauri-plugin-gpui: cannot attach to the JVM for {name}: {error}");
            return;
        }
    };
    if let Err(error) = f(&mut env, view.as_obj()) {
        log::warn!("tauri-plugin-gpui: GpuiView.{name} failed: {error}");
        let _ = env.exception_describe();
        let _ = env.exception_clear();
    }
}

pub(crate) fn show_keyboard() {
    log::debug!("tauri-plugin-gpui: showing the soft keyboard");
    with_view("showKeyboard", |env, view| {
        env.call_method(view, "showKeyboard", "()V", &[]).map(drop)
    });
}

pub(crate) fn hide_keyboard() {
    log::debug!("tauri-plugin-gpui: hiding the soft keyboard");
    with_view("hideKeyboard", |env, view| {
        env.call_method(view, "hideKeyboard", "()V", &[]).map(drop)
    });
}

pub(crate) fn set_back_enabled(enabled: bool) {
    with_view("setBackEnabled", |env, view| {
        env.call_method(
            view,
            "setBackEnabled",
            "(Z)V",
            &[JValue::Bool(enabled.into())],
        )
        .map(drop)
    });
}

pub(crate) fn clipboard_text() -> Option<String> {
    let mut text = None;
    with_view("clipboardText", |env, view| {
        let value = env
            .call_method(view, "clipboardText", "()Ljava/lang/String;", &[])?
            .l()?;
        if !value.is_null() {
            text = Some(env.get_string(&JString::from(value))?.into());
        }
        Ok(())
    });
    text
}

pub(crate) fn set_clipboard_text(text: &str) {
    with_view("setClipboardText", |env, view| {
        let text = env.new_string(text)?;
        env.call_method(
            view,
            "setClipboardText",
            "(Ljava/lang/String;)V",
            &[JValue::Object(&text)],
        )
        .map(drop)
    });
}

/// Loads the app's Tauri plugins, which Tauri skips when there is no WebView.
/// They get a stand-in WebView, not a real one.
pub(crate) fn load_plugins() {
    with_view("loadPlugins", |env, view| {
        env.call_method(view, "loadPlugins", "()V", &[]).map(drop)
    });
}

/// Shows (or moves) the native selection toolbar over `rect`.
pub(crate) fn show_selection_toolbar(rect: selection::ViewRect, has_selection: bool) {
    with_view("showSelectionToolbar", |env, view| {
        env.call_method(
            view,
            "showSelectionToolbar",
            "(IIIIZ)V",
            &[
                JValue::Int(rect.left),
                JValue::Int(rect.top),
                JValue::Int(rect.right),
                JValue::Int(rect.bottom),
                JValue::Bool(has_selection.into()),
            ],
        )
        .map(drop)
    });
}

pub(crate) fn hide_selection_toolbar() {
    with_view("hideSelectionToolbar", |env, view| {
        env.call_method(view, "hideSelectionToolbar", "()V", &[])
            .map(drop)
    });
}

/// Tells the view about the focused input (for Autofill): its text and the
/// rectangle it occupies.
pub(crate) fn set_autofill_input(text: &str, rect: selection::ViewRect) {
    with_view("setAutofillInput", |env, view| {
        let text = env.new_string(text)?;
        env.call_method(
            view,
            "setAutofillInput",
            "(Ljava/lang/String;IIII)V",
            &[
                JValue::Object(&text),
                JValue::Int(rect.left),
                JValue::Int(rect.top),
                JValue::Int(rect.right),
                JValue::Int(rect.bottom),
            ],
        )
        .map(drop)
    });
}

pub(crate) fn clear_autofill_input() {
    with_view("clearAutofillInput", |env, view| {
        env.call_method(view, "clearAutofillInput", "()V", &[])
            .map(drop)
    });
}

// Registration.

/// Registers the `GpuiView` natives. Runs on the UI thread before Tauri
/// instantiates `GpuiPlugin`, which creates the view.
pub(crate) fn register_natives(env: &mut JNIEnv, activity: &JObject) -> jni::errors::Result<()> {
    let name = env.new_string("app.tauri.gpui.GpuiView")?;
    // WryActivity.getAppClass resolves through the app's class loader; a
    // plain FindClass would use the system one.
    let class = env
        .call_method(
            activity,
            "getAppClass",
            "(Ljava/lang/String;)Ljava/lang/Class;",
            &[JValue::Object(&name)],
        )?
        .l()?;
    let class = JClass::from(class);
    let method = |name: &str, sig: &str, fn_ptr: *mut c_void| NativeMethod {
        name: name.into(),
        sig: sig.into(),
        fn_ptr,
    };
    env.register_native_methods(
        &class,
        &[
            method(
                "nativeAttach",
                "(Lapp/tauri/gpui/GpuiView;F)V",
                native_attach as *mut c_void,
            ),
            method(
                "nativeSurfaceChanged",
                "(Landroid/view/Surface;IIF)V",
                native_surface_changed as *mut c_void,
            ),
            method(
                "nativeSurfaceDestroyed",
                "()V",
                native_surface_destroyed as *mut c_void,
            ),
            method("nativeTouch", "(IIFF)V", native_touch as *mut c_void),
            method("nativeKey", "(ZIIII)V", native_key as *mut c_void),
            method(
                "nativeCommitText",
                "(Ljava/lang/String;)V",
                native_commit_text as *mut c_void,
            ),
            method(
                "nativeSetComposingText",
                "(Ljava/lang/String;)V",
                native_set_composing_text as *mut c_void,
            ),
            method(
                "nativeFinishComposingText",
                "()V",
                native_finish_composing_text as *mut c_void,
            ),
            method(
                "nativeDeleteSurroundingText",
                "(II)V",
                native_delete_surrounding_text as *mut c_void,
            ),
            method("nativeInsets", "(IIIII)V", native_insets as *mut c_void),
            method("nativeBack", "()V", native_back as *mut c_void),
            method("nativeLifecycle", "(I)V", native_lifecycle as *mut c_void),
            method("nativeAppearance", "(Z)V", native_appearance as *mut c_void),
            method("nativeLongPress", "(FF)V", native_long_press as *mut c_void),
            method(
                "nativeEditAction",
                "(I)V",
                native_edit_action as *mut c_void,
            ),
            method(
                "nativeAutofill",
                "(Ljava/lang/String;)V",
                native_autofill as *mut c_void,
            ),
        ],
    )
}

// Natives. All run on the Android UI thread.

extern "system" fn native_attach(env: JNIEnv, _: JClass, view: JObject, density: jfloat) {
    if let Ok(vm) = env.get_java_vm() {
        let _ = VM.set(vm);
    }
    match env.new_global_ref(view) {
        Ok(view) => *VIEW.lock().unwrap() = Some(view),
        Err(error) => {
            log::error!("tauri-plugin-gpui: cannot keep a reference to GpuiView: {error}")
        }
    }
    *DENSITY.lock().unwrap() = density;
}

extern "system" fn native_surface_changed(
    env: JNIEnv,
    _: JClass,
    surface: JObject,
    width: jint,
    height: jint,
    density: jfloat,
) {
    // SAFETY: `surface` is a live android.view.Surface for this call.
    let window = unsafe { NativeWindow::from_surface(env.get_raw(), surface.as_raw()) };
    let Some(window) = window else {
        log::error!("tauri-plugin-gpui: ANativeWindow_fromSurface returned null");
        return;
    };
    *DENSITY.lock().unwrap() = density;
    push(ViewEvent::SurfaceChanged {
        window,
        width: width.max(1) as u32,
        height: height.max(1) as u32,
        density,
    });
}

extern "system" fn native_surface_destroyed(_: JNIEnv, _: JClass) {
    let released = Arc::new(Released::default());
    if push(ViewEvent::SurfaceDestroyed(released.clone())) {
        released.wait();
    }
}

extern "system" fn native_touch(_: JNIEnv, _: JClass, phase: jint, id: jint, x: jfloat, y: jfloat) {
    push(ViewEvent::Touch { phase, id, x, y });
}

extern "system" fn native_key(
    _: JNIEnv,
    _: JClass,
    down: jboolean,
    key_code: jint,
    unicode: jint,
    meta: jint,
    repeat: jint,
) {
    push(ViewEvent::Key {
        down: down != 0,
        key_code,
        unicode,
        meta,
        repeat: repeat > 0,
    });
}

fn string(env: &mut JNIEnv, text: &JString) -> Option<String> {
    env.get_string(text).ok().map(Into::into)
}

extern "system" fn native_commit_text(mut env: JNIEnv, _: JClass, text: JString) {
    if let Some(text) = string(&mut env, &text) {
        push(ViewEvent::CommitText(text));
    }
}

extern "system" fn native_set_composing_text(mut env: JNIEnv, _: JClass, text: JString) {
    if let Some(text) = string(&mut env, &text) {
        push(ViewEvent::ComposingText(text));
    }
}

extern "system" fn native_finish_composing_text(_: JNIEnv, _: JClass) {
    push(ViewEvent::FinishComposing);
}

extern "system" fn native_delete_surrounding_text(_: JNIEnv, _: JClass, before: jint, after: jint) {
    push(ViewEvent::DeleteSurrounding {
        before: before.max(0) as usize,
        after: after.max(0) as usize,
    });
}

extern "system" fn native_insets(
    _: JNIEnv,
    _: JClass,
    left: jint,
    top: jint,
    right: jint,
    bottom: jint,
    ime_bottom: jint,
) {
    push(ViewEvent::Insets {
        left,
        top,
        right,
        bottom,
        ime_bottom,
    });
}

extern "system" fn native_back(_: JNIEnv, _: JClass) {
    push(ViewEvent::Back);
}

extern "system" fn native_lifecycle(_: JNIEnv, _: JClass, phase: jint) {
    push(ViewEvent::Lifecycle { active: phase == 0 });
}

extern "system" fn native_appearance(_: JNIEnv, _: JClass, dark: jboolean) {
    *DARK.lock().unwrap() = dark != 0;
    push(ViewEvent::Appearance { dark: dark != 0 });
}

extern "system" fn native_long_press(_: JNIEnv, _: JClass, x: jfloat, y: jfloat) {
    push(ViewEvent::LongPress { x, y });
}

extern "system" fn native_edit_action(_: JNIEnv, _: JClass, action: jint) {
    if let Some(action) = keys::EditAction::from_code(action) {
        push(ViewEvent::EditAction(action));
    }
}

extern "system" fn native_autofill(mut env: JNIEnv, _: JClass, text: JString) {
    if let Some(text) = string(&mut env, &text) {
        push(ViewEvent::Autofill(text));
    }
}
