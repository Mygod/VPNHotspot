package be.mygod.vpnhotspot

import java.io.File

object NetUtils {
    private val spaces = " +".toPattern()
    private val mac = "^([0-9a-f]{2}:){5}[0-9a-f]{2}$".toPattern()

    fun arp(iface: String? = null) = File("/proc/net/arp").bufferedReader().useLines {
        // IP address       HW type     Flags       HW address            Mask     Device
        it.map { it.split(spaces) }
                .drop(1)
                .filter { it.size >= 4 && (iface == null || it.getOrNull(5) == iface) &&
                        mac.matcher(it[3]).matches() }
                .associateBy({ it[3] }, { it[0] })
    }
}
