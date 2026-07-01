package build.agora.dialf_phone

import android.app.role.RoleManager
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.PowerManager
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
                    // Replay the current connection status so a freshly-opened UI reflects the
                    // live connection — the service usually connected before this subscription
                    // existed, and EventChannel doesn't replay past events.
                    Dialf.lastStatus?.let { events?.success(it) }
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
                        "getWiredHeadset" -> result.success(prefs().getBoolean("wired_headset", true))
                        "setWiredHeadset" -> {
                            val wired = call.argument<Boolean>("wired") ?: true
                            prefs().edit().putBoolean("wired_headset", wired).apply()
                            DialfInCallService.applyRoute(wired)
                            result.success(null)
                        }
                        "getKeepRunning" -> result.success(prefs().getBoolean("keep_running", true))
                        "setKeepRunning" -> {
                            val keep = call.argument<Boolean>("keep") ?: true
                            prefs().edit().putBoolean("keep_running", keep).apply()
                            if (keep) {
                                requestIgnoreBatteryOptimizations()
                                if (prefs().getBoolean("enabled", false)) {
                                    ContextCompat.startForegroundService(this, serviceIntent())
                                }
                            }
                            result.success(null)
                        }
                        "isServiceEnabled" -> result.success(prefs().getBoolean("enabled", false))
                        "appVersion" -> result.success(appVersion())
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

        // (Re)start the control-plane service when the app opens if enabled — so it keeps
        // running/reconnecting. `--ez start true` force-enables it headlessly. Auto-start on
        // open is gated by "keep running"; the explicit Start button always works.
        val keep = prefs().getBoolean("keep_running", true)
        val forceStart = intent?.getBooleanExtra("start", false) == true
        if (forceStart) setEnabled(true)
        if (forceStart || (prefs().getBoolean("enabled", false) && keep)) {
            ContextCompat.startForegroundService(this, serviceIntent())
        }
        if (keep && prefs().getBoolean("enabled", false)) requestIgnoreBatteryOptimizations()
    }

    /** Ask the OS to exempt us from battery optimization, so the service isn't killed and
     *  background (re)starts from broadcasts are allowed. No-op if already exempt. */
    private fun requestIgnoreBatteryOptimizations() {
        val pm = getSystemService(PowerManager::class.java) ?: return
        if (pm.isIgnoringBatteryOptimizations(packageName)) return
        try {
            startActivity(
                Intent(
                    Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS,
                    Uri.parse("package:$packageName"),
                )
            )
        } catch (_: Exception) {}
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

    /** "<versionName>(<versionCode>)" for the title, e.g. "0.1.18(123)". */
    private fun appVersion(): String = try {
        val pi = packageManager.getPackageInfo(packageName, 0)
        val code = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) pi.longVersionCode
        else @Suppress("DEPRECATION") pi.versionCode.toLong()
        "${pi.versionName}($code)"
    } catch (e: Exception) {
        ""
    }

    private fun requestDialerRole() {
        val rm = getSystemService(RoleManager::class.java) ?: return
        // Gate on the authoritative check (Telecom.isDefaultDialer), not RoleManager.isRoleHeld,
        // which can be stale after a reinstall — otherwise we'd skip the prompt and wrongly
        // report "granted" while the app isn't actually the default dialer.
        if (rm.isRoleAvailable(RoleManager.ROLE_DIALER) && !Telecom.isDefaultDialer(this)) {
            startActivityForResult(rm.createRequestRoleIntent(RoleManager.ROLE_DIALER), reqRole)
        } else {
            Dialf.emit(mapOf("type" to "dialer_role", "granted" to Telecom.isDefaultDialer(this)))
        }
    }

    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode == reqRole) {
            // Re-read the real role rather than trusting the result code.
            Dialf.emit(mapOf("type" to "dialer_role", "granted" to Telecom.isDefaultDialer(this)))
        }
    }

    override fun onResume() {
        super.onResume()
        // Re-sync the dialer-role status every time the app is shown, so a role lost to a
        // reinstall (or revoked in Settings) is reflected immediately instead of staying "✓".
        Dialf.emit(mapOf("type" to "dialer_role", "granted" to Telecom.isDefaultDialer(this)))
    }
}
