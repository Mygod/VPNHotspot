package be.mygod.vpnhotspot.manage

import android.content.Intent
import android.graphics.drawable.Icon
import android.service.quicksettings.Tile
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.RepeaterService
import be.mygod.vpnhotspot.util.KillableTileService
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.bindServiceFlow
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.flow.collectLatest
import kotlinx.coroutines.flow.merge
import kotlinx.coroutines.launch

class RepeaterTileService : KillableTileService() {
    private val tile by lazy { Icon.createWithResource(application, R.drawable.ic_router) }

    private val scope = CoroutineScope(Dispatchers.Main.immediate + SupervisorJob())
    private var binder: RepeaterService.Binder? = null
    private var serviceJob: Job? = null

    override fun onStartListening() {
        super.onStartListening()
        if (Services.p2p == null) {
            updateTile()
            return
        }
        serviceJob = scope.launch {
            try {
                bindServiceFlow(Intent(this@RepeaterTileService, RepeaterService::class.java)).collectLatest { service ->
                    val binder = service as RepeaterService.Binder?
                    this@RepeaterTileService.binder = binder
                    if (binder == null) return@collectLatest
                    resolveTapPending()
                    merge(binder.active, binder.group).collect { updateTile() }
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

    override fun onClick() {
        val binder = binder
        if (binder == null) tapPending = true else {
            RepeaterService.dismissHandle = dismissHandle
            if (binder.active.value) {
                binder.shutdown()
            } else startForegroundServiceCompat(Intent(this, RepeaterService::class.java))
        }
    }

    override fun onDestroy() {
        scope.cancel()
        super.onDestroy()
    }

    private fun updateTile() {
        val binder = binder ?: return
        qsTile?.run {
            subtitle = null
            if (binder.active.value) {
                state = Tile.STATE_ACTIVE
                val group = binder.group.value
                label = group?.networkName
                val size = group?.clientList?.size ?: 0
                if (size > 0) subtitle = resources.getQuantityString(
                    R.plurals.quick_settings_hotspot_secondary_label_num_devices, size, size)
            } else {
                state = Tile.STATE_INACTIVE
                label = getText(R.string.title_repeater)
            }
            icon = tile
            updateTile()
        }
    }
}
