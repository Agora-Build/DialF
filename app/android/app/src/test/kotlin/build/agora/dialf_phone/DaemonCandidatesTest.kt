package build.agora.dialf_phone

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

/**
 * Pairing/failover rules for a LAN with several dialfd daemons (see [DaemonCandidates]).
 * These pin the behaviour that took the phone ~90s to find its own daemon before.
 */
class DaemonCandidatesTest {

    private val ttl = 10 * 60_000L
    private fun candidates() = DaemonCandidates(ttl)

    @Test
    fun `picks the first discovered daemon`() {
        val c = candidates()
        c.add("192.168.1.10", 8765)
        c.add("192.168.1.20", 8765)
        assertEquals("ws://192.168.1.10:8765", c.next(0)?.url)
    }

    @Test
    fun `fails over to the next daemon after a key rejection`() {
        val c = candidates()
        c.add("192.168.1.10", 8765)
        c.add("192.168.1.20", 8765)
        c.markKeyRejected("ws://192.168.1.10:8765", 0)
        val next = c.next(0)
        assertEquals("ws://192.168.1.20:8765", next?.url)
        assertEquals("192.168.1.20", next?.host)
        assertEquals(8765, next?.port)
    }

    @Test
    fun `no candidate when every discovered daemon rejected us`() {
        val c = candidates()
        c.add("192.168.1.10", 8765)
        c.add("192.168.1.20", 18800)
        c.markKeyRejected("ws://192.168.1.10:8765", 0)
        c.markKeyRejected("ws://192.168.1.20:18800", 0)
        assertNull(c.next(0))
        assertEquals(2, c.skipped(0).size)
    }

    @Test
    fun `rejections survive a discovery round, discovered set does not`() {
        // The whole point: a new round must not re-offer the daemon that just rejected us.
        val c = candidates()
        c.add("192.168.1.10", 8765)
        c.markKeyRejected("ws://192.168.1.10:8765", 0)
        c.clear()
        assertNull(c.next(0)) // nothing discovered yet this round
        c.add("192.168.1.10", 8765) // re-discovered: still skipped
        assertNull(c.next(0))
        c.add("192.168.1.20", 8765) // our own daemon shows up
        assertEquals("ws://192.168.1.20:8765", c.next(0)?.url)
    }

    @Test
    fun `a rejection expires so a re-keyed daemon is retried`() {
        val c = candidates()
        c.add("192.168.1.10", 8765)
        c.markKeyRejected("ws://192.168.1.10:8765", 1_000)
        assertTrue(c.isKeyRejected("ws://192.168.1.10:8765", 1_000 + ttl))
        assertNull(c.next(1_000 + ttl))
        val after = 1_000 + ttl + 1
        assertFalse(c.isKeyRejected("ws://192.168.1.10:8765", after))
        assertEquals("ws://192.168.1.10:8765", c.next(after)?.url)
    }

    @Test
    fun `re-announcements do not duplicate or reorder candidates`() {
        // mDNS re-announces constantly; the first-discovered daemon must stay first.
        val c = candidates()
        c.add("192.168.1.10", 8765)
        c.add("192.168.1.20", 8765)
        c.add("192.168.1.10", 8765)
        c.markKeyRejected("ws://192.168.1.10:8765", 0)
        assertEquals("ws://192.168.1.20:8765", c.next(0)?.url)
        assertEquals(listOf("ws://192.168.1.10:8765"), c.skipped(0))
    }

    @Test
    fun `same host on different ports are distinct daemons`() {
        val c = candidates()
        c.add("192.168.1.10", 8765)
        c.add("192.168.1.10", 18800)
        c.markKeyRejected("ws://192.168.1.10:8765", 0)
        assertEquals("ws://192.168.1.10:18800", c.next(0)?.url)
    }

    // --- regressions from a real crash loop on a two-daemon LAN (2026-09-11) ---

    @Test
    fun `ipv6 literals are bracketed`() {
        // Unbracketed, this is not a valid URL and the request builder throws — which killed
        // the app process on every discovery round.
        assertEquals("ws://[2001:db8::1]:8765", wsUrl("2001:db8::1", 8765))
        assertEquals("ws://192.168.1.10:8765", wsUrl("192.168.1.10", 8765))
    }

    @Test
    fun `link-local ipv6 is not dialable`() {
        // mDNS hands these out freely; without a zone id they can't be reached, and they are
        // exactly what the app used to crash on.
        assertNull(wsUrl("fe80::2faf:53ce:39c1:2c55", 8765))
        assertNull(wsUrl("FE80::1", 8765))
        assertNull(wsUrl("febf::1", 8765))
        assertNotNull(wsUrl("fec0::1", 8765)) // outside fe80::/10
    }

    @Test
    fun `undialable hosts are not added as candidates`() {
        val c = candidates()
        assertNull(c.add("fe80::2faf:53ce:39c1:2c55", 8765))
        assertNull(c.next(0))
        assertNotNull(c.add("192.168.1.10", 8765))
        assertEquals("ws://192.168.1.10:8765", c.next(0)?.url)
    }

    @Test
    fun `a daemon that never connects yields to one that can`() {
        // The black-hole case: a host advertises dialfd but never completes a handshake, so
        // it never rejects our key either. It must not hold the front of the queue.
        val c = candidates()
        c.add("192.168.1.100", 8765) // discovered first, unreachable
        c.add("192.168.1.208", 8765) // the real daemon
        assertEquals("ws://192.168.1.100:8765", c.next(0)?.url)
        c.markUnreachable("ws://192.168.1.100:8765", 0)
        assertEquals("must move on after a failed connect", "ws://192.168.1.208:8765", c.next(0)?.url)
    }

    @Test
    fun `an unreachable mark expires and clears on success`() {
        val c = DaemonCandidates(ttl, unreachableTtlMs = 60_000)
        c.add("192.168.1.100", 8765)
        c.markUnreachable("ws://192.168.1.100:8765", 1_000)
        assertTrue(c.isUnreachable("ws://192.168.1.100:8765", 1_000 + 60_000))
        assertFalse(c.isUnreachable("ws://192.168.1.100:8765", 1_000 + 60_001))
        c.markUnreachable("ws://192.168.1.100:8765", 2_000)
        c.markReachable("ws://192.168.1.100:8765")
        assertFalse("a handshake clears it", c.isUnreachable("ws://192.168.1.100:8765", 2_000))
    }

    @Test
    fun `all candidates unreachable still returns one to retry`() {
        // Never strand the phone: if everything is in the doghouse, keep trying the first.
        val c = candidates()
        c.add("192.168.1.100", 8765)
        c.add("192.168.1.208", 8765)
        c.markUnreachable("ws://192.168.1.100:8765", 0)
        c.markUnreachable("ws://192.168.1.208:8765", 0)
        assertNotNull(c.next(0))
    }

    @Test
    fun `key rejection still outranks reachability`() {
        val c = candidates()
        c.add("192.168.1.186", 8765) // wrong key
        c.add("192.168.1.100", 8765) // black hole
        c.markKeyRejected("ws://192.168.1.186:8765", 0)
        c.markUnreachable("ws://192.168.1.100:8765", 0)
        // Both are bad, but a key-rejected daemon is never ours — the unreachable one might be.
        assertEquals("ws://192.168.1.100:8765", c.next(0)?.url)
    }
}
