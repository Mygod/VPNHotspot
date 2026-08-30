package be.mygod.vpnhotspot.shizuku

import android.content.Context
import android.net.ConnectivityManager
import android.net.LinkProperties
import android.net.Network
import android.net.NetworkAgent
import android.net.NetworkAgentConfig
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.net.NetworkProvider
import android.os.Looper
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.UnblockCentral
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.Deferred
import kotlinx.coroutines.selects.select
import java.lang.reflect.Modifier

/**
 * App-hosted restricted TestNetwork agent. The integer-score constructor exists on Android 11-17;
 * the created/destroyed lifecycle callbacks are runtime-probed (baseline Android 12+).
 * https://android.googlesource.com/platform/frameworks/base/+/refs/tags/android-11.0.0_r1/core/java/android/net/NetworkAgent.java#375
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/framework/src/android/net/NetworkAgent.java#390
 * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/NetworkAgent.java#443
 */
@RequiresApi(30)
internal class RestrictedAgent(
    context: Context,
    capabilities: NetworkCapabilities,
    properties: LinkProperties,
    legacyType: Int,
) : NetworkAgent(context, Services.mainHandler.looper, "VpnHotspotTestNetwork", capabilities, properties,
    1,
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
     * https://android.googlesource.com/platform/frameworks/base/+/refs/tags/android-11.0.0_r1/core/java/android/net/NetworkAgent.java#559
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/framework/src/android/net/NetworkAgent.java#893
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/NetworkAgent.java#1031
     */
    val published: Network? get() = network

    override fun toString() = "network agent(${network ?: "unregistered"})"

    companion object {
        /**
         * Android 11 has neither callback. Connectivity Mainline owns this class from Android 12, so
         * probe the runtime pair instead of deriving it from the base SDK level.
         */
        val lifecycleCallbacks by lazy {
            val clazz = NetworkAgent::class.java
            clazz.getDeclaredConstructor(Context::class.java,
                Looper::class.java, String::class.java, NetworkCapabilities::class.java,
                LinkProperties::class.java, Int::class.javaPrimitiveType, NetworkAgentConfig::class.java,
                NetworkProvider::class.java).also {
                if (!Modifier.isPublic(it.modifiers) && !Modifier.isProtected(it.modifiers)) {
                    throw NoSuchMethodException("NetworkAgent integer-score constructor is not subclass-accessible")
                }
            }
            for ((name, returnType) in arrayOf(
                "register" to Network::class.java,
                "markConnected" to Void.TYPE,
                "unregister" to Void.TYPE,
                "getNetwork" to Network::class.java,
            )) clazz.getDeclaredMethod(name).also {
                if (!Modifier.isPublic(it.modifiers) || Modifier.isStatic(it.modifiers) ||
                    it.returnType != returnType) {
                    throw NoSuchMethodException("Incompatible NetworkAgent.$name")
                }
            }
            clazz.getDeclaredMethod("onNetworkUnwanted").also {
                if ((!Modifier.isPublic(it.modifiers) && !Modifier.isProtected(it.modifiers)) ||
                    Modifier.isStatic(it.modifiers) || Modifier.isFinal(it.modifiers) ||
                    it.returnType != Void.TYPE) {
                    throw NoSuchMethodException("Incompatible NetworkAgent.onNetworkUnwanted")
                }
            }
            val created = try {
                clazz.getDeclaredMethod("onNetworkCreated")
            } catch (_: NoSuchMethodException) {
                null
            }
            val destroyed = try {
                clazz.getDeclaredMethod("onNetworkDestroyed")
            } catch (_: NoSuchMethodException) {
                null
            }
            if ((created == null) != (destroyed == null)) throw NoSuchMethodException(
                "NetworkAgent has only one of onNetworkCreated/onNetworkDestroyed")
            for (method in listOfNotNull(created, destroyed)) {
                if ((!Modifier.isPublic(method.modifiers) && !Modifier.isProtected(method.modifiers)) ||
                    Modifier.isStatic(method.modifiers) || Modifier.isFinal(method.modifiers) ||
                    method.returnType != Void.TYPE) {
                    throw NoSuchMethodException("Incompatible NetworkAgent.${method.name}")
                }
            }
            created != null
        }
    }
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
     * https://android.googlesource.com/platform/frameworks/base/+/refs/tags/android-11.0.0_r1/core/java/android/net/ConnectivityManager.java#3493
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/framework/src/android/net/ConnectivityManager.java#4189
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/ConnectivityManager.java#4783
     */
    @Throws(ReflectiveOperationException::class)
    fun readBack(): NetworkRequest? =
        (UnblockCentral.NetworkCallback_networkRequest.get(callback) as NetworkRequest?).also { handle = it }

    override fun toString() = "exact request(${handle ?: "unnamed"})"
}
