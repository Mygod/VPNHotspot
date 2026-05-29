package be.mygod.vpnhotspot.manage

import android.bluetooth.BluetoothManager
import android.content.ComponentName
import android.content.Intent
import android.graphics.drawable.Icon
import android.net.TetheringManager
import android.os.Build
import android.os.IBinder
import android.service.quicksettings.Tile
import android.widget.Toast
import androidx.annotation.RequiresApi
import androidx.core.content.getSystemService
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.TetheringService
import be.mygod.vpnhotspot.net.TetherStates
import be.mygod.vpnhotspot.net.TetherType
import be.mygod.vpnhotspot.net.TetheringManagerCompat
import be.mygod.vpnhotspot.ui.tetherErrorLabel
import be.mygod.vpnhotspot.util.readableMessage
import be.mygod.vpnhotspot.util.stopAndUnbind
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.Job
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.launch
import timber.log.Timber

sealed class TetheringTileService : NetlinkNeighbourMonitoringTileService() {
    protected val tileOff by lazy { Icon.createWithResource(application, icon) }
    protected val tileOn by lazy { Icon.createWithResource(application, R.drawable.ic_quick_settings_tile_on) }

    protected abstract val labelString: Int
    protected abstract val tetherType: TetherType
    protected open val icon get() = tetherType.icon
    private var tethered: List<String>? = null
    protected val interested get() = tethered?.filter { TetherType.ofInterface(it).isA(tetherType) }
    protected var binder: TetheringService.Binder? = null
    private var listeningJob: Job? = null
    private var serviceJob: Job? = null

    protected abstract fun start()
    protected abstract fun stop()

    override fun onStartListening() {
        super.onStartListening()
        bindService(Intent(this, TetheringService::class.java), this, BIND_AUTO_CREATE)
        listeningJob = scope.launch {
            if (Build.VERSION.SDK_INT >= 30) launch(start = CoroutineStart.UNDISPATCHED) {
                TetherType.changes.collect { updateTile() }
            }
            TetherStates.flow.map { it.tethered }.distinctUntilChanged().collect { ifaces ->
                tethered = ifaces.toList()
                updateTile()
                if (tapPending && binder != null) {
                    tapPending = false
                    onClick()
                }
            }
        }
    }

    override fun onStopListening() {
        listeningJob?.cancel()
        listeningJob = null
        stopAndUnbind(this)
        super.onStopListening()
    }

    override fun onServiceConnected(name: ComponentName?, service: IBinder?) {
        binder = service as TetheringService.Binder
        serviceJob = scope.launch(start = CoroutineStart.UNDISPATCHED) {
            service.managedIfaces.collect { updateTile() }
        }
        super.onServiceConnected(name, service)
    }

    override fun onServiceDisconnected(name: ComponentName?) {
        serviceJob?.cancel()
        serviceJob = null
        binder = null
    }

    override fun updateTile() {
        qsTile?.run {
            subtitle = null
            val interested = interested
            when {
                interested == null -> {
                    label = getText(labelString)
                    updateTile()
                    return
                }
                interested.isEmpty() -> {
                    state = Tile.STATE_INACTIVE
                    icon = tileOff
                }
                else -> {
                    val binder = binder ?: return
                    state = Tile.STATE_ACTIVE
                    icon = if (interested.all(binder::isActive)) tileOn else tileOff
                    subtitleDevices(interested::contains)
                }
            }
            label = getText(labelString)
            updateTile()
        }
    }

    override fun onClick() {
        val interested = interested ?: run {
            tapPending = true
            return
        }
        if (interested.isEmpty()) start() else {
            val binder = binder
            if (binder == null) tapPending = true else {
                val inactive = interested.filterNot(binder::isActive)
                if (inactive.isEmpty()) stop() else {
                    TetheringService.dismissHandle = dismissHandle
                    startForegroundServiceCompat(Intent(this, TetheringService::class.java)
                        .putExtra(TetheringService.EXTRA_ADD_INTERFACES, inactive.toTypedArray()))
                }
            }
        }
    }

