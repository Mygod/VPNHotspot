package be.mygod.vpnhotspot.shizuku

import android.content.Context
import android.net.ConnectivityManager
import android.net.LinkProperties
import android.net.Network
import android.net.NetworkAgent
import android.net.NetworkAgentConfig
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.net.NetworkScore
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.UnblockCentral
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.Deferred
import kotlinx.coroutines.selects.select

/**
 * App-hosted restricted TestNetwork agent using the same hidden constructor/callback shape on Android 13-17.
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/framework/src/android/net/NetworkAgent.java#390
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/NetworkAgent.java#443
 */
internal class RestrictedAgent(
    context: Context,
    capabilities: NetworkCapabilities,
    properties: LinkProperties,
    legacyType: Int,
) : NetworkAgent(context, Services.mainHandler.looper, "VpnHotspotTestNetwork", capabilities, properties,
    NetworkScore.Builder().setLegacyInt(1).build(),
    NetworkAgentConfig.Builder().setLegacyType(legacyType).setLegacyTypeName("TEST").build(), null) {
    val created = CompletableDeferred<Unit>()
    val destroyed = CompletableDeferred<Unit>()
    /** `unwanted` is always owed after unregister; `destroyed` only when `created` arrived. */
    val unwanted = CompletableDeferred<Unit>()

    override fun onNetworkCreated() = created.complete(Unit).run { }
    override fun onNetworkDestroyed() = destroyed.complete(Unit).run { }
    override fun onNetworkUnwanted() = unwanted.complete(Unit).run { }

    /**
     * Reads the framework-assigned Network even when registration threw after assigning it.
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/framework/src/android/net/NetworkAgent.java#893
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/NetworkAgent.java#1031
     */
    val published: Network? get() = network

    override fun toString() = "network agent(${network ?: "unregistered"})"
}

internal class NetworkRequestExpiredException : Exception(
    "ConnectivityService did not publish the restricted TestNetwork before its request expired")

internal suspend fun <T> awaitNetworkRequest(result: Deferred<T>, unavailable: Deferred<Unit>): T = select {
    unavailable.onAwait { throw NetworkRequestExpiredException() }
    result.onAwait { it }
}

internal class RequestCallback : ConnectivityManager.NetworkCallback() {
    val available = CompletableDeferred<Network>()
    val lost = CompletableDeferred<Unit>()
    val unavailable = CompletableDeferred<Unit>()
    val capabilities = CompletableDeferred<NetworkCapabilities>()
    val properties = CompletableDeferred<LinkProperties>()

    override fun onAvailable(network: Network) = available.complete(network).run { }
    override fun onLost(network: Network) = lost.complete(Unit).run { }
    override fun onUnavailable() = unavailable.complete(Unit).run { }
    override fun onCapabilitiesChanged(network: Network, capabilities: NetworkCapabilities) {
        this.capabilities.complete(capabilities).run { }
    }
    override fun onLinkPropertiesChanged(network: Network, properties: LinkProperties) {
        this.properties.complete(properties).run { }
    }
}

internal class ExactRequest(val callback: RequestCallback = RequestCallback()) {
    var handle: NetworkRequest? = null
        private set

    /**
     * Reads the exact handle assigned to this callback; null is not proof that issuance created nothing.
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/framework/src/android/net/ConnectivityManager.java#4189
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/ConnectivityManager.java#4783
     */
    @Throws(ReflectiveOperationException::class)
    fun readBack(): NetworkRequest? =
        (UnblockCentral.NetworkCallback_networkRequest.get(callback) as NetworkRequest?).also { handle = it }

    override fun toString() = "exact request(${handle ?: "unnamed"})"
}
