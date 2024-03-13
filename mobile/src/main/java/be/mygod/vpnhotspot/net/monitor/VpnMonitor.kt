package be.mygod.vpnhotspot.net.monitor

import android.net.ConnectivityManager
import android.net.LinkProperties
import android.net.Network
import android.net.NetworkCapabilities
import be.mygod.vpnhotspot.net.VpnFirewallManager
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.globalNetworkRequestBuilder
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.launch
import timber.log.Timber

object VpnMonitor : UpstreamMonitor() {
    private val request = globalNetworkRequestBuilder().apply {
        addTransportType(NetworkCapabilities.TRANSPORT_VPN)
        removeCapability(NetworkCapabilities.NET_CAPABILITY_NOT_VPN)
    }.build()
    private var registered = false

    private val available = HashMap<Network, LinkProperties?>()
    override val currentLinkProperties: LinkProperties? get() = currentNetwork?.let { available[it] }
    private val networkCallback = object : ConnectivityManager.NetworkCallback() {
        private fun fireCallbacks(properties: LinkProperties?, callbacks: Iterable<Callback>) = GlobalScope.launch {
            if (properties != null) VpnFirewallManager.excludeIfNeeded(this)
            callbacks.forEach { it.onAvailable(properties) }
        }

        override fun onAvailable(network: Network) {
            val properties = Services.connectivity.getLinkProperties(network)
            fireCallbacks(properties, synchronized(this@VpnMonitor) {
                available[network] = properties
                currentNetwork = network
                callbacks.toList()
            })
        }

        override fun onLinkPropertiesChanged(network: Network, properties: LinkProperties) {
            fireCallbacks(properties, synchronized(this@VpnMonitor) {
                available[network] = properties
                if (currentNetwork == null) currentNetwork = network
                else if (currentNetwork != network) return
                callbacks.toList()
            })
        }

        override fun onLost(network: Network) {
            var properties: LinkProperties? = null
            val callbacks = synchronized(this@VpnMonitor) {
                if (available.remove(network) == null || currentNetwork != network) return
                if (available.isNotEmpty()) {
                    val next = available.entries.first()
                    currentNetwork = next.key
                    Timber.d("Switching to ${next.value} as VPN interface")
                    properties = next.value
                } else currentNetwork = null
                callbacks.toList()
            }
            fireCallbacks(properties, callbacks)
        }
    }

    override fun registerCallbackLocked(callback: Callback) {
        if (registered) {
            val currentLinkProperties = currentLinkProperties
            if (currentLinkProperties != null) GlobalScope.launch {
                callback.onAvailable(currentLinkProperties)
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
