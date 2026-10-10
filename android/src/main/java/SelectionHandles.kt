package app.tauri.gpui

import android.content.Context
import android.graphics.drawable.Drawable
import android.util.Log
import android.view.Gravity
import android.view.MotionEvent
import android.view.View
import android.view.WindowManager
import android.widget.PopupWindow
import androidx.core.widget.PopupWindowCompat

/**
 * The teardrop handles at the ends of a selection (or under a lone caret),
 * drawn by the platform instead of by GPUI and placed as Android's own text
 * fields place them.
 *
 * Each handle is a [PopupWindow] anchored to the [GpuiView] that takes its own
 * touches, so the view never sees them. A drag is reported to Rust as the
 * point where the caret should go; Rust moves the selection and sends the new
 * handle positions back through [show]. UI thread only.
 */
internal class SelectionHandles(
    private val view: View,
    // The finger went down on a handle: the toolbar must get out of the way.
    private val onGrab: () -> Unit,
) {
    private val handles = arrayOf(
        Handle(HANDLE_START, android.R.attr.textSelectHandleLeft, 0.75f, "Selection start"),
        Handle(HANDLE_END, android.R.attr.textSelectHandleRight, 0.25f, "Selection end"),
        Handle(HANDLE_INSERTION, android.R.attr.textSelectHandle, 0.5f, "Text cursor"),
    )
    private val start get() = handles[HANDLE_START]
    private val end get() = handles[HANDLE_END]
    private val insertion get() = handles[HANDLE_INSERTION]

    // What Rust last asked for, so the handles can come back after the window
    // has been in the background.
    private var wanted: Request? = null
    private var suspended = false
    private var lit = false
    private var paired = false
    private val location = IntArray(2)
    private val screen = IntArray(2)

    private class Request(
        val startX: Float,
        val startY: Float,
        val endX: Float,
        val endY: Float,
        val lineHeight: Float,
        val collapsed: Boolean,
    )

    /**
     * Shows the handles for a selection from (startX, startY) to (endX, endY),
     * the bottoms of its two carets in view pixels, or only the insertion
     * handle at the start when [collapsed]. A handle that is being dragged
     * keeps showing wherever it is.
     */
    fun show(
        startX: Float,
        startY: Float,
        endX: Float,
        endY: Float,
        lineHeight: Float,
        collapsed: Boolean,
    ) {
        val request = Request(startX, startY, endX, endY, lineHeight, collapsed)
        wanted = request
        if (!suspended) apply(request)
    }

    fun hide() {
        wanted = null
        dismissAll()
    }

    /** The window lost focus: the handles go, and return with it. */
    fun suspend(suspended: Boolean) {
        this.suspended = suspended
        if (suspended) {
            dismissAll()
        } else {
            wanted?.let { apply(it) }
        }
    }

    private fun apply(request: Request) {
        if (view.windowToken == null) return
        view.getLocationInWindow(location)
        val collapsed = request.collapsed
        start.place(!collapsed, request.startX, request.startY, request)
        end.place(!collapsed, request.endX, request.endY, request)
        insertion.place(collapsed, request.startX, request.startY, request)
        val anyShown = handles.any { it.popup?.isShowing == true }
        if (anyShown && !lit) Log.d(TAG, "selection handles shown")
        lit = anyShown
        val pair = start.popup?.isShowing == true && end.popup?.isShowing == true
        if (pair && !paired) Log.d(TAG, "selection range handles shown")
        paired = pair
    }

    private fun dismissAll() {
        handles.forEach { it.dismiss() }
        lit = false
        paired = false
    }

    private inner class Handle(
        val id: Int,
        attr: Int,
        // Where the hotspot sits across the drawable: the left handle's at its
        // right edge, the right handle's at its left, the insertion handle's
        // in the middle. It is always at the top, where the line ends.
        private val hotspot: Float,
        private val description: String,
    ) {
        private val drawable: Drawable? = view.context.theme.obtainStyledAttributes(intArrayOf(attr)).let {
            try {
                it.getDrawable(0)?.mutate()
            } finally {
                it.recycle()
            }
        }
        init {
            if (drawable == null) Log.w(TAG, "the theme has no selection handle drawable $description")
        }

        var popup: PopupWindow? = null

        // The hotspot in view pixels, and the offset of a grabbing finger from it.
        private var hotspotX = 0f
        private var hotspotY = 0f
        private var grabX = 0f
        private var grabY = 0f
        private var lineHeight = 0f
        var dragging = false
            private set

        fun place(visible: Boolean, x: Float, y: Float, request: Request) {
            val drawable = drawable
            if (!visible || drawable == null || (!dragging && !inView(x, y))) {
                dismiss()
                return
            }
            hotspotX = x
            hotspotY = y
            lineHeight = request.lineHeight
            val width = drawable.intrinsicWidth
            val height = drawable.intrinsicHeight
            val left = (location[0] + x - width * hotspot).toInt()
            val top = (location[1] + y).toInt()
            val existing = popup
            if (existing != null && existing.isShowing) {
                existing.update(left, top, width, height)
                return
            }
            val handle = HandleView(view.context)
            handle.setImageDrawable(drawable)
            handle.contentDescription = description
            val window = PopupWindow(handle, width, height).apply {
                isClippingEnabled = false
                isTouchable = true
                isFocusable = false
                isOutsideTouchable = false
            }
            PopupWindowCompat.setWindowLayoutType(
                window,
                WindowManager.LayoutParams.TYPE_APPLICATION_SUB_PANEL,
            )
            try {
                window.showAtLocation(view, Gravity.NO_GRAVITY, left, top)
                popup = window
            } catch (e: WindowManager.BadTokenException) {
                // The view's window is going away.
                Log.w(TAG, "showing a selection handle failed", e)
            }
        }

        fun dismiss() {
            dragging = false
            popup?.dismiss()
            popup = null
        }

        // A handle scrolled out of the view (or under the keyboard) is not drawn.
        private fun inView(x: Float, y: Float) = x >= 0 && x <= view.width && y >= 0 && y <= view.height

        private inner class HandleView(context: Context) : android.widget.ImageView(context) {
            override fun onTouchEvent(event: MotionEvent): Boolean {
                when (event.actionMasked) {
                    MotionEvent.ACTION_DOWN -> {
                        // Raw coordinates: the handle moves under the finger
                        // as the selection does, so its own do not hold still.
                        view.getLocationOnScreen(screen)
                        grabX = event.rawX - (screen[0] + hotspotX)
                        grabY = event.rawY - (screen[1] + hotspotY)
                        dragging = true
                        onGrab()
                        report(HANDLE_DRAG_STARTED, event)
                    }
                    MotionEvent.ACTION_MOVE -> if (dragging) report(HANDLE_DRAG_MOVED, event)
                    MotionEvent.ACTION_UP -> release(HANDLE_DRAG_ENDED, event)
                    MotionEvent.ACTION_CANCEL -> release(HANDLE_DRAG_CANCELLED, event)
                }
                return true
            }

            private fun release(phase: Int, event: MotionEvent) {
                if (!dragging) return
                report(phase, event)
                dragging = false
            }

            // The point the caret should go to: where the finger would put the
            // hotspot, a half line up so that it is on the line of text.
            private fun report(phase: Int, event: MotionEvent) {
                view.getLocationOnScreen(screen)
                val x = event.rawX - grabX - screen[0]
                val y = event.rawY - grabY - screen[1] - lineHeight / 2
                GpuiView.nativeHandleDrag(id, phase, x, y)
            }
        }
    }

    companion object {
        const val HANDLE_START = 0
        const val HANDLE_END = 1
        const val HANDLE_INSERTION = 2

        // The codes match `DragPhase::from_code` in Rust.
        const val HANDLE_DRAG_STARTED = 0
        const val HANDLE_DRAG_MOVED = 1
        const val HANDLE_DRAG_ENDED = 2
        const val HANDLE_DRAG_CANCELLED = 3

        private const val TAG = "tauri-plugin-gpui"
    }
}
