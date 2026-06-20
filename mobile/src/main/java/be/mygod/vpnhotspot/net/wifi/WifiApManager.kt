package be.mygod.vpnhotspot.net.wifi

import android.annotation.SuppressLint
import android.annotation.TargetApi
import android.content.Intent
import android.content.pm.ActivityInfo
import android.content.pm.PackageManager
import android.content.res.Resources
import android.net.wifi.ISoftApCallback
import android.net.wifi.IWifiManager
import android.net.wifi.DeauthenticationReasonCode
import android.net.wifi.SoftApCapability
import android.net.wifi.SoftApConfiguration
import android.net.wifi.SoftApInfo
import android.net.wifi.WifiClient
import android.net.wifi.WifiConfiguration
import android.net.wifi.WifiManager
import android.net.wifi.`WifiManager$SoftApCallback`
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.IBinder
import android.os.Parcelable
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.wifi.SoftApConfigurationCompat.Companion.toCompat
import be.mygod.vpnhotspot.net.wifi.WifiApManager.EXTRA_WIFI_AP_STATE
import be.mygod.vpnhotspot.net.wifi.WifiApManager.WIFI_AP_STATE_CHANGED_ACTION
import be.mygod.vpnhotspot.net.wifi.WifiApManager.WIFI_AP_STATE_DISABLED
import be.mygod.vpnhotspot.net.wifi.WifiApManager.WIFI_AP_STATE_DISABLING
import be.mygod.vpnhotspot.net.wifi.WifiApManager.WIFI_AP_STATE_ENABLED
import be.mygod.vpnhotspot.net.wifi.WifiApManager.WIFI_AP_STATE_ENABLING
import be.mygod.vpnhotspot.net.wifi.WifiApManager.WIFI_AP_STATE_FAILED
import be.mygod.vpnhotspot.util.ConstantLookup
import be.mygod.vpnhotspot.util.InPlaceExecutor
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.UnblockCentral
import be.mygod.vpnhotspot.util.binderCallbackFlow
import be.mygod.vpnhotspot.util.callSuper
import be.mygod.vpnhotspot.util.findIdentifier
import be.mygod.vpnhotspot.util.matches
import kotlinx.parcelize.Parcelize
import timber.log.Timber
import java.lang.reflect.InvocationHandler
import java.lang.reflect.InvocationTargetException
import java.lang.reflect.Method
import java.lang.reflect.Proxy
import java.util.concurrent.Executor

object WifiApManager {
    /**
     * https://android.googlesource.com/platform/frameworks/opt/net/wifi/+/000ad45/service/java/com/android/server/wifi/WifiContext.java#41
     */
    @RequiresApi(30)
    private const val ACTION_RESOURCES_APK = "com.android.server.wifi.intent.action.SERVICE_WIFI_RESOURCES_APK"
    @RequiresApi(30)
    const val RESOURCES_PACKAGE = "com.android.wifi.resources"
    /**
     * Based on: https://cs.android.com/android/platform/superproject/+/master:packages/modules/Wifi/framework/java/android/net/wifi/WifiContext.java;l=66;drc=5ca657189aac546af0aafaba11bbc9c5d889eab3
     */
    @get:RequiresApi(30)
    val resolvedActivity: ActivityInfo get() {
        val list = app.packageManager.queryIntentActivities(Intent(ACTION_RESOURCES_APK),
            PackageManager.MATCH_SYSTEM_ONLY).distinctBy { it.activityInfo.applicationInfo.packageName }
        require(list.isNotEmpty()) { "Missing $ACTION_RESOURCES_APK" }
        if (list.size > 1) {
            list.singleOrNull {
                it.activityInfo.applicationInfo.sourceDir.startsWith("/apex/com.android.wifi")
            }?.let { return it.activityInfo }
            Timber.w(Exception("Found > 1 apk: " + list.joinToString {
                val info = it.activityInfo.applicationInfo
                "${info.packageName} (${info.sourceDir})"
            }))
        }
        return list[0].activityInfo
    }

