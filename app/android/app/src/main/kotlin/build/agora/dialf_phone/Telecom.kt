package build.agora.dialf_phone

import android.content.Context
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.provider.CallLog
import android.provider.Telephony
import android.telecom.TelecomManager
import android.telecom.VideoProfile
import android.telephony.SmsManager
import android.telephony.SubscriptionManager

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

    /** Place a call; if [simSubId] is given, route it to that SIM, else the system default. */
    fun placeCall(ctx: Context, number: String, simSubId: Int?) {
        val tm = ctx.getSystemService(TelecomManager::class.java) ?: return
        val extras = Bundle()
        if (simSubId != null) {
            val handle = tm.callCapablePhoneAccounts.firstOrNull { it.id == simSubId.toString() }
                ?: throw IllegalArgumentException("no SIM with subscription id $simSubId")
            extras.putParcelable(TelecomManager.EXTRA_PHONE_ACCOUNT_HANDLE, handle)
        }
        tm.placeCall(Uri.fromParts("tel", number, null), extras)
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

    /** Decline a ringing call (specific id, or the current ringing one). */
    fun reject(callId: String?) {
        val c = Dialf.call(callId) ?: Dialf.ringingCall()
            ?: throw IllegalStateException("no call to reject")
        c.reject(false, null)
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

    /** Read the recent call log from the provider (newest first). */
    fun listCallLog(ctx: Context, limit: Int): List<Map<String, Any?>> {
        val out = ArrayList<Map<String, Any?>>()
        val proj = arrayOf(
            CallLog.Calls.NUMBER,
            CallLog.Calls.TYPE,
            CallLog.Calls.DATE,
            CallLog.Calls.DURATION,
        )
        ctx.contentResolver.query(
            CallLog.Calls.CONTENT_URI,
            proj,
            null,
            null,
            "${CallLog.Calls.DATE} DESC LIMIT $limit",
        )?.use { c ->
            val ni = c.getColumnIndexOrThrow(CallLog.Calls.NUMBER)
            val ti = c.getColumnIndexOrThrow(CallLog.Calls.TYPE)
            val di = c.getColumnIndexOrThrow(CallLog.Calls.DATE)
            val ui = c.getColumnIndexOrThrow(CallLog.Calls.DURATION)
            while (c.moveToNext()) {
                val kind = when (c.getInt(ti)) {
                    CallLog.Calls.INCOMING_TYPE -> "incoming"
                    CallLog.Calls.OUTGOING_TYPE -> "outgoing"
                    CallLog.Calls.MISSED_TYPE -> "missed"
                    CallLog.Calls.VOICEMAIL_TYPE -> "voicemail"
                    CallLog.Calls.REJECTED_TYPE -> "rejected"
                    CallLog.Calls.BLOCKED_TYPE -> "blocked"
                    else -> "unknown"
                }
                out.add(
                    mapOf(
                        "number" to c.getString(ni),
                        "kind" to kind,
                        "ts" to c.getLong(di),
                        "duration" to c.getLong(ui),
                    )
                )
            }
        }
        return out
    }

    /** List the device's active SIMs (slot, subscription id, name, carrier, number). */
    fun listSims(ctx: Context): List<Map<String, Any?>> {
        val out = ArrayList<Map<String, Any?>>()
        val sm = ctx.getSystemService(SubscriptionManager::class.java) ?: return out
        val subs = try {
            sm.activeSubscriptionInfoList
        } catch (_: SecurityException) {
            null
        } ?: return out
        val defaultVoice = SubscriptionManager.getDefaultVoiceSubscriptionId()
        for (s in subs) {
            out.add(
                mapOf(
                    "slot" to s.simSlotIndex,
                    "sub_id" to s.subscriptionId,
                    "name" to s.displayName?.toString()?.ifBlank { null },
                    "carrier" to s.carrierName?.toString()?.ifBlank { null },
                    "number" to s.number?.ifBlank { null },
                    "is_default" to (s.subscriptionId == defaultVoice),
                )
            )
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
