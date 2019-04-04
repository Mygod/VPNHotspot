package be.mygod.vpnhotspot.net.wifi

import android.net.wifi.WifiConfiguration
import android.net.wifi.WifiManager
import be.mygod.vpnhotspot.App.Companion.app
import java.lang.IllegalArgumentException

object WifiApManager {
    private val getWifiApConfiguration by lazy { WifiManager::class.java.getDeclaredMethod("getWifiApConfiguration") }
    private val setWifiApConfiguration by lazy {
        WifiManager::class.java.getDeclaredMethod("setWifiApConfiguration", WifiConfiguration::class.java)
    }
    var configuration: WifiConfiguration
        get() = getWifiApConfiguration.invoke(app.wifi) as WifiConfiguration
        set(value) {
            if (setWifiApConfiguration.invoke(app.wifi, value) as? Boolean != true) throw IllegalArgumentException()
        }

    private val setWifiApEnabled by lazy {
        WifiManager::class.java.getDeclaredMethod("setWifiApEnabled",
                WifiConfiguration::class.java, Boolean::class.java)
    }
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

    /**
     * Although the functionalities were removed in API 26, it is already not functioning correctly on API 25.
     *
     * See also: https://android.googlesource.com/platform/frameworks/base/+/5c0b10a4a9eecc5307bb89a271221f2b20448797%5E%21/
     */
    @Deprecated("Not usable since API 26, malfunctioning on API 25")
    fun start(wifiConfig: WifiConfiguration? = null) {
        app.wifi.isWifiEnabled = false
        app.wifi.setWifiApEnabled(wifiConfig, true)
    }
    @Deprecated("Not usable since API 26")
    fun stop() {
        app.wifi.setWifiApEnabled(null, false)
        app.wifi.isWifiEnabled = true
    }
}
