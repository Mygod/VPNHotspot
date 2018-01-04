package be.mygod.vpnhotspot

import android.net.wifi.WifiConfiguration
import android.util.Log
import java.io.DataInputStream
import java.io.File
import java.io.IOException

object NetUtils {
    private const val TAG = "NetUtils"
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

    /**
     * Load AP configuration from persistent storage.
     *
     * Based on: https://android.googlesource.com/platform/frameworks/opt/net/wifi/+/0cafbe0/service/java/com/android/server/wifi/WifiApConfigStore.java#138
     */
    fun loadApConfiguration(): WifiConfiguration? = try {
        loggerSuStream("cat /data/misc/wifi/softap.conf").buffered().use {
            val data = DataInputStream(it)
            val version = data.readInt()
            when (version) {
                1, 2 -> {
                    val config = WifiConfiguration()
                    config.SSID = data.readUTF()
                    if (version >= 2) data.readLong()   // apBand and apChannel
                    val authType = data.readInt()
                    config.allowedKeyManagement.set(authType)
                    if (authType != WifiConfiguration.KeyMgmt.NONE) config.preSharedKey = data.readUTF()
                    config
                }
                else -> {
                    Log.e(TAG, "Bad version on hotspot configuration file $version")
                    null
                }
            }
        }
    } catch (e: IOException) {
        Log.e(TAG, "Error reading hotspot configuration $e")
        null
    }
}
