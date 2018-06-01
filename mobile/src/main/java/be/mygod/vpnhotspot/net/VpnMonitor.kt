package be.mygod.vpnhotspot.net

import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.util.debugLog

object VpnMonitor : UpstreamMonitor() {
    private const val TAG = "VpnMonitor"

    private val request = NetworkRequest.Builder()
            .addTransportType(NetworkCapabilities.TRANSPORT_VPN)
            .removeCapability(NetworkCapabilities.NET_CAPABILITY_NOT_VPN)
            .build()
    private var registered = false

    /**
     * Obtaining ifname in onLost doesn't work so we need to cache it in onAvailable.
     */
    private val available = HashMap<Network, String>()
    private var currentNetwork: Network? = null
    override val currentIface: String? get() {
        val currentNetwork = currentNetwork
        return if (currentNetwork == null) null else available[currentNetwork]
    }
    private val networkCallback = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) {
            val properties = app.connectivity.getLinkProperties(network)
            val ifname = properties?.interfaceName ?: return
            synchronized(this@VpnMonitor) {
                if (available.put(network, ifname) != null) return
                debugLog(TAG, "onAvailable: $ifname, ${properties.dnsServers.joinToString()}")
                val old = currentNetwork
                if (old != null) debugLog(TAG, "Assuming old VPN interface ${available[old]} is dying")
                currentNetwork = network
                callbacks.forEach { it.onAvailable(ifname, properties.dnsServers) }
            }
        }

        override fun onLost(network: Network) = synchronized(this@VpnMonitor) {
            val ifname = available.remove(network) ?: return
            debugLog(TAG, "onLost: $ifname")
            if (currentNetwork != network) return
            while (available.isNotEmpty()) {
                val next = available.entries.first()
                currentNetwork = next.key
                val properties = app.connectivity.getLinkProperties(next.key)
                if (properties != null) {
                    debugLog(TAG, "Switching to ${next.value} as VPN interface")
                    callbacks.forEach { it.onAvailable(next.value, properties.dnsServers) }
                    return
                }
                available.remove(next.key)
            }
            callbacks.forEach { it.onLost() }
            currentNetwork = null
        }
    }

    override fun registerCallbackLocked(callback: Callback) = if (registered) {
        val currentNetwork = currentNetwork
        if (currentNetwork == null) true else {
            callback.onAvailable(available[currentNetwork]!!,
                    app.connectivity.getLinkProperties(currentNetwork)?.dnsServers ?: emptyList())
            false
        }
    } else {
        app.connectivity.registerNetworkCallback(request, networkCallback)
        registered = true
        app.connectivity.allNetworks.all {
            val cap = app.connectivity.getNetworkCapabilities(it)
            cap == null || !cap.hasTransport(NetworkCapabilities.TRANSPORT_VPN) ||
                    cap.hasCapability(NetworkCapabilities.NET_CAPABILITY_NOT_VPN)
        }
    }

    override fun destroyLocked() {
        if (!registered) return
        app.connectivity.unregisterNetworkCallback(networkCallback)
        registered = false
        available.clear()
        currentNetwork = null
    }
}
