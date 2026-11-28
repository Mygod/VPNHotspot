package be.mygod.vpnhotspot.net.monitor

import android.content.SharedPreferences
import android.net.ConnectivityManager
import android.net.LinkProperties
import android.net.Network
import android.net.NetworkCapabilities
import android.os.Build
import android.os.ext.SdkExtensions
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.allInterfaceNames
import be.mygod.vpnhotspot.util.globalNetworkRequestBuilder
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.channels.awaitClose
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.SharingStarted
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.buffer
import kotlinx.coroutines.flow.callbackFlow
import kotlinx.coroutines.flow.catch
import kotlinx.coroutines.flow.flatMapLatest
import kotlinx.coroutines.flow.stateIn
import timber.log.Timber
import java.util.regex.PatternSyntaxException

data class Upstream(val network: Network, val properties: LinkProperties)

object Upstreams {
    const val KEY_PRIMARY = "service.upstream"
    const val KEY_FALLBACK = "service.upstream.fallback"

    private val scope = CoroutineScope(Dispatchers.Default.limitedParallelism(1, "Upstreams") + SupervisorJob())

    private val vpnRequest = globalNetworkRequestBuilder().apply {
        addTransportType(NetworkCapabilities.TRANSPORT_VPN)
        removeCapability(NetworkCapabilities.NET_CAPABILITY_NOT_VPN)
    }.build()
    /**
     * Unfortunately registerDefaultNetworkCallback is going to return VPN interface since Android P DP1:
     * https://android.googlesource.com/platform/frameworks/base/+/dda156ab0c5d66ad82bdcf76cda07cbc0a9c8a2e
     */
    private val defaultRequest = globalNetworkRequestBuilder().apply {
        addCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
        addCapability(NetworkCapabilities.NET_CAPABILITY_NOT_RESTRICTED)
    }.build()
    private val ifaceRequest = globalNetworkRequestBuilder().apply {
        removeCapability(NetworkCapabilities.NET_CAPABILITY_NOT_RESTRICTED)
        removeCapability(NetworkCapabilities.NET_CAPABILITY_TRUSTED)
        removeCapability(NetworkCapabilities.NET_CAPABILITY_NOT_VPN)
    }.build()
    /**
     * [NetworkCapabilities.NET_CAPABILITY_LOCAL_NETWORK] was added in API 35 and U extension 12:
     * https://developer.android.com/reference/android/net/NetworkCapabilities#NET_CAPABILITY_LOCAL_NETWORK
     * which also covers Android 14 Thread networks:
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-14.0.0_r50/thread/service/java/com/android/server/thread/ThreadNetworkControllerService.java#542
     * Older platforms may give its value to something else, such as 5G slices in this unofficial decompilation of
     * Huawei EMUI 10.1.1:
     * https://github.com/dstmath/HWFramework/blob/672bb34094b8780806a10ba9b1d21036fd808b8e/DVC-AN20_EMUI10.1.1/src/main/java/android/net/NetworkCapabilities.java#L98
     * https://github.com/dstmath/HWFramework/blob/672bb34094b8780806a10ba9b1d21036fd808b8e/DVC-AN20_EMUI10.1.1/src/main/java/android/telephony/data/ApnSetting.java#L225-L230
     */
    val localNetworkCapabilitySupported = Build.VERSION.SDK_INT >= 35 || Build.VERSION.SDK_INT == 34 &&
            SdkExtensions.getExtensionVersion(Build.VERSION_CODES.UPSIDE_DOWN_CAKE) >= 12

    val vpn = source(
        register = { Services.registerNetworkCallback(vpnRequest, it) },
        onLinkPropertiesChanged = { network, properties -> selectAvailable(network, properties, true) },
        onLost = { network -> removeAvailable(network) { Timber.d("Switching to $it as VPN interface") } },
    )
    val default = source(
        register = {
            if (Build.VERSION.SDK_INT >= 31) {
                Services.connectivity.registerBestMatchingNetworkCallback(defaultRequest, it, Services.mainHandler)
            } else Services.connectivity.requestNetwork(defaultRequest, it, Services.mainHandler)
        },
        onLinkPropertiesChanged = { network, properties ->
            current = network
            Emission(Upstream(network, properties))
        },
        onLost = { network ->
            if (current != network) null else {
                current = null
                Emission(null)
            }
        },
    )

    fun iface(ifaceRegex: String): Flow<Upstream?> {
        val iface: (String) -> Boolean = try {
            val regex = ifaceRegex.toRegex()
            ({ value: String -> regex.matches(value) })
        } catch (e: PatternSyntaxException) {
            Timber.d(e)
            ({ value: String -> value == ifaceRegex })
        }
        return source(
            register = { Services.registerNetworkCallback(ifaceRequest, it) },
            onLinkPropertiesChanged = { network, properties ->
                selectAvailable(network, properties, properties.allInterfaceNames.any(iface))
            },
            onLost = { network -> removeAvailable(network) },
            ignoreLocalNetworks = true,
        )
    }

    val primary: StateFlow<Upstream?> = role(KEY_PRIMARY, vpn)
    val fallback: StateFlow<Upstream?> = role(KEY_FALLBACK, default)

