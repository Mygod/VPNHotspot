package be.mygod.vpnhotspot

import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.graphics.drawable.Icon
import android.net.wifi.p2p.WifiP2pGroup
import android.os.IBinder
import android.service.quicksettings.Tile
import android.service.quicksettings.TileService
import android.support.annotation.RequiresApi
import android.support.v4.content.ContextCompat

@RequiresApi(24)
class RepeaterTileService : TileService(), ServiceConnection {
    private val tileOff by lazy { Icon.createWithResource(application, R.drawable.ic_quick_settings_tile_off) }
    private val tileOn by lazy { Icon.createWithResource(application, R.drawable.ic_quick_settings_tile_on) }

    private var binder: RepeaterService.Binder? = null

    override fun onStartListening() {
        super.onStartListening()
        bindService(Intent(this, RepeaterService::class.java), this, Context.BIND_AUTO_CREATE)
    }

    override fun onStopListening() {
        super.onStopListening()
        unbindService(this)
    }

    override fun onClick() {
        val binder = binder
        when (binder?.service?.status) {
            RepeaterService.Status.ACTIVE -> binder.shutdown()
            RepeaterService.Status.IDLE ->
                ContextCompat.startForegroundService(this, Intent(this, RepeaterService::class.java))
            else -> { }
        }
    }

    override fun onServiceConnected(name: ComponentName?, service: IBinder) {
        val binder = service as RepeaterService.Binder
        this.binder = binder
        binder.statusChanged[this] = { updateTile() }
        binder.groupChanged[this] = this::updateTile
    }

    override fun onServiceDisconnected(name: ComponentName?) {
        val binder = binder ?: return
        this.binder = null
        binder.statusChanged -= this
        binder.groupChanged -= this
    }

    private fun updateTile(group: WifiP2pGroup? = binder?.service?.group) {
        val qsTile = qsTile ?: return
        when (binder?.service?.status) {
            RepeaterService.Status.IDLE -> {
                qsTile.state = Tile.STATE_INACTIVE
                qsTile.icon = tileOff
                qsTile.label = getString(R.string.title_repeater)
            }
            RepeaterService.Status.STARTING -> {
                qsTile.state = Tile.STATE_UNAVAILABLE
                qsTile.icon = tileOn
                qsTile.label = getString(R.string.title_repeater)
            }
            RepeaterService.Status.ACTIVE -> {
                qsTile.state = Tile.STATE_ACTIVE
                qsTile.icon = tileOn
                qsTile.label = group?.networkName
            }
            null -> {
                qsTile.state = Tile.STATE_UNAVAILABLE
                qsTile.icon = tileOff
                qsTile.label = getString(R.string.title_repeater)
            }
        }
        qsTile.updateTile()
    }
}
