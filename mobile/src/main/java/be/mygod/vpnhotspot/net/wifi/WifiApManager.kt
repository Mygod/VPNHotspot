package be.mygod.vpnhotspot.net.wifi

import android.annotation.TargetApi
import android.content.Intent
import android.content.pm.PackageManager
import android.net.MacAddress
import android.net.wifi.SoftApConfiguration
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Handler
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.wifi.SoftApConfigurationCompat.Companion.toCompat
import be.mygod.vpnhotspot.util.ConstantLookup
import be.mygod.vpnhotspot.util.LongConstantLookup
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.callSuper
import timber.log.Timber
import java.lang.reflect.InvocationHandler
import java.lang.reflect.Method
import java.lang.reflect.Proxy
import java.util.concurrent.Executor

object WifiApManager {
    /**
     * https://android.googlesource.com/platform/frameworks/opt/net/wifi/+/000ad45/service/java/com/android/server/wifi/WifiContext.java#41
     */
    @RequiresApi(30)
    private const val ACTION_RESOURCES_APK = "com.android.server.wifi.intent.action.SERVICE_WIFI_RESOURCES_APK"
    /**
     * Based on: https://android.googlesource.com/platform/frameworks/opt/net/wifi/+/000ad45/service/java/com/android/server/wifi/WifiContext.java#66
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

    @RequiresApi(28)
    interface SoftApCallbackCompat {
        /**
         * Called when soft AP state changes.
         *
         * @param state new new AP state. One of {@link #WIFI_AP_STATE_DISABLED},
         *        {@link #WIFI_AP_STATE_DISABLING}, {@link #WIFI_AP_STATE_ENABLED},
         *        {@link #WIFI_AP_STATE_ENABLING}, {@link #WIFI_AP_STATE_FAILED}
         * @param failureReason reason when in failed state. One of
         *        {@link #SAP_START_FAILURE_GENERAL}, {@link #SAP_START_FAILURE_NO_CHANNEL}
         */
        fun onStateChanged(state: Int, failureReason: Int) { }

        /**
         * Called when number of connected clients to soft AP changes.
         *
         * @param numClients number of connected clients
         */
        @Deprecated("onConnectedClientsChanged")
        fun onNumClientsChanged(numClients: Int) { }

        @RequiresApi(30)
        fun onConnectedClientsChanged(clients: List<MacAddress>) {
            @Suppress("DEPRECATION")
            onNumClientsChanged(clients.size)
        }

        @RequiresApi(30)
        fun onInfoChanged(frequency: Int, bandwidth: Int) { }

        @RequiresApi(30)
        fun onCapabilityChanged(maxSupportedClients: Int, supportedFeatures: Long) { }

