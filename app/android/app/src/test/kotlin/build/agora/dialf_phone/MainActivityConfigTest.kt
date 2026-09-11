package build.agora.dialf_phone

import org.junit.Assert.assertEquals
import org.junit.Test

/**
 * The save path must never replace a real shared key with a default. The UI writes back
 * whatever it displays, so before the read-back fix, opening the app and tapping Start
 * service silently reset the key to `change-me` and unpaired the phone.
 */
class MainActivityConfigTest {

    @Test
    fun `a real key replaces the stored one`() {
        assertEquals("prod-key", PhoneConfig.mergedKey("old-key", "prod-key"))
        assertEquals("prod-key", PhoneConfig.mergedKey(null, "prod-key"))
    }

    @Test
    fun `a blank key keeps what is stored`() {
        // A UI that never loaded the config sends blank — that is not a request to unpair.
        assertEquals("prod-key", PhoneConfig.mergedKey("prod-key", ""))
        assertEquals("prod-key", PhoneConfig.mergedKey("prod-key", null))
        assertEquals("prod-key", PhoneConfig.mergedKey("prod-key", "   "))
    }

    @Test
    fun `falls back to the default only with nothing saved`() {
        assertEquals(PhoneConfig.DEFAULT_KEY, PhoneConfig.mergedKey(null, null))
        assertEquals(PhoneConfig.DEFAULT_KEY, PhoneConfig.mergedKey("", ""))
        assertEquals("change-me", PhoneConfig.DEFAULT_KEY) // must match the daemon's default
    }

    @Test
    fun `keys are trimmed`() {
        // Copy/paste from a terminal picks up whitespace; a key with a stray space would fail
        // the daemon's exact-match check with no useful error.
        assertEquals("prod-key", PhoneConfig.mergedKey(null, "  prod-key  "))
    }
}
