package be.mygod.vpnhotspot.net

import android.util.Log

data class IpNeighbour(val ip: String, val dev: String, val lladdr: String, val state: State) {
    enum class State {
        INCOMPLETE, VALID, VALID_DELAY, FAILED, DELETING
    }

    companion object {
        private const val TAG = "IpNeighbour"

        /**
         * Parser based on:
         *   https://android.googlesource.com/platform/external/iproute2/+/ad0a6a2/ip/ipneigh.c#194
         *   https://people.cs.clemson.edu/~westall/853/notes/arpstate.pdf
         * Assumptions: IP addr (key) always present, IPv4 only, RTM_GETNEIGH is never used and show_stats = 0
         */
        private val parser =
                "^(Deleted )?(.+?) (dev (.+?) )?(lladdr (.+?))?( proxy)?( ([INCOMPLET,RAHBSDYF]+))?\$".toRegex()
        fun parse(line: String): IpNeighbour? {
            val match = parser.matchEntire(line)
            if (match == null) {
                if (!line.isBlank()) Log.w(TAG, line)
                return null
            }
            val ip = match.groupValues[2]
            val dev = match.groupValues[4]
            var lladdr = match.groupValues[6]
            // use ARP as fallback
            if (dev.isNotBlank() && lladdr.isBlank()) lladdr = (NetUtils.arp()
                    .filter { it[NetUtils.ARP_IP_ADDRESS] == ip && it[NetUtils.ARP_DEVICE] == dev }
                    .map { it[NetUtils.ARP_HW_ADDRESS] }
                    .singleOrNull() ?: "")
            val state = if (match.groupValues[1].isNotEmpty()) State.DELETING else when (match.groupValues[9]) {
                "", "INCOMPLETE" -> State.INCOMPLETE
                "REACHABLE", "STALE", "PROBE", "PERMANENT" -> State.VALID
                "DELAY" -> State.VALID_DELAY
                "FAILED" -> State.FAILED
                "NOARP" -> return null  // skip
                else -> {
                    Log.w(TAG, "Unknown state encountered: ${match.groupValues[9]}")
                    return null
                }
            }
            return IpNeighbour(ip, dev, lladdr, state)
        }
    }
}
