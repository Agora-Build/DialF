package build.agora.dialf_phone

import android.app.role.RoleManager
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.provider.Telephony
import android.telecom.TelecomManager
import android.telecom.VideoProfile
import android.telephony.SmsManager
import androidx.core.content.ContextCompat
import io.flutter.embedding.android.FlutterActivity
import io.flutter.embedding.engine.FlutterEngine
import io.flutter.plugin.common.EventChannel
import io.flutter.plugin.common.MethodChannel

class MainActivity : FlutterActivity() {

    private val methodChannelName = "dialf/telecom"
    private val eventChannelName = "dialf/events"
    private val reqRole = 4711

    override fun configureFlutterEngine(flutterEngine: FlutterEngine) {
        super.configureFlutterEngine(flutterEngine)

        EventChannel(flutterEngine.dartExecutor.binaryMessenger, eventChannelName)
            .setStreamHandler(object : EventChannel.StreamHandler {
                override fun onListen(arguments: Any?, events: EventChannel.EventSink?) {
                    Dialf.eventSink = events
                }

                override fun onCancel(arguments: Any?) {
                    Dialf.eventSink = null
                }
            })

        MethodChannel(flutterEngine.dartExecutor.binaryMessenger, methodChannelName)
            .setMethodCallHandler { call, result ->
                try {
                    when (call.method) {
                        "isDefaultDialer" -> result.success(isDefaultDialer())
                        "requestDialerRole" -> {
                            requestDialerRole()
                            result.success(null)
                        }
                        "placeCall" -> {
                            placeCall(call.argument<String>("number")!!)
                            result.success(null)
                        }
                        "answer" -> {
                            val id = call.argument<String>("call_id")
                            val c = Dialf.call(id) ?: Dialf.ringingCall()
                            if (c == null) {
                                result.error("no_call", "no call to answer", null)
                            } else {
                                c.answer(VideoProfile.STATE_AUDIO_ONLY)
                                result.success(null)
                            }
                        }
                        "hangup" -> {
                            val id = call.argument<String>("call_id")
                            val c = Dialf.call(id)
                            if (c == null) {
                                result.error("no_call", "no call to hang up", null)
                            } else {
                                c.disconnect()
                                result.success(null)
                            }
                        }
                        "sendSms" -> {
                            sendSms(
                                call.argument<String>("to")!!,
                                call.argument<String>("body")!!,
                            )
                            result.success(null)
                        }
                        "listSms" -> result.success(listSms(call.argument<Int>("limit") ?: 20))
                        "startService" -> {
                            ContextCompat.startForegroundService(
                                this,
                                Intent(this, ConnForegroundService::class.java),
                            )
                            result.success(null)
                        }
                        "stopService" -> {
                            stopService(Intent(this, ConnForegroundService::class.java))
                            result.success(null)
                        }
                        else -> result.notImplemented()
                    }
                } catch (e: Exception) {
                    result.error("telecom_error", e.message, null)
                }
            }
    }

    private fun isDefaultDialer(): Boolean {
        val rm = getSystemService(RoleManager::class.java)
        return rm?.isRoleHeld(RoleManager.ROLE_DIALER) == true
    }

    private fun requestDialerRole() {
        val rm = getSystemService(RoleManager::class.java) ?: return
        if (rm.isRoleAvailable(RoleManager.ROLE_DIALER) && !rm.isRoleHeld(RoleManager.ROLE_DIALER)) {
            startActivityForResult(rm.createRequestRoleIntent(RoleManager.ROLE_DIALER), reqRole)
        } else {
            Dialf.emit(
                mapOf("type" to "dialer_role", "granted" to rm.isRoleHeld(RoleManager.ROLE_DIALER))
            )
        }
    }

    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode == reqRole) {
            Dialf.emit(mapOf("type" to "dialer_role", "granted" to (resultCode == RESULT_OK)))
        }
    }

    private fun placeCall(number: String) {
        val tm = getSystemService(TelecomManager::class.java)
        val uri = Uri.fromParts("tel", number, null)
        tm?.placeCall(uri, Bundle())
    }

    private fun smsManager(): SmsManager =
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
            getSystemService(SmsManager::class.java)
        } else {
            @Suppress("DEPRECATION")
            SmsManager.getDefault()
        }

    private fun sendSms(to: String, body: String) {
        val sms = smsManager()
        val parts = sms.divideMessage(body)
        if (parts.size > 1) {
            sms.sendMultipartTextMessage(to, null, parts, null, null)
        } else {
            sms.sendTextMessage(to, null, body, null, null)
        }
    }

    private fun listSms(limit: Int): List<Map<String, Any?>> {
        val out = ArrayList<Map<String, Any?>>()
        val proj = arrayOf(
            Telephony.Sms.ADDRESS,
            Telephony.Sms.BODY,
            Telephony.Sms.DATE,
            Telephony.Sms.TYPE,
        )
        contentResolver.query(
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
                val type = c.getInt(ti)
                out.add(
                    mapOf(
                        "from" to c.getString(ai),
                        "body" to c.getString(bi),
                        "ts" to c.getLong(di),
                        "direction" to if (type == Telephony.Sms.MESSAGE_TYPE_SENT) "out" else "in",
                    )
                )
            }
        }
        return out
    }
}
