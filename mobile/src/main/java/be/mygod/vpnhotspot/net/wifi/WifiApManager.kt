package be.mygod.vpnhotspot.net.wifi

import android.annotation.TargetApi
import android.net.wifi.SoftApConfiguration
import android.net.wifi.WifiManager
import android.os.Build
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.wifi.SoftApConfigurationCompat.Companion.toCompat

object WifiApManager {
    private val getWifiApConfiguration by lazy { WifiManager::class.java.getDeclaredMethod("getWifiApConfiguration") }
    @Suppress("DEPRECATION")
    private val setWifiApConfiguration by lazy {
        WifiManager::class.java.getDeclaredMethod("setWifiApConfiguration",
                android.net.wifi.WifiConfiguration::class.java)
    }
    @get:RequiresApi(30)
    private val getSoftApConfiguration by lazy { WifiManager::class.java.getDeclaredMethod("getSoftApConfiguration") }
    @get:RequiresApi(30)
    private val setSoftApConfiguration by lazy @TargetApi(30) {
        WifiManager::class.java.getDeclaredMethod("setSoftApConfiguration", SoftApConfiguration::class.java)
    }

    var configuration: SoftApConfigurationCompat
        get() = if (Build.VERSION.SDK_INT < 30) @Suppress("DEPRECATION") {
            (getWifiApConfiguration(app.wifi) as android.net.wifi.WifiConfiguration?)?.toCompat()
                    ?: SoftApConfigurationCompat.empty()
        } else (getSoftApConfiguration(app.wifi) as SoftApConfiguration).toCompat()
        set(value) = if (Build.VERSION.SDK_INT < 30) @Suppress("DEPRECATION") {
            require(setWifiApConfiguration(app.wifi,
                    value.toWifiConfiguration()) as Boolean) { "setWifiApConfiguration failed" }
        } else require(setSoftApConfiguration(app.wifi, value.toPlatform()) as Boolean) {
            "setSoftApConfiguration failed"
        }

    private val cancelLocalOnlyHotspotRequest by lazy {
        WifiManager::class.java.getDeclaredMethod("cancelLocalOnlyHotspotRequest")
    }
    fun cancelLocalOnlyHotspotRequest() = cancelLocalOnlyHotspotRequest(app.wifi)

    @Suppress("DEPRECATION")
    private val setWifiApEnabled by lazy {
        WifiManager::class.java.getDeclaredMethod("setWifiApEnabled",
                android.net.wifi.WifiConfiguration::class.java, Boolean::class.java)
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
    @Suppress("DEPRECATION")
    private fun WifiManager.setWifiApEnabled(wifiConfig: android.net.wifi.WifiConfiguration?, enabled: Boolean) =
            setWifiApEnabled(this, wifiConfig, enabled) as Boolean

    /**
     * Although the functionalities were removed in API 26, it is already not functioning correctly on API 25.
     *
     * See also: https://android.googlesource.com/platform/frameworks/base/+/5c0b10a4a9eecc5307bb89a271221f2b20448797%5E%21/
     */
    @Suppress("DEPRECATION")
    @Deprecated("Not usable since API 26, malfunctioning on API 25")
    fun start(wifiConfig: android.net.wifi.WifiConfiguration? = null) {
        app.wifi.isWifiEnabled = false
        app.wifi.setWifiApEnabled(wifiConfig, true)
    }
    @Suppress("DEPRECATION")
    @Deprecated("Not usable since API 26")
    fun stop() {
        app.wifi.setWifiApEnabled(null, false)
        app.wifi.isWifiEnabled = true
    }
}
