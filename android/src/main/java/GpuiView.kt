package app.tauri.gpui

import android.app.Activity
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.res.Configuration
import android.graphics.Rect
import android.os.Build
import android.text.InputType
import android.util.Log
import android.util.SparseArray
import android.view.ActionMode
import android.view.GestureDetector
import android.view.KeyEvent
import android.view.Menu
import android.view.MotionEvent
import android.view.Surface
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.View
import android.view.ViewStructure
import android.view.autofill.AutofillManager
import android.view.autofill.AutofillValue
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputMethodManager
import android.webkit.WebView
import androidx.core.view.ViewCompat
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import app.tauri.plugin.PluginManager

/**
 * The native surface GPUI renders into, and the source of its input.
 *
 * Every callback forwards to Rust, which queues the event for the Tauri event
 * loop thread (where GPUI runs) and returns immediately. The one exception is
 * [surfaceDestroyed]: Android invalidates the surface once it returns, so the
 * native side blocks until the renderer has let go of it.
 *
 * GPUI draws its text inputs itself, so the view stands in for them towards
 * Android: a floating selection toolbar (Cut, Copy, Paste, Select all,
 * Autofill) and one virtual Autofill field for the focused input. Rust drives
 * both through [showSelectionToolbar], [setAutofillInput] and friends.
 */
class GpuiView(private val activity: Activity) : SurfaceView(activity), SurfaceHolder.Callback {
    @Volatile
    private var backEnabled = false
    private val imm = activity.getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager
    private val clipboard = activity.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager

    // UI thread only.
    private var pluginsLoaded = false
    private var touching = false
    private var toolbar: ActionMode? = null
    private var toolbarPending = false
    private var toolbarHasSelection = false
    private val toolbarRect = Rect()
    private var autofillInput: AutofillInput? = null

    private class AutofillInput(val text: String, val rect: Rect)

    private val gestures = GestureDetector(
        activity,
        object : GestureDetector.SimpleOnGestureListener() {
            // Reported to Rust, which knows whether the press hit a text input.
            override fun onLongPress(e: MotionEvent) {
                nativeLongPress(e.x, e.y)
            }
        }
    )

