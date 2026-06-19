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

    /** Event sink to Dart (set by MainActivity's EventChannel). */
    @Volatile
    var eventSink: EventChannel.EventSink? = null

    /** The bound InCallService, if any (set while the system has us as default dialer). */
    @Volatile
    var inCallService: DialfInCallService? = null

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

    /** Emit a JSON-ish map event to Dart on the main thread. */
    fun emit(event: Map<String, Any?>) {
        main.post { eventSink?.success(event) }
    }

    fun emitCallState(call: Call) {
        val id = idFor(call)
        val state = when (call.state) {
            Call.STATE_RINGING -> "ringing"
            Call.STATE_DISCONNECTED -> "ended"
            else -> "active" // dialing/connecting/active/holding
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
