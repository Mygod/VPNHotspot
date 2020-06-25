package be.mygod.vpnhotspot.net

import android.os.Build
import android.system.ErrnoException
import android.system.OsConstants
import be.mygod.vpnhotspot.root.ReadArp
import be.mygod.vpnhotspot.root.RootManager
import be.mygod.vpnhotspot.util.parseNumericAddress
import kotlinx.coroutines.runBlocking
import timber.log.Timber
import java.io.File
import java.io.FileNotFoundException
import java.io.IOException
import java.net.Inet4Address
import java.net.InetAddress
import java.net.NetworkInterface
import java.net.SocketException

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
        private val parser = "^(Deleted )?([^ ]+) dev ([^ ]+) (lladdr ([^ ]*))?.*?( ([INCOMPLET,RAHBSDYF]+))?\$"
                .toRegex()
        /**
         * Fallback format will be used if if_indextoname returns null, which some stupid devices do.
         *
         * Source: https://android.googlesource.com/platform/external/iproute2/+/4b9e917/lib/ll_map.c#152
         */
        private val devFallback = "^if(\\d+)\$".toRegex()

        private fun populateList(base: IpNeighbour): List<IpNeighbour> {
            val devParser = devFallback.matchEntire(base.dev)
            if (devParser != null) try {
                val index = devParser.groupValues[1].toInt()
                val iface = NetworkInterface.getByIndex(index)
                if (iface == null) Timber.w("Failed to find network interface #$index")
                else return listOf(base.copy(dev = iface.name), base)
            } catch (_: SocketException) { }
            return listOf(base)
        }

        fun parse(line: String, fullMode: Boolean): List<IpNeighbour> {
            return if (line.isBlank()) emptyList() else try {
                val match = parser.matchEntire(line)!!
                val ip = parseNumericAddress(match.groupValues[2])  // by regex, ip is non-empty
                val dev = match.groupValues[3]                      // by regex, dev is non-empty as well
                val state = if (match.groupValues[1].isNotEmpty()) State.DELETING else when (match.groupValues[7]) {
                    "", "INCOMPLETE" -> State.INCOMPLETE
                    "REACHABLE", "DELAY", "STALE", "PROBE", "PERMANENT" -> State.VALID
                    "FAILED" -> State.FAILED
                    "NOARP" -> return emptyList()   // skip
                    else -> throw IllegalArgumentException("Unknown state encountered: ${match.groupValues[7]}")
                }
                var lladdr = MacAddressCompat.ALL_ZEROS_ADDRESS
                if (!fullMode && state != State.VALID) {
                    // skip parsing lladdr to avoid requesting root
                    return populateList(IpNeighbour(ip, dev, lladdr, State.DELETING))
                }
                if (match.groups[4] != null) try {
                    lladdr = MacAddressCompat.fromString(match.groupValues[5])
                } catch (e: IllegalArgumentException) {
                    if (state != State.INCOMPLETE && state != State.DELETING) {
                        Timber.w(IOException("Failed to find MAC address for $line", e))
                    }
                }
                // use ARP as fallback for IPv4, except for INCOMPLETE which by definition does not have arp entry,
                // or for DELETING, which we do not care about MAC not present
                if (ip is Inet4Address && lladdr == MacAddressCompat.ALL_ZEROS_ADDRESS && state != State.INCOMPLETE &&
                        state != State.DELETING) try {
                    lladdr = MacAddressCompat.fromString(arp()
                            .asSequence()
                            .filter { parseNumericAddress(it[ARP_IP_ADDRESS]) == ip && it[ARP_DEVICE] == dev }
                            .map { it[ARP_HW_ADDRESS] }
                            .distinct()
                            .singleOrNull() ?: throw IllegalArgumentException("singleOrNull"))
                } catch (e: IllegalArgumentException) {
                    Timber.w(e)
                }
                populateList(IpNeighbour(ip, dev, lladdr, state))
            } catch (e: Exception) {
                Timber.w(IllegalArgumentException("Unable to parse line: $line", e))
                emptyList<IpNeighbour>()
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
        private fun arp(): List<List<String>> {
            if (System.nanoTime() - arpCacheTime >= ARP_CACHE_EXPIRE) try {
                arpCache = File("/proc/net/arp").bufferedReader().lineSequence().makeArp()
            } catch (e: IOException) {
                if (e is FileNotFoundException && Build.VERSION.SDK_INT >= 29 &&
                        (e.cause as? ErrnoException)?.errno == OsConstants.EACCES) try {
                    arpCache = runBlocking {
                        RootManager.use { it.execute(ReadArp()) }
                    }.value.lineSequence().makeArp()
                } catch (eRoot: Exception) {
                    eRoot.addSuppressed(e)
                    Timber.w(eRoot)
                } else Timber.w(e)
            }
            return arpCache
        }
    }
}

data class IpDev(val ip: InetAddress, val dev: String) {
    override fun toString() = "$ip%$dev"
}
fun IpDev(neighbour: IpNeighbour) = IpDev(neighbour.ip, neighbour.dev)
