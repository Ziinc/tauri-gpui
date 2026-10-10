package app.tauri.gpui

import android.app.Activity
import android.graphics.Color
import androidx.appcompat.app.AppCompatActivity
import androidx.core.view.WindowCompat
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Plugin

/**
 * Installs a [GpuiView] as the activity's content view. Tauri instantiates
 * this class on the UI thread when tauri-plugin-gpui initializes on Android.
 * The app draws edge to edge; GPUI reads the system bar and keyboard
 * regions through its window insets.
 */
@TauriPlugin
class GpuiPlugin(private val activity: Activity) : Plugin(activity) {
    private val view = GpuiView(activity)

    init {
        WindowCompat.setDecorFitsSystemWindows(activity.window, false)
        @Suppress("DEPRECATION")
        activity.window.statusBarColor = Color.TRANSPARENT
        @Suppress("DEPRECATION")
        activity.window.navigationBarColor = Color.TRANSPARENT
        activity.setContentView(view)
        view.requestFocus()
    }

    override fun onResume(activity: AppCompatActivity) {
        view.lifecycle(GpuiView.LIFECYCLE_ACTIVE)
    }

    override fun onPause(activity: AppCompatActivity) {
        view.lifecycle(GpuiView.LIFECYCLE_BACKGROUND)
    }
}
