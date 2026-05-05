package be.mygod.vpnhotspot.net.monitor

import be.mygod.vpnhotspot.net.IpDev
import be.mygod.vpnhotspot.net.NetlinkNeighbour
import be.mygod.vpnhotspot.net.MacAddressCompat
import be.mygod.vpnhotspot.root.daemon.DaemonController
import be.mygod.vpnhotspot.root.daemon.DaemonProtocol
import be.mygod.vpnhotspot.widget.SmartSnackbar
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
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import timber.log.Timber
import java.io.IOException

class NetlinkNeighbourMonitor private constructor(private val previousDestroy: Job?) {
    companion object {
        private val callbacks = mutableMapOf<Callback, Boolean>()
        private val lifecycleLock = Mutex()
        private var pendingDestroy: Job? = null
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
                monitor = NetlinkNeighbourMonitor(pendingDestroy)
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
            val monitor = instance
            val destroyJob = monitor?.scope?.launch {
                try {
                    monitor.worker.join()
                    lifecycleLock.withLock { DaemonController.stopNeighbourMonitor(monitor.listener) }
                } catch (_: CancellationException) {
                } catch (e: Exception) {
                    Timber.w(e)
                } finally {
                    monitor.scope.cancel()
                }
            }
            pendingDestroy = destroyJob
            destroyJob?.invokeOnCompletion {
                synchronized(callbacks) {
                    if (pendingDestroy === destroyJob) pendingDestroy = null
                }
            }
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
            previousDestroy?.join()
            lifecycleLock.withLock { DaemonController.startNeighbourMonitor(listener) }
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            Timber.w(e)
            SmartSnackbar.make(e).show()
        }
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

    private suspend fun flush() {
        val neighbours = try {
            DaemonController.dumpNeighbours()
        } catch (e: CancellationException) {
            throw e
        } catch (e: IOException) {
            Timber.w(e)
            SmartSnackbar.make(e).show()
            return
        }
        updateLocked(DaemonProtocol.NeighbourUpdate(true, neighbours))
    }
    fun flushAsync() = scope.launch { flush() }
}
