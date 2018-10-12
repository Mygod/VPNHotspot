package be.mygod.vpnhotspot.net.monitor

import android.net.*
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.debugLog
import timber.log.Timber

object VpnMonitor : UpstreamMonitor() {
    private const val TAG = "VpnMonitor"

    private val request = NetworkRequest.Builder()
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
            val properties = app.connectivity.getLinkProperties(network)
            val ifname = properties?.interfaceName ?: return
            synchronized(this@VpnMonitor) {
                val old = currentNetwork
                val oldProperties = available.put(network, properties)
                if (old != network) {
                    if (old != null) {
                        debugLog(TAG, "Assuming old VPN interface ${available[old]} is dying")
                        callbacks.forEach { it.onLost() }
                    }
                    currentNetwork = network
                } else {
                    check(ifname == oldProperties!!.interfaceName)
                    if (properties.dnsServers == oldProperties.dnsServers) return
                }
                callbacks.forEach { it.onAvailable(ifname, properties.dnsServers) }
            }
        }

        override fun onLinkPropertiesChanged(network: Network, properties: LinkProperties) {
            synchronized(this@VpnMonitor) {
                if (currentNetwork != network) return
                val oldProperties = available.put(network, properties)!!
                val ifname = properties.interfaceName
                when {
                    ifname == null -> {
                        Timber.w(RuntimeException("interfaceName became null: $oldProperties -> $properties"))
                        onLost(network)
                    }
                    ifname != oldProperties.interfaceName -> {
                        Timber.w(RuntimeException("interfaceName changed: $oldProperties -> $properties"))
                        callbacks.forEach {
                            it.onLost()
                            it.onAvailable(ifname, properties.dnsServers)
                        }
                    }
                    properties.dnsServers != oldProperties.dnsServers -> {
                        callbacks.forEach { it.onAvailable(ifname, properties.dnsServers) }
                    }
                }
            }
        }

        override fun onLost(network: Network) = synchronized(this@VpnMonitor) {
            if (available.remove(network) == null || currentNetwork != network) return
            if (available.isNotEmpty()) {
                val next = available.entries.first()
                currentNetwork = next.key
                debugLog(TAG, "Switching to ${next.value.interfaceName} as VPN interface")
                callbacks.forEach { it.onAvailable(next.value.interfaceName!!, next.value.dnsServers) }
            } else {
                callbacks.forEach { it.onLost() }
                currentNetwork = null
            }
        }
    }

    override fun registerCallbackLocked(callback: Callback) {
        if (registered) {
            val currentLinkProperties = currentLinkProperties
            if (currentLinkProperties != null) {
                callback.onAvailable(currentLinkProperties.interfaceName!!, currentLinkProperties.dnsServers)
            }
        } else {
            app.connectivity.registerNetworkCallback(request, networkCallback)
            registered = true
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
