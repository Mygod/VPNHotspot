package be.mygod.vpnhotspot

import android.net.ConnectivityManager
import java.io.File

object NetUtils {
    // hidden constants from ConnectivityManager
    const val ACTION_TETHER_STATE_CHANGED = "android.net.conn.TETHER_STATE_CHANGED"
    const val EXTRA_ACTIVE_TETHER = "tetherArray"

    private val spaces = " +".toPattern()
    private val mac = "^([0-9a-f]{2}:){5}[0-9a-f]{2}$".toPattern()

    private val getTetheredIfaces = ConnectivityManager::class.java.getDeclaredMethod("getTetheredIfaces")
    @Suppress("UNCHECKED_CAST")
    val ConnectivityManager.tetheredIfaces get() = getTetheredIfaces.invoke(this) as Array<String>

    fun arp(iface: String? = null) = File("/proc/net/arp").bufferedReader().useLines {
        // IP address       HW type     Flags       HW address            Mask     Device
        it.map { it.split(spaces) }
                .drop(1)
                .filter { it.size >= 4 && (iface == null || it.getOrNull(5) == iface) &&
                        mac.matcher(it[3]).matches() }
                .associateBy({ it[3] }, { it[0] })
    }
}
