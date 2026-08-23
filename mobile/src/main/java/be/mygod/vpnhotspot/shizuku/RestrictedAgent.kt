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

internal const val AGENT_TAG = "VpnHotspotTestNetwork"

/**
 * The app-hosted agent that publishes one session's TUN, and its own destruction barriers.
 *
 * The agent and its TUN survive Shizuku death once published, because both belong to this process.
 * `onNetworkUnwanted` deliberately does not withdraw anything: the retained exact request is what keeps a
 * score-1 agent wanted, and withdrawal is the session owner's decision, never the framework's. It is
 * nevertheless overridden, because it is the one barrier a withdrawal can always demand - see [unwanted].
 *
 * The constructor overload and all three callbacks have the same shape at both ends of the supported range,
 * which is what makes the compile-only stub safe to declare once.
 *
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/framework/src/android/net/NetworkAgent.java#390
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/NetworkAgent.java#443
 */
internal class RestrictedAgent(
    context: Context,
    capabilities: NetworkCapabilities,
    properties: LinkProperties,
    legacyType: Int,
) : NetworkAgent(context, Services.mainHandler.looper, AGENT_TAG, capabilities, properties,
    NetworkScore.Builder().setLegacyInt(1).build(),
    NetworkAgentConfig.Builder().setLegacyType(legacyType).setLegacyTypeName("TEST").build(), null) {
    /**
     * The native network exists. Optional: ConnectivityService creates it while registering, but an agent
     * whose registration never got that far never receives this, and never owes [destroyed] either.
     */
    val created = CompletableDeferred<Unit>()
    /** The native network is gone. Owed only once [created] arrived. */
    val destroyed = CompletableDeferred<Unit>()
    /**
     * ConnectivityService is done with this agent, and the one barrier that is owed unconditionally.
     *
     * `unregister()` sends a `DISCONNECTED` `NetworkInfo`; ConnectivityService answers on its own handler
     * through `NetworkAgentInfo.disconnect()`, which calls `INetworkAgent.onDisconnected()` and lands here as
     * `EVENT_AGENT_DISCONNECTED`. That path does not depend on a native network ever having been created, so
     * this is what a withdrawal can require of a *known* agent whose `onNetworkCreated` never arrived -
     * whereas the absence of a callback is never proof that no remote agent exists.
     *
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/framework/src/android/net/NetworkAgent.java#590
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/NetworkAgent.java#675
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/service/src/com/android/server/connectivity/NetworkAgentInfo.java#794
     */
    val unwanted = CompletableDeferred<Unit>()

    override fun onNetworkCreated() = created.complete(Unit).run { }
    override fun onNetworkDestroyed() = destroyed.complete(Unit).run { }
    override fun onNetworkUnwanted() = unwanted.complete(Unit).run { }

    /**
     * What the platform says this agent published, read back rather than remembered.
     *
     * `register()` assigns `mNetwork` before returning, so this answers the same value on a normal return and
     * remains the only answer available when registration threw - which is exactly the case a ledger has to
     * classify, since an exception is not proof that nothing was created.
     *
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/framework/src/android/net/NetworkAgent.java#893
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/NetworkAgent.java#1031
     */
    val published: Network? get() = network

    override fun toString() = "network agent(${network ?: "unregistered"})"
}

/**
 * Observation alone does not keep a score-1 agent wanted, so this is registered through `requestNetwork`
 * rather than as a passive callback. It is also the only permission-correct readback source: it was
 * registered with the privileged identity, so ConnectivityService delivers unredacted capabilities and link
 * properties for a restricted network.
 */
internal class RequestCallback : ConnectivityManager.NetworkCallback() {
    val available = CompletableDeferred<Network>()
    val lost = CompletableDeferred<Unit>()
    /**
     * Separate barrier inputs rather than fields read after `onAvailable`: repeated publication observed
     * availability arriving before the initial capabilities and link-properties callbacks, so treating
     * availability as proof that the readback had arrived failed intermittently.
     */
    val capabilities = CompletableDeferred<NetworkCapabilities>()
    val properties = CompletableDeferred<LinkProperties>()

    override fun onAvailable(network: Network) = available.complete(network).run { }
    override fun onLost(network: Network) = lost.complete(Unit).run { }
    override fun onCapabilitiesChanged(network: Network, capabilities: NetworkCapabilities) {
        this.capabilities.complete(capabilities).run { }
    }
    override fun onLinkPropertiesChanged(network: Network, properties: LinkProperties) {
        this.properties.complete(properties).run { }
    }
}

/**
 * The exact foreground request as a thing one session owns: the callback it registered with, and the
 * `NetworkRequest` ConnectivityService returned for it.
 *
 * Both halves are needed and neither substitutes for the other. The callback is the app's half and exists
 * before the transaction, which is what lets the ledger own the request from issuance rather than from a
 * result: `ConnectivityManager` holds `sCallbacks` across the Binder call and assigns the service-returned
 * `NetworkRequest` to the callback and the map only after the reply, so the builder's own request object is
 * never the exact handle. The returned handle is the platform's half and is what a release must name,
 * because the only app-facing call that can find it again is also the call that throws it away:
 * `unregisterNetworkCallback` releases through that process-static map and then, on any normal RPC return -
 * including the return of a release ConnectivityService authorized against a different UID and therefore
 * ignored - removes the mapping and marks the callback already-unregistered.
 *
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/framework/src/android/net/ConnectivityManager.java#3689
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/ConnectivityManager.java#4781
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/ConnectivityManager.java#5578
 */
internal class ExactRequest(val callback: RequestCallback = RequestCallback()) {
    /**
     * The handle ConnectivityService returned for [callback], read back the moment registration answers -
     * normally or by throwing - and before anything can release it.
     *
     * Assigned by `ConnectivityManager` inside the same monitor as the transaction that produced it, so a
     * non-null value is the platform's own proof that the request exists. Null after issuance is therefore
     * not proof of the opposite; it is the absence of an answer, and the ledger treats it as such.
     */
    var handle: NetworkRequest? = null
        private set

    /**
     * Reads the handle back, and throws rather than answering a guess when the field cannot be reached.
     *
     * The two callers want opposite things from that failure and neither is served by a silent null. Before
     * any mutation it is a refusal to start, which costs nothing; after the transaction it is the caller's
     * job to fail closed and record the request as unknown, because a release that cannot name its request
     * cannot be authorized against the right UID either.
     */
    @Throws(ReflectiveOperationException::class)
    fun readBack(): NetworkRequest? =
        (UnblockCentral.NetworkCallback_networkRequest.get(callback) as NetworkRequest?).also { handle = it }

    override fun toString() = "exact request(${handle ?: "unnamed"})"
}
