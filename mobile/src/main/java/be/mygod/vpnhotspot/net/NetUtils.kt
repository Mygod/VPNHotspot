package be.mygod.vpnhotspot.net

import android.os.Build
import android.os.Bundle
import android.support.annotation.RequiresApi
import java.io.File
import java.io.IOException

object NetUtils {
    // hidden constants from ConnectivityManager
    /**
     * This is a sticky broadcast since almost forever.
     *
     * https://android.googlesource.com/platform/frameworks/base.git/+/2a091d7aa0c174986387e5d56bf97a87fe075bdb%5E%21/services/java/com/android/server/connectivity/Tethering.java
     */
    const val ACTION_TETHER_STATE_CHANGED = "android.net.conn.TETHER_STATE_CHANGED"
    @Deprecated("No longer used on Android 8+ (API 26+)")
    private const val EXTRA_ACTIVE_TETHER_LEGACY = "activeArray"
    @RequiresApi(26)
    private const val EXTRA_ACTIVE_LOCAL_ONLY = "localOnlyArray"
    @RequiresApi(26)
    private const val EXTRA_ACTIVE_TETHER = "tetherArray"

    private val spaces = " +".toPattern()
    private val mac = "^([0-9a-f]{2}:){5}[0-9a-f]{2}$".toPattern()

    fun getTetheredIfaces(extras: Bundle) = if (Build.VERSION.SDK_INT >= 26)
        extras.getStringArrayList(EXTRA_ACTIVE_TETHER).toSet() + extras.getStringArrayList(EXTRA_ACTIVE_LOCAL_ONLY)
    else extras.getStringArrayList(EXTRA_ACTIVE_TETHER_LEGACY).toSet()

    // IP address       HW type     Flags       HW address            Mask     Device
    const val ARP_IP_ADDRESS = 0
    const val ARP_HW_ADDRESS = 3
    const val ARP_DEVICE = 5
    private const val ARP_CACHE_EXPIRE = 1L * 1000 * 1000 * 1000
    private var arpCache = emptyList<List<String>>()
    private var arpCacheTime = -ARP_CACHE_EXPIRE
    fun arp(): List<List<String>> {
        if (System.nanoTime() - arpCacheTime >= ARP_CACHE_EXPIRE) try {
            arpCache = File("/proc/net/arp").bufferedReader().useLines {
                it.map { it.split(spaces) }
                        .drop(1)
                        .filter { it.size >= 6 && mac.matcher(it[ARP_HW_ADDRESS]).matches() }
                        .toList()
            }
        } catch (e: IOException) {
            e.printStackTrace()
        }
        return arpCache
    }
}
