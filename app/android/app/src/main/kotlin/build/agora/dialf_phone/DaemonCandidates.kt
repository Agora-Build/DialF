package build.agora.dialf_phone

/**
 * The dialfd daemons discovered on the LAN, and which of them rejected our shared key.
 *
 * A LAN can host several daemons (one per pair); only the one whose shared key matches
 * accepts us. Rejections must therefore outlive a discovery round — otherwise the phone
 * re-picks the same foreign daemon forever — while the discovered set is per-round, since
 * daemons come and go.
 *
 * Pure logic (no Android dependencies) so it is unit-testable; synchronized because
 * discovery resolves on the main thread while WebSocket close callbacks arrive on OkHttp's.
 */
class DaemonCandidates(private val keyRejectTtlMs: Long) {

    data class Endpoint(val url: String, val host: String, val port: Int)

    /** Discovered this round, in discovery order. */
    private val found = LinkedHashMap<String, Endpoint>()
    /** url -> epoch ms after which the daemon may be tried again. Survives [clear]. */
    private val rejectedUntil = HashMap<String, Long>()

    @Synchronized
    fun add(host: String, port: Int): Endpoint {
        val url = "ws://$host:$port"
        return found.getOrPut(url) { Endpoint(url, host, port) }
    }

    /** Drop the discovered set for a new discovery round. Rejections are kept on purpose. */
    @Synchronized
    fun clear() {
        found.clear()
    }

    @Synchronized
    fun markKeyRejected(url: String, now: Long) {
        rejectedUntil[url] = now + keyRejectTtlMs
    }

    @Synchronized
    fun isKeyRejected(url: String, now: Long): Boolean {
        val until = rejectedUntil[url] ?: return false
        if (now > until) {
            rejectedUntil.remove(url) // expired — the daemon may have been re-keyed since
            return false
        }
        return true
    }

    /** The first discovered daemon that hasn't rejected our key, or null if none is usable. */
    @Synchronized
    fun next(now: Long): Endpoint? = found.values.firstOrNull { !isKeyRejected(it.url, now) }

    /** Discovered daemons currently being skipped (for logging). */
    @Synchronized
    fun skipped(now: Long): List<String> = found.keys.filter { isKeyRejected(it, now) }
}
