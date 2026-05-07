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
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import timber.log.Timber

class NetlinkNeighbourMonitor private constructor(private val previousDestroy: Job?) {
    companion object {
        private val callbacks = mutableSetOf<Callback>()
        private val lifecycleLock = Mutex()
        private var pendingDestroy: Job? = null
        var instance: NetlinkNeighbourMonitor? = null

        fun registerCallback(callback: Callback) {
            val monitor = synchronized(callbacks) {
                if (!callbacks.add(callback)) return@synchronized null
                instance?.also { it.flushAsync() } ?: run {
                    instance = NetlinkNeighbourMonitor(pendingDestroy)
                    null
                }
            } ?: return
            monitor.scope.launch { callback.onNetlinkNeighbourAvailable(monitor.neighbours.values) }
        }
        fun unregisterCallback(callback: Callback) = synchronized(callbacks) {
            if (!callbacks.remove(callback)) return@synchronized
            if (callbacks.isNotEmpty()) return@synchronized
            val monitor = instance
            val destroyJob = monitor?.scope?.launch {
                try {
                    lifecycleLock.withLock {
                        monitor.worker.cancelAndJoin()
                        monitor.neighbours = persistentMapOf()
                    }
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
    private var worker: Job = launchGeneration(previousDestroy)

    private fun launchGeneration(waitFor: Job? = null) = scope.launch {
        try {
            neighbours = persistentMapOf()
            waitFor?.join()
            var first = true
            DaemonController.neighbourMonitor().collect { deltas ->
                val old = neighbours
                neighbours = old.mutate {
                    for (delta in deltas) when (delta) {
                        is DaemonProtocol.NeighbourDelta.Upsert -> it[IpDev(delta.neighbour)] = delta.neighbour
                        is DaemonProtocol.NeighbourDelta.Delete -> it.remove(IpDev(delta.ip, delta.dev))
                    }
                }
                if (first || neighbours != old) aggregateChannel.trySend(neighbours).exceptionOrNull()?.let { throw it }
                first = false
            }
            neighbours = persistentMapOf()
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            Timber.w(e)
            SmartSnackbar.make(e).show()
            neighbours = persistentMapOf()
        }
    }

    fun flushAsync() = scope.launch {
        lifecycleLock.withLock {
            worker.cancelAndJoin()
            worker = launchGeneration(previousDestroy)
        }
    }
}
