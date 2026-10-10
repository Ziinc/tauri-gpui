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
 *
 * The plugin outlives its activity: a configuration change the manifest does
 * not handle (an overlay or asset path change, for one) recreates the
 * activity with a WebView as its content, so the view is reinstalled on
 * whichever activity resumes.
 */
@TauriPlugin
class GpuiPlugin(activity: Activity) : Plugin(activity) {
    private var host: Activity = activity
    private var view = install(activity)

    private fun install(activity: Activity): GpuiView {
        val view = GpuiView(activity)
        WindowCompat.setDecorFitsSystemWindows(activity.window, false)
        @Suppress("DEPRECATION")
        activity.window.statusBarColor = Color.TRANSPARENT
        @Suppress("DEPRECATION")
        activity.window.navigationBarColor = Color.TRANSPARENT
        activity.setContentView(view)
        view.requestFocus()
        return view
    }

    override fun onResume(activity: AppCompatActivity) {
        if (activity !== host) {
            host = activity
            view = install(activity)
        }
        view.lifecycle(GpuiView.LIFECYCLE_ACTIVE)
    }

    override fun onPause(activity: AppCompatActivity) {
        view.lifecycle(GpuiView.LIFECYCLE_BACKGROUND)
    }
}
