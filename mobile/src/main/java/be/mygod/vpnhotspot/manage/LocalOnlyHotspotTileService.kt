package be.mygod.vpnhotspot.manage

import android.content.Intent
import android.graphics.drawable.Icon
import android.service.quicksettings.Tile
import be.mygod.vpnhotspot.LocalOnlyHotspotService
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.util.bindServiceFlow
import kotlinx.coroutines.Job
import kotlinx.coroutines.flow.collectLatest
import kotlinx.coroutines.flow.merge
import kotlinx.coroutines.launch

class LocalOnlyHotspotTileService : NetlinkNeighbourMonitoringTileService() {
    private val tile by lazy { Icon.createWithResource(application, R.drawable.ic_android_wifi_3_bar_plus) }

    private var binder: LocalOnlyHotspotService.Binder? = null
    private var serviceJob: Job? = null

    override fun onStartListening() {
        super.onStartListening()
        serviceJob = scope.launch {
            try {
                bindServiceFlow(Intent(this@LocalOnlyHotspotTileService, LocalOnlyHotspotService::class.java))
                    .collectLatest { service ->
                        val binder = service as LocalOnlyHotspotService.Binder?
                        this@LocalOnlyHotspotTileService.binder = binder
                        if (binder == null) return@collectLatest
                        resolveTapPending()
                        merge(binder.iface, binder.configuration).collect { updateTile() }
                    }
            } finally {
                // explicit unbind does not trigger onServiceDisconnected, so clear the stale binder ourselves
                binder = null
            }
        }
    }

    override fun onStopListening() {
        serviceJob?.cancel()
        serviceJob = null
        super.onStopListening()
    }

    override fun updateTile() {
        val binder = binder ?: return
        qsTile?.run {
            icon = tile
            subtitle = null
            val iface = binder.iface.value
            if (iface.isNullOrEmpty()) {
                state = Tile.STATE_INACTIVE
                label = getText(R.string.tethering_temp_hotspot)
            } else {
                state = Tile.STATE_ACTIVE
                label = binder.configuration.value?.ssid?.toString() ?: getText(R.string.tethering_temp_hotspot)
                subtitleDevices { it == iface }
            }
            updateTile()
        }
    }

    override fun onClick() {
        val binder = binder
        when {
            binder == null -> tapPending = true
            binder.iface.value == null -> {
                LocalOnlyHotspotService.dismissHandle = dismissHandle
                startForegroundServiceCompat(Intent(this, LocalOnlyHotspotService::class.java))
            }
            else -> binder.stop()
        }
    }
}
