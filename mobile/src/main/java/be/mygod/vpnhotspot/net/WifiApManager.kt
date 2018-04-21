package be.mygod.vpnhotspot.net

import android.content.Context
import android.net.wifi.WifiConfiguration
import android.net.wifi.WifiManager
import be.mygod.vpnhotspot.App.Companion.app

@Deprecated("No longer usable since API 26.")
object WifiApManager {
    private val wifi = app.getSystemService(Context.WIFI_SERVICE) as WifiManager
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

    fun start(wifiConfig: WifiConfiguration? = null) {
        wifi.isWifiEnabled = false
        wifi.setWifiApEnabled(wifiConfig, true)
    }
    fun stop() {
        wifi.setWifiApEnabled(null, false)
        wifi.isWifiEnabled = true
    }
}
