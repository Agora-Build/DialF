package build.agora.dialf_phone

/**
 * Finds this phone's own wireless-debugging port among the `_adb-tls-connect._tcp` services on
 * the LAN. Every phone with wireless debugging on is visible, so the only safe pick is the one
 * advertised at this phone's own address — taking the first would report another phone's port.
 */
object AdbEndpoint {
    const val SERVICE_TYPE = "_adb-tls-connect._tcp"

    /** Port of the service advertised at one of [ownIps], or null if none is. */
    fun pickOwn(resolved: List<Pair<String, Int>>, ownIps: Set<String>): Int? =
        resolved.firstOrNull { (ip, port) -> ip in ownIps && port in 1..65535 }?.second

    /** Whether an NSD service type (which may carry a trailing dot) is the adb one. */
    fun isAdbService(serviceType: String?): Boolean =
        serviceType?.trimEnd('.')?.trimStart('.')?.startsWith(SERVICE_TYPE) == true
}
