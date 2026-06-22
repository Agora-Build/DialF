package build.agora.dialf_phone

import android.app.role.RoleManager
import android.content.Context
import android.content.Intent
import androidx.core.content.ContextCompat
import io.flutter.embedding.android.FlutterActivity
import io.flutter.embedding.engine.FlutterEngine
import io.flutter.plugin.common.EventChannel
import io.flutter.plugin.common.MethodChannel

/**
 * Thin UI host: configures the control-plane service, requests the default-dialer role,
 * and relays status/events to the Flutter UI. The WebSocket + telephony control runs in
 * [ConnForegroundService] (lock-independent), not here.
 */
class MainActivity : FlutterActivity() {

    private val methodChannel = "dialf/telecom"
    private val eventChannel = "dialf/events"
    private val reqRole = 4711

    override fun configureFlutterEngine(flutterEngine: FlutterEngine) {
        super.configureFlutterEngine(flutterEngine)

        EventChannel(flutterEngine.dartExecutor.binaryMessenger, eventChannel)
            .setStreamHandler(object : EventChannel.StreamHandler {
                override fun onListen(arguments: Any?, events: EventChannel.EventSink?) {
                    Dialf.eventSink = events
                }
                override fun onCancel(arguments: Any?) {
                    Dialf.eventSink = null
                }
            })

        MethodChannel(flutterEngine.dartExecutor.binaryMessenger, methodChannel)
            .setMethodCallHandler { call, result ->
                try {
                    when (call.method) {
                        "isDefaultDialer" -> result.success(Telecom.isDefaultDialer(this))
                        "requestDialerRole" -> {
                            requestDialerRole()
                            result.success(null)
                        }
                        "saveConfig" -> {
                            saveConfig(call)
                            result.success(null)
                        }
                        "startService" -> {
                            setEnabled(true)
                            ContextCompat.startForegroundService(this, serviceIntent())
                            result.success(null)
                        }
                        "stopService" -> {
                            setEnabled(false)
                            stopService(serviceIntent())
                            result.success(null)
                        }
                        else -> result.notImplemented()
                    }
                } catch (e: Exception) {
                    result.error("error", e.message, null)
                }
            }

        // Allow `am start -n .../.MainActivity --ez start true` to launch the service
        // headlessly (also used to auto-resume on app open).
        if (intent?.getBooleanExtra("start", false) == true) {
            setEnabled(true)
            ContextCompat.startForegroundService(this, serviceIntent())
        }
    }

    private fun serviceIntent() = Intent(this, ConnForegroundService::class.java)

    private fun prefs() = getSharedPreferences(ConnForegroundService.PREFS, Context.MODE_PRIVATE)

    private fun saveConfig(call: io.flutter.plugin.common.MethodCall) {
        prefs().edit()
            .putString("device_id", call.argument<String>("device_id") ?: "phone1")
            .putString("name", call.argument<String>("name") ?: "DialF Phone")
            .putString("key", call.argument<String>("key") ?: "change-me")
            .putString("server", call.argument<String>("server") ?: "")
            .apply()
    }

    private fun setEnabled(enabled: Boolean) {
        prefs().edit().putBoolean("enabled", enabled).apply()
    }

    private fun requestDialerRole() {
        val rm = getSystemService(RoleManager::class.java) ?: return
        if (rm.isRoleAvailable(RoleManager.ROLE_DIALER) && !rm.isRoleHeld(RoleManager.ROLE_DIALER)) {
            startActivityForResult(rm.createRequestRoleIntent(RoleManager.ROLE_DIALER), reqRole)
        } else {
            Dialf.emit(mapOf("type" to "dialer_role", "granted" to rm.isRoleHeld(RoleManager.ROLE_DIALER)))
        }
    }

    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode == reqRole) {
            Dialf.emit(mapOf("type" to "dialer_role", "granted" to (resultCode == RESULT_OK)))
        }
    }
}
