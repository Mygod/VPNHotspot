package be.mygod.vpnhotspot.manage

import android.content.IntentFilter
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.net.TetheringManager
import be.mygod.vpnhotspot.util.KillableTileService
import be.mygod.vpnhotspot.util.broadcastReceiver

@RequiresApi(24)
abstract class TetherListeningTileService : KillableTileService() {
    protected var tethered: List<String> = emptyList()

    private val receiver = broadcastReceiver { _, intent ->
        tethered = TetheringManager.getTetheredIfaces(intent.extras ?: return@broadcastReceiver)
        updateTile()
    }

    override fun onStartListening() {
        super.onStartListening()
        tethered = TetheringManager.getTetheredIfaces(registerReceiver(
                receiver, IntentFilter(TetheringManager.ACTION_TETHER_STATE_CHANGED))?.extras ?: return)
    }

    override fun onStopListening() {
        unregisterReceiver(receiver)
        super.onStopListening()
    }

    protected abstract fun updateTile()
}
