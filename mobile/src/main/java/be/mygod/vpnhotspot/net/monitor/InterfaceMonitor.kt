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
    private var currentNetwork: Network? = null
    override val currentLinkProperties: LinkProperties? get() = currentNetwork?.let { available[it] }
    private val networkCallback = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) {
            val properties = Services.connectivity.getLinkProperties(network)
            if (properties?.allInterfaceNames?.any(iface) != true) return
            synchronized(this@InterfaceMonitor) {
                available[network] = properties
                currentNetwork = network
                callbacks.toList()
            }.forEach { it.onAvailable(properties) }
        }

        override fun onLinkPropertiesChanged(network: Network, properties: LinkProperties) {
            val matched = properties.allInterfaceNames.any(iface)
            synchronized(this@InterfaceMonitor) {
                if (!matched) {
                    if (currentNetwork == network) currentNetwork = null
                    available.remove(network)
                    return
                }
                available[network] = properties
                if (currentNetwork == null) currentNetwork = network
                else if (currentNetwork != network) return
                callbacks.toList()
            }.forEach { it.onAvailable(properties) }
        }

        override fun onLost(network: Network) {
            var properties: LinkProperties? = null
            synchronized(this@InterfaceMonitor) {
                if (available.remove(network) == null || currentNetwork != network) return
                if (available.isNotEmpty()) {
                    val next = available.entries.first()
                    currentNetwork = next.key
                    Timber.d("Switching to ${next.value} for $ifaceRegex")
                    properties = next.value
                } else currentNetwork = null
                callbacks.toList()
            }.forEach { it.onAvailable(properties) }
        }
    }

    override fun registerCallbackLocked(callback: Callback) {
        if (registered) {
            val currentLinkProperties = currentLinkProperties
            if (currentLinkProperties != null) GlobalScope.launch {
                callback.onAvailable(currentLinkProperties)
            }
        } else {
            Services.registerNetworkCallbackCompat(request, networkCallback)
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
