package build.agora.dialf_phone

import android.app.AlarmManager
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.net.ConnectivityManager
import android.net.Network
import android.net.nsd.NsdManager
import android.os.BatteryManager
import android.net.nsd.NsdServiceInfo
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.os.PowerManager
import android.util.Log
import okhttp3.OkHttpClient
import okhttp3.Request
import okhttp3.Response
import okhttp3.WebSocket
import okhttp3.WebSocketListener
import org.json.JSONObject
import java.util.concurrent.TimeUnit

/**
 * Headless control-plane service: maintains the dialfd WebSocket independent of the UI, so
 * the phone stays controllable while backgrounded / locked / across reboots.
 *
 * Discovers dialfd via NSD (`_dialfd._tcp`) — or a saved `server` address — sends hello +
 * a 30s heartbeat, dispatches commands to [Telecom], and forwards call/SMS events (via
 * [Dialf.serviceListener]) back to dialfd. Reconnects with a short backoff.
 */
class ConnForegroundService : Service() {

    companion object {
        const val PREFS = "dialf"
        const val SERVICE_TYPE = "_dialfd._tcp"
        private const val CHANNEL = "dialf_conn"
        private const val NOTIF_ID = 1

        // Reconnect backoff: start short, double each failed attempt up to a cap. The cap
        // is tighter while charging (retry often) and relaxed on battery (save power).
        private const val MIN_RECONNECT_MS = 2_000L
        private const val MAX_RECONNECT_CHARGING_MS = 30_000L
        private const val MAX_RECONNECT_BATTERY_MS = 120_000L
        // How long to leave NSD discovery (multicast) running per attempt before backing off.
        private const val DISCOVERY_WINDOW_MS = 20_000L
        // App-level liveness: heartbeat cadence, and how long the daemon may go silent (missed
        // heartbeat acks) before we treat the link as dead and reconnect (~3 missed beats).
        private const val HEARTBEAT_MS = 30_000L
        private const val LIVENESS_TIMEOUT_MS = 90_000L
        private const val TAG = "DialfConn" // `adb logcat -s DialfConn` to watch connection state
    }

    private val client: OkHttpClient = OkHttpClient.Builder()
        .pingInterval(20, TimeUnit.SECONDS)
        .build()
    private val main = Handler(Looper.getMainLooper())

    private lateinit var nsd: NsdManager
    private var discovery: NsdManager.DiscoveryListener? = null
    @Volatile private var ws: WebSocket? = null
    @Volatile private var running = false
    // Last time we heard anything from the daemon, and whether it acks heartbeats. The liveness
    // check only applies once the daemon has proven it acks, so a new app against an older daemon
    // (which never acks) doesn't reconnect-loop.
    @Volatile private var lastDaemonResponseMs = 0L
    @Volatile private var daemonAcksHeartbeats = false
    private var heartbeat: Runnable? = null
    private var netCallback: ConnectivityManager.NetworkCallback? = null
    @Volatile private var statusText = "Starting…"
    private var reconnectDelayMs = MIN_RECONNECT_MS
    private var reconnectRunnable: Runnable? = null
    private var discoveryTimeout: Runnable? = null
    // Held whenever the phone is on external power, to keep the CPU out of deep sleep so the
    // heartbeat keeps flowing and the daemon can place calls / send SMS the moment it's docked.
    // Released the instant it's unplugged, so it never costs battery in normal use. PARTIAL = CPU
    // only; the screen stays off.
    private var wakeLock: PowerManager.WakeLock? = null
    private val powerReceiver = object : BroadcastReceiver() {
        override fun onReceive(context: Context?, intent: Intent?) {
            Log.i(TAG, "power changed: ${intent?.action}")
            updateWakeLock()
            // Plugging in wakes the phone — heal the dialfd link right away (even if it won't
            // actually "charge": a data-only port, or a battery held at an 80% cap), so it's
            // reachable the instant it's on the cable rather than after the next heartbeat.
            if (intent?.action == Intent.ACTION_POWER_CONNECTED) {
                verifyLink("power connected")
            }
        }
    }

