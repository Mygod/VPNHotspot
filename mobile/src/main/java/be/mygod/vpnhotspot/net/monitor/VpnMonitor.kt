package be.mygod.vpnhotspot.net.monitor

import android.net.ConnectivityManager
import android.net.LinkProperties
import android.net.Network
import android.net.NetworkCapabilities
import be.mygod.vpnhotspot.util.Services
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.launch
import timber.log.Timber

object VpnMonitor : UpstreamMonitor() {
    private val request = networkRequestBuilder()
            .addTransportType(NetworkCapabilities.TRANSPORT_VPN)
            .removeCapability(NetworkCapabilities.NET_CAPABILITY_NOT_VPN)
            .build()
    private var registered = false

    private val available = HashMap<Network, LinkProperties>()
    private var currentNetwork: Network? = null
    override val currentLinkProperties: LinkProperties? get() {
        val currentNetwork = currentNetwork
        return if (currentNetwork == null) null else available[currentNetwork]
    }
    private val networkCallback = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) {
            val properties = Services.connectivity.getLinkProperties(network)
            val ifname = properties?.interfaceName ?: return
            var switching = false
            synchronized(this@VpnMonitor) {
                val oldProperties = available.put(network, properties)
                if (currentNetwork != network || ifname != oldProperties?.interfaceName) {
                    if (currentNetwork != null) switching = true
                    currentNetwork = network
                }
                callbacks.toList()
            }.forEach {
                if (switching) it.onLost()
                it.onAvailable(ifname, properties)
            }
        }

        override fun onLinkPropertiesChanged(network: Network, properties: LinkProperties) {
            var losing = true
            var ifname: String?
            synchronized(this@VpnMonitor) {
                if (currentNetwork == null) {
                    onAvailable(network)
                    return
                }
                if (currentNetwork != network) return
                val oldProperties = available.put(network, properties)!!
                ifname = properties.interfaceName
                when (ifname) {
                    null -> Timber.w("interfaceName became null: $oldProperties -> $properties")
                    oldProperties.interfaceName -> losing = false
                    else -> Timber.w("interfaceName changed: $oldProperties -> $properties")
                }
                callbacks.toList()
            }.forEach {
                if (losing) {
                    if (ifname == null) return onLost(network)
                    it.onLost()
                }
                ifname?.let { ifname -> it.onAvailable(ifname, properties) }
            }
        }

        override fun onLost(network: Network) {
            var newProperties: LinkProperties? = null
            synchronized(this@VpnMonitor) {
                if (available.remove(network) == null || currentNetwork != network) return
                if (available.isNotEmpty()) {
                    val next = available.entries.first()
                    currentNetwork = next.key
                    Timber.d("Switching to ${next.value.interfaceName} as VPN interface")
                    newProperties = next.value
                } else currentNetwork = null
                callbacks.toList()
            }.forEach {
                it.onLost()
                newProperties?.let { prop -> it.onAvailable(prop.interfaceName!!, prop) }
            }
        }
    }

    override fun registerCallbackLocked(callback: Callback) {
        if (registered) {
            val currentLinkProperties = currentLinkProperties
            if (currentLinkProperties != null) GlobalScope.launch {
                callback.onAvailable(currentLinkProperties.interfaceName!!, currentLinkProperties)
            }
        } else {
            Services.connectivity.registerNetworkCallback(request, networkCallback)
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
