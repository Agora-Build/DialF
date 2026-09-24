package build.agora.dialf_phone

import android.content.Context
import android.net.wifi.WifiManager
import android.util.Log

/**
 * Opens the Wi-Fi multicast filter for the length of a discovery window.
 *
 * Phones with an APF packet filter (verified on a Pixel 4 XL, Android 13) drop every inbound
 * multicast frame unless some app holds a [WifiManager.MulticastLock] — and mDNS replies are
 * multicast. NsdManager does not take the lock for us there, so without this, discovery ran
 * every 30s and never saw a single `_dialfd._tcp` reply (APF dump: `Multicast: DROP`).
 * Phones that don't filter are unaffected by holding it.
 *
 * Held only while discovering: an open filter wakes the CPU for every multicast frame on the
 * LAN. Not reference-counted, so acquire/release are idempotent. Failures are reported and
 * swallowed — discovery must still run (and work where there is no filter) without the lock.
 */
class DiscoveryMulticastLock internal constructor(
    private val lock: Lock?,
    private val onError: (String, RuntimeException) -> Unit,
) {
    constructor(context: Context) : this(
        systemLock(context),
        { what, e -> Log.w("DialfConn", "multicast lock $what failed", e) },
    )

    /** The part of [WifiManager.MulticastLock] used here, so tests can stand in for it. */
    internal interface Lock {
        val isHeld: Boolean
        fun acquire()
        fun release()
    }

    fun acquire() = guard("acquire") { if (!it.isHeld) it.acquire() }

    fun release() = guard("release") { if (it.isHeld) it.release() }

    private fun guard(what: String, op: (Lock) -> Unit) {
        val l = lock ?: return
        try {
            op(l)
        } catch (e: RuntimeException) {
            onError(what, e)
        }
    }

    private companion object {
        fun systemLock(context: Context): Lock? = try {
            val wifi = context.getSystemService(WifiManager::class.java) ?: return null
            val ml = wifi.createMulticastLock("dialf-discovery").apply { setReferenceCounted(false) }
            object : Lock {
                override val isHeld get() = ml.isHeld
                override fun acquire() = ml.acquire()
                override fun release() = ml.release()
            }
        } catch (e: RuntimeException) {
            Log.w("DialfConn", "no multicast lock available", e)
            null
        }
    }
}