    // Fires when the device wakes — screen on, or exits Doze (which an incoming call forces). On
    // wake we can't trust the socket: it may have gone half-open while the CPU was suspended, so we
    // verify the dialfd link and rebuild it if it's stale (see verifyLink()).
    private val wakeReceiver = object : BroadcastReceiver() {
        override fun onReceive(context: Context?, intent: Intent?) {
            val action = intent?.action ?: return
            // ACTION_DEVICE_IDLE_MODE_CHANGED fires on both enter and exit — only act on exit.
            if (action == PowerManager.ACTION_DEVICE_IDLE_MODE_CHANGED &&
                getSystemService(PowerManager::class.java)?.isDeviceIdleMode == true
            ) {
                return
            }
            main.post { verifyLink(action) }
        }
    }

    private fun isCharging() =
        getSystemService(BatteryManager::class.java)?.isCharging == true

    /** True when the phone is on any external power source (AC/USB/wireless). NOT the same as
     *  isCharging(), which goes false once the battery is full or held at a charge limit (e.g. an
     *  80% cap) — at which point the phone is still powered but not "charging". We gate the wake
     *  lock on this so a plugged-in-but-full phone still stays awake. */
    private fun isPluggedIn(): Boolean {
        val batt = registerReceiver(null, IntentFilter(Intent.ACTION_BATTERY_CHANGED))
        val plugged = batt?.getIntExtra(BatteryManager.EXTRA_PLUGGED, 0) ?: 0
        return plugged != 0
    }

