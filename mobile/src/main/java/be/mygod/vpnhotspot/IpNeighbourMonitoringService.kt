package be.mygod.vpnhotspot

import android.app.Service
import be.mygod.vpnhotspot.net.IpNeighbour
import be.mygod.vpnhotspot.net.IpNeighbourMonitor

abstract class IpNeighbourMonitoringService : Service(), IpNeighbourMonitor.Callback {
    private var neighbours = emptyList<IpNeighbour>()

    protected abstract val activeIfaces: List<String>

    override fun onIpNeighbourAvailable(neighbours: List<IpNeighbour>) {
        this.neighbours = neighbours
        updateNotification()
    }
    protected fun updateNotification() {
        val sizeLookup = neighbours.groupBy { it.dev }.mapValues { (_, neighbours) ->
            neighbours
                    .filter { it.state != IpNeighbour.State.FAILED }
                    .distinctBy { it.lladdr }
                    .size
        }
        ServiceNotification.startForeground(this, activeIfaces.associate { Pair(it, sizeLookup[it] ?: 0) })
    }
}
