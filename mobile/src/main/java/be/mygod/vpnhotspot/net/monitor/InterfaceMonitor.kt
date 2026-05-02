package be.mygod.vpnhotspot.net.monitor

import android.net.ConnectivityManager
import android.net.LinkProperties
import android.net.Network
import android.net.NetworkCapabilities
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.allInterfaceNames
import be.mygod.vpnhotspot.util.globalNetworkRequestBuilder
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.launch
import timber.log.Timber
import java.util.regex.PatternSyntaxException

class InterfaceMonitor(private val ifaceRegex: String) : UpstreamMonitor() {
    private val iface = try {
        ifaceRegex.toRegex()::matches
    } catch (e: PatternSyntaxException) {
        Timber.d(e);
        { it == ifaceRegex }
    }
    private val request = globalNetworkRequestBuilder().apply {
        removeCapability(NetworkCapabilities.NET_CAPABILITY_NOT_RESTRICTED)
        removeCapability(NetworkCapabilities.NET_CAPABILITY_TRUSTED)
        removeCapability(NetworkCapabilities.NET_CAPABILITY_NOT_VPN)
    }.build()
    private var registered = false

    private val available = HashMap<Network, LinkProperties?>()
    override val currentLinkProperties: LinkProperties? get() = currentNetwork?.let { available[it] }
    private val networkCallback = object : ConnectivityManager.NetworkCallback() {
        override fun onLinkPropertiesChanged(network: Network, properties: LinkProperties) {
            val matched = properties.allInterfaceNames.any(iface)
            val (callbacks, newNetwork, newProperties) = synchronized(this@InterfaceMonitor) {
                if (matched) {
                    available[network] = properties
                    if (currentNetwork == null) currentNetwork = network else if (currentNetwork != network) return
                    Triple(callbacks.toList(), network, properties)
                } else {
                    available.remove(network)
                    if (currentNetwork != network) return
                    val nextBest = available.entries.firstOrNull()
                    currentNetwork = nextBest?.key
                    Triple(callbacks.toList(), nextBest?.key, nextBest?.value)
                }
            }
            GlobalScope.launch { callbacks.forEach { it.onAvailable(newNetwork, newProperties) } }
        }

        override fun onLost(network: Network) {
            var nextNetwork: Network? = null
            var properties: LinkProperties? = null
            val callbacks = synchronized(this@InterfaceMonitor) {
                if (available.remove(network) == null || currentNetwork != network) return
                val next = available.entries.firstOrNull()
                currentNetwork = next?.run {
                    nextNetwork = key
                    properties = value
                    key
                }
                callbacks.toList()
            }
            GlobalScope.launch { callbacks.forEach { it.onAvailable(nextNetwork, properties) } }
        }
    }

    override fun registerCallbackLocked(callback: Callback) {
        if (registered) {
            val currentLinkProperties = currentLinkProperties
            if (currentLinkProperties != null) GlobalScope.launch {
                callback.onAvailable(currentNetwork, currentLinkProperties)
            }
        } else {
            Services.registerNetworkCallback(request, networkCallback)
            registered = true
        }
    }

    override fun destroyLocked() {
        if (!registered) return
        Services.connectivity.unregisterNetworkCallback(networkCallback)
        registered = false
        available.clear()
        currentNetwork = null
    }
}
