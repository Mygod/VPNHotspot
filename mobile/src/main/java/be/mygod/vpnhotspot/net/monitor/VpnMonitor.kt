package be.mygod.vpnhotspot.net.monitor

import android.net.ConnectivityManager
import android.net.LinkProperties
import android.net.Network
import android.net.NetworkCapabilities
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
        private fun fireCallbacks(
            network: Network?,
            properties: LinkProperties?,
            callbacks: Iterable<Callback>,
        ) = GlobalScope.launch {
            callbacks.forEach { it.onAvailable(network, properties) }
        }

        override fun onLinkPropertiesChanged(network: Network, properties: LinkProperties) {
            fireCallbacks(network, properties, synchronized(this@VpnMonitor) {
                available[network] = properties
                if (currentNetwork == null) currentNetwork = network
                else if (currentNetwork != network) return
                callbacks.toList()
            })
        }

        override fun onLost(network: Network) {
            var nextNetwork: Network? = null
            var properties: LinkProperties? = null
            val callbacks = synchronized(this@VpnMonitor) {
                if (available.remove(network) == null || currentNetwork != network) return
                if (available.isNotEmpty()) {
                    val next = available.entries.first()
                    currentNetwork = next.key
                    Timber.d("Switching to ${next.value} as VPN interface")
                    nextNetwork = next.key
                    properties = next.value
                } else currentNetwork = null
                callbacks.toList()
            }
            fireCallbacks(nextNetwork, properties, callbacks)
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
