package build.agora.dialf_phone

import android.content.Context
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.provider.Telephony
import android.telecom.TelecomManager
import android.telecom.VideoProfile
import android.telephony.SmsManager

/**
 * Telephony primitives, callable from both the UI (MainActivity) and the headless
 * foreground service (ConnForegroundService). Call control goes through the bound
 * [DialfInCallService] (tracked in [Dialf]); placing calls + SMS use system services.
 */
object Telecom {

    fun isDefaultDialer(ctx: Context): Boolean {
        val rm = ctx.getSystemService(android.app.role.RoleManager::class.java)
        return rm?.isRoleHeld(android.app.role.RoleManager.ROLE_DIALER) == true
    }

    fun placeCall(ctx: Context, number: String) {
        val tm = ctx.getSystemService(TelecomManager::class.java)
        tm?.placeCall(Uri.fromParts("tel", number, null), Bundle())
    }

    /** Answer the ringing call (specific id, or the current ringing one). */
    fun answer(callId: String?) {
        val c = Dialf.call(callId) ?: Dialf.ringingCall()
            ?: throw IllegalStateException("no call to answer")
        c.answer(VideoProfile.STATE_AUDIO_ONLY)
    }

    /** Hang up a call (specific id, or the current one). */
    fun hangup(callId: String?) {
        val c = Dialf.call(callId) ?: throw IllegalStateException("no call to hang up")
        c.disconnect()
    }

    fun sendSms(ctx: Context, to: String, body: String) {
        val sms = smsManager(ctx)
        val parts = sms.divideMessage(body)
        if (parts.size > 1) {
            sms.sendMultipartTextMessage(to, null, parts, null, null)
        } else {
            sms.sendTextMessage(to, null, body, null, null)
        }
    }

    /** Read recent SMS from the provider (newest first). */
    fun listSms(ctx: Context, limit: Int): List<Map<String, Any?>> {
        val out = ArrayList<Map<String, Any?>>()
        val proj = arrayOf(
            Telephony.Sms.ADDRESS,
            Telephony.Sms.BODY,
            Telephony.Sms.DATE,
            Telephony.Sms.TYPE,
        )
        ctx.contentResolver.query(
            Telephony.Sms.CONTENT_URI,
            proj,
            null,
            null,
            "${Telephony.Sms.DATE} DESC LIMIT $limit",
        )?.use { c ->
            val ai = c.getColumnIndexOrThrow(Telephony.Sms.ADDRESS)
            val bi = c.getColumnIndexOrThrow(Telephony.Sms.BODY)
            val di = c.getColumnIndexOrThrow(Telephony.Sms.DATE)
            val ti = c.getColumnIndexOrThrow(Telephony.Sms.TYPE)
            while (c.moveToNext()) {
                val sent = c.getInt(ti) == Telephony.Sms.MESSAGE_TYPE_SENT
                out.add(
                    mapOf(
                        "from" to c.getString(ai),
                        "body" to c.getString(bi),
                        "ts" to c.getLong(di),
                        "direction" to if (sent) "out" else "in",
                    )
                )
            }
        }
        return out
    }

    private fun smsManager(ctx: Context): SmsManager =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            ctx.getSystemService(SmsManager::class.java)
        } else {
            @Suppress("DEPRECATION")
            SmsManager.getDefault()
        }
}
