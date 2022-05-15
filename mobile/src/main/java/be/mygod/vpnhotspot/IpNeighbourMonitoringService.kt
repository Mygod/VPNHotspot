package be.mygod.vpnhotspot

import android.app.Service
import be.mygod.vpnhotspot.net.IpNeighbour
import be.mygod.vpnhotspot.net.monitor.IpNeighbourMonitor
import java.net.Inet4Address

abstract class IpNeighbourMonitoringService : Service(), IpNeighbourMonitor.Callback {
    private var neighbours: Collection<IpNeighbour> = emptyList()

    protected abstract val activeIfaces: List<String>
    protected open val inactiveIfaces get() = emptyList<String>()

    override fun onIpNeighbourAvailable(neighbours: Collection<IpNeighbour>) {
        this.neighbours = neighbours
        updateNotification()
    }
    protected open fun updateNotification() {
        val sizeLookup = neighbours.groupBy { it.dev }.mapValues { (_, neighbours) ->
            neighbours
                    .filter { it.ip is Inet4Address && it.state == IpNeighbour.State.VALID }
                    .distinctBy { it.lladdr }
                    .size
        }
        ServiceNotification.startForeground(this, activeIfaces.associateWith { sizeLookup[it] ?: 0 }, inactiveIfaces)
    }
}
