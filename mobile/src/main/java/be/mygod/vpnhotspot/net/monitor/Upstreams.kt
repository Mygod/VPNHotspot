package be.mygod.vpnhotspot.net.monitor

import android.content.SharedPreferences
import android.net.ConnectivityManager
import android.net.LinkProperties
import android.net.Network
import android.net.NetworkCapabilities
import android.os.Build
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

/**
 * The ordered-callback state machine behind [Upstreams.appDefault], and the whole of its decision.
 *
 * Extracted rather than inlined in the callback because the decision is not about networks at all - it is
 * about which of three ordered facts have arrived for the *current* one - so it is testable without a device,
 * and every rule below is one that has been got wrong here before. It is generic in the two payload types for
 * exactly that reason and for no other.
 *
 * Three rules, in the order they matter:
 *
 * 1. **A new [available] retires the old value immediately.** The framework delivers availability first and
 *    the properties and blocked status after, so keeping the previous network published across that window
 *    would name a network Android has already replaced, and every consumer here treats an emission as a
 *    handover.
 * 2. **Nothing is published until both later facts arrive.** Properties are what an upstream *is*, and a
 *    blocked network is one this UID may not use, so publishing early would offer an egress that fails every
 *    send and retract it in the same breath.
 * 3. **It fails closed.** Blocked or lost is null, not the last good value.
 */
internal class AppDefaultState<N : Any, P : Any> {
    private var current: N? = null
    private var properties: P? = null
    private var blocked: Boolean? = null

    /** The value to publish: null unless the current network is fully described and usable. */
    val upstream get() = current?.let { network -> properties?.takeIf { blocked == false }?.let { network to it } }

    fun available(network: N) {
        current = network
        properties = null
        blocked = null
    }

    /** False for a callback about a network that is no longer current, which is nothing to publish. */
    fun properties(network: N, properties: P): Boolean {
        if (network != current) return false
        this.properties = properties
        return true
    }

    fun blocked(network: N, blocked: Boolean): Boolean {
        if (network != current) return false
        this.blocked = blocked
        return true
    }

    fun lost(network: N): Boolean {
        if (network != current) return false
        current = null
        properties = null
        blocked = null
        return true
    }
}

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
        )
    }

    val primary: StateFlow<Upstream?> = role(KEY_PRIMARY, vpn)
    val fallback: StateFlow<Upstream?> = role(KEY_FALLBACK, default)

    /**
     * The `Network` Android has made this app's own default: a VPN when one applies to this UID, and the
     * ordinary per-UID default when none does.
     *
     * Deliberately not a [role] and deliberately not [default]. [KEY_PRIMARY] and [KEY_FALLBACK] choose where
     * *root mode* sends its tethered clients, and root mode can send them anywhere because it writes the
     * routes itself. Rootless mode makes a different promise: tethered clients share whatever egress Android
     * has already applied to this app, so its egress is that selection and not a user preference. This is a
     * product decision about what the mode is, not an inability to bind - an ordinary app UID can bind a
     * socket to any accessible `Network` handle it names.
     *
     * `registerDefaultNetworkCallback` is per calling UID rather than the system default. `TRACK_DEFAULT`
     * discards the capabilities the caller passed and rebuilds them for the calling UID: `NOT_VPN` removed
     * so a VPN can satisfy it, and the UID pinned so only a VPN that applies to *this* app does. Identical
     * on Android 13 and 17.
     *
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/ConnectivityManager.java#5434
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/service/src/com/android/server/ConnectivityService.java#9455
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/service/src/com/android/server/ConnectivityService.java#10215
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/service/src/com/android/server/ConnectivityService.java#7143
     */
    val appDefault: StateFlow<Upstream?> = callbackFlow {
        /**
         * The order this relies on is the framework's: `onAvailable`, then the
         * suspended/capabilities/link-properties callbacks, then `onBlockedStatusChanged`, all from one
         * `CALLBACK_AVAILABLE` message, on Android 13 and 17 alike.
         *
         * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/ConnectivityManager.java#4182
         * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/framework/src/android/net/ConnectivityManager.java#3770
         */
        val state = AppDefaultState<Network, LinkProperties>()
        val callback = object : ConnectivityManager.NetworkCallback() {
            private fun emit() = trySend(state.upstream?.let { Upstream(it.first, it.second) })

            override fun onAvailable(network: Network) {
                state.available(network)
                emit()
            }
            override fun onLinkPropertiesChanged(network: Network, properties: LinkProperties) {
                if (state.properties(network, properties)) emit()
            }
            override fun onBlockedStatusChanged(network: Network, blocked: Boolean) {
                if (state.blocked(network, blocked)) emit()
            }
            override fun onLost(network: Network) {
                if (state.lost(network)) emit()
            }
        }
        var registered = false
        try {
            Services.connectivity.registerDefaultNetworkCallback(callback, Services.mainHandler)
            registered = true
        } catch (e: RuntimeException) {
            // the same outcome as no default network, which stateIn is already reporting
            Timber.w(e)
            SmartSnackbar.make(e).show()
        }
        awaitClose { if (registered) Services.connectivity.unregisterNetworkCallback(callback) }
    }.buffer(Channel.CONFLATED).stateIn(scope, SharingStarted.WhileSubscribed(replayExpirationMillis = 0), null)

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
    ) = callbackFlow {
        trySend(null)
        val state = SourceState()
        val callback = object : ConnectivityManager.NetworkCallback() {
            override fun onLinkPropertiesChanged(network: Network, properties: LinkProperties) {
                state.onLinkPropertiesChanged(network, properties)?.let { trySend(it.upstream) }
            }

            override fun onLost(network: Network) {
                state.onLost(network)?.let { trySend(it.upstream) }
            }
        }
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
