package be.mygod.vpnhotspot

import android.app.Service
import be.mygod.vpnhotspot.net.netlinkNeighbours
import be.mygod.vpnhotspot.net.validIpv4ClientMac
import be.mygod.vpnhotspot.root.daemon.DaemonProto
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.launch

abstract class NetlinkNeighbourMonitoringService : Service(), CoroutineScope {
    private var neighboursJob: Job? = null
    private var neighbours: Collection<DaemonProto.Neighbour> = emptyList()

    protected abstract val activeIfaces: List<String>
    protected open val inactiveIfaces get() = emptyList<String>()

    protected fun startNetlinkNeighbours() {
        if (neighboursJob == null) neighboursJob = launch {
            netlinkNeighbours.collect { onNetlinkNeighboursChanged(it) }
        }
    }
    protected fun stopNetlinkNeighbours() {
        neighboursJob?.cancel()
        neighboursJob = null
        neighbours = emptyList()
    }

    protected open fun onNetlinkNeighboursChanged(neighbours: Collection<DaemonProto.Neighbour>) {
        this.neighbours = neighbours
        updateNotification()
    }
    protected open fun updateNotification() {
        val sizeLookup = neighbours.groupBy { it.`interface` }.mapValues { (_, neighbours) ->
            neighbours
                    .mapNotNull { it.validIpv4ClientMac() }
                    .distinct()
                    .size
        }
        ServiceNotification.startForeground(this, activeIfaces.associateWith { sizeLookup[it] ?: 0 }, inactiveIfaces,
            false)
    }
}
