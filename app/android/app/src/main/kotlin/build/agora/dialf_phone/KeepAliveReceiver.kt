package build.agora.dialf_phone

import android.app.AlarmManager
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import androidx.core.content.ContextCompat

/**
 * Revives [ConnForegroundService] if anything killed it — a crash, a system kill, an FGS
 * timeout. Two triggers land here:
 *
 * - A self-re-arming [AlarmManager] chain ([ACTION_TICK], every ~15 min): alarms live in
 *   the SYSTEM, so they survive app-process death, and an explicit broadcast to a manifest
 *   receiver is always deliverable. This is the one revival path that always fires.
 * - MY_PACKAGE_REPLACED after an app update.
 *
 * Gated by the user's "keep running" preference. The alarm re-arms even when the service
 * start fails, so one blocked start never breaks the chain.
 */
class KeepAliveReceiver : BroadcastReceiver() {
    companion object {
        const val ACTION_TICK = "build.agora.dialf_phone.KEEPALIVE_TICK"
        private const val INTERVAL_MS = 15 * 60_000L
        private const val REQUEST_CODE = 2

        /** (Re-)arm the next tick. Idempotent — UPDATE_CURRENT replaces the pending alarm. */
        fun schedule(ctx: Context) {
            val tick = Intent(ctx, KeepAliveReceiver::class.java).setAction(ACTION_TICK)
            val pi = PendingIntent.getBroadcast(
                ctx,
                REQUEST_CODE,
                tick,
                PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
            )
            // AllowWhileIdle: fires in Doze maintenance windows too.
            ctx.getSystemService(AlarmManager::class.java)?.setAndAllowWhileIdle(
                AlarmManager.RTC_WAKEUP,
                System.currentTimeMillis() + INTERVAL_MS,
                pi,
            )
        }
    }

    override fun onReceive(context: Context, intent: Intent) {
        val prefs = context.getSharedPreferences(ConnForegroundService.PREFS, Context.MODE_PRIVATE)
        if (!prefs.getBoolean("enabled", false)) return
        if (!prefs.getBoolean("keep_running", true)) return
        schedule(context) // keep the chain alive no matter what happens below
        try {
            ContextCompat.startForegroundService(
                context,
                Intent(context, ConnForegroundService::class.java),
            )
        } catch (_: Exception) {
            // Background FGS start can be blocked when the app isn't battery-exempt; the
            // next tick (or boot / incoming call / app open) retries.
        }
    }
}
