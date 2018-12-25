package be.mygod.vpnhotspot.manage

import android.content.IntentFilter
import android.service.quicksettings.TileService
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.net.TetheringManager
import be.mygod.vpnhotspot.util.broadcastReceiver

@RequiresApi(24)
abstract class TetherListeningTileService : TileService() {
    protected var tethered: List<String> = emptyList()

    private val receiver = broadcastReceiver { _, intent ->
        tethered = TetheringManager.getTetheredIfaces(intent.extras ?: return@broadcastReceiver)
        updateTile()
    }

    override fun onStartListening() {
        super.onStartListening()
        registerReceiver(receiver, IntentFilter(TetheringManager.ACTION_TETHER_STATE_CHANGED))
    }

    override fun onStopListening() {
        unregisterReceiver(receiver)
        super.onStopListening()
    }

    protected abstract fun updateTile()
}
