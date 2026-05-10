package be.mygod.vpnhotspot

import android.app.Service
import be.mygod.vpnhotspot.net.NetlinkNeighbour
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.launch

abstract class NetlinkNeighbourMonitoringService : Service(), CoroutineScope {
    private var neighboursJob: Job? = null
    private var neighbours: Collection<NetlinkNeighbour> = emptyList()

    protected abstract val activeIfaces: List<String>
    protected open val inactiveIfaces get() = emptyList<String>()

    protected fun startNetlinkNeighbours() {
        if (neighboursJob == null) neighboursJob = launch {
            NetlinkNeighbour.snapshots.collect { onNetlinkNeighboursChanged(it) }
        }
    }
    protected fun stopNetlinkNeighbours() {
        neighboursJob?.cancel()
        neighboursJob = null
        neighbours = emptyList()
    }

    protected open fun onNetlinkNeighboursChanged(neighbours: Collection<NetlinkNeighbour>) {
        this.neighbours = neighbours
        updateNotification()
    }
    protected open fun updateNotification() {
        val sizeLookup = neighbours.groupBy { it.dev }.mapValues { (_, neighbours) ->
            neighbours
                    .mapNotNull { it.validIpv4ClientMac }
                    .distinct()
                    .size
        }
        ServiceNotification.startForeground(this, activeIfaces.associateWith { sizeLookup[it] ?: 0 }, inactiveIfaces,
            false)
    }
}
