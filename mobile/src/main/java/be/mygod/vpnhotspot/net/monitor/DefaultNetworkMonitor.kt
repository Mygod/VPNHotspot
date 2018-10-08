package be.mygod.vpnhotspot.net.monitor

import android.annotation.TargetApi
import android.net.*
import android.os.Build
import be.mygod.vpnhotspot.App.Companion.app
import timber.log.Timber

object DefaultNetworkMonitor : UpstreamMonitor() {
    private var registered = false
    private var currentNetwork: Network? = null
    override var currentLinkProperties: LinkProperties? = null
        private set
    /**
     * Unfortunately registerDefaultNetworkCallback is going to return VPN interface since Android P DP1:
     * https://android.googlesource.com/platform/frameworks/base/+/dda156ab0c5d66ad82bdcf76cda07cbc0a9c8a2e
     */
    private val networkRequest = NetworkRequest.Builder()
            .addCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
            .addCapability(NetworkCapabilities.NET_CAPABILITY_NOT_RESTRICTED)
            .build()
    private val networkCallback = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) {
            val properties = app.connectivity.getLinkProperties(network)
            val ifname = properties?.interfaceName ?: return
            when (currentNetwork) {
                null -> { }
                network -> {
                    val oldProperties = currentLinkProperties!!
                    check(ifname == oldProperties.interfaceName)
                    if (properties.dnsServers == oldProperties.dnsServers) return
                }
                else -> callbacks.forEach { it.onLost() }   // we are using the other default network now
            }
            currentNetwork = network
            currentLinkProperties = properties
            callbacks.forEach { it.onAvailable(ifname, properties.dnsServers) }
        }

        override fun onLinkPropertiesChanged(network: Network, properties: LinkProperties) {
            if (currentNetwork != network) return
            val oldProperties = currentLinkProperties!!
            currentLinkProperties = properties
            val ifname = properties.interfaceName!!
            check(ifname == oldProperties.interfaceName)
            if (properties.dnsServers != oldProperties.dnsServers)
                callbacks.forEach { it.onAvailable(ifname, properties.dnsServers) }
        }

        override fun onLost(network: Network) {
            if (currentNetwork != network) return
            callbacks.forEach { it.onLost() }
            currentNetwork = null
            currentLinkProperties = null
        }
    }

    override fun registerCallbackLocked(callback: Callback) {
        if (registered) {
            val currentLinkProperties = currentLinkProperties
            if (currentLinkProperties != null) {
                callback.onAvailable(currentLinkProperties.interfaceName!!, currentLinkProperties.dnsServers)
            }
        } else {
            if (Build.VERSION.SDK_INT in 24..27) @TargetApi(24) {
                app.connectivity.registerDefaultNetworkCallback(networkCallback)
            } else try {
                app.connectivity.requestNetwork(networkRequest, networkCallback)
            } catch (e: SecurityException) {
                if (Build.VERSION.SDK_INT != 23) throw e
                // SecurityException would be thrown in requestNetwork on Android 6.0 thanks to Google's stupid bug
                Timber.w(e)
                callback.onFallback()
                return
            }
            registered = true
        }
    }

    override fun destroyLocked() {
        if (!registered) return
        app.connectivity.unregisterNetworkCallback(networkCallback)
        registered = false
        currentNetwork = null
        currentLinkProperties = null
    }
}
