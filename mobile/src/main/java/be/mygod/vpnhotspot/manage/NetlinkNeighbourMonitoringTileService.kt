package be.mygod.vpnhotspot.manage

import android.service.quicksettings.Tile
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.net.NetlinkNeighbour
import be.mygod.vpnhotspot.net.monitor.NetlinkNeighbours
import be.mygod.vpnhotspot.util.KillableTileService
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch
import java.net.Inet4Address

abstract class NetlinkNeighbourMonitoringTileService : KillableTileService() {
    private val scope = CoroutineScope(Dispatchers.Main.immediate + SupervisorJob())
    private var neighboursJob: Job? = null
    private var neighbours: Collection<NetlinkNeighbour> = emptyList()
    abstract fun updateTile()

    override fun onStartListening() {
        super.onStartListening()
        neighboursJob = scope.launch {
            NetlinkNeighbours.snapshots.collect {
                neighbours = it
                updateTile()
            }
        }
    }

    override fun onStopListening() {
        neighboursJob?.cancel()
        neighboursJob = null
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

    override fun onDestroy() {
        scope.cancel()
        super.onDestroy()
    }
}
