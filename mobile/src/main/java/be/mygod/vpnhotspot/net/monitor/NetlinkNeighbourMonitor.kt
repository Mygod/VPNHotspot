package be.mygod.vpnhotspot.net.monitor

import be.mygod.vpnhotspot.net.IpDev
import be.mygod.vpnhotspot.net.NetlinkNeighbour
import be.mygod.vpnhotspot.root.daemon.DaemonController
import be.mygod.vpnhotspot.root.daemon.DaemonProtocol
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.collections.immutable.PersistentMap
import kotlinx.collections.immutable.mutate
import kotlinx.collections.immutable.persistentMapOf
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
        private val callbacks = mutableSetOf<Callback>()
        private val lifecycleLock = Mutex()
        private var pendingDestroy: Job? = null
        var instance: NetlinkNeighbourMonitor? = null

        fun registerCallback(callback: Callback) = synchronized(callbacks) {
            if (!callbacks.add(callback)) return@synchronized null
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
            if (!callbacks.remove(callback)) return@synchronized
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
                for (callback in synchronized(callbacks) { callbacks.toList() }) {
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

    private fun updateLocked(update: DaemonProtocol.NeighbourUpdate) {
        if (update.replace) {
            neighbours = persistentMapOf<IpDev, NetlinkNeighbour>().mutate {
                for (candidate in update.neighbours) it[IpDev(candidate)] = candidate
            }
            aggregateChannel.trySend(neighbours).onFailure { throw it!! }
            return
        }
        val old = neighbours
        neighbours = old.mutate {
            for (candidate in update.neighbours) when (candidate.state) {
                NetlinkNeighbour.State.DELETING -> it.remove(IpDev(candidate))
                else -> it[IpDev(candidate)] = candidate
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
