package be.mygod.vpnhotspot.net.monitor

import be.mygod.vpnhotspot.App.Companion.app

class InterfaceMonitor(val iface: String) : UpstreamMonitor() {
    private fun setPresent(present: Boolean) = synchronized(this) {
        val old = currentIface != null
        if (present == old) return
        currentIface = if (present) iface else null
        if (present) {
            val dns = currentDns
            callbacks.forEach { it.onAvailable(iface, dns) }
        } else callbacks.forEach { it.onLost() }
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
        } else if (currentIface != null) callback.onAvailable(iface, currentDns)
    }

    override fun destroyLocked() {
        IpLinkMonitor.unregisterCallback(this)
        currentIface = null
        registered = false
    }
}
