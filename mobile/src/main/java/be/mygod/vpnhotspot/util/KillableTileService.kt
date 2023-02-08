package be.mygod.vpnhotspot.util

import android.content.ComponentName
import android.content.Intent
import android.content.ServiceConnection
import android.os.Build
import android.os.DeadObjectException
import android.os.IBinder
import android.service.quicksettings.Tile
import android.service.quicksettings.TileService
import be.mygod.vpnhotspot.BootReceiver

abstract class KillableTileService : TileService(), ServiceConnection {
    protected var tapPending = false

    /**
     * Compat helper for setSubtitle.
     */
    protected fun Tile.subtitle(value: CharSequence?) {
        if (Build.VERSION.SDK_INT >= 29) subtitle = value
    }

    override fun onServiceConnected(name: ComponentName?, service: IBinder?) {
        if (tapPending) {
            tapPending = false
            onClick()
        }
    }

    override fun onBind(intent: Intent?) = try {
        super.onBind(intent)
    } catch (_: DeadObjectException) {
        null
    }.also { BootReceiver.startIfEnabled() }
}
