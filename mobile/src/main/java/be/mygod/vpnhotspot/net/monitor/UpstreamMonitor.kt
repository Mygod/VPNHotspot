package be.mygod.vpnhotspot.net.monitor

import android.content.SharedPreferences
import android.net.LinkProperties
import be.mygod.vpnhotspot.App.Companion.app
import java.net.InetAddress

abstract class UpstreamMonitor {
    companion object : SharedPreferences.OnSharedPreferenceChangeListener {
        const val KEY = "service.upstream"

        init {
            app.pref.registerOnSharedPreferenceChangeListener(this)
        }

        private fun generateMonitor(): UpstreamMonitor {
            val upstream = app.pref.getString(KEY, null)
            return if (upstream.isNullOrEmpty()) VpnMonitor else InterfaceMonitor(upstream!!)
        }
        private var monitor = generateMonitor()

        fun registerCallback(callback: Callback) = synchronized(this) { monitor.registerCallback(callback) }
        fun unregisterCallback(callback: Callback) = synchronized(this) { monitor.unregisterCallback(callback) }

        override fun onSharedPreferenceChanged(sharedPreferences: SharedPreferences?, key: String?) {
            if (key == KEY) synchronized(this) {
                val old = monitor
                val (active, callbacks) = synchronized(old) {
                    val active = old.currentIface != null
                    val callbacks = old.callbacks.toList()
                    old.callbacks.clear()
                    old.destroyLocked()
                    Pair(active, callbacks)
                }
                val new = generateMonitor()
                monitor = new
                for (callback in callbacks) {
                    if (active) callback.onLost()
                    new.registerCallback(callback)
                }
            }
        }
    }

    interface Callback {
        /**
         * Called if some interface is available. This might be called on different ifname without having called onLost.
         * This might also be called on the same ifname but with updated DNS list.
         */
        fun onAvailable(ifname: String, dns: List<InetAddress>)
        /**
         * Called if no interface is available.
         */
        fun onLost()
        /**
         * Called on API 23- from DefaultNetworkMonitor. This indicates that there isn't a good way of telling the
         * default network (see DefaultNetworkMonitor) and we are using rules at priority 22000
         * (RULE_PRIORITY_DEFAULT_NETWORK) as our fallback rules, which would work fine until Android 9.0 broke it in
         * commit: https://android.googlesource.com/platform/system/netd/+/758627c4d93392190b08e9aaea3bbbfb92a5f364
         */
        fun onFallback() {
            throw NotImplementedError()
        }
    }

    val callbacks = HashSet<Callback>()
    protected abstract val currentLinkProperties: LinkProperties?
    open val currentIface: String? get() = currentLinkProperties?.interfaceName
    /**
     * There's no need for overriding currentDns for now.
     */
    val currentDns: List<InetAddress> get() = currentLinkProperties?.dnsServers ?: emptyList()
    protected abstract fun registerCallbackLocked(callback: Callback)
    abstract fun destroyLocked()

    fun registerCallback(callback: Callback) {
        synchronized(this) {
            if (!callbacks.add(callback)) return
            registerCallbackLocked(callback)
        }
    }
    fun unregisterCallback(callback: Callback) = synchronized(this) {
        if (callbacks.remove(callback) && callbacks.isEmpty()) destroyLocked()
    }
}
