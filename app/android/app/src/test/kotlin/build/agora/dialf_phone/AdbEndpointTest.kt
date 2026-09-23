package build.agora.dialf_phone

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class AdbEndpointTest {
    private val own = setOf("192.168.100.179")

    @Test
    fun `picks the service at this phone's own address`() {
        val seen = listOf("192.168.100.42" to 37001, "192.168.100.179" to 41195)
        assertEquals(41195, AdbEndpoint.pickOwn(seen, own))
    }

    @Test
    fun `never reports another phone's port`() {
        assertNull(AdbEndpoint.pickOwn(listOf("192.168.100.42" to 37001), own))
        assertNull(AdbEndpoint.pickOwn(emptyList(), own))
    }

    @Test
    fun `ignores the link-local duplicate of its own advert`() {
        // The probe on the Pixel saw its own service twice: IPv4, and fe80:: on wlan0.
        val seen = listOf("fe80::605b:d9ff:fe3d:d9d0%wlan0" to 41195, "192.168.100.179" to 41195)
        assertEquals(41195, AdbEndpoint.pickOwn(seen, own))
        assertNull(AdbEndpoint.pickOwn(seen.take(1), own))
    }

    @Test
    fun `rejects a nonsense port`() {
        assertNull(AdbEndpoint.pickOwn(listOf("192.168.100.179" to 0), own))
    }

    @Test
    fun `recognises the adb service type in the forms NSD reports`() {
        assertTrue(AdbEndpoint.isAdbService("_adb-tls-connect._tcp"))
        assertTrue(AdbEndpoint.isAdbService("_adb-tls-connect._tcp."))
        assertTrue(AdbEndpoint.isAdbService("._adb-tls-connect._tcp"))
        assertFalse(AdbEndpoint.isAdbService("_dialfd._tcp."))
        assertFalse(AdbEndpoint.isAdbService(null))
    }
}
