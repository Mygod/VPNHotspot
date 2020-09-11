package be.mygod.vpnhotspot.net.monitor

import android.content.SharedPreferences
import be.mygod.vpnhotspot.App.Companion.app
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.launch

abstract class FallbackUpstreamMonitor private constructor() : UpstreamMonitor() {
    companion object : SharedPreferences.OnSharedPreferenceChangeListener {
        const val KEY = "service.upstream.fallback"

        init {
            app.pref.registerOnSharedPreferenceChangeListener(this)
        }

        private fun generateMonitor(): UpstreamMonitor {
            val upstream = app.pref.getString(KEY, null)
            return if (upstream.isNullOrEmpty()) DefaultNetworkMonitor else InterfaceMonitor(upstream)
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
}
