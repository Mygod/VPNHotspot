package be.mygod.vpnhotspot

import android.app.Service
import be.mygod.vpnhotspot.net.NetlinkNeighbour
import be.mygod.vpnhotspot.net.monitor.NetlinkNeighbours
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.launch
import java.net.Inet4Address

abstract class NetlinkNeighbourMonitoringService : Service(), CoroutineScope {
    private var neighboursJob: Job? = null
    private var neighbours: Collection<NetlinkNeighbour> = emptyList()

    protected abstract val activeIfaces: List<String>
    protected open val inactiveIfaces get() = emptyList<String>()

    protected fun startNetlinkNeighbours() {
        if (neighboursJob == null) neighboursJob = launch {
            NetlinkNeighbours.snapshots.collect { onNetlinkNeighboursChanged(it) }
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
                    .filter { it.lladdr != null && it.ip is Inet4Address && it.state == NetlinkNeighbour.State.VALID }
                    .distinctBy { it.lladdr }
                    .size
        }
        ServiceNotification.startForeground(this, activeIfaces.associateWith { sizeLookup[it] ?: 0 }, inactiveIfaces,
            false)
    }
}
