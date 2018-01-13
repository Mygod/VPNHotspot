package be.mygod.vpnhotspot

import android.os.Build
import android.os.Bundle
import android.support.annotation.RequiresApi
import java.io.File

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

    fun arp(iface: String? = null) = File("/proc/net/arp").bufferedReader().useLines {
        // IP address       HW type     Flags       HW address            Mask     Device
        it.map { it.split(spaces) }
                .drop(1)
                .filter { it.size >= 4 && (iface == null || it.getOrNull(5) == iface) &&
                        mac.matcher(it[3]).matches() }
                .associateBy({ it[3] }, { it[0] })
    }
}
