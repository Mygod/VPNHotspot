package be.mygod.vpnhotspot.net

import android.os.Build
import android.system.ErrnoException
import android.system.Os
import android.system.OsConstants
import be.mygod.vpnhotspot.root.ReadArp
import be.mygod.vpnhotspot.root.RootManager
import be.mygod.vpnhotspot.util.parseNumericAddress
import kotlinx.coroutines.CancellationException
import timber.log.Timber
import java.io.File
import java.io.FileNotFoundException
import java.io.IOException
import java.net.Inet4Address
import java.net.InetAddress

data class IpNeighbour(val ip: InetAddress, val dev: String, val lladdr: MacAddressCompat, val state: State) {
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
        private val parser = ("^(Deleted )?(?:([^ ]+) )?dev ([^ ]+) (?:lladdr ([^ ]*))?.*?" +
                "(?: ([INCOMPLET,RAHBSDYF]+))?\$").toRegex()
        /**
         * Fallback format will be used if if_indextoname returns null, which some stupid devices do.
         *
         * Source: https://android.googlesource.com/platform/external/iproute2/+/4b9e917/lib/ll_map.c#152
         */
        private val devFallback = "^if(\\d+)\$".toRegex()

        private fun substituteDev(dev: String): Set<String> {
            val devParser = devFallback.matchEntire(dev)
            if (devParser != null) {
                val index = devParser.groupValues[1].toInt()
                Os.if_indextoname(index)?.let { iface -> return setOf(dev, iface) }
                Timber.w("Failed to find network interface #$index")
            }
            return setOf(dev)
        }

        suspend fun parse(line: String, fullMode: Boolean): List<IpNeighbour> {
            return if (line.isBlank()) emptyList() else try {
                val match = parser.matchEntire(line)!!
                if (match.groups[2] == null) return emptyList()
                val ip = parseNumericAddress(match.groupValues[2])
                val devs = substituteDev(match.groupValues[3])  // by regex, dev is non-empty
                val state = if (match.groupValues[1].isNotEmpty()) State.DELETING else when (match.groupValues[5]) {
                    "", "INCOMPLETE" -> State.INCOMPLETE
                    "REACHABLE", "DELAY", "STALE", "PROBE", "PERMANENT" -> State.VALID
                    "FAILED" -> State.FAILED
                    "NOARP" -> return emptyList()   // skip
                    else -> throw IllegalArgumentException("Unknown state encountered: ${match.groupValues[5]}")
                }
                var lladdr = MacAddressCompat.ALL_ZEROS_ADDRESS
                if (!fullMode && state != State.VALID) {
                    // skip parsing lladdr to avoid requesting root
                    return devs.map { IpNeighbour(ip, it, lladdr, State.DELETING) }
                }
                if (match.groups[4] != null) try {
                    lladdr = MacAddressCompat.fromString(match.groupValues[4])
                } catch (e: IllegalArgumentException) {
                    if (state != State.INCOMPLETE && state != State.DELETING) {
                        Timber.w(IOException("Failed to find MAC address for $line", e))
                    }
                }
                // use ARP as fallback for IPv4, except for INCOMPLETE which by definition does not have arp entry,
                // or for DELETING, which we do not care about MAC not present
                if (lladdr == MacAddressCompat.ALL_ZEROS_ADDRESS && state != State.INCOMPLETE &&
                        state != State.DELETING) {
                    if (ip is Inet4Address) try {
                        val list = arp()
                                .asSequence()
                                .filter { parseNumericAddress(it[ARP_IP_ADDRESS]) == ip && it[ARP_DEVICE] in devs }
                                .map { MacAddressCompat.fromString(it[ARP_HW_ADDRESS]) }
                                .filter { it != MacAddressCompat.ALL_ZEROS_ADDRESS }
                                .distinct()
                                .toList()
                        when (list.size) {
                            1 -> lladdr = list.single()
                            0 -> { }
                            else -> throw IllegalArgumentException("Unexpected output in arp: ${list.joinToString()}")
                        }
                    } catch (e: IllegalArgumentException) {
                        Timber.w(e)
                    }
                    if (lladdr == MacAddressCompat.ALL_ZEROS_ADDRESS) {
                        Timber.d(line)
                        return emptyList()
                    }
                }
                devs.map { IpNeighbour(ip, it, lladdr, state) }
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
        private fun Sequence<String>.makeArp() = this
                .map { it.split(spaces) }
                .drop(1)
                .filter { it.size >= 6 && mac.matcher(it[ARP_HW_ADDRESS]).matches() }
                .toList()
        private suspend fun arp(): List<List<String>> {
            if (System.nanoTime() - arpCacheTime >= ARP_CACHE_EXPIRE) try {
                arpCache = File("/proc/net/arp").bufferedReader().useLines { it.makeArp() }
            } catch (e: IOException) {
                if (e is FileNotFoundException && Build.VERSION.SDK_INT >= 29 &&
                        (e.cause as? ErrnoException)?.errno == OsConstants.EACCES) try {
                    arpCache = RootManager.use { it.execute(ReadArp()) }.value.lineSequence().makeArp()
                } catch (eRoot: Exception) {
                    eRoot.addSuppressed(e)
                    if (eRoot !is CancellationException) Timber.w(eRoot)
                } else Timber.w(e)
            }
            return arpCache
        }
    }
}

data class IpDev(val ip: InetAddress, val dev: String) {
    override fun toString() = "$ip%$dev"
}
@Suppress("FunctionName")
fun IpDev(neighbour: IpNeighbour) = IpDev(neighbour.ip, neighbour.dev)