    private const val CONFIG_P2P_MAC_RANDOMIZATION_SUPPORTED = "config_wifi_p2p_mac_randomization_supported"
    val p2pMacRandomizationSupported get() = try {
        when (Build.VERSION.SDK_INT) {
            29 -> Resources.getSystem().run {
                getBoolean(getIdentifier(CONFIG_P2P_MAC_RANDOMIZATION_SUPPORTED, "bool", "android"))
            }
            in 30..Int.MAX_VALUE -> @TargetApi(30) {
                val info = resolvedActivity
                val resources = app.packageManager.getResourcesForApplication(info.applicationInfo)
                resources.getBoolean(resources.findIdentifier(CONFIG_P2P_MAC_RANDOMIZATION_SUPPORTED, "bool",
                    RESOURCES_PACKAGE, info.packageName))
            }
            else -> false
        }
    } catch (e: RuntimeException) {
        Timber.w(e)
        false
    }

    @get:RequiresApi(30)
    private val apMacRandomizationSupported by lazy {
        WifiManager::class.java.getDeclaredMethod("isApMacRandomizationSupported")
    }
    @get:RequiresApi(30)
    val isApMacRandomizationSupported get() = apMacRandomizationSupported(Services.wifi) as Boolean

    /**
     * Broadcast intent action indicating that Wi-Fi AP has been enabled, disabled,
     * enabling, disabling, or failed.
     */
    const val WIFI_AP_STATE_CHANGED_ACTION = "android.net.wifi.WIFI_AP_STATE_CHANGED"
    /**
     * The lookup key for an int that indicates whether Wi-Fi AP is enabled,
     * disabled, enabling, disabling, or failed.  Retrieve it with [Intent.getIntExtra].
     *
     * @see WIFI_AP_STATE_DISABLED
     * @see WIFI_AP_STATE_DISABLING
     * @see WIFI_AP_STATE_ENABLED
     * @see WIFI_AP_STATE_ENABLING
     * @see WIFI_AP_STATE_FAILED
     */
    private const val EXTRA_WIFI_AP_STATE = "wifi_state"
    /**
     * An extra containing the int error code for Soft AP start failure.
     * Can be obtained from the [WIFI_AP_STATE_CHANGED_ACTION] using [Intent.getIntExtra].
     * This extra will only be attached if [EXTRA_WIFI_AP_STATE] is
     * attached and is equal to [WIFI_AP_STATE_FAILED].
     *
     * The error code will be one of:
     * {@link #SAP_START_FAILURE_GENERAL},
     * {@link #SAP_START_FAILURE_NO_CHANNEL},
     * {@link #SAP_START_FAILURE_UNSUPPORTED_CONFIGURATION}
     *
     * Source: https://android.googlesource.com/platform/frameworks/base/+/android-6.0.0_r1/wifi/java/android/net/wifi/WifiManager.java#210
     */
    val EXTRA_WIFI_AP_FAILURE_REASON get() =
        if (Build.VERSION.SDK_INT >= 30) "android.net.wifi.extra.WIFI_AP_FAILURE_REASON" else "wifi_ap_error_code"
    /**
     * The lookup key for a String extra that stores the interface name used for the Soft AP.
     * This extra is included in the broadcast [WIFI_AP_STATE_CHANGED_ACTION].
     * Retrieve its value with [Intent.getStringExtra].
     *
     * Source: https://android.googlesource.com/platform/frameworks/base/+/android-8.0.0_r1/wifi/java/android/net/wifi/WifiManager.java#413
     */
    val EXTRA_WIFI_AP_INTERFACE_NAME get() =
        if (Build.VERSION.SDK_INT >= 30) "android.net.wifi.extra.WIFI_AP_INTERFACE_NAME" else "wifi_ap_interface_name"

