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
        // How long to keep the CPU awake on an inbound ring while we (re)build the link, report the
        // call, and let the daemon answer. Self-releasing so it can never leak.
        private const val RING_WAKE_MS = 30_000L
        /** WebSocket close code dialfd sends for a shared-key mismatch (another pair's daemon). */
        private const val CLOSE_BAD_KEY = 4001
        /** How long a key-rejecting daemon is skipped during discovery before retrying it. */
        private const val KEY_REJECT_TTL_MS = 10 * 60_000L
        /** Retries for an NSD resolve that lost the one-at-a-time race (FAILURE_ALREADY_ACTIVE). */
        private const val RESOLVE_RETRIES = 3
        private const val RESOLVE_RETRY_MS = 400L
        private const val TAG = "DialfConn" // `adb logcat -s DialfConn` to watch connection state

        /** Random id generated once per app *process* launch, sent in every `hello`. A changed
         *  value tells the daemon the app relaunched (crashed/restarted) vs merely reconnected, so
         *  it can abort an in-flight job rather than resume it against a fresh process. */
        private val INSTANCE_ID: String = java.util.UUID.randomUUID().toString()

        /** Live instance, so [DialfInCallService] can nudge the link the moment a call rings. */
        @Volatile
        private var instance: ConnForegroundService? = null

        /** An inbound call is ringing: make sure the dialfd link is alive so the daemon hears about
         *  it and can auto-answer — even with the screen off. No-op if the service isn't running.
         *  Safe to call from the InCallService (main) thread. */
        fun onIncomingCall() {
            instance?.handleIncomingCall()
        }
    }

    private val client: OkHttpClient = OkHttpClient.Builder()
        .pingInterval(20, TimeUnit.SECONDS)
        // A LAN daemon answers in milliseconds; the 10s default just delays failing over to
        // the next candidate on a multi-daemon network.
        .connectTimeout(4, TimeUnit.SECONDS)
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
    // A separate, short, self-releasing CPU wake lock held only around an inbound ring — so the
    // link can be rebuilt and the call reported even on battery (when the power-gated `wakeLock`
    // above isn't held). Independent of power state; times out via acquire(RING_WAKE_MS).
    private var ringWakeLock: PowerManager.WakeLock? = null
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

    /** An inbound call is ringing. The system already woke the CPU to bind the InCallService and
     *  run onCallAdded, but the dialfd socket may be stale (dead from Doze). Hold the CPU awake for
     *  a short window and verify/rebuild the link so the ringing call is reported and the daemon can
     *  auto-answer — the screen need not turn on. onOpen re-reports the ringing call after a
     *  reconnect, so a report that raced a dead socket isn't lost. */
    private fun handleIncomingCall() {
        if (!running) return
        acquireRingWakeLock()
        main.post { verifyLink("incoming call") }
    }

    /** Hold a short CPU-only wake lock covering the inbound-ring reconnect/report/answer window,
     *  regardless of power state. Self-releasing (times out) so it can never leak; idempotent. */
    private fun acquireRingWakeLock() {
        ringWakeLock?.let { if (it.isHeld) return }
        ringWakeLock = getSystemService(PowerManager::class.java)
            ?.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "DialF:ring")
            ?.apply {
                setReferenceCounted(false)
                acquire(RING_WAKE_MS)
            }
        Log.i(TAG, "incoming ring -> ring wake lock (${RING_WAKE_MS}ms), verifying link")
    }

    private fun keepRunning() =
        getSharedPreferences(PREFS, Context.MODE_PRIVATE).getBoolean("keep_running", true)

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        instance = this
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
        KeepAliveReceiver.schedule(this)
        return START_STICKY
    }

    /** FGS time budget exhausted (only time-limited types get this; specialUse shouldn't).
     *  Without an override the SYSTEM CRASHES the process a few seconds later — stop cleanly
     *  and schedule a comeback instead. */
    override fun onTimeout(startId: Int, fgsType: Int) {
        Log.w(TAG, "foreground service timed out (type=$fgsType) — stopping, restart in 15m")
        scheduleRestart(15 * 60_000L)
        stopSelf()
    }

    /** Arm an alarm that restarts this service in `delayMs` (used by onTimeout/onTaskRemoved). */
    private fun scheduleRestart(delayMs: Long) {
        if (!keepRunning()) return
        val restart = Intent(applicationContext, ConnForegroundService::class.java)
        val pi = PendingIntent.getForegroundService(
            this,
            1,
            restart,
            PendingIntent.FLAG_ONE_SHOT or PendingIntent.FLAG_IMMUTABLE,
        )
        getSystemService(AlarmManager::class.java)
            ?.set(AlarmManager.RTC, System.currentTimeMillis() + delayMs, pi)
    }

    /** App swiped from recents — reschedule a restart so the service keeps running. */
    override fun onTaskRemoved(rootIntent: Intent?) {
        if (running) {
            scheduleRestart(1500)
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
        ringWakeLock?.let { if (it.isHeld) it.release() }
        ringWakeLock = null
        instance = null
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
        daemons.clear()
        resolveQueue.clear()
        resolveAttempts.clear()
        val listener = object : NsdManager.DiscoveryListener {
            override fun onServiceFound(info: NsdServiceInfo) {
                main.post { enqueueResolve(info) }
            }
            override fun onServiceLost(info: NsdServiceInfo) {}
            override fun onDiscoveryStarted(t: String) {}
            override fun onDiscoveryStopped(t: String) {}
            override fun onStartDiscoveryFailed(t: String, e: Int) {}
            override fun onStopDiscoveryFailed(t: String, e: Int) {}
        }
        discovery = listener
        try {
            nsd.discoverServices(SERVICE_TYPE, NsdManager.PROTOCOL_DNS_SD, listener)
            // Keep discovering for the whole window even after connecting: on a LAN with
            // several daemons we want every candidate on hand, so a shared-key rejection can
            // fail over to the next one instantly instead of waiting out another round.
            val t = Runnable {
                stopDiscovery()
                if (running && ws == null) scheduleReconnect()
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

    // --- candidate resolution -------------------------------------------------
    // NsdManager resolves ONE service at a time: a second resolveService() while one is in
    // flight fails with FAILURE_ALREADY_ACTIVE. Several daemons announce together, so the
    // losers of that race used to be dropped for the entire discovery window (a wrong-key
    // daemon then kept winning round after round — ~90s to find the right one). Queue the
    // resolves, run them one at a time, and retry the failures.

    /** Daemons discovered on the LAN + which of them rejected our shared key. */
    private val daemons = DaemonCandidates(KEY_REJECT_TTL_MS)
    private val resolveQueue = ArrayDeque<NsdServiceInfo>()
    private val resolveAttempts = HashMap<String, Int>()
    private var resolving = false

    private fun enqueueResolve(info: NsdServiceInfo) {
        resolveQueue.addLast(info)
        pumpResolve()
    }

    @Suppress("DEPRECATION")
    private fun pumpResolve() {
        if (resolving || !running) return
        val info = resolveQueue.removeFirstOrNull() ?: return
        resolving = true
        nsd.resolveService(info, object : NsdManager.ResolveListener {
            override fun onServiceResolved(resolved: NsdServiceInfo) {
                main.post {
                    resolving = false
                    val host = resolved.host?.hostAddress
                    if (host != null) {
                        daemons.add(host, resolved.port)
                        connectNextCandidate()
                    }
                    pumpResolve()
                }
            }

            override fun onResolveFailed(s: NsdServiceInfo, code: Int) {
                main.post {
                    resolving = false
                    // Mostly FAILURE_ALREADY_ACTIVE from a concurrent resolve — retry rather
                    // than losing this daemon until the next discovery round.
                    val n = (resolveAttempts[s.serviceName] ?: 0) + 1
                    resolveAttempts[s.serviceName] = n
                    if (n <= RESOLVE_RETRIES) {
                        main.postDelayed({ enqueueResolve(s) }, RESOLVE_RETRY_MS)
                    } else {
                        Log.w(TAG, "giving up on ${s.serviceName} after $n resolve failures (code $code)")
                    }
                    pumpResolve()
                }
            }
        })
    }

    /** Connect to the first discovered daemon that hasn't rejected our shared key. */
    private fun connectNextCandidate(): Boolean {
        if (!running || ws != null) return false
        val now = System.currentTimeMillis()
        val next = daemons.next(now)
        if (next == null) {
            daemons.skipped(now).forEach { Log.i(TAG, "skipping $it — rejected our shared key recently") }
            return false
        }
        connect(next.host, next.port)
        return true
    }

    private fun connect(host: String, port: Int) {
        // Never open a second socket while one is live. mDNS re-announces repeatedly, so
        // onServiceFound -> resolve -> connect can fire many times; without this guard each
        // call leaked another WebSocket (the daemon then saw several connections for one
        // device, and replies/heartbeats went out on a different socket than commands came in
        // on — commands "worked" but acks never matched). `ws` is cleared in dropped().
        if (!running || ws != null) return
        // wsUrl() brackets IPv6 literals and drops link-local addresses. An unbracketed
        // IPv6 host used to throw IllegalArgumentException here and kill the process on
        // every discovery round; belt-and-braces, a malformed URL must never be fatal.
        val url = wsUrl(host, port)
        if (url == null) {
            Log.w(TAG, "skipping undialable address $host:$port")
            return
        }
        Log.i(TAG, "connecting to $url")
        val req = try {
            Request.Builder().url(url).build()
        } catch (e: Exception) {
            Log.w(TAG, "bad daemon URL $url", e)
            daemons.markUnreachable(url, System.currentTimeMillis())
            return
        }
        connecting = url
        ws = client.newWebSocket(req, Listener(url))
    }

    /** URL of the socket we opened but haven't seen a handshake for yet. */
    @Volatile private var connecting: String? = null

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
                .put("key", prefs.getString("key", PhoneConfig.DEFAULT_KEY))
                .put("caps", org.json.JSONArray(listOf("call", "sms")))
                .put("app_version", appVersion)
                .put("instance_id", INSTANCE_ID)
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
            connecting = null
            daemons.markReachable(url)
            Dialf.emit(mapOf("type" to "status", "connected" to true, "server" to url))
            // Re-report the current call (any state) so the freshly-registered daemon has accurate
            // state after a reconnect: a ringing call so it can still auto-answer, and — crucially —
            // an active call so it doesn't misread the reconnect as "the call ended" mid-job. The
            // original event may have gone out on the now-dead socket.
            Dialf.call(null)?.let {
                Log.i(TAG, "reconnected mid-call -> re-reporting current call")
                Dialf.emitCallState(it)
            }
        }

        override fun onMessage(webSocket: WebSocket, text: String) {
            lastDaemonResponseMs = System.currentTimeMillis() // heard from the daemon
            handle(webSocket, text)
        }
        override fun onClosing(webSocket: WebSocket, code: Int, reason: String) {
            // The close code arrives here (server-initiated close); record a key rejection
            // BEFORE acknowledging — the server may TCP-drop right after its close frame,
            // in which case onClosed never fires (onFailure does, without the code).
            if (code == CLOSE_BAD_KEY) {
                Log.w(TAG, "$url rejected our shared key — skipping it for ${KEY_REJECT_TTL_MS / 60_000} min")
                daemons.markKeyRejected(url, System.currentTimeMillis())
                notify("Shared key rejected · $url")
                // Fail over to another discovered daemon immediately: this one is another
                // pair's, so there is nothing to back off from.
                keyRejectFailover = true
            }
            webSocket.close(1000, null)
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
        val keyReject = keyRejectFailover
        keyRejectFailover = false
        // Did this socket ever complete a handshake? If not, the endpoint is a black hole
        // (connect timeout / refused / not a daemon). It never rejects our key, so without
        // this it would hold the front of the queue forever, burning a connect timeout per
        // attempt while a working daemon sits untried.
        val neverOpened = connecting
        connecting = null
        main.post {
            if (neverOpened != null) {
                Log.i(TAG, "$neverOpened never completed a handshake — trying another daemon")
                daemons.markUnreachable(neverOpened, System.currentTimeMillis())
            }
            // A wrong-key or dead daemon: try the next one we already discovered, right now.
            // Any other drop goes through the normal backoff (our own daemon may be blipping).
            if ((keyReject || neverOpened != null) && connectNextCandidate()) return@post
            scheduleReconnect()
        }
    }

    /** Set when the current socket was closed for a shared-key mismatch (see [CLOSE_BAD_KEY]). */
    @Volatile private var keyRejectFailover = false

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
