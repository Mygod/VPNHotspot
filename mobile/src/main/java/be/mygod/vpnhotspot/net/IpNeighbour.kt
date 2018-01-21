package be.mygod.vpnhotspot.net

import android.util.Log
import be.mygod.vpnhotspot.debugLog

data class IpNeighbour(val ip: String, val dev: String, val lladdr: String) {
    enum class State {
        INCOMPLETE, VALID, VALID_DELAY, FAILED, DELETING
    }

    companion object {
        private const val TAG = "IpNeighbour"

        /**
         * Parser based on:
         *   https://android.googlesource.com/platform/external/iproute2/+/ad0a6a2/ip/ipneigh.c#194
         *   https://people.cs.clemson.edu/~westall/853/notes/arpstate.pdf
         * Assumptions: IPv4 only, RTM_GETNEIGH is never used and show_stats = 0
         */
        private val parser =
                "^(Deleted )?((.+?) )?(dev (.+?) )?(lladdr (.+?))?( proxy)?( ([INCOMPLET,RAHBSDYF]+))?\$".toRegex()
        fun parse(line: String): Pair<IpNeighbour, State>? {
            val match = parser.matchEntire(line)
            if (match == null) {
                if (!line.isBlank()) Log.w(TAG, line)
                return null
            }
            val neighbour = IpNeighbour(match.groupValues[3], match.groupValues[5], match.groupValues[7])
            val state = if (match.groupValues[1].isNotEmpty()) State.DELETING else when (match.groupValues[10]) {
                "", "INCOMPLETE" -> State.INCOMPLETE
                "REACHABLE", "STALE", "PROBE", "PERMANENT" -> State.VALID
                "DELAY" -> State.VALID_DELAY
                "FAILED" -> State.FAILED
                "NOARP" -> return null  // skip
                else -> {
                    Log.w(TAG, "Unknown state encountered: ${match.groupValues[10]}")
                    return null
                }
            }
            return Pair(neighbour, state)
        }
    }
}
