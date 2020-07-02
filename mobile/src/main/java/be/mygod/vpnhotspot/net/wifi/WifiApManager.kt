package be.mygod.vpnhotspot.net.wifi

import android.annotation.TargetApi
import android.content.Intent
import android.content.pm.PackageManager
import android.net.wifi.SoftApConfiguration
import android.net.wifi.WifiManager
import android.os.Build
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.App
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.TetheringManager
import be.mygod.vpnhotspot.net.wifi.SoftApConfigurationCompat.Companion.toCompat
import be.mygod.vpnhotspot.util.Services

object WifiApManager {
    /**
     * TODO [com.android.server.wifi.WifiContext.ACTION_RESOURCES_APK]
     */
    @RequiresApi(30)
    private const val ACTION_RESOURCES_APK = "com.android.server.wifi.intent.action.SERVICE_WIFI_RESOURCES_APK"
    /**
     * Based on: TODO [com.android.server.wifi.WifiContext.getWifiOverlayApkPkgName]
     */
    @get:RequiresApi(30)
    val resolvedActivity get() = app.packageManager.queryIntentActivities(Intent(ACTION_RESOURCES_APK),
            PackageManager.MATCH_SYSTEM_ONLY).single()

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

    /**
     * Requires NETWORK_SETTINGS permission (or root) on API 30+, and OVERRIDE_WIFI_CONFIG on API 29-.
     */
    val configuration get() = if (Build.VERSION.SDK_INT < 30) @Suppress("DEPRECATION") {
        (getWifiApConfiguration(Services.wifi) as android.net.wifi.WifiConfiguration?)?.toCompat()
                ?: SoftApConfigurationCompat()
    } else (getSoftApConfiguration(Services.wifi) as SoftApConfiguration).toCompat()
    fun setConfiguration(value: SoftApConfigurationCompat) = (if (Build.VERSION.SDK_INT < 30) @Suppress("DEPRECATION") {
        setWifiApConfiguration(Services.wifi, value.toWifiConfiguration())
    } else setSoftApConfiguration(Services.wifi, value.toPlatform())) as Boolean

    private val cancelLocalOnlyHotspotRequest by lazy {
        WifiManager::class.java.getDeclaredMethod("cancelLocalOnlyHotspotRequest")
    }
    fun cancelLocalOnlyHotspotRequest() = cancelLocalOnlyHotspotRequest(Services.wifi)

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
        Services.wifi.isWifiEnabled = false
        Services.wifi.setWifiApEnabled(wifiConfig, true)
    }
    @Suppress("DEPRECATION")
    @Deprecated("Not usable since API 26")
    fun stop() {
        Services.wifi.setWifiApEnabled(null, false)
        Services.wifi.isWifiEnabled = true
    }
}
