package build.agora.dialf_phone

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import androidx.core.content.ContextCompat

/**
 * Re-ensures the control-plane service is running whenever the system hands us a broadcast
 * (power connected/disconnected, battery low/okay, wifi state change, app updated). This
 * keeps DialF up as long as possible. Gated by the user's "keep running" preference — if it's
 * off, we never (re)launch from here.
 */
class KeepAliveReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val prefs = context.getSharedPreferences(ConnForegroundService.PREFS, Context.MODE_PRIVATE)
        if (!prefs.getBoolean("enabled", false)) return
        if (!prefs.getBoolean("keep_running", true)) return
        try {
            ContextCompat.startForegroundService(
                context,
                Intent(context, ConnForegroundService::class.java),
            )
        } catch (_: Exception) {
            // Background FGS start can be blocked on newer Android; ignore — other
            // triggers (boot, app open, onTaskRemoved) will recover it.
        }
    }
}
