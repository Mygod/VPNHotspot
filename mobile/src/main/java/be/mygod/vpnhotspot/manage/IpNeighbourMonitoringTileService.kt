package be.mygod.vpnhotspot.manage

import android.service.quicksettings.Tile
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.net.IpNeighbour
import be.mygod.vpnhotspot.net.monitor.IpNeighbourMonitor
import be.mygod.vpnhotspot.util.KillableTileService
import java.net.Inet4Address

abstract class IpNeighbourMonitoringTileService : KillableTileService(), IpNeighbourMonitor.Callback {
    private var neighbours: Collection<IpNeighbour> = emptyList()
    abstract fun updateTile()

    override fun onStartListening() {
        super.onStartListening()
        IpNeighbourMonitor.registerCallback(this)
    }

    override fun onStopListening() {
        IpNeighbourMonitor.unregisterCallback(this)
        super.onStopListening()
    }

    protected fun Tile.subtitleDevices(filter: (String) -> Boolean) {
        val size = neighbours
                .filter { it.ip is Inet4Address && it.state == IpNeighbour.State.VALID && filter(it.dev) }
                .distinctBy { it.lladdr }
                .size
        if (size > 0) subtitle(resources.getQuantityString(
                R.plurals.quick_settings_hotspot_secondary_label_num_devices, size, size))
    }

    override fun onIpNeighbourAvailable(neighbours: Collection<IpNeighbour>) {
        this.neighbours = neighbours
        updateTile()
    }
}
