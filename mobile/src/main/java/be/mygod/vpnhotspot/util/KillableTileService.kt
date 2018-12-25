package be.mygod.vpnhotspot.util

import android.content.ComponentName
import android.content.ServiceConnection
import android.os.IBinder
import android.service.quicksettings.TileService
import androidx.annotation.RequiresApi

@RequiresApi(24)
abstract class KillableTileService : TileService(), ServiceConnection {
    protected var tapPending = false

    override fun onServiceConnected(name: ComponentName?, service: IBinder?) {
        if (tapPending) {
            tapPending = false
            onClick()
        }
    }
}
