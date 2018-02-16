package be.mygod.vpnhotspot

import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.graphics.drawable.Icon
import android.os.Build
import android.os.IBinder
import android.service.quicksettings.Tile
import android.support.annotation.RequiresApi
import android.support.v4.content.ContextCompat
import android.support.v4.content.LocalBroadcastManager
import android.service.quicksettings.TileService as BaseTileService

@RequiresApi(Build.VERSION_CODES.N)
class TileService : BaseTileService(), ServiceConnection {
    private var binder: RepeaterService.RepeaterBinder? = null

    val onStatusChangedReceive = broadcastReceiver { _, _ ->
        updateTile()
    }

    var qsTileState: Int
        get() = qsTile.state
        set(value) {
            qsTile.state = value
            when (value) {
                Tile.STATE_ACTIVE -> {
                    qsTile.icon = Icon.createWithResource(application,
                            R.drawable.ic_quick_settings_tile_on)
                    qsTile.label = "${getString(R.string.repeater_password)}:\n${binder?.service?.password}"
                }
                Tile.STATE_INACTIVE -> {
                    qsTile.icon = Icon.createWithResource(application,
                            R.drawable.ic_quick_settings_tile_off)
                    qsTile.label = getString(R.string.app_name)
                }
            }
            qsTile.updateTile()
        }

    override fun onStartListening() {
        super.onStartListening()
        bindService(Intent(this, RepeaterService::class.java), this,
                Context.BIND_AUTO_CREATE)
    }

    override fun onStopListening() {
        super.onStopListening()
        unbindService(this)
    }

    override fun onClick() {
        val binder = binder
        when (binder?.service?.status) {
            RepeaterService.Status.ACTIVE -> binder.shutdown()
            RepeaterService.Status.IDLE -> ContextCompat.startForegroundService(this,
                    Intent(this, RepeaterService::class.java))
            else -> {
            }
        }
    }

    override fun onServiceConnected(name: ComponentName?, service: IBinder) {
        binder = service as RepeaterService.RepeaterBinder
        updateTile()
        LocalBroadcastManager.getInstance(this).registerReceiver(onStatusChangedReceive,
                intentFilter(RepeaterService.ACTION_STATUS_CHANGED))
    }

    override fun onServiceDisconnected(name: ComponentName?) {
        binder = null
        LocalBroadcastManager.getInstance(this).unregisterReceiver(onStatusChangedReceive)
    }

    fun updateTile() {
        qsTileState = when (binder?.service?.status) {
            RepeaterService.Status.ACTIVE -> Tile.STATE_ACTIVE
            RepeaterService.Status.IDLE -> Tile.STATE_INACTIVE
            else -> Tile.STATE_UNAVAILABLE
        }
    }
}