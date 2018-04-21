package be.mygod.vpnhotspot.net

import android.content.Context
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.debugLog
import java.net.InetAddress

object VpnMonitor : ConnectivityManager.NetworkCallback() {
    interface Callback {
        fun onAvailable(ifname: String, dns: List<InetAddress>)
        fun onLost(ifname: String)
    }

    private const val TAG = "VpnMonitor"

    private val request = NetworkRequest.Builder()
            .addTransportType(NetworkCapabilities.TRANSPORT_VPN)
            .removeCapability(NetworkCapabilities.NET_CAPABILITY_NOT_VPN)
            .build()
    private val callbacks = HashSet<Callback>()
    private var registered = false

    /**
     * Obtaining ifname in onLost doesn't work so we need to cache it in onAvailable.
     */
    private val available = HashMap<Network, String>()
    private var currentNetwork: Network? = null
    override fun onAvailable(network: Network) {
        val properties = app.connectivity.getLinkProperties(network)
        val ifname = properties?.interfaceName ?: return
        synchronized(this) {
            if (available.put(network, ifname) != null) return
            debugLog(TAG, "onAvailable: $ifname, ${properties.dnsServers.joinToString()}")
            val old = currentNetwork
            if (old != null) {
                val name = available[old]!!
                debugLog(TAG, "Assuming old VPN interface $name is dying")
                callbacks.forEach { it.onLost(name) }
            }
            currentNetwork = network
            callbacks.forEach { it.onAvailable(ifname, properties.dnsServers) }
        }
    }

    override fun onLost(network: Network) = synchronized(this) {
        val ifname = available.remove(network) ?: return
        debugLog(TAG, "onLost: $ifname")
        if (currentNetwork != network) return
        callbacks.forEach { it.onLost(ifname) }
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
        currentNetwork = null
    }

    fun registerCallback(callback: Callback, failfast: (() -> Unit)? = null) {
        if (synchronized(this) {
                    if (!callbacks.add(callback)) return
                    if (!registered) {
                        app.connectivity.registerNetworkCallback(request, this)
                        registered = true
                        app.connectivity.allNetworks.all {
                            val cap = app.connectivity.getNetworkCapabilities(it)
                            !cap.hasTransport(NetworkCapabilities.TRANSPORT_VPN) ||
                                    cap.hasCapability(NetworkCapabilities.NET_CAPABILITY_NOT_VPN)
                        }
                    } else if (available.isEmpty()) true else {
                        available.forEach {
                            callback.onAvailable(it.value,
                                    app.connectivity.getLinkProperties(it.key)?.dnsServers ?: emptyList())
                        }
                        false
                    }
        }) failfast?.invoke()
    }
    fun unregisterCallback(callback: Callback) = synchronized(this) {
        if (!callbacks.remove(callback) || callbacks.isNotEmpty() || !registered) return
        app.connectivity.unregisterNetworkCallback(this)
        registered = false
        available.clear()
        currentNetwork = null
    }
}
