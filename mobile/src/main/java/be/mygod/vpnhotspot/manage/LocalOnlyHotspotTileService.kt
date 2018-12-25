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
import be.mygod.vpnhotspot.util.stopAndUnbind

@RequiresApi(26)
class LocalOnlyHotspotTileService : TetherListeningTileService() {
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
        when {
            binder == null -> tapPending = true
            binder.iface != null -> binder.stop()
            else -> ContextCompat.startForegroundService(this, Intent(this, LocalOnlyHotspotService::class.java))
        }
    }

    override fun onServiceConnected(name: ComponentName?, service: IBinder?) {
        binder = service as LocalOnlyHotspotService.Binder
        updateTile()
        super.onServiceConnected(name, service)
    }

    override fun onServiceDisconnected(name: ComponentName?) {
        binder = null
    }

    override fun updateTile() {
        qsTile?.run {
            state = if (binder?.iface == null) Tile.STATE_INACTIVE else Tile.STATE_ACTIVE
            icon = tile
            label = getText(R.string.tethering_temp_hotspot)
            updateTile()
        }
    }
}
