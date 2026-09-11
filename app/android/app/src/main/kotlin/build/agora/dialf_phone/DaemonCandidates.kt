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
class DaemonCandidates(
    private val keyRejectTtlMs: Long,
    private val unreachableTtlMs: Long = 60_000L,
) {

    data class Endpoint(val url: String, val host: String, val port: Int)

    /** Discovered this round, in discovery order. */
    private val found = LinkedHashMap<String, Endpoint>()
    /** url -> epoch ms after which the daemon may be tried again. Survives [clear]. */
    private val rejectedUntil = HashMap<String, Long>()
    /** Same, for endpoints whose connection never came up (timeout / refused). A host that
     *  advertises dialfd but never completes a handshake would otherwise sit at the head of
     *  the list forever, burning a connect timeout per attempt while a working daemon waits. */
    private val unreachableUntil = HashMap<String, Long>()

    /** Add a discovered endpoint, or null if `host` can't be dialed (see [wsUrl]). */
    @Synchronized
    fun add(host: String, port: Int): Endpoint? {
        val url = wsUrl(host, port) ?: return null
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

    /** The connection to `url` never came up. Deprioritize it briefly. */
    @Synchronized
    fun markUnreachable(url: String, now: Long) {
        unreachableUntil[url] = now + unreachableTtlMs
    }

    /** `url` completed a handshake — it is healthy again. */
    @Synchronized
    fun markReachable(url: String) {
        unreachableUntil.remove(url)
    }

    @Synchronized
    fun isUnreachable(url: String, now: Long): Boolean {
        val until = unreachableUntil[url] ?: return false
        if (now > until) {
            unreachableUntil.remove(url)
            return false
        }
        return true
    }

    /** The best daemon to try: the first that neither rejected our key nor just failed to
     *  connect. If every candidate is in the doghouse, fall back to the first one that
     *  hasn't rejected our key — a stale unreachable mark must never strand us. */
    @Synchronized
    fun next(now: Long): Endpoint? {
        val eligible = found.values.filter { !isKeyRejected(it.url, now) }
        return eligible.firstOrNull { !isUnreachable(it.url, now) } ?: eligible.firstOrNull()
    }

    /** Discovered daemons currently being skipped for a key rejection (for logging). */
    @Synchronized
    fun skipped(now: Long): List<String> = found.keys.filter { isKeyRejected(it, now) }
}

/**
 * The `ws://` URL for a discovered endpoint, or null if it can't be dialed.
 *
 * IPv6 literals MUST be bracketed or the URL parser rejects them (an unbracketed
 * `ws://fe80::1:8765` is not a valid URL and throws). Link-local addresses are dropped
 * outright: without a zone/scope id they are unreachable, and mDNS hands them out freely.
 */
fun wsUrl(host: String, port: Int): String? {
    val h = host.trim()
    if (h.isEmpty() || port <= 0) return null
    if (!h.contains(':')) return "ws://$h:$port"
    // Link-local is fe80::/10 (fe80–febf in the first group).
    val firstGroup = h.substringBefore('%').substringBefore(':').toIntOrNull(16)
    if (firstGroup != null && (firstGroup and 0xffc0) == 0xfe80) return null
    return "ws://[$h]:$port"
}
