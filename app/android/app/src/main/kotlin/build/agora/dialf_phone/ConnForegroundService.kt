package build.agora.dialf_phone

import android.app.AlarmManager
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
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
    }

    private val client: OkHttpClient = OkHttpClient.Builder()
        .pingInterval(20, TimeUnit.SECONDS)
        .build()
    private val main = Handler(Looper.getMainLooper())

    private lateinit var nsd: NsdManager
    private var discovery: NsdManager.DiscoveryListener? = null
    @Volatile private var ws: WebSocket? = null
    @Volatile private var running = false
    private var heartbeat: Runnable? = null

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onCreate() {
        super.onCreate()
        nsd = getSystemService(NsdManager::class.java)
        Dialf.serviceListener = { ev -> send(ev) }
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        startForeground(NOTIF_ID, notification("Starting…"))
        if (!running) {
            running = true
            connectOrDiscover()
        }
        return START_STICKY
    }

    /** App swiped from recents — reschedule a restart so the service keeps running. */
    override fun onTaskRemoved(rootIntent: Intent?) {
        if (running) {
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
        ws?.close(1000, "service stopping")
        ws = null
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
        } catch (_: Exception) {
            scheduleReconnect()
        }
    }

    private fun stopDiscovery() {
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
        if (!running) return
        val url = "ws://$host:$port"
        val req = Request.Builder().url(url).build()
        ws = client.newWebSocket(req, Listener(url))
    }

    private fun scheduleReconnect() {
        if (!running) return
        main.postDelayed({ connectOrDiscover() }, 2_000)
    }

    // --- websocket ------------------------------------------------------------

    private inner class Listener(private val url: String) : WebSocketListener() {
        override fun onOpen(webSocket: WebSocket, response: Response) {
            val prefs = getSharedPreferences(PREFS, Context.MODE_PRIVATE)
            val hello = JSONObject()
                .put("type", "hello")
                .put("device_id", prefs.getString("device_id", "phone1"))
                .put("name", prefs.getString("name", "DialF Phone"))
                .put("key", prefs.getString("key", "change-me"))
                .put("caps", org.json.JSONArray(listOf("call", "sms")))
                .put("app_version", "0.1")
            webSocket.send(hello.toString())
            startHeartbeat()
            notify("Connected · $url")
            Dialf.emit(mapOf("type" to "status", "connected" to true, "server" to url))
        }

        override fun onMessage(webSocket: WebSocket, text: String) = handle(text)
        override fun onClosed(webSocket: WebSocket, code: Int, reason: String) = dropped()
        override fun onFailure(webSocket: WebSocket, t: Throwable, response: Response?) = dropped()
    }

    private fun dropped() {
        cancelHeartbeat()
        ws = null
        notify("Reconnecting…")
        Dialf.emit(mapOf("type" to "status", "connected" to false))
        scheduleReconnect()
    }

    private fun startHeartbeat() {
        cancelHeartbeat()
        val r = object : Runnable {
            override fun run() {
                ws?.send(
                    JSONObject().put("type", "heartbeat").put("ts", System.currentTimeMillis()).toString()
                )
                main.postDelayed(this, 30_000)
            }
        }
        heartbeat = r
        main.postDelayed(r, 30_000)
    }

    private fun cancelHeartbeat() {
        heartbeat?.let { main.removeCallbacks(it) }
        heartbeat = null
    }

    // --- command dispatch -----------------------------------------------------

    private fun handle(text: String) {
        val msg = try {
            JSONObject(text)
        } catch (_: Exception) {
            return
        }
        if (msg.optString("type") != "cmd") return
        val cmdId = msg.optString("cmd_id", "")
        val action = msg.optString("action")
        try {
            when (action) {
                "dial" -> Telecom.placeCall(
                    this,
                    msg.getString("number"),
                    if (msg.has("sim_sub_id") && !msg.isNull("sim_sub_id")) msg.getInt("sim_sub_id") else null,
                )
                "pickup" -> Telecom.answer(msg.optString("call_id").ifEmpty { null })
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
                    ws?.send(JSONObject().put("type", "calls").put("entries", arr).toString())
                }
                "list_sims" -> {
                    val arr = org.json.JSONArray()
                    Telecom.listSims(this).forEach { arr.put(JSONObject(it)) }
                    ws?.send(JSONObject().put("type", "sims").put("entries", arr).toString())
                }
                "mmi" -> {
                    val code = msg.getString("code")
                    val sim = if (msg.has("sim_sub_id") && !msg.isNull("sim_sub_id")) msg.getInt("sim_sub_id") else null
                    Telecom.sendMmi(this, code, sim) { ok, resp ->
                        val o = JSONObject().put("type", "mmi_result").put("code", code).put("success", ok)
                        if (resp != null) o.put("response", resp)
                        ws?.send(o.toString())
                    }
                }
                "set_voicemail" -> {
                    val enabled = msg.getBoolean("enabled")
                    val number = if (msg.has("number") && !msg.isNull("number")) msg.getString("number") else null
                    val sim = if (msg.has("sim_sub_id") && !msg.isNull("sim_sub_id")) msg.getInt("sim_sub_id") else null
                    Telecom.setVoicemail(this, enabled, number, sim) { ok, resp ->
                        val o = JSONObject().put("type", "voicemail_result").put("enabled", enabled).put("success", ok)
                        if (resp != null) o.put("response", resp)
                        ws?.send(o.toString())
                    }
                }
                "set_autopickup" -> {} // dialfd owns the picklist
                else -> {
                    sendError(cmdId, "unknown action $action")
                    return
                }
            }
            ws?.send(JSONObject().put("type", "ack").put("cmd_id", cmdId).put("ok", true).toString())
        } catch (e: Exception) {
            sendError(cmdId, e.message ?: "command failed")
        }
    }

    private fun sendError(cmdId: String, msg: String) {
        ws?.send(JSONObject().put("type", "error").put("cmd_id", cmdId).put("msg", msg).toString())
    }

    /** Forward a Dialf event map (call_state / sms) to dialfd as JSON. */
    private fun send(event: Map<String, Any?>) {
        val o = JSONObject()
        for ((k, v) in event) o.put(k, v ?: JSONObject.NULL)
        ws?.send(o.toString())
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
        getSystemService(NotificationManager::class.java).notify(NOTIF_ID, notification(text))
    }
}
