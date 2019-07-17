package be.mygod.vpnhotspot.net.monitor

import be.mygod.vpnhotspot.net.IpNeighbour
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.launch
import java.net.InetAddress
import java.util.*

class IpNeighbourMonitor private constructor() : IpMonitor() {
    companion object {
        private val callbacks = mutableSetOf<Callback>()
        var instance: IpNeighbourMonitor? = null

        fun registerCallback(callback: Callback) = synchronized(callbacks) {
            if (!callbacks.add(callback)) return@synchronized
            var monitor = instance
            if (monitor == null) {
                monitor = IpNeighbourMonitor()
                instance = monitor
                monitor.flush()
            } else {
                callback.onIpNeighbourAvailable(synchronized(monitor.neighbours) { monitor.neighbours.values.toList() })
            }
        }
        fun unregisterCallback(callback: Callback) = synchronized(callbacks) {
            if (!callbacks.remove(callback) || callbacks.isNotEmpty()) return@synchronized
            instance?.destroy()
            instance = null
        }
    }

    interface Callback {
        fun onIpNeighbourAvailable(neighbours: List<IpNeighbour>)
    }

    private var updatePosted = false
    private val neighbours = HashMap<InetAddress, IpNeighbour>()

    override val monitoredObject: String get() = "neigh"

    override fun processLine(line: String) {
        synchronized(neighbours) {
            if (IpNeighbour.parse(line).map { neighbour ->
                if (neighbour.state == IpNeighbour.State.DELETING)
                    neighbours.remove(neighbour.ip) != null
                else neighbours.put(neighbour.ip, neighbour) != neighbour
            }.any { it }) postUpdateLocked()
        }
    }

    override fun processLines(lines: Sequence<String>) {
        synchronized(neighbours) {
            neighbours.clear()
            neighbours.putAll(lines
                    .flatMap { IpNeighbour.parse(it).asSequence() }
                    .filter { it.state != IpNeighbour.State.DELETING }  // skip entries without lladdr
                    .associateBy { it.ip })
            postUpdateLocked()
        }
    }

    private fun postUpdateLocked() {
        if (updatePosted || instance != this) return
        GlobalScope.launch(Dispatchers.Main) {
            val neighbours = synchronized(neighbours) {
                updatePosted = false
                neighbours.values.toList()
            }
            synchronized(callbacks) {
                for (callback in callbacks) callback.onIpNeighbourAvailable(neighbours)
            }
        }
        updatePosted = true
    }
}