    fun checkWifiApState(state: Int) = if (state !in WIFI_AP_STATE_DISABLING..WIFI_AP_STATE_FAILED) {
        Timber.w(Exception("Unknown state $state"))
        false
    } else true
    val Intent.wifiApState get() =
        getIntExtra(EXTRA_WIFI_AP_STATE, WIFI_AP_STATE_DISABLED).also { checkWifiApState(it) }
    /**
     * Wi-Fi AP is currently being disabled. The state will change to
     * [WIFI_AP_STATE_DISABLED] if it finishes successfully.
     *
     * @see WIFI_AP_STATE_CHANGED_ACTION
     * @see #getWifiApState()
     */
    const val WIFI_AP_STATE_DISABLING = 10
    /**
     * Wi-Fi AP is disabled.
     *
     * @see WIFI_AP_STATE_CHANGED_ACTION
     * @see #getWifiState()
     */
    const val WIFI_AP_STATE_DISABLED = 11
    /**
     * Wi-Fi AP is currently being enabled. The state will change to
     * {@link #WIFI_AP_STATE_ENABLED} if it finishes successfully.
     *
     * @see WIFI_AP_STATE_CHANGED_ACTION
     * @see #getWifiApState()
     */
    const val WIFI_AP_STATE_ENABLING = 12
    /**
     * Wi-Fi AP is enabled.
     *
     * @see WIFI_AP_STATE_CHANGED_ACTION
     * @see #getWifiApState()
     */
    const val WIFI_AP_STATE_ENABLED = 13
    /**
     * Wi-Fi AP is in a failed state. This state will occur when an error occurs during
     * enabling or disabling
     *
     * @see WIFI_AP_STATE_CHANGED_ACTION
     * @see #getWifiApState()
     */
    const val WIFI_AP_STATE_FAILED = 14

    private val getWifiApConfiguration by lazy { WifiManager::class.java.getDeclaredMethod("getWifiApConfiguration") }
    @Suppress("DEPRECATION")
    private val setWifiApConfiguration by lazy {
        WifiManager::class.java.getDeclaredMethod("setWifiApConfiguration", WifiConfiguration::class.java)
    }
    @get:RequiresApi(30)
    private val getSoftApConfiguration by lazy { WifiManager::class.java.getDeclaredMethod("getSoftApConfiguration") }
    @get:RequiresApi(30)
    private val setSoftApConfiguration by lazy @TargetApi(30) {
        WifiManager::class.java.getDeclaredMethod("setSoftApConfiguration", SoftApConfiguration::class.java)
    }

    /**
     * Requires NETWORK_SETTINGS permission (or root) on API 30+, and OVERRIDE_WIFI_CONFIG on API 29.
     */
    val configurationLegacy get() = getWifiApConfiguration(Services.wifi) as WifiConfiguration?
    /**
     * Requires NETWORK_SETTINGS permission (or root).
     */
    @get:RequiresApi(30)
    val configuration get() = getSoftApConfiguration(Services.wifi) as SoftApConfiguration
    fun setConfiguration(value: WifiConfiguration?) = setWifiApConfiguration(Services.wifi, value) as Boolean
    fun setConfiguration(value: SoftApConfiguration) = setSoftApConfiguration(Services.wifi, value) as Boolean

    val failureReasonLookup = ConstantLookup<WifiManager>("SAP_START_FAILURE_", "GENERAL", "NO_CHANNEL")
    @get:RequiresApi(30)
    val clientBlockLookup by lazy { ConstantLookup<WifiManager>("SAP_CLIENT_") }
    @get:RequiresApi(30)
    val deauthenticationReasonLookup by lazy {
        ConstantLookup("REASON_") { DeauthenticationReasonCode::class.java }
    }

