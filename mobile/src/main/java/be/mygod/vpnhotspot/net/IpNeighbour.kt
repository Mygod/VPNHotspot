package be.mygod.vpnhotspot.net

import be.mygod.vpnhotspot.room.macToLong
import be.mygod.vpnhotspot.util.parseNumericAddress
import timber.log.Timber
import java.io.File
import java.io.IOException
import java.net.InetAddress
import java.net.NetworkInterface
import java.net.SocketException

data class IpNeighbour(val ip: InetAddress, val dev: String, val lladdr: Long, val state: State) {
    enum class State {
        INCOMPLETE, VALID, FAILED, DELETING
    }

    companion object {
        /**
         * Parser based on:
         *   https://android.googlesource.com/platform/external/iproute2/+/ad0a6a2/ip/ipneigh.c#194
         *   https://people.cs.clemson.edu/~westall/853/notes/arpstate.pdf
         * Assumptions: IP addr (key) always present and RTM_GETNEIGH is never used
         */
        private val parser = "^(Deleted )?([^ ]+) dev ([^ ]+) (lladdr ([^ ]*))?.*?( ([INCOMPLET,RAHBSDYF]+))?\$"
                .toRegex()
        /**
         * Fallback format will be used if if_indextoname returns null, which some stupid devices do.
         *
         * Source: https://android.googlesource.com/platform/external/iproute2/+/4b9e917/lib/ll_map.c#152
         */
        private val devFallback = "^if(\\d+)\$".toRegex()
        private fun checkLladdrNotLoopback(lladdr: String) = if (lladdr == "00:00:00:00:00:00") "" else lladdr

        fun parse(line: String): List<IpNeighbour> {
            return try {
                val match = parser.matchEntire(line)!!
                val ip = parseNumericAddress(match.groupValues[2])  // by regex, ip is non-empty
                val dev = match.groupValues[3]                      // by regex, dev is non-empty as well
                var lladdr = checkLladdrNotLoopback(match.groupValues[5])
                // use ARP as fallback
                if (lladdr.isEmpty()) lladdr = checkLladdrNotLoopback(arp()
                        .asSequence()
                        .filter { parseNumericAddress(it[ARP_IP_ADDRESS]) == ip && it[ARP_DEVICE] == dev }
                        .map { it[ARP_HW_ADDRESS] }
                        .singleOrNull() ?: "")
                val state = if (match.groupValues[1].isNotEmpty()) State.DELETING else
                    when (match.groupValues[7]) {
                        "", "INCOMPLETE" -> State.INCOMPLETE
                        "REACHABLE", "DELAY", "STALE", "PROBE", "PERMANENT" -> State.VALID
                        "FAILED" -> State.FAILED
                        "NOARP" -> return emptyList()   // skip
                        else -> throw IllegalArgumentException("Unknown state encountered: ${match.groupValues[7]}")
                    }
                val mac = if (lladdr.isEmpty()) {
                    if (match.groups[4] == null) return emptyList()
                    Timber.w(IOException("Failed to find MAC address for $line"))
                    0
                } else lladdr.macToLong()
                val result = IpNeighbour(ip, dev, mac, state)
                val devParser = devFallback.matchEntire(dev)
                if (devParser != null) try {
                    val index = devParser.groupValues[1].toInt()
                    val iface = NetworkInterface.getByIndex(index)
                    if (iface == null) Timber.w("Failed to find network interface #$index")
                    else return listOf(IpNeighbour(ip, iface.name, mac, state), result)
                } catch (_: SocketException) { }
                listOf(result)
            } catch (e: Exception) {
                Timber.w(IllegalArgumentException("Unable to parse line: $line", e))
                emptyList()
            }
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
