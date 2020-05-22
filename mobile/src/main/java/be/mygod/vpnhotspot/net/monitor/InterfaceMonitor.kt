package be.mygod.vpnhotspot.net.monitor

import android.net.LinkProperties
import be.mygod.vpnhotspot.App.Companion.app
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.launch

class InterfaceMonitor(val iface: String) : UpstreamMonitor() {
    private fun setPresent(present: Boolean) {
        var available: Pair<String, LinkProperties>? = null
        synchronized(this) {
            val old = currentIface != null
            if (present == old) return
            currentIface = if (present) iface else null
            if (present) available = iface to (currentLinkProperties ?: return)
            callbacks.toList()
        }.forEach {
            @Suppress("NAME_SHADOWING")
            val available = available
            if (available != null) {
                val (iface, lp) = available
                it.onAvailable(iface, lp)
            } else it.onLost()
        }
    }

    private var registered = false
    override var currentIface: String? = null
        private set
    override val currentLinkProperties get() = app.connectivity.allNetworks
            .map { app.connectivity.getLinkProperties(it) }
            .singleOrNull { it?.interfaceName == iface }

    override fun registerCallbackLocked(callback: Callback) {
        if (!registered) {
            IpLinkMonitor.registerCallback(this, iface, this::setPresent)
            registered = true
        } else if (currentIface != null) GlobalScope.launch {
            callback.onAvailable(iface, currentLinkProperties ?: return@launch)
        }
    }

    override fun destroyLocked() {
        IpLinkMonitor.unregisterCallback(this)
        currentIface = null
        registered = false
    }
}