    class SoftApCallbackUnavailableException(
        cause: Throwable? = null,
    ) : RuntimeException("Soft AP callback is unavailable", cause)
    private enum class SoftApCallbackCapability { Unknown, Available, Unavailable }
    private var directSoftApCallbackCapability = SoftApCallbackCapability.Unknown
    private var directLocalOnlyHotspotSoftApCallbackCapability = SoftApCallbackCapability.Unknown
    /**
     * https://android.googlesource.com/platform/packages/modules/Wifi/+/android-13.0.0_r1/framework/java/android/net/wifi/WifiManager.java#360
     */
    private const val EXTRA_PARAM_KEY_ATTRIBUTION_SOURCE = "EXTRA_PARAM_KEY_ATTRIBUTION_SOURCE"
    sealed class Event : Parcelable {
        @Parcelize
        data class OnStateChanged(val state: Int, val failureReason: Int) : Event()
        @Parcelize
        data class OnNumClientsChanged(val numClients: Int) : Event()
        @Parcelize
        @RequiresApi(30)
        data class OnConnectedClientsChanged(val clients: List<WifiClient>) : Event()
        @Parcelize
        @RequiresApi(30)
        data class OnInfoChanged(val info: List<SoftApInfo>) : Event()
        @Parcelize
        @RequiresApi(30)
        data class OnCapabilityChanged(val capability: SoftApCapability) : Event()
        @Parcelize
        @RequiresApi(30)
        data class OnBlockedClientConnecting(val client: WifiClient, val blockedReason: Int) : Event()
        @Parcelize
        @RequiresApi(30)
        data class OnClientsDisconnected(val info: SoftApInfo, val clients: List<WifiClient>) : Event()
    }

    @RequiresApi(30)
    fun softApCallback(push: (Event) -> Boolean) = object : `WifiManager$SoftApCallback` {
        override fun onStateChanged(state: Int, failureReason: Int) {
            push(Event.OnStateChanged(state, failureReason))
        }
        override fun onConnectedClientsChanged(clients: List<WifiClient>) {
            push(Event.OnConnectedClientsChanged(clients))
        }
        override fun onInfoChanged(info: SoftApInfo) {
            push(Event.OnInfoChanged(if (info.frequency == 0 && info.bandwidth == SoftApInfo.CHANNEL_WIDTH_INVALID) {
                emptyList()
            } else listOf(info)))
        }
        override fun onInfoChanged(info: List<SoftApInfo>) {
            push(Event.OnInfoChanged(info))
        }
        override fun onCapabilityChanged(capability: SoftApCapability) {
            push(Event.OnCapabilityChanged(capability))
        }
        override fun onBlockedClientConnecting(client: WifiClient, blockedReason: Int) {
            push(Event.OnBlockedClientConnecting(client, blockedReason))
        }
        override fun onClientsDisconnected(info: SoftApInfo, clients: List<WifiClient>) {
            push(Event.OnClientsDisconnected(info, clients))
        }
    }

