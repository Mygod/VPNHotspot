package be.mygod.vpnhotspot.net.monitor

import android.annotation.TargetApi
import android.net.ConnectivityManager
import android.net.LinkProperties
import android.net.Network
import android.net.NetworkCapabilities
import android.os.Build
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.globalNetworkRequestBuilder
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.launch

object DefaultNetworkMonitor : UpstreamMonitor() {
    private var registered = false
    override var currentLinkProperties: LinkProperties? = null
        private set
    /**
     * Unfortunately registerDefaultNetworkCallback is going to return VPN interface since Android P DP1:
     * https://android.googlesource.com/platform/frameworks/base/+/dda156ab0c5d66ad82bdcf76cda07cbc0a9c8a2e
     */
    private val networkRequest = globalNetworkRequestBuilder().apply {
        addCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)
        addCapability(NetworkCapabilities.NET_CAPABILITY_NOT_RESTRICTED)
    }.build()
    private val networkCallback = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) {
            val properties = Services.connectivity.getLinkProperties(network)
            synchronized(this@DefaultNetworkMonitor) {
                currentLinkProperties = properties
                callbacks.toList()
            }.forEach { it.onAvailable(properties) }
        }

        override fun onLinkPropertiesChanged(network: Network, properties: LinkProperties) {
            synchronized(this@DefaultNetworkMonitor) {
                currentLinkProperties = properties
                callbacks.toList()
            }.forEach { it.onAvailable(properties) }
        }

        override fun onLost(network: Network) = synchronized(this@DefaultNetworkMonitor) {
            currentLinkProperties = null
            callbacks.toList()
        }.forEach { it.onAvailable() }
    }

    override fun registerCallbackLocked(callback: Callback) {
        if (registered) {
            val currentLinkProperties = currentLinkProperties
            if (currentLinkProperties != null) GlobalScope.launch {
                callback.onAvailable(currentLinkProperties)
            }
        } else {
            when (Build.VERSION.SDK_INT) {
                in 31..Int.MAX_VALUE -> @TargetApi(31) {
                    Services.connectivity.registerBestMatchingNetworkCallback(networkRequest, networkCallback,
                        Services.mainHandler)
                }
                in 28..30 -> @TargetApi(28) {
                    Services.connectivity.requestNetwork(networkRequest, networkCallback, Services.mainHandler)
                }
                in 26..27 -> @TargetApi(26) {
                    Services.connectivity.registerDefaultNetworkCallback(networkCallback, Services.mainHandler)
                }
                in 24..25 -> @TargetApi(24) {
                    Services.connectivity.registerDefaultNetworkCallback(networkCallback)
                }
                else -> try {
                    Services.connectivity.requestNetwork(networkRequest, networkCallback)
                } catch (e: RuntimeException) {
                    // SecurityException would be thrown in requestNetwork on Android 6.0 thanks to Google's stupid bug
                    if (Build.VERSION.SDK_INT != 23) throw e
                    GlobalScope.launch { callback.onFallback() }
                    return
                }
            }
            registered = true
        }
    }

    override fun destroyLocked() {
        if (!registered) return
        Services.connectivity.unregisterNetworkCallback(networkCallback)
        registered = false
        currentLinkProperties = null
    }
}
