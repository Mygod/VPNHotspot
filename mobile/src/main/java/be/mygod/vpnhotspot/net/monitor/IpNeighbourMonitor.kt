package be.mygod.vpnhotspot.net.monitor

import be.mygod.vpnhotspot.net.IpNeighbour
import kotlinx.collections.immutable.PersistentMap
import kotlinx.collections.immutable.persistentMapOf
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.channels.actor
import kotlinx.coroutines.channels.sendBlocking
import java.net.InetAddress

class IpNeighbourMonitor private constructor() : IpMonitor() {
    companion object {
        private val callbacks = mutableSetOf<Callback>()
        var instance: IpNeighbourMonitor? = null

        fun registerCallback(callback: Callback) = synchronized(callbacks) {
            if (!callbacks.add(callback)) return@synchronized null
            var monitor = instance
            if (monitor == null) {
                monitor = IpNeighbourMonitor()
                instance = monitor
                monitor.flushAsync()
                null
            } else monitor.neighbours.values
        }?.let { callback.onIpNeighbourAvailable(it) }
        fun unregisterCallback(callback: Callback) = synchronized(callbacks) {
            if (!callbacks.remove(callback) || callbacks.isNotEmpty()) return@synchronized
            instance?.destroy()
            instance = null
        }
    }

    interface Callback {
        fun onIpNeighbourAvailable(neighbours: Collection<IpNeighbour>)
    }

    private val aggregator = GlobalScope.actor<PersistentMap<InetAddress, IpNeighbour>>(capacity = Channel.CONFLATED) {
        for (value in channel) {
            val neighbours = value.values
            synchronized(callbacks) { for (callback in callbacks) callback.onIpNeighbourAvailable(neighbours) }
        }
    }
    private var neighbours = persistentMapOf<InetAddress, IpNeighbour>()

    override val monitoredObject: String get() = "neigh"

    override fun processLine(line: String) {
        val old = neighbours
        for (neighbour in IpNeighbour.parse(line)) neighbours = when (neighbour.state) {
            IpNeighbour.State.DELETING -> neighbours.remove(neighbour.ip)
            else -> neighbours.put(neighbour.ip, neighbour)
        }
        if (neighbours != old) aggregator.sendBlocking(neighbours)
    }

    override fun processLines(lines: Sequence<String>) {
        neighbours = lines
                .flatMap { IpNeighbour.parse(it).asSequence() }
                .filter { it.state != IpNeighbour.State.DELETING }  // skip entries without lladdr
                .associateByTo(persistentMapOf<InetAddress, IpNeighbour>().builder()) { it.ip }
                .build()
        aggregator.sendBlocking(neighbours)
    }
}
