package build.agora.dialf_phone

import android.app.role.RoleManager
import android.content.Context
import android.content.Intent
import android.os.Build
import android.provider.Settings
import androidx.core.content.ContextCompat
import io.flutter.embedding.android.FlutterActivity
import io.flutter.embedding.engine.FlutterEngine
import io.flutter.plugin.common.EventChannel
import io.flutter.plugin.common.MethodChannel
import kotlin.random.Random

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
                        "deviceDefaults" -> result.success(deviceDefaults())
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

        // Always (re)start the control-plane service when the app opens if it's enabled —
        // so it keeps running/reconnecting. `--ez start true` force-enables it headlessly.
        val forceStart = intent?.getBooleanExtra("start", false) == true
        if (forceStart) setEnabled(true)
        if (forceStart || prefs().getBoolean("enabled", false)) {
            ContextCompat.startForegroundService(this, serviceIntent())
        }
    }

    private fun serviceIntent() = Intent(this, ConnForegroundService::class.java)

    private fun prefs() = getSharedPreferences(ConnForegroundService.PREFS, Context.MODE_PRIVATE)

    /**
     * Suggested config for first-run UI: a friendly phone name (the user's "Device name", or
     * the brand + model) and a device id of `<slug>-<4 digits>`. Persisted on first call so
     * the random suffix stays stable across launches.
     */
    private fun deviceDefaults(): Map<String, String> {
        val p = prefs()
        val name = p.getString("name", null)?.takeIf { it.isNotBlank() } ?: detectPhoneName()
        val id = p.getString("device_id", null)?.takeIf { it.isNotBlank() } ?: deviceIdFor(name)
        p.edit().putString("name", name).putString("device_id", id).apply()
        return mapOf("device_id" to id, "name" to name)
    }

    /** The user-set "Device name", falling back to "<Manufacturer> <Model>". */
    private fun detectPhoneName(): String {
        Settings.Global.getString(contentResolver, Settings.Global.DEVICE_NAME)
            ?.takeIf { it.isNotBlank() }
            ?.let { return it }
        val mfr = Build.MANUFACTURER?.replaceFirstChar { it.uppercase() }.orEmpty()
        val model = Build.MODEL?.takeIf { it.isNotBlank() } ?: "phone"
        return if (mfr.isBlank() || model.startsWith(mfr, ignoreCase = true)) model else "$mfr $model"
    }

    /** Slugify a name and append 4 random digits, e.g. "Pixel 9 Pro" -> "pixel-9-pro-4827". */
    private fun deviceIdFor(name: String): String {
        val slug = name.lowercase()
            .replace(Regex("[^a-z0-9]+"), "-")
            .trim('-')
            .ifBlank { "phone" }
        return "$slug-${Random.nextInt(1000, 10000)}"
    }

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
