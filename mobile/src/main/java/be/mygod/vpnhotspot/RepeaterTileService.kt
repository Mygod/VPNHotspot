package be.mygod.vpnhotspot

import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.graphics.drawable.Icon
import android.os.IBinder
import android.service.quicksettings.Tile
import android.service.quicksettings.TileService
import android.support.annotation.RequiresApi
import android.support.v4.content.ContextCompat
import android.support.v4.content.LocalBroadcastManager

@RequiresApi(24)
class RepeaterTileService : TileService(), ServiceConnection {
    private val statusListener = broadcastReceiver { _, _ -> updateTile() }
    private val tileOff by lazy { Icon.createWithResource(application, R.drawable.ic_quick_settings_tile_off) }
    private val tileOn by lazy { Icon.createWithResource(application, R.drawable.ic_quick_settings_tile_on) }

    private var binder: RepeaterService.RepeaterBinder? = null

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
        binder = service as RepeaterService.RepeaterBinder
        updateTile()
        LocalBroadcastManager.getInstance(this).registerReceiver(statusListener,
                intentFilter(RepeaterService.ACTION_STATUS_CHANGED))
    }

    override fun onServiceDisconnected(name: ComponentName?) {
        binder = null
        LocalBroadcastManager.getInstance(this).unregisterReceiver(statusListener)
    }

    private fun updateTile() {
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
                qsTile.label = binder?.ssid
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
