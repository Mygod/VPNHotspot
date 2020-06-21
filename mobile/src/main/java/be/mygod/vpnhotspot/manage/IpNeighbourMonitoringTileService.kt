package be.mygod.vpnhotspot.manage

import android.service.quicksettings.Tile
import androidx.annotation.CallSuper
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.net.IpNeighbour
import be.mygod.vpnhotspot.net.monitor.IpNeighbourMonitor
import be.mygod.vpnhotspot.util.KillableTileService
import java.net.Inet4Address

@RequiresApi(24)
abstract class IpNeighbourMonitoringTileService : KillableTileService(), IpNeighbourMonitor.Callback {
    private var neighbours: Collection<IpNeighbour> = emptyList()
    private var canRegister = false
    abstract fun updateTile()

    @CallSuper
    override fun onStartListening() {
        super.onStartListening()
        synchronized(this) { canRegister = true }
    }

    /**
     * Lazily start [IpNeighbourMonitor], which could invoke root.
     */
    protected fun listenForClients() = synchronized(this) {
        if (canRegister) IpNeighbourMonitor.registerCallback(this)
    }

    @CallSuper
    override fun onStopListening() {
        synchronized(this) {
            canRegister = false
            IpNeighbourMonitor.unregisterCallback(this)
        }
        super.onStopListening()
    }

    protected fun Tile.subtitleDevices(filter: (String) -> Boolean) {
        val size = neighbours
                .filter { it.ip is Inet4Address && it.state != IpNeighbour.State.FAILED && filter(it.dev) }
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
