package build.agora.dialf_phone

import android.os.Handler
import android.os.Looper
import android.telecom.Call
import io.flutter.plugin.common.EventChannel

/**
 * Process-wide bridge between the Telecom components (InCallService) and the Flutter
 * side. Holds the current calls and forwards call-state events to Dart.
 */
object Dialf {
    private val main = Handler(Looper.getMainLooper())

    /** Event sink to Dart (set by MainActivity's EventChannel; UI status only). */
    @Volatile
    var eventSink: EventChannel.EventSink? = null

    /** Listener for the headless foreground service (forwards events over WS). Runs
     *  independent of the UI, so call/SMS events reach dialfd even when locked. */
    @Volatile
    var serviceListener: ((Map<String, Any?>) -> Unit)? = null

    private var seq = 1
    private val idToCall = HashMap<String, Call>()
    private val callToId = HashMap<Call, String>()

    @Synchronized
    fun idFor(call: Call): String {
        callToId[call]?.let { return it }
        val id = "ac-${seq++}"
        callToId[call] = id
        idToCall[id] = call
        return id
    }

    @Synchronized
    fun forget(call: Call) {
        val id = callToId.remove(call)
        if (id != null) idToCall.remove(id)
    }

    @Synchronized
    fun call(id: String?): Call? {
        if (id != null) idToCall[id]?.let { return it }
        // Fall back to the single tracked call (typical for one active/ringing call).
        return idToCall.values.firstOrNull()
    }

    @Synchronized
    fun ringingCall(): Call? =
        idToCall.values.firstOrNull { it.state == Call.STATE_RINGING }

    /** Emit an event to the Dart UI (if alive) and the headless service (if running). */
    fun emit(event: Map<String, Any?>) {
        main.post { eventSink?.success(event) }
        serviceListener?.invoke(event)
    }

    fun emitCallState(call: Call) {
        val id = idFor(call)
        val state = when (call.state) {
            Call.STATE_RINGING -> "ringing"
            // Outbound call placed but not yet answered — keep distinct from "active" so the
            // daemon's call.wait_answered can block until the callee actually picks up.
            Call.STATE_DIALING, Call.STATE_CONNECTING -> "dialing"
            Call.STATE_DISCONNECTED -> "ended"
            else -> "active" // active / holding
        }
        val details = call.details
        val number = details?.handle?.schemeSpecificPart
        val direction = when (details?.callDirection) {
            Call.Details.DIRECTION_INCOMING -> "in"
            Call.Details.DIRECTION_OUTGOING -> "out"
            else -> "in"
        }
        emit(
            mapOf(
                "type" to "call_state",
                "call_id" to id,
                "state" to state,
                "number" to number,
                "direction" to direction,
            )
        )
    }
}