        @RequiresApi(30)
        fun onBlockedClientConnecting(client: MacAddress, blockedReason: Int) { }
    }
    @RequiresApi(28)
    val failureReasonLookup = ConstantLookup<WifiManager>("SAP_START_FAILURE_",
            "SAP_START_FAILURE_GENERAL", "SAP_START_FAILURE_NO_CHANNEL")
    @get:RequiresApi(30)
    val clientBlockLookup by lazy { ConstantLookup<WifiManager>("SAP_CLIENT_") }

    private val interfaceSoftApCallback by lazy { Class.forName("android.net.wifi.WifiManager\$SoftApCallback") }
    private val registerSoftApCallback by lazy {
        val parameters = if (Build.VERSION.SDK_INT >= 30) {
            arrayOf(Executor::class.java, interfaceSoftApCallback)
        } else arrayOf(interfaceSoftApCallback, Handler::class.java)
        WifiManager::class.java.getDeclaredMethod("registerSoftApCallback", *parameters)
    }
    private val unregisterSoftApCallback by lazy {
        WifiManager::class.java.getDeclaredMethod("unregisterSoftApCallback", interfaceSoftApCallback)
    }

    private val getMacAddress by lazy {
        Class.forName("android.net.wifi.WifiClient").getDeclaredMethod("getMacAddress")
    }

    private val classSoftApInfo by lazy { Class.forName("android.net.wifi.SoftApInfo") }
    private val getFrequency by lazy { classSoftApInfo.getDeclaredMethod("getFrequency") }
    private val getBandwidth by lazy { classSoftApInfo.getDeclaredMethod("getBandwidth") }
    @RequiresApi(30)
    val channelWidthLookup = ConstantLookup("CHANNEL_WIDTH_") { classSoftApInfo }
    const val CHANNEL_WIDTH_INVALID = 0

    private val classSoftApCapability by lazy { Class.forName("android.net.wifi.SoftApCapability") }
    private val getMaxSupportedClients by lazy { classSoftApCapability.getDeclaredMethod("getMaxSupportedClients") }
    private val areFeaturesSupported by lazy {
        classSoftApCapability.getDeclaredMethod("areFeaturesSupported", Long::class.java)
    }
    @get:RequiresApi(30)
    val featureLookup by lazy { LongConstantLookup(classSoftApCapability, "SOFTAP_FEATURE_") }

    @RequiresApi(28)
    fun registerSoftApCallback(callback: SoftApCallbackCompat, executor: Executor): Any {
        val proxy = Proxy.newProxyInstance(interfaceSoftApCallback.classLoader,
                arrayOf(interfaceSoftApCallback), object : InvocationHandler {
            override fun invoke(proxy: Any, method: Method, args: Array<out Any?>?) =
                    if (Build.VERSION.SDK_INT < 30 && interfaceSoftApCallback === method.declaringClass) {
                        executor.execute { invokeActual(proxy, method, args) }
                        null    // no return value as of API 30
                    } else invokeActual(proxy, method, args)

            private fun invokeActual(proxy: Any, method: Method, args: Array<out Any?>?): Any? {
                val noArgs = args?.size ?: 0
                return when (val name = method.name) {
                    "onStateChanged" -> {
                        if (noArgs != 2) Timber.w("Unexpected args for $name: ${args?.contentToString()}")
                        callback.onStateChanged(args!![0] as Int, args[1] as Int)
                    }
                    "onNumClientsChanged" -> @Suppress("DEPRECATION") {
                        if (Build.VERSION.SDK_INT >= 30) Timber.w(Exception("Unexpected onNumClientsChanged"))
                        if (noArgs != 1) Timber.w("Unexpected args for $name: ${args?.contentToString()}")
                        callback.onNumClientsChanged(args!![0] as Int)
                    }
                    "onConnectedClientsChanged" -> @TargetApi(30) {
                        if (Build.VERSION.SDK_INT < 30) Timber.w(Exception("Unexpected onConnectedClientsChanged"))
                        if (noArgs != 1) Timber.w("Unexpected args for $name: ${args?.contentToString()}")
                        callback.onConnectedClientsChanged((args!![0] as? Iterable<*> ?: return null)
                                .map { getMacAddress(it) as MacAddress })
                    }
                    "onInfoChanged" -> @TargetApi(30) {
                        if (Build.VERSION.SDK_INT < 30) Timber.w(Exception("Unexpected onInfoChanged"))
                        if (noArgs != 1) Timber.w("Unexpected args for $name: ${args?.contentToString()}")
                        val softApInfo = args!![0]
                        if (softApInfo != null && classSoftApInfo.isAssignableFrom(softApInfo.javaClass)) {
                            callback.onInfoChanged(getFrequency(softApInfo) as Int, getBandwidth(softApInfo) as Int)
                        } else null
                    }
                    "onCapabilityChanged" -> @TargetApi(30) {
                        if (Build.VERSION.SDK_INT < 30) Timber.w(Exception("Unexpected onCapabilityChanged"))
                        if (noArgs != 1) Timber.w("Unexpected args for $name: ${args?.contentToString()}")
                        val softApCapability = args!![0]
                        var supportedFeatures = 0L
                        var probe = 1L
                        while (probe != 0L) {
                            if (areFeaturesSupported(softApCapability, probe) as Boolean) {
                                supportedFeatures = supportedFeatures or probe
                            }
                            probe += probe
                        }
                        callback.onCapabilityChanged(getMaxSupportedClients(softApCapability) as Int, supportedFeatures)
                    }
                    "onBlockedClientConnecting" -> @TargetApi(30) {
                        if (Build.VERSION.SDK_INT < 30) Timber.w(Exception("Unexpected onBlockedClientConnecting"))
                        if (noArgs != 2) Timber.w("Unexpected args for $name: ${args?.contentToString()}")
                        callback.onBlockedClientConnecting(getMacAddress(args!![0]) as MacAddress, args[1] as Int)
                    }
                    else -> callSuper(interfaceSoftApCallback, proxy, method, args)
                }
            }
        })
        if (Build.VERSION.SDK_INT >= 30) {
            registerSoftApCallback(Services.wifi, executor, proxy)
        } else registerSoftApCallback(Services.wifi, proxy, null)
        return proxy
    }
    @RequiresApi(28)
    fun unregisterSoftApCallback(key: Any) = unregisterSoftApCallback(Services.wifi, key)

    private val cancelLocalOnlyHotspotRequest by lazy {
        WifiManager::class.java.getDeclaredMethod("cancelLocalOnlyHotspotRequest")
    }
    /**
     * This is the only way to unregister requests besides app exiting.
     * Therefore, we are happy with crashing the app if reflection fails.
     */
    @RequiresApi(26)
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
