package build.agora.dialf_phone

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.provider.Telephony

/**
 * Forwards inbound SMS to Dart (and onward to dialfd) in real time. Requires the
 * RECEIVE_SMS permission; fires for every received message (not just for the default SMS
 * app). Multipart messages are concatenated by sender.
 */
class SmsReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action != Telephony.Sms.Intents.SMS_RECEIVED_ACTION) return
        val msgs = Telephony.Sms.Intents.getMessagesFromIntent(intent) ?: return
        if (msgs.isEmpty()) return

        val from = msgs[0].originatingAddress
        val body = msgs.joinToString("") { it.messageBody ?: "" }
        val ts = msgs[0].timestampMillis

        Dialf.emit(
            mapOf(
                "type" to "sms",
                "direction" to "in",
                "from" to from,
                "body" to body,
                "ts" to ts,
            )
        )
    }
}