    private val registerSoftApCallback by lazy {
        if (Build.VERSION.SDK_INT >= 30) {
            WifiManager::class.java.getDeclaredMethod("registerSoftApCallback", Executor::class.java,
                `WifiManager$SoftApCallback`::class.java)
        } else WifiManager::class.java.getDeclaredMethod("registerSoftApCallback",
            `WifiManager$SoftApCallback`::class.java, Handler::class.java)
    }
    private val unregisterSoftApCallback by lazy {
        WifiManager::class.java.getDeclaredMethod("unregisterSoftApCallback", `WifiManager$SoftApCallback`::class.java)
    }
    @get:RequiresApi(33)
    private val registerLocalOnlyHotspotSoftApCallback by lazy {
        WifiManager::class.java.getDeclaredMethod("registerLocalOnlyHotspotSoftApCallback", Executor::class.java,
            `WifiManager$SoftApCallback`::class.java)
    }
    @get:RequiresApi(33)
    private val unregisterLocalOnlyHotspotSoftApCallback by lazy {
        WifiManager::class.java.getDeclaredMethod("unregisterLocalOnlyHotspotSoftApCallback",
            `WifiManager$SoftApCallback`::class.java)
    }
    val softApCallbackFlow = binderCallbackFlow("Soft AP callback") {
        if (directSoftApCallbackCapability == SoftApCallbackCapability.Unavailable) {
            throw SoftApCallbackUnavailableException()
        }
        val callback = if (Build.VERSION.SDK_INT < 30) Proxy.newProxyInstance(
            `WifiManager$SoftApCallback`::class.java.classLoader,
            arrayOf(`WifiManager$SoftApCallback`::class.java),
            object : InvocationHandler {
                override fun invoke(proxy: Any, method: Method, args: Array<out Any?>?) = when {
                    method.matches("onStateChanged", Integer.TYPE, Integer.TYPE) -> null.also {
                        push(Event.OnStateChanged(args!![0] as Int, args[1] as Int))
                    }
                    method.matches("onNumClientsChanged", Integer.TYPE) -> null.also {
                        push(Event.OnNumClientsChanged(args!![0] as Int))
                    }
                    else -> try {
                        callSuper(`WifiManager$SoftApCallback`::class.java, proxy, method, args)
                    } catch (e: Throwable) {
                        // Legacy SoftApCallback is a hidden interface. OEM Android 10 builds may add abstract callbacks
                        // like onStaConnected/onStaDisconnected, so the proxy must tolerate runtime-only void methods.
                        Timber.w(e)
                        null
                    }
                }
            },
        ) else softApCallback(::push)
        try {
            if (Build.VERSION.SDK_INT >= 30) {
                registerSoftApCallback(Services.wifi, InPlaceExecutor, callback)
            } else registerSoftApCallback(Services.wifi, callback, null)
            directSoftApCallbackCapability = SoftApCallbackCapability.Available
        } catch (e: Throwable) {
            when (e) {
                is InvocationTargetException -> when (val target = e.targetException) {
                    is SecurityException, is LinkageError -> target
                    else -> null
                }
                is SecurityException, is ReflectiveOperationException, is LinkageError -> e
                else -> null
            }?.let {
                directSoftApCallbackCapability = SoftApCallbackCapability.Unavailable
                throw SoftApCallbackUnavailableException(it)
            }
            throw e
        }
        return@binderCallbackFlow {
            try {
                unregisterSoftApCallback(Services.wifi, callback)
            } catch (e: Exception) {
                Timber.w(e)
            }
        }
    }
    @get:RequiresApi(33)
    val localOnlyHotspotSoftApCallbackFlow = binderCallbackFlow("local-only hotspot Soft AP callback") {
        if (directLocalOnlyHotspotSoftApCallbackCapability == SoftApCallbackCapability.Unavailable) {
            throw SoftApCallbackUnavailableException()
        }
        val callback = softApCallback(::push)
        try {
            registerLocalOnlyHotspotSoftApCallback(Services.wifi, InPlaceExecutor, callback)
            directLocalOnlyHotspotSoftApCallbackCapability = SoftApCallbackCapability.Available
        } catch (e: Throwable) {
            when (e) {
                is InvocationTargetException -> when (val target = e.targetException) {
                    is SecurityException, is UnsupportedOperationException, is LinkageError -> target
                    else -> null
                }
                is SecurityException, is ReflectiveOperationException, is UnsupportedOperationException,
                is LinkageError -> e
                else -> null
            }?.let {
                directLocalOnlyHotspotSoftApCallbackCapability = SoftApCallbackCapability.Unavailable
                throw SoftApCallbackUnavailableException(it)
            }
            throw e
        }
        return@binderCallbackFlow {
            try {
                unregisterLocalOnlyHotspotSoftApCallback(Services.wifi, callback)
            } catch (e: Exception) {
                Timber.w(e)
            }
        }
    }

    @get:RequiresApi(31)
    private val iWifiManager by lazy { UnblockCentral.WifiManager_mService.get(Services.wifi) as IWifiManager }
    @RequiresApi(31)
    fun registerSoftApCallback(callback: IBinder) =
        iWifiManager.registerSoftApCallback(ISoftApCallback.Stub.asInterface(callback))
    @RequiresApi(31)
    fun unregisterSoftApCallback(callback: IBinder) =
        iWifiManager.unregisterSoftApCallback(ISoftApCallback.Stub.asInterface(callback))
    @RequiresApi(33)
    fun registerLocalOnlyHotspotSoftApCallback(callback: IBinder) =
        iWifiManager.registerLocalOnlyHotspotSoftApCallback(ISoftApCallback.Stub.asInterface(callback), Bundle().apply {
            putParcelable(EXTRA_PARAM_KEY_ATTRIBUTION_SOURCE, Services.context.attributionSource)
        })
    @RequiresApi(33)
    fun unregisterLocalOnlyHotspotSoftApCallback(callback: IBinder) =
        iWifiManager.unregisterLocalOnlyHotspotSoftApCallback(ISoftApCallback.Stub.asInterface(callback), Bundle().apply {
            putParcelable(EXTRA_PARAM_KEY_ATTRIBUTION_SOURCE, Services.context.attributionSource)
        })

