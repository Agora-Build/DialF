package build.agora.dialf_phone

import android.content.Context
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.provider.CallLog
import android.provider.Telephony
import android.telecom.TelecomManager
import android.telecom.VideoProfile
import android.telephony.SmsManager
import android.telephony.SubscriptionManager
import android.telephony.TelephonyManager

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
        // Encode so MMI/USSD codes survive ('#' -> %23); '+' kept for international numbers.
        val uri = Uri.parse("tel:" + Uri.encode(number, "+"))
        tm.placeCall(uri, extras)
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

    /**
     * Enable/disable carrier voicemail on a SIM. The host only sends the intent
     * (`enabled`/optional `number`); mapping it to the platform mechanism is the device's
     * job. On Android that's GSM supplementary-service MMI codes for conditional
     * call-forwarding (all conditions, SC 004):
     *   - disable:  `#004#`   (caller no longer forwarded to voicemail)
     *   - enable:   `**004*<number>#` to (re)register a target, else `*004#` to reactivate
     */
    fun setVoicemail(
        ctx: Context,
        enabled: Boolean,
        number: String?,
        simSubId: Int?,
        onResult: (Boolean, String?) -> Unit,
    ) {
        if (enabled) {
            // Re-enable: register a target if given, else reactivate all conditional CF.
            val code = if (number != null) "**004*$number#" else "*004#"
            sendMmi(ctx, code, simSubId, onResult)
            return
        }
        // Disable: carriers vary, so try the standard erase/deactivate codes one by one and
        // report each. 004=all conditional CF, 002=all CF, 21=unconditional CF.
        val codes = listOf("#004#", "##002#", "##21#")
        val log = StringBuilder()
        fun tryNext(i: Int, anyOk: Boolean) {
            if (i >= codes.size) {
                onResult(anyOk, log.toString().trim())
                return
            }
            val c = codes[i]
            sendMmi(ctx, c, simSubId) { ok, resp ->
                log.append(c).append(": ").append(if (ok) "ok" else "failed")
                if (resp != null) log.append(" — ").append(resp.replace("\n", " "))
                log.append('\n')
                tryNext(i + 1, anyOk || ok)
            }
        }
        tryNext(0, false)
    }

    /** Low-level: run an MMI/USSD code on a SIM and deliver the network reply via [onResult]. */
    fun sendMmi(ctx: Context, code: String, simSubId: Int?, onResult: (Boolean, String?) -> Unit) {
        var tm = ctx.getSystemService(TelephonyManager::class.java)
            ?: return onResult(false, "no telephony service")
        val sub = simSubId ?: SubscriptionManager.getDefaultVoiceSubscriptionId()
        if (sub != SubscriptionManager.INVALID_SUBSCRIPTION_ID) {
            tm = tm.createForSubscriptionId(sub)
        }
        val cb = object : TelephonyManager.UssdResponseCallback() {
            override fun onReceiveUssdResponse(t: TelephonyManager, request: String, response: CharSequence) {
                onResult(true, response.toString())
            }
            override fun onReceiveUssdResponseFailed(t: TelephonyManager, request: String, failureCode: Int) {
                onResult(false, "request failed (code $failureCode)")
            }
        }
        tm.sendUssdRequest(code, cb, Handler(Looper.getMainLooper()))
    }

    private fun smsManager(ctx: Context): SmsManager =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            ctx.getSystemService(SmsManager::class.java)
        } else {
            @Suppress("DEPRECATION")
            SmsManager.getDefault()
        }
}