    @OptIn(ExperimentalCoroutinesApi::class)
    private fun role(preferenceKey: String, defaultSource: Flow<Upstream?>) = preferenceFlow(preferenceKey)
            .flatMapLatest { upstream -> if (upstream.isNullOrEmpty()) defaultSource else iface(upstream) }
            .catch { e ->
                if (e is CancellationException) throw e
                Timber.w(e)
                SmartSnackbar.make(e).show()
                emit(null)
            }
            .stateIn(scope, SharingStarted.WhileSubscribed(replayExpirationMillis = 0), null)

    private class Emission(val upstream: Upstream?)

    private class SourceState {
        private val available = HashMap<Network, LinkProperties>()
        var current: Network? = null

        fun selectAvailable(network: Network, properties: LinkProperties, matched: Boolean): Emission? {
            if (matched) {
                available[network] = properties
                if (current == null) current = network else if (current != network) return null
                return Emission(Upstream(network, properties))
            }
            if (available.remove(network) == null || current != network) return null
            return selectNext()
        }

        fun removeAvailable(network: Network, onNext: (LinkProperties) -> Unit = { }): Emission? {
            if (available.remove(network) == null || current != network) return null
            return selectNext(onNext)
        }

        private fun selectNext(onNext: (LinkProperties) -> Unit = { }): Emission {
            val next = available.entries.firstOrNull()
            current = next?.key
            if (next != null) onNext(next.value)
            return Emission(next?.let { Upstream(it.key, it.value) })
        }
    }

    private fun source(
        register: (ConnectivityManager.NetworkCallback) -> Unit,
        onLinkPropertiesChanged: SourceState.(Network, LinkProperties) -> Emission?,
        onLost: SourceState.(Network) -> Emission?,
        ignoreLocalNetworks: Boolean = false,
    ) = callbackFlow {
        trySend(null)
        val state = SourceState()
        /**
         * Since Android 15, Tethering may register a NetworkAgent with
         * [NetworkCapabilities.NET_CAPABILITY_LOCAL_NETWORK] and without INTERNET for each tethering downstream
         * and Wi-Fi P2P group it serves:
         * https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-17.0.0_r1/Tethering/src/android/net/ip/IpServer.java#408
         * https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-17.0.0_r1/Tethering/src/android/net/ip/IpServer.java#1227
         * https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-17.0.0_r1/framework/src/android/net/connectivity/ConnectivityInternalApiUtil.java#91
         * Since Android 17, apps targeting SDK 37 see these unless their request forbids the capability:
         * https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-17.0.0_r1/service/src/com/android/server/ConnectivityService.java#3604
         * so an [ifaceRequest] pattern that also matches a downstream would pick it as its own upstream.
         * Tethering forbids the capability for its own upstreams:
         * https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-17.0.0_r1/Tethering/src/com/android/networkstack/tethering/UpstreamNetworkMonitor.java#175
         * but NetworkRequest.Builder.addForbiddenCapability is a module-lib API:
         * https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-17.0.0_r1/framework/src/android/net/NetworkRequest.java#440
         * so drop these networks here instead. onCapabilitiesChanged always precedes their first
         * onLinkPropertiesChanged, and a network can never gain or lose the capability:
         * https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-17.0.0_r1/service/src/com/android/server/connectivity/NetworkAgentInfo.java#462
         * Only [ifaceRequest] needs this: [vpnRequest] and [defaultRequest] need the VPN transport or INTERNET, and
         * networks that provide Internet access are never local:
         * https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-17.0.0_r1/framework/src/android/net/NetworkCapabilities.java#753
         * so only its callback overrides onCapabilitiesChanged; ConnectivityService skips callbacks that are not
         * overridden:
         * https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-17.0.0_r1/service/src/com/android/server/ConnectivityService.java#12514
         */
        val localNetworks = HashSet<Network>()
        open class Callback : ConnectivityManager.NetworkCallback() {
            override fun onLinkPropertiesChanged(network: Network, properties: LinkProperties) {
                if (network in localNetworks) return
                state.onLinkPropertiesChanged(network, properties)?.let { trySend(it.upstream) }
            }

            override fun onLost(network: Network) {
                localNetworks.remove(network)
                state.onLost(network)?.let { trySend(it.upstream) }
            }
        }
        val callback = if (ignoreLocalNetworks) object : Callback() {
            override fun onCapabilitiesChanged(network: Network, networkCapabilities: NetworkCapabilities) {
                if (localNetworkCapabilitySupported &&
                    networkCapabilities.hasCapability(NetworkCapabilities.NET_CAPABILITY_LOCAL_NETWORK)) {
                    localNetworks.add(network)
                }
            }
        } else Callback()
        var registered = false
        try {
            register(callback)
            registered = true
        } catch (e: Exception) {
            if (e is CancellationException) throw e
            Timber.w(e)
            SmartSnackbar.make(e).show()
        }
        awaitClose { if (registered) Services.connectivity.unregisterNetworkCallback(callback) }
    }.buffer(Channel.CONFLATED)

    private fun preferenceFlow(key: String) = callbackFlow {
        trySend(app.pref.getString(key, null))
        val listener = SharedPreferences.OnSharedPreferenceChangeListener { pref, changed ->
            if (changed == key) trySend(pref.getString(key, null))
        }
        app.pref.registerOnSharedPreferenceChangeListener(listener)
        awaitClose { app.pref.unregisterOnSharedPreferenceChangeListener(listener) }
    }
}
