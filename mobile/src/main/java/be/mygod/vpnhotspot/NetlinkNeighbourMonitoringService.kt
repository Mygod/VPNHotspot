package be.mygod.vpnhotspot

import android.app.Service
import be.mygod.vpnhotspot.net.NetlinkNeighbour
import be.mygod.vpnhotspot.net.monitor.NetlinkNeighbourMonitor
import java.net.Inet4Address

abstract class NetlinkNeighbourMonitoringService : Service(), NetlinkNeighbourMonitor.Callback {
    private var neighbours: Collection<NetlinkNeighbour> = emptyList()

    protected abstract val activeIfaces: List<String>
    protected open val inactiveIfaces get() = emptyList<String>()

    override fun onNetlinkNeighbourAvailable(neighbours: Collection<NetlinkNeighbour>) {
        this.neighbours = neighbours
        updateNotification()
    }
    protected open fun updateNotification() {
        val sizeLookup = neighbours.groupBy { it.dev }.mapValues { (_, neighbours) ->
            neighbours
                    .filter { it.ip is Inet4Address && it.state == NetlinkNeighbour.State.VALID }
                    .distinctBy { it.lladdr }
                    .size
        }
        ServiceNotification.startForeground(this, activeIfaces.associateWith { sizeLookup[it] ?: 0 }, inactiveIfaces,
            false)
    }
}
