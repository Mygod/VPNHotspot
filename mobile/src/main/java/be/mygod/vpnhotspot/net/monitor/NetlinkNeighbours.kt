package be.mygod.vpnhotspot.net.monitor

import be.mygod.vpnhotspot.net.IpDev
import be.mygod.vpnhotspot.net.NetlinkNeighbour
import be.mygod.vpnhotspot.root.daemon.DaemonController
import be.mygod.vpnhotspot.root.daemon.DaemonProtocol
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.collections.immutable.mutate
import kotlinx.collections.immutable.persistentMapOf
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.filterNotNull
import kotlinx.coroutines.launch
import timber.log.Timber

object NetlinkNeighbours {
    private val scope = CoroutineScope(Dispatchers.Default.limitedParallelism(1, "NetlinkNeighbours") + SupervisorJob())
    private val _snapshots = MutableStateFlow<Collection<NetlinkNeighbour>?>(null)
    val snapshots = _snapshots.filterNotNull()
    private var neighbours = persistentMapOf<IpDev, NetlinkNeighbour>()
    private var worker: Job? = null

    init {
        scope.launch {
            _snapshots.subscriptionCount.collect { count ->
                if (count == 0) {
                    worker?.cancelAndJoin()
                    worker = null
                } else if (worker?.isActive != true) worker = launchGeneration()
            }
        }
    }

    private fun launchGeneration() = scope.launch {
        try {
            DaemonController.neighbourMonitor().collect { deltas ->
                val old = neighbours
                neighbours = old.mutate {
                    for (delta in deltas) when (delta) {
                        is DaemonProtocol.NeighbourDelta.Upsert ->
                            it[IpDev(delta.neighbour.ip, delta.neighbour.dev)] = delta.neighbour
                        is DaemonProtocol.NeighbourDelta.Delete -> it.remove(IpDev(delta.ip, delta.dev))
                    }
                }
                // values do not override equals
                if (_snapshots.value == null || neighbours != old) _snapshots.value = neighbours.values
            }
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            Timber.w(e)
            SmartSnackbar.make(e).show()
        } finally {
            _snapshots.value = null
            neighbours = persistentMapOf()
        }
    }
}