    /**
     * Run a tether start/stop on [scope] and reflect the outcome on the tile: refresh on success,
     * toast + dismiss on a [TetheringManagerCompat.Failure] error code, and toast the message for any
     * other exception (mirrors the old onTetheringFailed/onStopTetheringFailed/onException callbacks).
     */
    protected fun launchTile(failureLog: String, block: suspend () -> Unit) = scope.launch {
        try {
            block()
            updateTile()
        } catch (e: CancellationException) {
            throw e
        } catch (e: TetheringManagerCompat.Failure) {
            Timber.d("$failureLog: ${e.errorCode}")
            e.errorCode?.let {
                dismiss()
                Toast.makeText(this@TetheringTileService, tetherErrorLabel(this@TetheringTileService, it),
                    Toast.LENGTH_LONG).show()
            }
            updateTile()
        } catch (e: Exception) {
            TetheringManagerCompat.reportException(e)
            dismiss()
            Toast.makeText(this@TetheringTileService, e.readableMessage, Toast.LENGTH_LONG).show()
        }
    }.run { }
    protected fun launchStart(type: Int) = launchTile("onTetheringFailed") {
        TetheringManagerCompat.startTethering(type, true)
    }
    protected fun launchStop(type: Int) = launchTile("onStopTetheringFailed") {
        TetheringManagerCompat.stopTethering(type)
    }

    class Wifi : TetheringTileService() {
        override val labelString get() = R.string.tethering_manage_wifi
        override val tetherType get() = TetherType.WIFI
        override val icon get() = R.drawable.ic_wifi_tethering

        override fun start() = launchStart(TetheringManager.TETHERING_WIFI)
        override fun stop() = launchStop(TetheringManager.TETHERING_WIFI)
    }
    class Usb : TetheringTileService() {
        override val labelString get() = R.string.tethering_manage_usb
        override val tetherType get() = TetherType.USB

        override fun start() = launchStart(TetheringManagerCompat.TETHERING_USB)
        override fun stop() = launchStop(TetheringManagerCompat.TETHERING_USB)
    }
    class Bluetooth : TetheringTileService() {
        private var tethering: BluetoothTethering? = null

        override val labelString get() = R.string.tethering_manage_bluetooth
        override val tetherType get() = TetherType.BLUETOOTH

        override fun start() = launchTile("onTetheringFailed") { tethering!!.start(this@Bluetooth) }
        override fun stop() = launchTile("onStopTetheringFailed") { tethering!!.stop() }

        override fun onStartListening() {
            tethering = getSystemService<BluetoothManager>()?.adapter?.let {
                BluetoothTethering(it) { updateTile() }
            }
            super.onStartListening()
        }
        override fun onStopListening() {
            super.onStopListening()
            tethering?.close()
            tethering = null
        }

        override fun updateTile() {
            qsTile?.run {
                subtitle = null
                val interested = interested
                if (interested == null) {
                    state = Tile.STATE_UNAVAILABLE
                    icon = tileOff
                } else when (tethering?.active) {
                    true -> {
                        val binder = binder ?: return
                        state = Tile.STATE_ACTIVE
                        icon = if (interested.isNotEmpty() && interested.all(binder::isActive)) tileOn else tileOff
                        subtitleDevices(interested::contains)
                    }
                    false -> {
                        state = Tile.STATE_INACTIVE
                        icon = tileOff
                    }
                    null -> {
                        state = Tile.STATE_INACTIVE
                        icon = tileOff
                        subtitle = tethering?.activeFailureCause?.readableMessage
                    }
                }
                label = getText(labelString)
                updateTile()
            }
        }

        override fun onClick() {
            val tethering = tethering
            if (tethering == null) tapPending = true else when (tethering.active) {
                true -> {
                    val binder = binder
                    if (binder == null) tapPending = true else {
                        val inactive = (interested ?: return).filterNot(binder::isActive)
                        if (inactive.isEmpty()) stop() else {
                            TetheringService.dismissHandle = dismissHandle
                            startForegroundServiceCompat(Intent(this, TetheringService::class.java)
                                .putExtra(TetheringService.EXTRA_ADD_INTERFACES, inactive.toTypedArray()))
                        }
                    }
                }
                false -> start()
                else -> ManageBar.start(this::runActivity)
            }
        }
    }
    @RequiresApi(30)
    class Ethernet : TetheringTileService() {
        override val labelString get() = R.string.tethering_manage_ethernet
        override val tetherType get() = TetherType.ETHERNET

        override fun start() = launchStart(TetheringManagerCompat.TETHERING_ETHERNET)
        override fun stop() = launchStop(TetheringManagerCompat.TETHERING_ETHERNET)
    }
}
