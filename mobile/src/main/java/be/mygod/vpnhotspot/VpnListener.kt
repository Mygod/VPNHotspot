package be.mygod.vpnhotspot

import android.content.Context
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import be.mygod.vpnhotspot.App.Companion.app

object VpnListener : ConnectivityManager.NetworkCallback() {
    interface Callback {
        fun onAvailable(ifname: String)
        fun onLost(ifname: String)
    }

    private const val TAG = "VpnListener"

    val connectivityManager = app.getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager
    private val request by lazy {
        NetworkRequest.Builder()
                .addTransportType(NetworkCapabilities.TRANSPORT_VPN)
                .removeCapability(NetworkCapabilities.NET_CAPABILITY_NOT_VPN)
                .build()
    }
    private val callbacks = HashSet<Callback>()
    private var registered = false

    /**
     * Obtaining ifname in onLost doesn't work so we need to cache it in onAvailable.
     */
    private val available = HashMap<Network, String>()
    override fun onAvailable(network: Network) {
        val ifname = connectivityManager.getLinkProperties(network)?.interfaceName ?: return
        available.put(network, ifname)
        debugLog(TAG, "onAvailable: $ifname")
        callbacks.forEach { it.onAvailable(ifname) }
    }

    override fun onLost(network: Network) {
        val ifname = available.remove(network) ?: return
        debugLog(TAG, "onLost: $ifname")
        callbacks.forEach { it.onLost(ifname) }
    }

    fun registerCallback(callback: Callback) {
        if (!callbacks.add(callback)) return
        if (registered) available.forEach { callback.onAvailable(it.value) } else {
            connectivityManager.registerNetworkCallback(request, this)
            registered = false
        }
    }
    fun unregisterCallback(callback: Callback) {
        if (!callbacks.remove(callback) || callbacks.isNotEmpty() || !registered) return
        connectivityManager.unregisterNetworkCallback(this)
        registered = false
        available.clear()
    }
}
