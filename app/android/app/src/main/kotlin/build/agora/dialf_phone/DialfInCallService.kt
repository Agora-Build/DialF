package build.agora.dialf_phone

import android.telecom.Call
import android.telecom.InCallService

/**
 * Bound by the system while this app is the default dialer. Tracks each [Call], reports
 * state changes to Dart via [Dialf], and is the handle through which we answer/hang up.
 */
class DialfInCallService : InCallService() {

    private val callbacks = HashMap<Call, Call.Callback>()

    override fun onCreate() {
        super.onCreate()
        Dialf.inCallService = this
    }

    override fun onDestroy() {
        Dialf.inCallService = null
        super.onDestroy()
    }

    override fun onCallAdded(call: Call) {
        val cb = object : Call.Callback() {
            override fun onStateChanged(c: Call, state: Int) {
                Dialf.emitCallState(c)
            }
        }
        callbacks[call] = cb
        call.registerCallback(cb)
        Dialf.emitCallState(call) // initial state (often RINGING or DIALING)
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
