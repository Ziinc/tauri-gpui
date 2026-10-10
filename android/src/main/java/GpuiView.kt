package app.tauri.gpui

import android.app.Activity
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.res.Configuration
import android.text.InputType
import android.view.KeyEvent
import android.view.MotionEvent
import android.view.Surface
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.inputmethod.BaseInputConnection
import android.view.inputmethod.EditorInfo
import android.view.inputmethod.InputConnection
import android.view.inputmethod.InputMethodManager
import androidx.core.view.ViewCompat
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat

/**
 * The native surface GPUI renders into, and the source of its input.
 *
 * Every callback forwards to Rust, which queues the event for the Tauri event
 * loop thread (where GPUI runs) and returns immediately. The one exception is
 * [surfaceDestroyed]: Android invalidates the surface once it returns, so the
 * native side blocks until the renderer has let go of it.
 */
class GpuiView(private val activity: Activity) : SurfaceView(activity), SurfaceHolder.Callback {
    @Volatile
    private var backEnabled = false
    private val imm = activity.getSystemService(Context.INPUT_METHOD_SERVICE) as InputMethodManager
    private val clipboard = activity.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager

    init {
        holder.addCallback(this)
        isFocusable = true
        isFocusableInTouchMode = true
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
        when (event.actionMasked) {
            MotionEvent.ACTION_DOWN, MotionEvent.ACTION_POINTER_DOWN -> {
                if (!hasFocus()) requestFocus()
                touch(event, event.actionIndex, TOUCH_STARTED)
            }
            MotionEvent.ACTION_UP, MotionEvent.ACTION_POINTER_UP ->
                touch(event, event.actionIndex, TOUCH_ENDED)
            MotionEvent.ACTION_MOVE ->
                for (i in 0 until event.pointerCount) touch(event, i, TOUCH_MOVED)
            MotionEvent.ACTION_CANCEL ->
                for (i in 0 until event.pointerCount) touch(event, i, TOUCH_CANCELLED)
        }
        return true
    }

    private fun touch(event: MotionEvent, index: Int, phase: Int) {
        nativeTouch(phase, event.getPointerId(index), event.getX(index), event.getY(index))
    }

    // Keys.

    override fun onKeyDown(keyCode: Int, event: KeyEvent): Boolean {
        if (keyCode == KeyEvent.KEYCODE_BACK) return true
        if (event.isSystem) return super.onKeyDown(keyCode, event)
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

    private class GpuiInputConnection(view: GpuiView) : BaseInputConnection(view, false) {
        override fun commitText(text: CharSequence, newCursorPosition: Int): Boolean {
            nativeCommitText(text.toString())
            return true
        }

        override fun setComposingText(text: CharSequence, newCursorPosition: Int): Boolean {
            nativeSetComposingText(text.toString())
            return true
        }

        override fun finishComposingText(): Boolean {
            nativeFinishComposingText()
            return true
        }

        override fun deleteSurroundingText(beforeLength: Int, afterLength: Int): Boolean {
            nativeDeleteSurroundingText(beforeLength, afterLength)
            return true
        }
    }

    companion object {
        const val TOUCH_STARTED = 0
        const val TOUCH_MOVED = 1
        const val TOUCH_ENDED = 2
        const val TOUCH_CANCELLED = 3

        const val LIFECYCLE_ACTIVE = 0
        const val LIFECYCLE_BACKGROUND = 1

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
    }
}
