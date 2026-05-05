package be.mygod.vpnhotspot.net.monitor

import be.mygod.vpnhotspot.net.IpDev
import be.mygod.vpnhotspot.net.NetlinkNeighbour
import be.mygod.vpnhotspot.net.MacAddressCompat
import be.mygod.vpnhotspot.root.daemon.DaemonController
import be.mygod.vpnhotspot.root.daemon.DaemonProtocol
import kotlinx.collections.immutable.PersistentMap
import kotlinx.collections.immutable.persistentMapOf
import kotlinx.collections.immutable.toPersistentMap
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.channels.onFailure
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import timber.log.Timber

class NetlinkNeighbourMonitor private constructor() {
    companion object {
        private val callbacks = mutableMapOf<Callback, Boolean>()
        var instance: NetlinkNeighbourMonitor? = null
        var fullMode = false

        /**
         * @param full Whether the invalid entries should also be parsed.
         *  However, even in light mode, caller should still filter out invalid entries in
         *  [Callback.onNetlinkNeighbourAvailable] in case the full mode was requested by other callers.
         */
        fun registerCallback(callback: Callback, full: Boolean = false) = synchronized(callbacks) {
            if (callbacks.put(callback, full) == full) return@synchronized null
            fullMode = full || callbacks.any { it.value }
            var monitor = instance
            if (monitor == null) {
                monitor = NetlinkNeighbourMonitor()
                instance = monitor
                null
            } else {
                monitor.flushAsync()
                monitor
            }
        }?.let { monitor -> monitor.scope.launch { callback.onNetlinkNeighbourAvailable(monitor.neighbours.values) } }
        fun unregisterCallback(callback: Callback) = synchronized(callbacks) {
            if (callbacks.remove(callback) == null) return@synchronized
            fullMode = callbacks.any { it.value }
            if (callbacks.isNotEmpty()) return@synchronized
            instance?.destroy()
            instance = null
        }
    }

    interface Callback {
        fun onNetlinkNeighbourAvailable(neighbours: Collection<NetlinkNeighbour>)
    }

    private val dispatcher = Dispatchers.Default.limitedParallelism(1, "NetlinkNeighbourMonitor")
    private val scope = CoroutineScope(dispatcher + SupervisorJob())
    private val aggregateChannel = Channel<PersistentMap<IpDev, NetlinkNeighbour>>(Channel.CONFLATED)
    init {
        scope.launch {
            for (value in aggregateChannel) {
                val neighbours = value.values
                for (callback in synchronized(callbacks) { callbacks.keys.toList() }) {
                    callback.onNetlinkNeighbourAvailable(neighbours)
                }
            }
        }
    }
    private var neighbours = persistentMapOf<IpDev, NetlinkNeighbour>()
    private val listener: suspend (DaemonProtocol.NeighbourUpdate) -> Unit = {
        withContext(dispatcher) { updateLocked(it) }
    }
    private val worker: Job = scope.launch {
        try {
            DaemonController.startNeighbourMonitor(listener)
        } catch (_: CancellationException) {
        } catch (e: Exception) {
            Timber.w(e)
        }
        flush()
    }

    private fun NetlinkNeighbour.toMonitorMode(full: Boolean): NetlinkNeighbour? {
        val state = if (!full && state != NetlinkNeighbour.State.VALID) NetlinkNeighbour.State.DELETING else state
        if (lladdr == MacAddressCompat.ALL_ZEROS_ADDRESS && state != NetlinkNeighbour.State.INCOMPLETE &&
                state != NetlinkNeighbour.State.DELETING) {
            Timber.d("Missing neighbour lladdr for $ip dev $dev state $state")
            return null
        }
        return if (state == this.state) this else copy(state = state)
    }

    private fun updateLocked(update: DaemonProtocol.NeighbourUpdate) {
        val full = synchronized(callbacks) { fullMode }
        if (update.replace) {
            neighbours = mutableMapOf<IpDev, NetlinkNeighbour>().apply {
                for (candidate in update.neighbours) {
                    val neighbour = candidate.toMonitorMode(full) ?: continue
                    if (neighbour.state != NetlinkNeighbour.State.DELETING) this[IpDev(neighbour)] = neighbour
                }
            }.toPersistentMap()
            aggregateChannel.trySend(neighbours).onFailure { throw it!! }
            return
        }
        val old = neighbours
        for (candidate in update.neighbours) {
            val neighbour = candidate.toMonitorMode(full) ?: continue
            neighbours = when (neighbour.state) {
                NetlinkNeighbour.State.DELETING -> neighbours.remove(IpDev(neighbour))
                else -> neighbours.put(IpDev(neighbour), neighbour)
            }
        }
        if (neighbours != old) aggregateChannel.trySend(neighbours).onFailure { throw it!! }
    }

    private suspend fun flush() = updateLocked(DaemonProtocol.NeighbourUpdate(true, DaemonController.dumpNeighbours()))
    fun flushAsync() = scope.launch { flush() }

    fun destroy() {
        scope.launch {
            try {
                worker.join()
                DaemonController.stopNeighbourMonitor(listener)
            } catch (_: CancellationException) {
            } catch (e: Exception) {
                Timber.w(e)
            } finally {
                scope.cancel()
            }
        }
    }
}
