package build.agora.dialf_phone

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import androidx.core.content.ContextCompat

/** Starts the control-plane service on boot when the user has enabled it. */
class BootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action != Intent.ACTION_BOOT_COMPLETED) return
        val prefs = context.getSharedPreferences(ConnForegroundService.PREFS, Context.MODE_PRIVATE)
        if (!prefs.getBoolean("enabled", false)) return
        ContextCompat.startForegroundService(
            context,
            Intent(context, ConnForegroundService::class.java),
        )
    }
}
