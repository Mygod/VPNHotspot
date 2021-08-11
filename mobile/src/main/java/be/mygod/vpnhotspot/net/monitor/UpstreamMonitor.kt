package be.mygod.vpnhotspot.net.monitor

import android.content.SharedPreferences
import android.net.LinkProperties
import be.mygod.vpnhotspot.App.Companion.app
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.launch

abstract class UpstreamMonitor {
    companion object : SharedPreferences.OnSharedPreferenceChangeListener {
        const val KEY = "service.upstream"

        init {
            app.pref.registerOnSharedPreferenceChangeListener(this)
        }

        private fun generateMonitor(): UpstreamMonitor {
            val upstream = app.pref.getString(KEY, null)
            return if (upstream.isNullOrEmpty()) VpnMonitor else InterfaceMonitor(upstream)
        }
        private var monitor = generateMonitor()

        fun registerCallback(callback: Callback) = synchronized(this) { monitor.registerCallback(callback) }
        fun unregisterCallback(callback: Callback) = synchronized(this) { monitor.unregisterCallback(callback) }

        override fun onSharedPreferenceChanged(sharedPreferences: SharedPreferences?, key: String?) {
            if (key == KEY) GlobalScope.launch {    // prevent callback called in main
                synchronized(this) {
                    val old = monitor
                    val callbacks = synchronized(old) {
                        old.callbacks.toList().also {
                            old.callbacks.clear()
                            old.destroyLocked()
                        }
                    }
                    val new = generateMonitor()
                    monitor = new
                    for (callback in callbacks) new.registerCallback(callback)
                }
            }
        }
    }

    interface Callback {
        /**
         * Called if some possibly stacked interface is available
         */
        fun onAvailable(properties: LinkProperties? = null)
        /**
         * Called on API 23- from DefaultNetworkMonitor. This indicates that there isn't a good way of telling the
         * default network (see DefaultNetworkMonitor) and we are using rules at priority 22000
         * (RULE_PRIORITY_DEFAULT_NETWORK) as our fallback rules, which would work fine until Android 9.0 broke it in
         * commit: https://android.googlesource.com/platform/system/netd/+/758627c4d93392190b08e9aaea3bbbfb92a5f364
         */
        fun onFallback() {
            throw UnsupportedOperationException()
        }
    }

    val callbacks = mutableSetOf<Callback>()
    protected abstract val currentLinkProperties: LinkProperties?
    protected abstract fun registerCallbackLocked(callback: Callback)
    abstract fun destroyLocked()

    fun registerCallback(callback: Callback) {
        synchronized(this) {
            if (callbacks.add(callback)) registerCallbackLocked(callback)
        }
    }
    fun unregisterCallback(callback: Callback) = synchronized(this) {
        if (callbacks.remove(callback) && callbacks.isEmpty()) destroyLocked()
    }
}
