package be.mygod.vpnhotspot.net.monitor

class IpLinkMonitor private constructor() : IpMonitor() {
    companion object {
        /**
         * Based on: https://android.googlesource.com/platform/external/iproute2/+/70556c1/ip/ipaddress.c#1053
         */
        private val parser = "^(Deleted )?-?\\d+: ([^:@]+)".toRegex()

        private val callbacks = HashMap<Any, Pair<String, (Boolean) -> Unit>>()
        private var instance: IpLinkMonitor? = null

        fun registerCallback(owner: Any, iface: String, callback: (Boolean) -> Unit) = synchronized(this) {
            check(callbacks.put(owner, Pair(iface, callback)) == null)
            var monitor = instance
            if (monitor == null) {
                monitor = IpLinkMonitor()
                instance = monitor
            }
            monitor.flushAsync()
        }
        fun unregisterCallback(owner: Any) = synchronized(this) {
            if (callbacks.remove(owner) == null || callbacks.isNotEmpty()) return@synchronized
            instance?.destroy()
            instance = null
        }
    }

    override val monitoredObject: String get() = "link"

    override fun processLine(line: String) {
        val match = parser.find(line) ?: return
        val iface = match.groupValues[2]
        val present = match.groupValues[1].isEmpty()
        synchronized(IpLinkMonitor) {
            for ((target, callback) in callbacks.values) if (target == iface) callback(present)
        }
    }

    override fun processLines(lines: Sequence<String>) {
        val present = HashSet<String>()
        for (it in lines) present.add((parser.find(it) ?: continue).groupValues[2])
        synchronized(IpLinkMonitor) { for ((iface, callback) in callbacks.values) callback(present.contains(iface)) }
    }
}
