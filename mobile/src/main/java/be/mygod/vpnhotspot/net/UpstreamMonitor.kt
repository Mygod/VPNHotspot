package be.mygod.vpnhotspot.net

import android.content.SharedPreferences
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
            return if (upstream.isNullOrEmpty()) VpnMonitor else InterfaceMonitor(upstream)
        }
        private var monitor = generateMonitor()

        fun registerCallback(callback: Callback, failfast: (() -> Unit)? = null) = synchronized(this) {
            monitor.registerCallback(callback, failfast)
        }
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
                callbacks.forEach { new.registerCallback(it) { if (active) it.onLost() } }
            }
        }
    }

    interface Callback {
        /**
         * Called if some interface is available. This might be called on different ifname without having called onLost.
         */
        fun onAvailable(ifname: String, dns: List<InetAddress>)
        /**
         * Called if no interface is available.
         */
        fun onLost()
    }

    protected val callbacks = HashSet<Callback>()
    abstract val currentIface: String?
    protected abstract fun registerCallbackLocked(callback: Callback): Boolean
    protected abstract fun destroyLocked()

    fun registerCallback(callback: Callback, failfast: (() -> Unit)? = null) {
        if (synchronized(this) {
            if (!callbacks.add(callback)) return
            registerCallbackLocked(callback)
        }) failfast?.invoke()
    }
    fun unregisterCallback(callback: Callback) = synchronized(this) {
        if (callbacks.remove(callback) && callbacks.isEmpty()) destroyLocked()
    }
}