    /** Acquire a CPU-only wake lock whenever the phone is on external power; release it otherwise.
     *  Keeping the phone out of Doze while docked means the heartbeat never stalls and the daemon
     *  stays able to operate the phone on demand (it can't wake a sleeping phone otherwise). Off
     *  power we release immediately so the phone Dozes normally — no drain. Gated on plugged-in,
     *  not isCharging(), so a full / charge-limited phone still stays awake. Idempotent; safe on
     *  any power/lifecycle change. */
    private fun updateWakeLock() {
        if (running && isPluggedIn()) {
            if (wakeLock == null) {
                wakeLock = getSystemService(PowerManager::class.java)
                    ?.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "DialF:powered")
                    ?.apply {
                        setReferenceCounted(false)
                        acquire()
                    }
                Log.i(TAG, "on power -> wake lock held (CPU awake, screen off)")
            }
        } else {
            wakeLock?.let {
                if (it.isHeld) it.release()
                Log.i(TAG, "unplugged/stopped -> wake lock released")
            }
            wakeLock = null
        }
    }

    private fun keepRunning() =
        getSharedPreferences(PREFS, Context.MODE_PRIVATE).getBoolean("keep_running", true)

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        nsd = getSystemService(NsdManager::class.java)
        Dialf.serviceListener = { ev -> send(ev) }
        // Reconnect promptly when the network comes back (e.g. wifi flaps / changes).
        val cm = getSystemService(ConnectivityManager::class.java)
        val cb = object : ConnectivityManager.NetworkCallback() {
            override fun onAvailable(network: Network) {
                if (!running) return
                main.post {
                    // Network (re)appeared. Reconnect if we have no socket, OR if we have one
                    // that's gone stale (no daemon response in a while) — after the phone wakes
                    // from sleep the old socket is usually dead while `ws` still looks set, which
                    // otherwise leaves us "Connected" but unreachable until a manual Stop/Start.
                    val stale = ws != null && daemonAcksHeartbeats &&
                        System.currentTimeMillis() - lastDaemonResponseMs > LIVENESS_TIMEOUT_MS
                    if (ws == null || stale) {
                        Log.i(TAG, "network available -> reconnect (stale=$stale)")
                        forceReconnect()
                    }
                }
            }
        }
        try {
            cm?.registerDefaultNetworkCallback(cb)
            netCallback = cb
        } catch (_: Exception) {}
        // Track charge state so we hold/release the wake lock as the cable goes in/out.
        try {
            val filter = IntentFilter().apply {
                addAction(Intent.ACTION_POWER_CONNECTED)
                addAction(Intent.ACTION_POWER_DISCONNECTED)
            }
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                registerReceiver(powerReceiver, filter, Context.RECEIVER_NOT_EXPORTED)
            } else {
                @Suppress("UnspecifiedRegisterReceiverFlag")
                registerReceiver(powerReceiver, filter)
            }
        } catch (_: Exception) {}
        // On wake (screen on / Doze exit), verify the dialfd link is really alive — a socket that
        // went half-open during sleep still looks connected but can't carry the next command/ring.
        try {
            val wakeFilter = IntentFilter().apply {
                addAction(Intent.ACTION_SCREEN_ON)
                addAction(PowerManager.ACTION_DEVICE_IDLE_MODE_CHANGED)
            }
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                registerReceiver(wakeReceiver, wakeFilter, Context.RECEIVER_NOT_EXPORTED)
            } else {
                @Suppress("UnspecifiedRegisterReceiverFlag")
                registerReceiver(wakeReceiver, wakeFilter)
            }
        } catch (_: Exception) {}
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        // Re-show the *current* status (a re-start while already connected must not reset
        // the notification to "Starting…").
        startForeground(NOTIF_ID, notification(statusText))
        if (!running) {
            running = true
            reconnectDelayMs = MIN_RECONNECT_MS
            connectOrDiscover()
        }
        updateWakeLock() // hold the lock now if we're already plugged in
        return START_STICKY
    }

    /** App swiped from recents — reschedule a restart so the service keeps running. */
    override fun onTaskRemoved(rootIntent: Intent?) {
        if (running && keepRunning()) {
            val restart = Intent(applicationContext, ConnForegroundService::class.java)
            val pi = PendingIntent.getForegroundService(
                this,
                1,
                restart,
                PendingIntent.FLAG_ONE_SHOT or PendingIntent.FLAG_IMMUTABLE,
            )
            getSystemService(AlarmManager::class.java)
                ?.set(AlarmManager.RTC, System.currentTimeMillis() + 1500, pi)
        }
        super.onTaskRemoved(rootIntent)
    }

    override fun onDestroy() {
        running = false
        Dialf.serviceListener = null
        stopDiscovery()
        cancelHeartbeat()
        reconnectRunnable?.let { main.removeCallbacks(it) }
        netCallback?.let {
            try {
                getSystemService(ConnectivityManager::class.java)?.unregisterNetworkCallback(it)
            } catch (_: Exception) {}
        }
        netCallback = null
        ws?.close(1000, "service stopping")
        ws = null
        try { unregisterReceiver(powerReceiver) } catch (_: Exception) {}
        try { unregisterReceiver(wakeReceiver) } catch (_: Exception) {}
        updateWakeLock() // running=false above -> releases the lock
        stopForeground(STOP_FOREGROUND_REMOVE)
        super.onDestroy()
    }

    // --- connect / discover ---------------------------------------------------

    private fun connectOrDiscover() {
        if (!running) return
        val prefs = getSharedPreferences(PREFS, Context.MODE_PRIVATE)
        val server = prefs.getString("server", "")?.trim().orEmpty()
        if (server.contains(":")) {
            val parts = server.split(":")
            connect(parts[0], parts.getOrNull(1)?.toIntOrNull() ?: 8765)
        } else {
            startDiscovery()
        }
    }

    private fun startDiscovery() {
        stopDiscovery()
        val listener = object : NsdManager.DiscoveryListener {
            override fun onServiceFound(info: NsdServiceInfo) = resolve(info)
            override fun onServiceLost(info: NsdServiceInfo) {}
            override fun onDiscoveryStarted(t: String) {}
            override fun onDiscoveryStopped(t: String) {}
            override fun onStartDiscoveryFailed(t: String, e: Int) {}
            override fun onStopDiscoveryFailed(t: String, e: Int) {}
        }
        discovery = listener
        try {
            nsd.discoverServices(SERVICE_TYPE, NsdManager.PROTOCOL_DNS_SD, listener)
            // Don't leave multicast running forever — stop after a window and back off.
            val t = Runnable {
                if (running && ws == null) {
                    stopDiscovery()
                    scheduleReconnect()
                }
            }
            discoveryTimeout = t
            main.postDelayed(t, DISCOVERY_WINDOW_MS)
        } catch (_: Exception) {
            scheduleReconnect()
        }
    }

    private fun stopDiscovery() {
        discoveryTimeout?.let { main.removeCallbacks(it) }
        discoveryTimeout = null
        discovery?.let {
            try {
                nsd.stopServiceDiscovery(it)
            } catch (_: Exception) {}
        }
        discovery = null
    }

    @Suppress("DEPRECATION")
    private fun resolve(info: NsdServiceInfo) {
        nsd.resolveService(info, object : NsdManager.ResolveListener {
            override fun onServiceResolved(resolved: NsdServiceInfo) {
                val host = resolved.host?.hostAddress ?: return
                stopDiscovery()
                connect(host, resolved.port)
            }
            override fun onResolveFailed(s: NsdServiceInfo, code: Int) {}
        })
    }

    private fun connect(host: String, port: Int) {
        // Never open a second socket while one is live. mDNS re-announces repeatedly, so
        // onServiceFound -> resolve -> connect can fire many times; without this guard each
        // call leaked another WebSocket (the daemon then saw several connections for one
        // device, and replies/heartbeats went out on a different socket than commands came in
        // on — commands "worked" but acks never matched). `ws` is cleared in dropped().
        if (!running || ws != null) return
        val url = "ws://$host:$port"
        Log.i(TAG, "connecting to $url")
        val req = Request.Builder().url(url).build()
        ws = client.newWebSocket(req, Listener(url))
    }

    private fun scheduleReconnect() {
        if (!running) return
        reconnectRunnable?.let { main.removeCallbacks(it) }
        val delay = reconnectDelayMs
        val r = Runnable { connectOrDiscover() }
        reconnectRunnable = r
        main.postDelayed(r, delay)
        // Grow the backoff toward the (charge-dependent) cap for the next attempt.
        val cap = if (isCharging()) MAX_RECONNECT_CHARGING_MS else MAX_RECONNECT_BATTERY_MS
        reconnectDelayMs = (delay * 2).coerceAtMost(cap)
    }

    // --- websocket ------------------------------------------------------------

    private inner class Listener(private val url: String) : WebSocketListener() {
        override fun onOpen(webSocket: WebSocket, response: Response) {
            val prefs = getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            // Report the real installed version (versionName) rather than a hardcoded string.
            val appVersion = try {
                packageManager.getPackageInfo(packageName, 0).versionName ?: "?"
            } catch (_: Exception) {
                "?"
            }
            val hello = JSONObject()
                .put("type", "hello")
                .put("device_id", prefs.getString("device_id", "phone1"))
                .put("name", prefs.getString("name", "DialF Phone"))
                .put("key", prefs.getString("key", "change-me"))
                .put("caps", org.json.JSONArray(listOf("call", "sms")))
                .put("app_version", appVersion)
            webSocket.send(hello.toString())
            // Fresh connection: assume alive now; re-learn whether this daemon acks heartbeats.
            lastDaemonResponseMs = System.currentTimeMillis()
            daemonAcksHeartbeats = false
            startHeartbeat()
            // Connected — reset the backoff and cancel any pending retry.
            reconnectDelayMs = MIN_RECONNECT_MS
            reconnectRunnable?.let { main.removeCallbacks(it) }
            notify("Connected · $url")
            Log.i(TAG, "connected to $url")
            Dialf.emit(mapOf("type" to "status", "connected" to true, "server" to url))
            // If a call is already ringing (e.g. an incoming call is what woke us and we just
            // rebuilt the link), re-report it so the freshly-registered daemon can still
            // auto-answer — the original "ringing" event may have gone out on the dead socket.
            Dialf.ringingCall()?.let {
                Log.i(TAG, "reconnected mid-ring -> re-reporting ringing call")
                Dialf.emitCallState(it)
            }
        }

        override fun onMessage(webSocket: WebSocket, text: String) {
            lastDaemonResponseMs = System.currentTimeMillis() // heard from the daemon
            handle(webSocket, text)
        }
        override fun onClosed(webSocket: WebSocket, code: Int, reason: String) = dropped(webSocket)
        override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) = dropped(webSocket)
    }

    private fun dropped(socket: WebSocket) {
        // Ignore a close/failure from a socket that isn't our current one — a stale/superseded
        // socket dying must not tear down the live connection (that bug stopped heartbeats and
        // got the device reaped).
        if (socket !== ws) return
        Log.i(TAG, "dropped current socket -> reconnecting")
        cancelHeartbeat()
        ws = null
        notify("Reconnecting…")
        Dialf.emit(mapOf("type" to "status", "connected" to false))
        scheduleReconnect()
    }

    /** Tear down the current socket and reconnect now — used when we *know* the link is dead
     *  (liveness timeout, or network back after sleep). Unlike waiting for a close callback, this
     *  always clears `ws` so connect() isn't blocked by the `ws != null` guard. */
    private fun forceReconnect() {
        cancelHeartbeat()
        ws?.close(1001, "reconnecting")
        ws = null
        notify("Reconnecting…")
        Dialf.emit(mapOf("type" to "status", "connected" to false))
        reconnectDelayMs = MIN_RECONNECT_MS
        reconnectRunnable?.let { main.removeCallbacks(it) }
        connectOrDiscover()
    }

    /** On wake, make sure the dialfd link is genuinely alive. A socket that went half-open while
     *  the CPU was suspended still looks "connected" (`ws != null`) but can't carry the next
     *  command or the incoming-call ring. If we can't vouch for it — no socket, or the daemon has
     *  been silent longer than a heartbeat interval — rebuild it; otherwise poke a heartbeat so a
     *  silently-dead socket surfaces at once instead of waiting for the next scheduled beat. */
    private fun verifyLink(reason: String) {
        if (!running) return
        val silentMs = System.currentTimeMillis() - lastDaemonResponseMs
        val suspect = ws == null || (daemonAcksHeartbeats && silentMs > HEARTBEAT_MS)
        if (suspect) {
            Log.i(TAG, "$reason -> link suspect (ws=${ws != null}, silent=${silentMs}ms) -> reconnect")
            forceReconnect()
        } else {
            Log.i(TAG, "$reason -> link looks alive (silent=${silentMs}ms); poking heartbeat")
            pokeHeartbeat()
        }
    }

    /** Send a heartbeat right now (out of the normal cadence) to probe the socket. */
    private fun pokeHeartbeat() {
        val sock = ws ?: return
        try {
            sock.send(
                JSONObject().put("type", "heartbeat").put("ts", System.currentTimeMillis()).toString()
            )
        } catch (_: Exception) {}
    }

    private fun startHeartbeat() {
        cancelHeartbeat()
        val r = object : Runnable {
            override fun run() {
                val sock = ws ?: return
                // Liveness: if the daemon (which acks heartbeats) has gone silent past the
                // timeout, the link is dead even if the socket looks open (e.g. it died while the
                // phone was asleep). Force a reconnect directly — a dead socket's close callback
                // may never fire, which would otherwise leave us stuck "Connected".
                if (daemonAcksHeartbeats &&
                    System.currentTimeMillis() - lastDaemonResponseMs > LIVENESS_TIMEOUT_MS
                ) {
                    Log.i(TAG, "liveness timeout (no daemon response) -> force reconnect")
                    forceReconnect()
                    return
                }
                sock.send(
                    JSONObject().put("type", "heartbeat").put("ts", System.currentTimeMillis()).toString()
                )
                main.postDelayed(this, HEARTBEAT_MS)
            }
        }
        heartbeat = r
        main.postDelayed(r, HEARTBEAT_MS)
    }

    private fun cancelHeartbeat() {
        heartbeat?.let { main.removeCallbacks(it) }
        heartbeat = null
    }

    // --- command dispatch -----------------------------------------------------

    private fun handle(socket: WebSocket, text: String) {
        val msg = try {
            JSONObject(text)
        } catch (_: Exception) {
            return
        }
        when (msg.optString("type")) {
            "heartbeat_ack" -> {
                daemonAcksHeartbeats = true // daemon supports liveness acks; arm the check
                return
            }
            "cmd" -> {} // fall through to dispatch
            else -> return
        }
        val cmdId = msg.optString("cmd_id", "")
        val action = msg.optString("action")
        try {
            when (action) {
                "dial" -> Telecom.placeCall(
                    this,
                    msg.getString("number"),
                    if (msg.has("sim_sub_id") && !msg.isNull("sim_sub_id")) msg.getInt("sim_sub_id") else null,
                )
                "answer" -> Telecom.answer(msg.optString("call_id").ifEmpty { null })
                "hangup" -> Telecom.hangup(msg.optString("call_id").ifEmpty { null })
                "reject" -> Telecom.reject(msg.optString("call_id").ifEmpty { null }, msg.optBoolean("drop", false))
                "send_sms" -> Telecom.sendSms(this, msg.getString("to"), msg.getString("body"))
                "list_sms" -> Telecom.listSms(this, 20).forEach { sms ->
                    val m = HashMap<String, Any?>(sms)
                    m["type"] = "sms"
                    m.putIfAbsent("direction", "in")
                    send(m)
                }
                "list_calls" -> {
                    val arr = org.json.JSONArray()
                    Telecom.listCallLog(this, 50).forEach { arr.put(JSONObject(it)) }
                    socket.send(JSONObject().put("type", "calls").put("entries", arr).toString())
                }
                "list_sims" -> {
                    val arr = org.json.JSONArray()
                    Telecom.listSims(this).forEach { arr.put(JSONObject(it)) }
                    socket.send(JSONObject().put("type", "sims").put("entries", arr).toString())
                }
                "mmi" -> {
                    val code = msg.getString("code")
                    val sim = if (msg.has("sim_sub_id") && !msg.isNull("sim_sub_id")) msg.getInt("sim_sub_id") else null
                    Telecom.sendMmi(this, code, sim) { ok, resp ->
                        val o = JSONObject().put("type", "mmi_result").put("code", code).put("success", ok)
                        if (resp != null) o.put("response", resp)
                        socket.send(o.toString())
                    }
                }
                "set_voicemail" -> {
                    val enabled = msg.getBoolean("enabled")
                    val number = if (msg.has("number") && !msg.isNull("number")) msg.getString("number") else null
                    val sim = if (msg.has("sim_sub_id") && !msg.isNull("sim_sub_id")) msg.getInt("sim_sub_id") else null
                    Telecom.setVoicemail(this, enabled, number, sim) { ok, resp ->
                        val o = JSONObject().put("type", "voicemail_result").put("enabled", enabled).put("success", ok)
                        if (resp != null) o.put("response", resp)
                        socket.send(o.toString())
                    }
                }
                "set_autoanswer" -> {} // dialfd owns the answer list
                else -> {
                    sendError(socket, cmdId, "unknown action $action")
                    return
                }
            }
            socket.send(JSONObject().put("type", "ack").put("cmd_id", cmdId).put("ok", true).toString())
        } catch (e: Exception) {
            sendError(socket, cmdId, e.message ?: "command failed")
        }
    }

    private fun sendError(socket: WebSocket, cmdId: String, msg: String) {
        socket.send(JSONObject().put("type", "error").put("cmd_id", cmdId).put("msg", msg).toString())
    }

    /** Forward a phone-side event to dialfd. Only frames dialfd understands are sent;
     *  UI-only events (status / dialer_role) are dropped. */
    private fun send(event: Map<String, Any?>) {
        when (event["type"]) {
            "call_state", "sms" -> {
                val o = JSONObject()
                for ((k, v) in event) o.put(k, v ?: JSONObject.NULL)
                ws?.send(o.toString())
            }
            else -> {} // status, dialer_role, etc. are for the Flutter UI only
        }
    }

    // --- notification ---------------------------------------------------------

    private fun notification(text: String): Notification {
        val nm = getSystemService(NotificationManager::class.java)
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            nm.createNotificationChannel(
                NotificationChannel(CHANNEL, "DialF connection", NotificationManager.IMPORTANCE_LOW)
            )
        }
        return Notification.Builder(this, CHANNEL)
            .setContentTitle("DialF")
            .setContentText(text)
            .setSmallIcon(android.R.drawable.stat_sys_phone_call)
            .setOngoing(true)
            .build()
    }

    private fun notify(text: String) {
        statusText = text
        getSystemService(NotificationManager::class.java).notify(NOTIF_ID, notification(text))
    }
}