    init {
        holder.addCallback(this)
        isFocusable = true
        isFocusableInTouchMode = true
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            importantForAutofill = IMPORTANT_FOR_AUTOFILL_YES
        }
        ViewCompat.setOnApplyWindowInsetsListener(this) { _, insets ->
            val bars = insets.getInsets(
                WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout()
            )
            val ime = insets.getInsets(WindowInsetsCompat.Type.ime())
            nativeInsets(bars.left, bars.top, bars.right, bars.bottom, ime.bottom)
            insets
        }
        nativeAttach(this, resources.displayMetrics.density)
        reportAppearance(resources.configuration)
    }

    // Called from Rust on the GPUI thread.

    fun showKeyboard() {
        post {
            requestFocus()
            imm.restartInput(this)
            imm.showSoftInput(this, InputMethodManager.SHOW_IMPLICIT)
        }
    }

    fun hideKeyboard() {
        post { imm.hideSoftInputFromWindow(windowToken, 0) }
    }

    fun setBackEnabled(enabled: Boolean) {
        backEnabled = enabled
    }

    fun clipboardText(): String? =
        clipboard.primaryClip?.takeIf { it.itemCount > 0 }?.getItemAt(0)?.coerceToText(context)?.toString()

    fun setClipboardText(text: String) {
        clipboard.setPrimaryClip(ClipData.newPlainText("text", text))
    }

    fun lifecycle(phase: Int) {
        nativeLifecycle(phase)
    }

    /**
     * Loads the app's Tauri plugins. Tauri's `PluginManager.load` only calls
     * `Plugin.load(webView)` when a WebView exists and otherwise leaves it to
     * `onWebViewCreated`, which never happens without one. Plugins such as
     * tauri-plugin-biometric set up their state in `load`, so they fail
     * without it. Rust calls this once the first GPUI window has mounted, by
     * which time the app's plugins are registered; plugins registered later
     * are not loaded.
     *
     * The plugins get a [DetachedWebView], not a real one: creating the
     * process's first WebView registers the WebView provider as a package
     * dependency, and Android then relaunches the activity, which GPUI does
     * not survive. The stand-in is never attached or initialised, so a plugin
     * that drives the WebView (evaluating JS, say) would fail. None of the
     * plugins in use do.
     */
    fun loadPlugins() {
        activity.runOnUiThread {
            if (pluginsLoaded) return@runOnUiThread
            pluginsLoaded = true
            try {
                val manager = activity.javaClass.getMethod("getPluginManager").invoke(activity) as PluginManager
                manager.onWebViewCreated(newDetachedWebView())
                Log.i(TAG, "loaded Android plugins")
            } catch (e: Throwable) {
                Log.e(TAG, "loading Android plugins failed", e)
            }
        }
    }

    /**
     * A [DetachedWebView] allocated without running any constructor, the way
     * Gson's `UnsafeAllocator` does on Android.
     */
    private fun newDetachedWebView(): WebView {
        val unsafeClass = Class.forName("sun.misc.Unsafe")
        val field = unsafeClass.getDeclaredField("theUnsafe").apply { isAccessible = true }
        val allocate = unsafeClass.getMethod("allocateInstance", Class::class.java)
        return allocate.invoke(field.get(null), DetachedWebView::class.java) as WebView
    }

    // Selection toolbar.

    /**
     * Shows the floating Cut/Copy/Paste toolbar over [left, top, right, bottom]
     * (view pixels), or moves it if it is open. While a finger is down it
     * waits for the lift, as Android's own text selection does.
     */
    fun showSelectionToolbar(left: Int, top: Int, right: Int, bottom: Int, hasSelection: Boolean) {
        post {
            toolbarRect.set(left, top, right, bottom)
            toolbarHasSelection = hasSelection
            if (touching) {
                toolbarPending = true
            } else {
                openToolbar()
            }
        }
    }

    fun hideSelectionToolbar() {
        post { closeToolbar() }
    }

    private fun openToolbar() {
        toolbarPending = false
        val open = toolbar
        if (open != null) {
            open.invalidate()
            open.invalidateContentRect()
            return
        }
        toolbar = startActionMode(toolbarCallback, ActionMode.TYPE_FLOATING)
        if (toolbar != null) Log.d(TAG, "selection toolbar shown")
    }

    private fun closeToolbar() {
        toolbarPending = false
        toolbar?.finish()
    }

    private fun clipboardHasText(): Boolean =
        clipboard.primaryClipDescription?.hasMimeType("text/*") == true

    private fun autofillManager(): AutofillManager? =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            context.getSystemService(AutofillManager::class.java)?.takeIf { it.isEnabled }
        } else {
            null
        }

    private val toolbarCallback = object : ActionMode.Callback2() {
        override fun onCreateActionMode(mode: ActionMode, menu: Menu): Boolean {
            buildMenu(menu)
            return true
        }

        override fun onPrepareActionMode(mode: ActionMode, menu: Menu): Boolean {
            menu.clear()
            buildMenu(menu)
            return true
        }

        private fun buildMenu(menu: Menu) {
            if (toolbarHasSelection) {
                menu.add(Menu.NONE, android.R.id.cut, 0, android.R.string.cut)
                menu.add(Menu.NONE, android.R.id.copy, 1, android.R.string.copy)
            }
            if (clipboardHasText()) {
                menu.add(Menu.NONE, android.R.id.paste, 2, android.R.string.paste)
            }
            menu.add(Menu.NONE, android.R.id.selectAll, 3, android.R.string.selectAll)
            if (autofillInput != null && autofillManager() != null) {
                menu.add(Menu.NONE, android.R.id.autofill, 4, android.R.string.autofill)
            }
        }

        override fun onActionItemClicked(mode: ActionMode, item: android.view.MenuItem): Boolean {
            when (item.itemId) {
                android.R.id.cut -> nativeEditAction(ACTION_CUT)
                android.R.id.copy -> nativeEditAction(ACTION_COPY)
                android.R.id.paste -> nativeEditAction(ACTION_PASTE)
                // Stays open: the selection is now everything, which Rust
                // reports back as a new toolbar position.
                android.R.id.selectAll -> {
                    nativeEditAction(ACTION_SELECT_ALL)
                    return true
                }
                android.R.id.autofill -> requestAutofill()
                else -> return false
            }
            mode.finish()
            return true
        }

        override fun onDestroyActionMode(mode: ActionMode) {
            if (toolbar === mode) toolbar = null
        }

        override fun onGetContentRect(mode: ActionMode, view: View, outRect: Rect) {
            outRect.set(toolbarRect)
        }
    }

    // Autofill. The focused GPUI input is exposed as one virtual child.

    /** Describes the focused input: its text and the rectangle it occupies. */
    fun setAutofillInput(text: String, left: Int, top: Int, right: Int, bottom: Int) {
        post {
            val previous = autofillInput
            val rect = Rect(left, top, right, bottom)
            autofillInput = AutofillInput(text, rect)
            val manager = autofillManager() ?: return@post
            if (previous == null) {
                manager.notifyViewEntered(this, AUTOFILL_VIRTUAL_ID, rect)
            } else if (previous.text != text) {
                manager.notifyValueChanged(this, AUTOFILL_VIRTUAL_ID, AutofillValue.forText(text))
            }
        }
    }

    fun clearAutofillInput() {
        post {
            if (autofillInput == null) return@post
            autofillInput = null
            autofillManager()?.notifyViewExited(this, AUTOFILL_VIRTUAL_ID)
        }
    }

    private fun requestAutofill() {
        val input = autofillInput ?: return
        autofillManager()?.requestAutofill(this, AUTOFILL_VIRTUAL_ID, input.rect)
    }

    override fun onProvideAutofillVirtualStructure(structure: ViewStructure, flags: Int) {
        super.onProvideAutofillVirtualStructure(structure, flags)
        val input = autofillInput ?: return
        val child = structure.newChild(structure.addChildCount(1))
        child.setAutofillId(structure.autofillId!!, AUTOFILL_VIRTUAL_ID)
        child.setId(AUTOFILL_VIRTUAL_ID, context.packageName, null, "gpui_text_input")
        child.setClassName("android.widget.EditText")
        child.setAutofillType(AUTOFILL_TYPE_TEXT)
        child.setAutofillValue(AutofillValue.forText(input.text))
        child.setInputType(InputType.TYPE_CLASS_TEXT)
        child.setFocusable(true)
        child.setFocused(true)
        child.setEnabled(true)
        child.setVisibility(VISIBLE)
        child.setDimens(input.rect.left, input.rect.top, 0, 0, input.rect.width(), input.rect.height())
    }

    override fun autofill(values: SparseArray<AutofillValue>) {
        val value = values.get(AUTOFILL_VIRTUAL_ID) ?: return
        if (value.isText) nativeAutofill(value.textValue.toString())
    }

    override fun onFocusChanged(gainFocus: Boolean, direction: Int, previouslyFocusedRect: Rect?) {
        super.onFocusChanged(gainFocus, direction, previouslyFocusedRect)
        if (!gainFocus) closeToolbar()
    }

    override fun onWindowFocusChanged(hasWindowFocus: Boolean) {
        super.onWindowFocusChanged(hasWindowFocus)
        if (!hasWindowFocus) closeToolbar()
    }

    // Surface.

    override fun surfaceCreated(holder: SurfaceHolder) {}

    override fun surfaceChanged(holder: SurfaceHolder, format: Int, width: Int, height: Int) {
        nativeSurfaceChanged(holder.surface, width, height, resources.displayMetrics.density)
    }

    override fun surfaceDestroyed(holder: SurfaceHolder) {
        nativeSurfaceDestroyed()
    }

    // Appearance.

    override fun onConfigurationChanged(newConfig: Configuration) {
        super.onConfigurationChanged(newConfig)
        reportAppearance(newConfig)
    }

    private fun reportAppearance(config: Configuration) {
        val dark = (config.uiMode and Configuration.UI_MODE_NIGHT_MASK) == Configuration.UI_MODE_NIGHT_YES
        WindowCompat.getInsetsController(activity.window, this).apply {
            isAppearanceLightStatusBars = !dark
            isAppearanceLightNavigationBars = !dark
        }
        nativeAppearance(dark)
    }

    // Touch.

    override fun onTouchEvent(event: MotionEvent): Boolean {
        gestures.onTouchEvent(event)
        when (event.actionMasked) {
            MotionEvent.ACTION_DOWN, MotionEvent.ACTION_POINTER_DOWN -> {
                if (event.actionMasked == MotionEvent.ACTION_DOWN) {
                    touching = true
                    closeToolbar()
                }
                if (!hasFocus()) requestFocus()
                touch(event, event.actionIndex, TOUCH_STARTED)
            }
            MotionEvent.ACTION_UP, MotionEvent.ACTION_POINTER_UP -> {
                touch(event, event.actionIndex, TOUCH_ENDED)
                if (event.actionMasked == MotionEvent.ACTION_UP) touchEnded()
            }
            MotionEvent.ACTION_MOVE ->
                for (i in 0 until event.pointerCount) touch(event, i, TOUCH_MOVED)
            MotionEvent.ACTION_CANCEL -> {
                for (i in 0 until event.pointerCount) touch(event, i, TOUCH_CANCELLED)
                touchEnded()
            }
        }
        return true
    }

    private fun touchEnded() {
        touching = false
        if (toolbarPending) openToolbar()
    }

    private fun touch(event: MotionEvent, index: Int, phase: Int) {
        nativeTouch(phase, event.getPointerId(index), event.getX(index), event.getY(index))
    }

    // Keys.

    override fun onKeyDown(keyCode: Int, event: KeyEvent): Boolean {
        if (keyCode == KeyEvent.KEYCODE_BACK) return true
        if (event.isSystem) return super.onKeyDown(keyCode, event)
        closeToolbar()
        key(true, keyCode, event)
        return true
    }

    override fun onKeyUp(keyCode: Int, event: KeyEvent): Boolean {
        if (keyCode == KeyEvent.KEYCODE_BACK) {
            // Not `super`: WryActivity's back handling assumes a WebView.
            if (backEnabled) nativeBack() else activity.moveTaskToBack(true)
            return true
        }
        if (event.isSystem) return super.onKeyUp(keyCode, event)
        key(false, keyCode, event)
        return true
    }

    private fun key(down: Boolean, keyCode: Int, event: KeyEvent) {
        nativeKey(down, keyCode, event.getUnicodeChar(event.metaState), event.metaState, event.repeatCount)
    }

    // Soft keyboard.

    override fun onCheckIsTextEditor(): Boolean = true

    override fun onCreateInputConnection(outAttrs: EditorInfo): InputConnection {
        // Visible-password text keeps IMEs from composing words, so each key
        // arrives as committed text or a key event (the approach terminals use).
        outAttrs.inputType = InputType.TYPE_CLASS_TEXT or
            InputType.TYPE_TEXT_VARIATION_VISIBLE_PASSWORD or
            InputType.TYPE_TEXT_FLAG_NO_SUGGESTIONS
        outAttrs.imeOptions = EditorInfo.IME_FLAG_NO_FULLSCREEN or
            EditorInfo.IME_FLAG_NO_EXTRACT_UI or
            EditorInfo.IME_ACTION_NONE
        return GpuiInputConnection(this)
    }

    private class GpuiInputConnection(private val view: GpuiView) : BaseInputConnection(view, false) {
        // Typing moves the caret away from where the toolbar points.
        override fun commitText(text: CharSequence, newCursorPosition: Int): Boolean {
            view.closeToolbar()
            nativeCommitText(text.toString())
            return true
        }

        override fun setComposingText(text: CharSequence, newCursorPosition: Int): Boolean {
            view.closeToolbar()
            nativeSetComposingText(text.toString())
            return true
        }

        override fun finishComposingText(): Boolean {
            nativeFinishComposingText()
            return true
        }

        override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean {
            view.closeToolbar()
            nativeDeleteSurroundingText(beforeLength, afterLength)
            return true
        }
    }

    /**
     * Stand-in handed to `Plugin.load`. Its constructor never runs, so
     * Chromium is not initialised. Tauri's `AppPlugin` is the only caller of
     * WebView methods (in its back-button handler): `canGoBack` and `goBack`
     * are overridden so back falls through to the activity, as with no WebView.
     */
    private class DetachedWebView(context: Context) : WebView(context) {
        override fun canGoBack(): Boolean = false

        override fun goBack() {}
    }

    companion object {
        const val TOUCH_STARTED = 0
        const val TOUCH_MOVED = 1
        const val TOUCH_ENDED = 2
        const val TOUCH_CANCELLED = 3

        const val LIFECYCLE_ACTIVE = 0
        const val LIFECYCLE_BACKGROUND = 1

        // Selection toolbar actions; the codes match `EditAction::from_code` in Rust.
        const val ACTION_CUT = 0
        const val ACTION_COPY = 1
        const val ACTION_PASTE = 2
        const val ACTION_SELECT_ALL = 3

        private const val AUTOFILL_VIRTUAL_ID = 1
        private const val TAG = "tauri-plugin-gpui"

        // Registered by tauri-plugin-gpui with RegisterNatives.
        @JvmStatic external fun nativeAttach(view: GpuiView, density: Float)
        @JvmStatic external fun nativeSurfaceChanged(surface: Surface, width: Int, height: Int, density: Float)
        @JvmStatic external fun nativeSurfaceDestroyed()
        @JvmStatic external fun nativeTouch(phase: Int, id: Int, x: Float, y: Float)
        @JvmStatic external fun nativeKey(down: Boolean, keyCode: Int, unicode: Int, meta: Int, repeat: Int)
        @JvmStatic external fun nativeCommitText(text: String)
        @JvmStatic external fun nativeSetComposingText(text: String)
        @JvmStatic external fun nativeFinishComposingText()
        @JvmStatic external fun nativeDeleteSurroundingText(before: Int, after: Int)
        @JvmStatic external fun nativeInsets(left: Int, top: Int, right: Int, bottom: Int, imeBottom: Int)
        @JvmStatic external fun nativeBack()
        @JvmStatic external fun nativeLifecycle(phase: Int)
        @JvmStatic external fun nativeAppearance(dark: Boolean)
        @JvmStatic external fun nativeLongPress(x: Float, y: Float)
        @JvmStatic external fun nativeEditAction(action: Int)
        @JvmStatic external fun nativeAutofill(text: String)
    }
}
