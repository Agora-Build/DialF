package build.agora.dialf_phone

import android.content.Context
import android.telecom.Call
import android.telecom.CallAudioState
import android.telecom.InCallService

/**
 * Bound by the system while this app is the default dialer. Tracks each [Call] in [Dialf],
 * reports state changes, and is the handle through which we answer/hang up. Also pins the
 * call audio route to the wired headset (the USB sound-card bridge) when enabled.
 */
class DialfInCallService : InCallService() {

    companion object {
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
                if (state == Call.STATE_ACTIVE || state == Call.STATE_DIALING) routeForCall()
            }
        }
        callbacks[call] = cb
        call.registerCallback(cb)
        Dialf.emitCallState(call) // initial state (often RINGING or DIALING)
        routeForCall()
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
