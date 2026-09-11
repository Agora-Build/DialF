package build.agora.dialf_phone

/**
 * Pure config rules shared by the UI bridge and the service — no Android dependencies, so
 * they can be unit-tested on the JVM.
 */
object PhoneConfig {

    /** Shared key a fresh install starts with; must match the daemon's config default. */
    const val DEFAULT_KEY = "change-me"

    /**
     * The key to persist, given what is stored and what the UI sent.
     *
     * The UI writes back whatever it displays, so a screen that never loaded the saved config
     * sends a blank (or default) key. That is not a request to unpair the phone — keep what is
     * stored. Only a real, non-blank value replaces it.
     */
    fun mergedKey(saved: String?, incoming: String?): String {
        val wanted = incoming?.trim().orEmpty()
        if (wanted.isNotEmpty()) return wanted
        return saved?.takeIf { it.isNotBlank() } ?: DEFAULT_KEY
    }
}
