package be.mygod.vpnhotspot.manage

import android.service.quicksettings.Tile
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.net.NetlinkNeighbour
import be.mygod.vpnhotspot.net.monitor.NetlinkNeighbourMonitor
import be.mygod.vpnhotspot.util.KillableTileService
import java.net.Inet4Address

abstract class NetlinkNeighbourMonitoringTileService : KillableTileService(), NetlinkNeighbourMonitor.Callback {
    private var neighbours: Collection<NetlinkNeighbour> = emptyList()
    abstract fun updateTile()

    override fun onStartListening() {
        super.onStartListening()
        NetlinkNeighbourMonitor.registerCallback(this)
    }

    override fun onStopListening() {
        NetlinkNeighbourMonitor.unregisterCallback(this)
        super.onStopListening()
    }

    protected fun Tile.subtitleDevices(filter: (String) -> Boolean) {
        val size = neighbours
                .filter { it.lladdr != null && it.ip is Inet4Address && it.state == NetlinkNeighbour.State.VALID &&
                        filter(it.dev) }
                .distinctBy { it.lladdr }
                .size
        if (size > 0) subtitle = resources.getQuantityString(
                R.plurals.quick_settings_hotspot_secondary_label_num_devices, size, size)
    }

    override fun onNetlinkNeighbourAvailable(neighbours: Collection<NetlinkNeighbour>) {
        this.neighbours = neighbours
        updateTile()
    }
}
