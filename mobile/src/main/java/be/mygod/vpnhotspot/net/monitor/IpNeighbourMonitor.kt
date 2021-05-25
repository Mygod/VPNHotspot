package be.mygod.vpnhotspot.net.monitor

import be.mygod.vpnhotspot.net.IpDev
import be.mygod.vpnhotspot.net.IpNeighbour
import kotlinx.collections.immutable.PersistentMap
import kotlinx.collections.immutable.persistentMapOf
import kotlinx.collections.immutable.toPersistentMap
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.channels.*

class IpNeighbourMonitor private constructor() : IpMonitor() {
    companion object {
        private val callbacks = mutableMapOf<Callback, Boolean>()
        var instance: IpNeighbourMonitor? = null
        var fullMode = false

        /**
         * @param full Whether the invalid entries should also be parsed.
         *  In this case it is more likely to trigger root request on API 29+.
         *  However, even in light mode, caller should still filter out invalid entries in
         *  [Callback.onIpNeighbourAvailable] in case the full mode was requested by other callers.
         */
        fun registerCallback(callback: Callback, full: Boolean = false) = synchronized(callbacks) {
            if (callbacks.put(callback, full) == full) return@synchronized null
            fullMode = full || callbacks.any { it.value }
            var monitor = instance
            if (monitor == null) {
                monitor = IpNeighbourMonitor()
                instance = monitor
                null
            } else {
                monitor.flushAsync()
                monitor.neighbours.values
            }
        }?.let { callback.onIpNeighbourAvailable(it) }
        fun unregisterCallback(callback: Callback) = synchronized(callbacks) {
            if (callbacks.remove(callback) == null) return@synchronized
            fullMode = callbacks.any { it.value }
            if (callbacks.isNotEmpty()) return@synchronized
            instance?.destroy()
            instance = null
        }
    }

    interface Callback {
        fun onIpNeighbourAvailable(neighbours: Collection<IpNeighbour>)
    }

    private val aggregator = GlobalScope.actor<PersistentMap<IpDev, IpNeighbour>>(capacity = Channel.CONFLATED) {
        for (value in channel) {
            val neighbours = value.values
            for (callback in synchronized(callbacks) { callbacks.keys.toList() }) {
                callback.onIpNeighbourAvailable(neighbours)
            }
        }
    }
    private var neighbours = persistentMapOf<IpDev, IpNeighbour>()

    init {
        init()
    }

    override val monitoredObject: String get() = "neigh"

    override suspend fun processLine(line: String) {
        val old = neighbours
        for (neighbour in IpNeighbour.parse(line, fullMode)) neighbours = when (neighbour.state) {
            IpNeighbour.State.DELETING -> neighbours.remove(IpDev(neighbour))
            else -> neighbours.put(IpDev(neighbour), neighbour)
        }
        if (neighbours != old) aggregator.trySendBlocking(neighbours).onFailure { throw it!! }
    }

    override suspend fun processLines(lines: Sequence<String>) {
        neighbours = mutableMapOf<IpDev, IpNeighbour>().apply {
            for (line in lines) for (neigh in IpNeighbour.parse(line, fullMode)) {
                if (neigh.state != IpNeighbour.State.DELETING) this[IpDev(neigh)] = neigh
            }
        }.toPersistentMap()
        aggregator.trySendBlocking(neighbours).onFailure { throw it!! }
    }
}
