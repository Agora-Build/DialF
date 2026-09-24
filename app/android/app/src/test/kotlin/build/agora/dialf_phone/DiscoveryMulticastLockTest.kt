package build.agora.dialf_phone

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class DiscoveryMulticastLockTest {
    private class FakeLock(var failWith: RuntimeException? = null) : DiscoveryMulticastLock.Lock {
        override var isHeld = false
        var acquires = 0
        var releases = 0
        override fun acquire() {
            failWith?.let { throw it }
            acquires++
            isHeld = true
        }
        override fun release() {
            failWith?.let { throw it }
            releases++
            isHeld = false
        }
    }

    private val errors = mutableListOf<String>()

    @Test
    fun repeatedStartsAndStopsNeverStackOrUnderflow() {
        // startDiscovery calls stopDiscovery first, and onDestroy stops again: the lock sees
        // release-before-acquire and double releases. A real MulticastLock throws on an
        // unbalanced release, so these must be no-ops.
        val fake = FakeLock()
        val lock = DiscoveryMulticastLock(fake) { what, _ -> errors += what }
        lock.release()
        lock.acquire()
        lock.acquire()
        assertTrue(fake.isHeld)
        lock.release()
        lock.release()
        assertFalse(fake.isHeld)
        assertEquals(1, fake.acquires)
        assertEquals(1, fake.releases)
        assertTrue(errors.isEmpty())
    }

    @Test
    fun aFailingLockNeverStopsDiscovery() {
        // A vendor build that refuses the lock must leave discovery running exactly as before.
        val lock = DiscoveryMulticastLock(FakeLock(SecurityException("denied"))) { what, _ -> errors += what }
        lock.acquire()
        assertEquals(listOf("acquire"), errors)
    }

    @Test
    fun noWifiServiceMeansNoLockAndNoErrors() {
        val lock = DiscoveryMulticastLock(null) { what, _ -> errors += what }
        lock.acquire()
        lock.release()
        assertTrue(errors.isEmpty())
    }
}
