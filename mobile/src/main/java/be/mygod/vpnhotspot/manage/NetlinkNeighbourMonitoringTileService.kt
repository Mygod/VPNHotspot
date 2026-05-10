package be.mygod.vpnhotspot.manage

import android.service.quicksettings.Tile
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.net.netlinkNeighbours
import be.mygod.vpnhotspot.net.validIpv4ClientMac
import be.mygod.vpnhotspot.root.daemon.DaemonProto
import be.mygod.vpnhotspot.util.KillableTileService
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch

abstract class NetlinkNeighbourMonitoringTileService : KillableTileService() {
    private val scope = CoroutineScope(Dispatchers.Main.immediate + SupervisorJob())
    private var neighboursJob: Job? = null
    private var neighbours: Collection<DaemonProto.Neighbour> = emptyList()
    abstract fun updateTile()

    override fun onStartListening() {
        super.onStartListening()
        neighboursJob = scope.launch {
            netlinkNeighbours.collect {
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
                .mapNotNull { if (filter(it.dev)) it.validIpv4ClientMac() else null }
                .distinct()
                .size
        if (size > 0) subtitle = resources.getQuantityString(
                R.plurals.quick_settings_hotspot_secondary_label_num_devices, size, size)
    }

    override fun onDestroy() {
        scope.cancel()
        super.onDestroy()
    }
}