    sealed class LocalOnlyHotspotEvent : Parcelable {
        @Parcelize
        data class Started(val config: SoftApConfigurationCompat?) : LocalOnlyHotspotEvent()
        @Parcelize
        class Stopped : LocalOnlyHotspotEvent() {
            override fun equals(other: Any?) = other is Stopped
            override fun hashCode() = javaClass.hashCode()
        }
        @Parcelize
        data class Failed(val reason: Int) : LocalOnlyHotspotEvent()
    }

    private val cancelLocalOnlyHotspotRequest by lazy {
        WifiManager::class.java.getDeclaredMethod("cancelLocalOnlyHotspotRequest")
    }
    /**
     * This is the only way to unregister requests besides app exiting.
     * Therefore, we are happy with crashing the app if reflection fails.
     */
    fun cancelLocalOnlyHotspotRequest() = cancelLocalOnlyHotspotRequest(Services.wifi)
    private fun localOnlyHotspotFlow(
        nullReservationReason: Int,
        start: (WifiManager.LocalOnlyHotspotCallback) -> Unit,
    ) = binderCallbackFlow("local-only hotspot callback") {
        var reservation: WifiManager.LocalOnlyHotspotReservation? = null
        start(object : WifiManager.LocalOnlyHotspotCallback() {
            override fun onStarted(startedReservation: WifiManager.LocalOnlyHotspotReservation?) {
                if (startedReservation == null) return finish(LocalOnlyHotspotEvent.Failed(nullReservationReason))
                reservation = startedReservation
                val config = if (Build.VERSION.SDK_INT < 30) @Suppress("DEPRECATION") {
                    startedReservation.wifiConfiguration?.toCompat()
                } else startedReservation.softApConfiguration.toCompat()
                push(LocalOnlyHotspotEvent.Started(config))
            }
            override fun onStopped() = finish(LocalOnlyHotspotEvent.Stopped())
            override fun onFailed(reason: Int) = finish(LocalOnlyHotspotEvent.Failed(reason))
        })
        return@binderCallbackFlow {
            reservation?.close() ?: cancelLocalOnlyHotspotRequest()
            reservation = null
        }
    }

    @get:RequiresApi(30)
    private val startLocalOnlyHotspot by lazy @TargetApi(30) {
        WifiManager::class.java.getDeclaredMethod("startLocalOnlyHotspot", SoftApConfiguration::class.java,
            Executor::class.java, WifiManager.LocalOnlyHotspotCallback::class.java)
    }
    @RequiresApi(30)
    fun startLocalOnlyHotspotFlow(config: SoftApConfiguration, executor: Executor? = InPlaceExecutor) =
        localOnlyHotspotFlow(-3) { startLocalOnlyHotspot(Services.wifi, config, executor, it) }
    @SuppressLint("MissingPermission")
    fun startLocalOnlyHotspotFlow(handler: Handler? = null) =
        localOnlyHotspotFlow(-2) { Services.wifi.startLocalOnlyHotspot(it, handler) }
    @RequiresApi(33)
    @SuppressLint("MissingPermission")
    fun startLocalOnlyHotspotWithConfigurationFlow(config: SoftApConfiguration,
                                                   executor: Executor = InPlaceExecutor) =
        localOnlyHotspotFlow(-2) { Services.wifi.startLocalOnlyHotspotWithConfiguration(config, executor, it) }
}
