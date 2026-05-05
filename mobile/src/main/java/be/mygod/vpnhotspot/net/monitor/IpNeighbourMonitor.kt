package be.mygod.vpnhotspot.net.monitor

import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.IpDev
import be.mygod.vpnhotspot.net.IpNeighbour
import be.mygod.vpnhotspot.net.MacAddressCompat
import be.mygod.vpnhotspot.root.daemon.DaemonController
import be.mygod.vpnhotspot.root.daemon.DaemonProtocol
import kotlinx.collections.immutable.PersistentMap
import kotlinx.collections.immutable.persistentMapOf
import kotlinx.collections.immutable.toPersistentMap
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.channels.actor
import kotlinx.coroutines.channels.onFailure
import kotlinx.coroutines.launch
import timber.log.Timber

class IpNeighbourMonitor private constructor() {
    companion object {
        private val callbacks = mutableMapOf<Callback, Boolean>()
        var instance: IpNeighbourMonitor? = null
        var fullMode = false

        /**
         * @param full Whether the invalid entries should also be parsed.
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
        }?.let { GlobalScope.launch { callback.onIpNeighbourAvailable(it) } }
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
    private val worker: Job

    init {
        worker = GlobalScope.launch {
            try {
                DaemonController.startNeighbourMonitor { processUpdate(it) }
            } catch (_: CancellationException) {
            } catch (e: Exception) {
                app.logEvent("ip_monitor_failure")
                Timber.w(e)
            }
        }
        flushAsync()
    }

    private fun IpNeighbour.toMonitorMode(): IpNeighbour? {
        val state = if (!fullMode && state != IpNeighbour.State.VALID) IpNeighbour.State.DELETING else state
        if (lladdr == MacAddressCompat.ALL_ZEROS_ADDRESS && state != IpNeighbour.State.INCOMPLETE &&
                state != IpNeighbour.State.DELETING) {
            Timber.d("Missing neighbour lladdr for $ip dev $dev state $state")
            return null
        }
        return if (state == this.state) this else copy(state = state)
    }

    private fun processUpdate(update: DaemonProtocol.NeighbourUpdate) {
        if (update.replace) {
            neighbours = mutableMapOf<IpDev, IpNeighbour>().apply {
                for (candidate in update.neighbours) {
                    val neighbour = candidate.toMonitorMode() ?: continue
                    if (neighbour.state != IpNeighbour.State.DELETING) this[IpDev(neighbour)] = neighbour
                }
            }.toPersistentMap()
            aggregator.trySend(neighbours).onFailure { throw it!! }
            return
        }
        val old = neighbours
        for (candidate in update.neighbours) {
            val neighbour = candidate.toMonitorMode() ?: continue
            neighbours = when (neighbour.state) {
                IpNeighbour.State.DELETING -> neighbours.remove(IpDev(neighbour))
                else -> neighbours.put(IpDev(neighbour), neighbour)
            }
        }
        if (neighbours != old) aggregator.trySend(neighbours).onFailure { throw it!! }
    }

    suspend fun flush() {
        processUpdate(DaemonProtocol.NeighbourUpdate(true, DaemonController.dumpNeighbours()))
    }
    fun flushAsync() = GlobalScope.launch { flush() }

    fun destroy() {
        worker.cancel()
        GlobalScope.launch {
            try {
                DaemonController.stopNeighbourMonitor()
            } catch (_: CancellationException) {
            } catch (e: Exception) {
                Timber.w(e)
            }
        }
    }
}
