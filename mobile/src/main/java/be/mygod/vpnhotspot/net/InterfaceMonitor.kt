package be.mygod.vpnhotspot.net

import be.mygod.vpnhotspot.App.Companion.app

class InterfaceMonitor(val iface: String) : UpstreamMonitor() {
    companion object {
        /**
         * Based on: https://android.googlesource.com/platform/external/iproute2/+/70556c1/ip/ipaddress.c#1053
         */
        private val parser = "^(Deleted )?-?\\d+: ([^:@]+)".toRegex()
    }

    private inner class IpLinkMonitor : IpMonitor() {
        override val monitoredObject: String get() = "link"

        override fun processLine(line: String) {
            val match = parser.find(line) ?: return
            if (match.groupValues[2] != iface) return
            setPresent(match.groupValues[1].isEmpty())
        }

        override fun processLines(lines: Sequence<String>) =
                setPresent(lines.any { parser.find(it)?.groupValues?.get(2) == iface })
    }

    private fun setPresent(present: Boolean) = if (initializing) {
        initializedPresent = present
        currentIface = if (present) iface else null
    } else synchronized(this) {
        val old = currentIface != null
        if (present == old) return
        currentIface = if (present) iface else null
        if (present) {
            val dns = dns
            callbacks.forEach { it.onAvailable(iface, dns) }
        } else callbacks.forEach { it.onLost() }
    }

    private var monitor: IpLinkMonitor? = null
    private var initializing = false
    private var initializedPresent: Boolean? = null
    override var currentIface: String? = null
        private set
    private val dns get() = app.connectivity.allNetworks
            .map { app.connectivity.getLinkProperties(it) }
            .singleOrNull { it.interfaceName == iface }
            ?.dnsServers ?: emptyList()

    override fun registerCallbackLocked(callback: Callback): Boolean {
        var monitor = monitor
        val present = if (monitor == null) {
            initializing = true
            initializedPresent = null
            monitor = IpLinkMonitor()
            this.monitor = monitor
            monitor.run()
            initializing = false
            initializedPresent!!
        } else currentIface != null
        if (present) callback.onAvailable(iface, dns)
        return !present
    }

    override fun destroyLocked() {
        val monitor = monitor ?: return
        this.monitor = null
        currentIface = null
        monitor.destroy()
    }
}
