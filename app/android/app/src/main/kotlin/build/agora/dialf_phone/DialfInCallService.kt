package build.agora.dialf_phone

import android.content.Context
import android.content.Intent
import android.telecom.Call
import android.telecom.CallAudioState
import android.telecom.InCallService
import android.util.Log
import androidx.core.content.ContextCompat

/**
 * Bound by the system while this app is the default dialer. Tracks each [Call] in [Dialf],
 * reports state changes, and is the handle through which we answer/hang up. Also pins the
 * call audio route to the wired headset (the USB sound-card bridge) when enabled.
 */
class DialfInCallService : InCallService() {

    companion object {
        private const val TAG = "DialfInCall"

        /** Live instance, so the UI can re-apply the audio route on an active call. */
        @Volatile
        var instance: DialfInCallService? = null

        /** Apply the preferred call-audio route (wired headset for the bridge, else earpiece). */
        fun applyRoute(wired: Boolean) {
            instance?.setAudioRoute(
                if (wired) CallAudioState.ROUTE_WIRED_HEADSET else CallAudioState.ROUTE_EARPIECE
            )
        }
    }

    private val callbacks = HashMap<Call, Call.Callback>()

    override fun onCreate() {
        super.onCreate()
        instance = this
    }

    override fun onDestroy() {
        instance = null
        super.onDestroy()
    }

    override fun onCallAdded(call: Call) {
        val cb = object : Call.Callback() {
            override fun onStateChanged(c: Call, state: Int) {
                Dialf.emitCallState(c)
                // A call that rings after being added (some devices add it before RINGING): make
                // sure the dialfd link is alive so the daemon hears it and can auto-answer.
                if (state == Call.STATE_RINGING) ensureControlPlaneForIncomingCall()
                if (state == Call.STATE_ACTIVE || state == Call.STATE_DIALING) routeForCall()
            }
        }
        callbacks[call] = cb
        call.registerCallback(cb)
        // Track + report the call first (over the current socket, if alive) so it's in Dialf's
        // registry before any reconnect — then onOpen's re-report can find it if the socket was dead.
        Dialf.emitCallState(call) // initial state (often RINGING or DIALING)
        // An inbound call rings even while the phone is dozing, when the dialfd socket may be stale.
        // Nudge the connection service to verify/rebuild the link so the ringing call is reported
        // (and can be auto-answered). The screen need not turn on — only the CPU, which is up now.
        if (call.state == Call.STATE_RINGING) ensureControlPlaneForIncomingCall()
        routeForCall()
    }

    /**
     * Incoming calls are delivered to the InCallService even if Android killed our foreground
     * connection service. Restart it first, then nudge any live instance to verify/rebuild the
     * dialfd link; onOpen will re-report the tracked ringing call if the initial event was lost.
     */
    private fun ensureControlPlaneForIncomingCall() {
        val prefs = getSharedPreferences(ConnForegroundService.PREFS, Context.MODE_PRIVATE)
        if (prefs.getBoolean("enabled", false)) {
            try {
                ContextCompat.startForegroundService(
                    this,
                    Intent(this, ConnForegroundService::class.java),
                )
            } catch (e: Exception) {
                Log.w(TAG, "incoming ring: failed to start control-plane service", e)
            }
        }
        ConnForegroundService.onIncomingCall()
    }

    /** Route call audio to the wired headset bridge by default (toggleable in the UI). */
    private fun routeForCall() {
        val wired = getSharedPreferences(ConnForegroundService.PREFS, Context.MODE_PRIVATE)
            .getBoolean("wired_headset", true)
        if (wired) setAudioRoute(CallAudioState.ROUTE_WIRED_HEADSET)
    }

    override fun onCallRemoved(call: Call) {
        callbacks.remove(call)?.let { call.unregisterCallback(it) }
        // Ensure Dart sees the terminal state, then forget the mapping.
        Dialf.emit(
            mapOf(
                "type" to "call_state",
                "call_id" to Dialf.idFor(call),
                "state" to "ended",
                "number" to call.details?.handle?.schemeSpecificPart,
                "direction" to "in",
            )
        )
        Dialf.forget(call)
    }
}
