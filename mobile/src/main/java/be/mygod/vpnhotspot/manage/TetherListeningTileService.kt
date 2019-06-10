package be.mygod.vpnhotspot.manage

import android.content.IntentFilter
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.net.TetheringManager
import be.mygod.vpnhotspot.net.TetheringManager.tetheredIfaces
import be.mygod.vpnhotspot.util.KillableTileService
import be.mygod.vpnhotspot.util.broadcastReceiver

@RequiresApi(24)
abstract class TetherListeningTileService : KillableTileService() {
    protected var tethered: List<String>? = null

    private val receiver = broadcastReceiver { _, intent ->
        tethered = intent.tetheredIfaces ?: return@broadcastReceiver
        updateTile()
    }

    override fun onStartListening() {
        super.onStartListening()
        tethered = registerReceiver(receiver, IntentFilter(TetheringManager.ACTION_TETHER_STATE_CHANGED))
                ?.tetheredIfaces
        updateTile()
    }

    override fun onStopListening() {
        unregisterReceiver(receiver)
        super.onStopListening()
    }

    protected abstract fun updateTile()
}
