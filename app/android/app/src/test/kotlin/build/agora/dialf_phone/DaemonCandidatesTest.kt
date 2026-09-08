package build.agora.dialf_phone

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
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
}
