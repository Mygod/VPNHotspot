package be.mygod.vpnhotspot.net

import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.util.debugLog

class IpNeighbourMonitor private constructor() : IpMonitor() {
    companion object {
        private val callbacks = HashSet<Callback>()
        var instance: IpNeighbourMonitor? = null

        fun registerCallback(callback: Callback) {
            if (!callbacks.add(callback)) return
            var monitor = instance
            if (monitor == null) {
                monitor = IpNeighbourMonitor()
                instance = monitor
                monitor.flush()
            } else {
                callback.onIpNeighbourAvailable(synchronized(monitor.neighbours) { monitor.neighbours.values.toList() })
            }
        }
        fun unregisterCallback(callback: Callback) {
            if (!callbacks.remove(callback) || callbacks.isNotEmpty()) return
            instance?.destroy()
            instance = null
        }
    }

    interface Callback {
        fun onIpNeighbourAvailable(neighbours: List<IpNeighbour>)
    }

    private var updatePosted = false
    val neighbours = HashMap<String, IpNeighbour>()

    override val monitoredObject: String get() = "neigh"

    override fun processLine(line: String) {
        synchronized(neighbours) {
            val neighbour = IpNeighbour.parse(line) ?: return
            debugLog(javaClass.simpleName, line)
            val changed = if (neighbour.state == IpNeighbour.State.DELETING)
                neighbours.remove(neighbour.ip) != null
            else neighbours.put(neighbour.ip, neighbour) != neighbour
            if (changed) postUpdateLocked()
        }
    }

    override fun processLines(lines: Sequence<String>) {
        synchronized(neighbours) {
            neighbours.clear()
            neighbours.putAll(lines
                    .map(IpNeighbour.Companion::parse)
                    .filterNotNull()
                    .filter { it.state != IpNeighbour.State.DELETING }  // skip entries without lladdr
                    .associateBy { it.ip })
            postUpdateLocked()
        }
    }

    private fun postUpdateLocked() {
        if (updatePosted || instance != this) return
        app.handler.post {
            val neighbours = synchronized(neighbours) {
                updatePosted = false
                neighbours.values.toList()
            }
            for (callback in callbacks) callback.onIpNeighbourAvailable(neighbours)
        }
        updatePosted = true
    }
}
