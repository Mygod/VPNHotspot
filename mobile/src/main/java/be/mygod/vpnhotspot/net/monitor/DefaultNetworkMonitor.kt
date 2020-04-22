package be.mygod.vpnhotspot.net.monitor

import android.annotation.TargetApi
import android.net.*
import android.os.Build
import be.mygod.vpnhotspot.App.Companion.app
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.launch
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
    private val networkRequest = networkRequestBuilder()
            .addCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
            .addCapability(NetworkCapabilities.NET_CAPABILITY_NOT_RESTRICTED)
            .build()
    private val networkCallback = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) {
            val properties = app.connectivity.getLinkProperties(network)
            val ifname = properties?.interfaceName ?: return
            synchronized(this@DefaultNetworkMonitor) {
                val oldProperties = currentLinkProperties
                if (currentNetwork != network || ifname != oldProperties?.interfaceName) {
                    callbacks.forEach { it.onLost() }   // we are using the other default network now
                    currentNetwork = network
                }
                currentLinkProperties = properties
                callbacks.forEach { it.onAvailable(ifname, properties) }
            }
        }

        override fun onLinkPropertiesChanged(network: Network, properties: LinkProperties) {
            synchronized(this@DefaultNetworkMonitor) {
                if (currentNetwork == null) {
                    onAvailable(network)
                    return
                }
                if (currentNetwork != network) return
                val oldProperties = currentLinkProperties!!
                currentLinkProperties = properties
                val ifname = properties.interfaceName
                when {
                    ifname == null -> {
                        Timber.w("interfaceName became null: $oldProperties -> $properties")
                        onLost(network)
                    }
                    ifname != oldProperties.interfaceName -> {
                        Timber.w(RuntimeException("interfaceName changed: $oldProperties -> $properties"))
                        callbacks.forEach {
                            it.onLost()
                            it.onAvailable(ifname, properties)
                        }
                    }
                    else -> callbacks.forEach { it.onAvailable(ifname, properties) }
                }
            }
        }

        override fun onLost(network: Network) = synchronized(this@DefaultNetworkMonitor) {
            if (currentNetwork != network) return
            callbacks.forEach { it.onLost() }
            currentNetwork = null
            currentLinkProperties = null
        }
    }

    override fun registerCallbackLocked(callback: Callback) {
        if (registered) {
            val currentLinkProperties = currentLinkProperties
            if (currentLinkProperties != null) GlobalScope.launch {
                callback.onAvailable(currentLinkProperties.interfaceName!!, currentLinkProperties)
            }
        } else {
            if (Build.VERSION.SDK_INT in 24..27) @TargetApi(24) {
                app.connectivity.registerDefaultNetworkCallback(networkCallback)
            } else try {
                app.connectivity.requestNetwork(networkRequest, networkCallback)
            } catch (e: SecurityException) {
                // SecurityException would be thrown in requestNetwork on Android 6.0 thanks to Google's stupid bug
                if (Build.VERSION.SDK_INT != 23) throw e
                GlobalScope.launch { callback.onFallback() }
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
