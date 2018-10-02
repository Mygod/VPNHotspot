package be.mygod.vpnhotspot.net.monitor

import be.mygod.vpnhotspot.util.parseNumericAddressNoThrow
import timber.log.Timber
import java.io.File
import java.io.IOException
import java.net.InetAddress

data class IpNeighbour(val ip: InetAddress, val dev: String, val lladdr: String, val state: State) {
    enum class State {
        INCOMPLETE, VALID, FAILED, DELETING
    }

    companion object {
        /**
         * Parser based on:
         *   https://android.googlesource.com/platform/external/iproute2/+/ad0a6a2/ip/ipneigh.c#194
         *   https://people.cs.clemson.edu/~westall/853/notes/arpstate.pdf
         * Assumptions: IP addr (key) always present, RTM_GETNEIGH is never used and show_stats = 0
         */
        private val parser = ("^(Deleted )?(.+?) (dev (.+?) )?(lladdr (.+?))?( router)?( proxy)?" +
                "( ([INCOMPLET,RAHBSDYF]+))?\$").toRegex()
        private fun checkLladdrNotLoopback(lladdr: String) = if (lladdr == "00:00:00:00:00:00") "" else lladdr

        fun parse(line: String): IpNeighbour? {
            val match = parser.matchEntire(line)
            if (match == null) {
                if (line.isNotEmpty()) Timber.w(line)
                return null
            }
            val ip = parseNumericAddressNoThrow(match.groupValues[2]) ?: return null
            val dev = match.groupValues[4]
            var lladdr = checkLladdrNotLoopback(match.groupValues[6])
            // use ARP as fallback
            if (dev.isNotEmpty() && lladdr.isEmpty()) lladdr = checkLladdrNotLoopback(arp()
                    .asSequence()
                    .filter { parseNumericAddressNoThrow(it[ARP_IP_ADDRESS]) == ip && it[ARP_DEVICE] == dev }
                    .map { it[ARP_HW_ADDRESS] }
                    .singleOrNull() ?: "")
            val state = if (match.groupValues[1].isNotEmpty() || lladdr.isEmpty()) State.DELETING else
                when (match.groupValues[10]) {
                    "", "INCOMPLETE" -> State.INCOMPLETE
                    "REACHABLE", "DELAY", "STALE", "PROBE", "PERMANENT" -> State.VALID
                    "FAILED" -> State.FAILED
                    "NOARP" -> return null  // skip
                    else -> {
                        Timber.w("Unknown state encountered: ${match.groupValues[10]}")
                        return null
                    }
                }
            return IpNeighbour(ip, dev, lladdr, state)
        }

        private val spaces = " +".toPattern()
        private val mac = "^([0-9a-f]{2}:){5}[0-9a-f]{2}$".toPattern()

        // IP address       HW type     Flags       HW address            Mask     Device
        private const val ARP_IP_ADDRESS = 0
        private const val ARP_HW_ADDRESS = 3
        private const val ARP_DEVICE = 5
        private const val ARP_CACHE_EXPIRE = 1L * 1000 * 1000 * 1000
        private var arpCache = emptyList<List<String>>()
        private var arpCacheTime = -ARP_CACHE_EXPIRE
        private fun arp(): List<List<String>> {
            if (System.nanoTime() - arpCacheTime >= ARP_CACHE_EXPIRE) try {
                arpCache = File("/proc/net/arp").bufferedReader().readLines()
                        .asSequence()
                        .map { it.split(spaces) }
                        .drop(1)
                        .filter { it.size >= 6 && mac.matcher(it[ARP_HW_ADDRESS]).matches() }
                        .toList()
            } catch (e: IOException) {
                Timber.w(e)
            }
            return arpCache
        }
    }
}
