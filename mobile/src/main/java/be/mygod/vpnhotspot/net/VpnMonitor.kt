package be.mygod.vpnhotspot.net

import android.content.Context
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.debugLog

object VpnMonitor : ConnectivityManager.NetworkCallback() {
    interface Callback {
        fun onAvailable(ifname: String)
        fun onLost(ifname: String)
    }

    private const val TAG = "VpnMonitor"

    private val manager = app.getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager
    private val request = NetworkRequest.Builder()
            .addTransportType(NetworkCapabilities.TRANSPORT_VPN)
            .removeCapability(NetworkCapabilities.NET_CAPABILITY_NOT_VPN)
            .build()
    private val callbacks = HashSet<Callback>()
    private var registered = false

    /**
     * Obtaining ifname in onLost doesn't work so we need to cache it in onAvailable.
     */
    val available = HashMap<Network, String>()
    override fun onAvailable(network: Network) {
        val ifname = manager.getLinkProperties(network)?.interfaceName ?: return
        synchronized(this) {
            if (available.put(network, ifname) != null) return
            debugLog(TAG, "onAvailable: $ifname")
            callbacks.forEach { it.onAvailable(ifname) }
        }
    }

    override fun onLost(network: Network) = synchronized(this) {
        val ifname = available.remove(network) ?: return
        debugLog(TAG, "onLost: $ifname")
        callbacks.forEach { it.onLost(ifname) }
    }

    fun registerCallback(callback: Callback, failfast: (() -> Unit)? = null) {
        if (synchronized(this) {
            if (!callbacks.add(callback)) return
            if (registered) {
                if (failfast != null && available.isEmpty()) {
                    callbacks.remove(callback)
                     true
                } else {
                    available.forEach { callback.onAvailable(it.value) }
                    false
                }
            } else if (failfast != null && manager.allNetworks.all {
                        val cap = manager.getNetworkCapabilities(it)
                        !cap.hasTransport(NetworkCapabilities.TRANSPORT_VPN) ||
                                cap.hasCapability(NetworkCapabilities.NET_CAPABILITY_NOT_VPN)
                    }) {
                callbacks.remove(callback)
                true
            } else {
                manager.registerNetworkCallback(request, this)
                registered = true
                false
            }
        }) failfast!!()
    }
    fun unregisterCallback(callback: Callback) = synchronized(this) {
        if (!callbacks.remove(callback) || callbacks.isNotEmpty() || !registered) return
        manager.unregisterNetworkCallback(this)
        registered = false
        available.clear()
    }
}
