package be.mygod.vpnhotspot.manage

import android.content.ComponentName
import android.content.Intent
import android.graphics.drawable.Icon
import android.os.IBinder
import android.service.quicksettings.Tile
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.RepeaterService
import be.mygod.vpnhotspot.util.KillableTileService
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.stopAndUnbind
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.flow.merge
import kotlinx.coroutines.launch

class RepeaterTileService : KillableTileService() {
    private val tile by lazy { Icon.createWithResource(application, R.drawable.ic_action_settings_input_antenna) }

    private val scope = CoroutineScope(Dispatchers.Main.immediate + SupervisorJob())
    private var binder: RepeaterService.Binder? = null
    private var serviceJob: Job? = null

    override fun onStartListening() {
        super.onStartListening()
        if (Services.p2p != null) {
            bindService(Intent(this, RepeaterService::class.java), this, BIND_AUTO_CREATE)
        } else updateTile()
    }

    override fun onStopListening() {
        if (Services.p2p != null) stopAndUnbind(this)
        super.onStopListening()
    }

    override fun onClick() {
        val binder = binder
        if (binder == null) tapPending = true else when (binder.status.value) {
            RepeaterService.Status.ACTIVE -> {
                RepeaterService.dismissHandle = dismissHandle
                binder.shutdown()
            }
            RepeaterService.Status.IDLE -> {
                RepeaterService.dismissHandle = dismissHandle
                startForegroundServiceCompat(Intent(this, RepeaterService::class.java))
            }
            else -> { }
        }
    }

    override fun onServiceConnected(name: ComponentName?, service: IBinder?) {
        binder = service as RepeaterService.Binder
        serviceJob = scope.launch(start = CoroutineStart.UNDISPATCHED) {
            merge(service.status, service.group).collect { updateTile() }
        }
        super.onServiceConnected(name, service)
    }

    override fun onServiceDisconnected(name: ComponentName?) {
        if (binder == null) return
        this.binder = null
        serviceJob?.cancel()
        serviceJob = null
    }

    override fun onDestroy() {
        scope.cancel()
        super.onDestroy()
    }

    private fun updateTile() {
        val binder = binder ?: return
        qsTile?.run {
            subtitle = null
            when (binder.status.value) {
                RepeaterService.Status.IDLE -> {
                    state = Tile.STATE_INACTIVE
                    label = getText(R.string.title_repeater)
                }
                RepeaterService.Status.ACTIVE -> {
                    state = Tile.STATE_ACTIVE
                    val group = binder.group.value
                    label = group?.networkName
                    val size = group?.clientList?.size ?: 0
                    if (size > 0) subtitle = resources.getQuantityString(
                            R.plurals.quick_settings_hotspot_secondary_label_num_devices, size, size)
                }
                else -> {   // STARTING, STOPPING, or DESTROYED, which should never occur
                    state = Tile.STATE_UNAVAILABLE
                    label = getText(R.string.title_repeater)
                }
            }
            icon = tile
            updateTile()
        }
    }
}
