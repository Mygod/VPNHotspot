package be.mygod.vpnhotspot.net.wifi

import android.net.wifi.WifiConfiguration
import android.net.wifi.WifiManager
import be.mygod.vpnhotspot.App.Companion.app

object WifiApManager {
    private val setWifiApEnabled = WifiManager::class.java.getDeclaredMethod("setWifiApEnabled",
            WifiConfiguration::class.java, Boolean::class.java)
    /**
     * Start AccessPoint mode with the specified
     * configuration. If the radio is already running in
     * AP mode, update the new configuration
     * Note that starting in access point mode disables station
     * mode operation
     * @param wifiConfig SSID, security and channel details as
     *        part of WifiConfiguration
     * @return {@code true} if the operation succeeds, {@code false} otherwise
     */
    private fun WifiManager.setWifiApEnabled(wifiConfig: WifiConfiguration?, enabled: Boolean) =
            setWifiApEnabled.invoke(this, wifiConfig, enabled) as Boolean

    @Deprecated("No longer usable since API 26.")
    fun start(wifiConfig: WifiConfiguration? = null) {
        app.wifi.isWifiEnabled = false
        app.wifi.setWifiApEnabled(wifiConfig, true)
    }
    @Deprecated("No longer usable since API 26.")
    fun stop() {
        app.wifi.setWifiApEnabled(null, false)
        app.wifi.isWifiEnabled = true
    }
}
