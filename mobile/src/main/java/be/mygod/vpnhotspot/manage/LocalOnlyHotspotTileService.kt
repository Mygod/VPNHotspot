package be.mygod.vpnhotspot.manage

import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.graphics.drawable.Icon
import android.os.IBinder
import android.service.quicksettings.Tile
import androidx.annotation.RequiresApi
import androidx.core.content.ContextCompat
import be.mygod.vpnhotspot.LocalOnlyHotspotService
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.util.KillableTileService
import be.mygod.vpnhotspot.util.stopAndUnbind

@RequiresApi(26)
class LocalOnlyHotspotTileService : KillableTileService() {
    private val tile by lazy { Icon.createWithResource(application, R.drawable.ic_device_wifi_tethering) }

    private var binder: LocalOnlyHotspotService.Binder? = null

    override fun onStartListening() {
        super.onStartListening()
        bindService(Intent(this, LocalOnlyHotspotService::class.java), this, Context.BIND_AUTO_CREATE)
    }

    override fun onStopListening() {
        stopAndUnbind(this)
        super.onStopListening()
    }

    override fun onClick() {
        val binder = binder
        if (binder == null) tapPending = true
        else when (binder.iface) {
            null -> ContextCompat.startForegroundService(this, Intent(this, LocalOnlyHotspotService::class.java))
            "" -> { }   // STARTING, ignored
            else -> binder.stop()
        }
    }

    override fun onServiceConnected(name: ComponentName?, service: IBinder?) {
        binder = service as LocalOnlyHotspotService.Binder
        service.ifaceChanged[this] = {
            qsTile?.run {
                icon = tile
                when (it) {
                    null -> {
                        state = Tile.STATE_INACTIVE
                        label = getText(R.string.tethering_temp_hotspot)
                    }
                    "" -> {
                        state = Tile.STATE_UNAVAILABLE
                        label = getText(R.string.tethering_temp_hotspot)
                    }
                    else -> {
                        state = Tile.STATE_ACTIVE
                        label = service.configuration!!.SSID
                    }
                }
                updateTile()
            }
        }
        super.onServiceConnected(name, service)
    }

    override fun onServiceDisconnected(name: ComponentName?) {
        binder?.ifaceChanged?.remove(this)
        binder = null
    }
}
