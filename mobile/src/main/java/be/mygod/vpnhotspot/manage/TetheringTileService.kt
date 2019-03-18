package be.mygod.vpnhotspot.manage

import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.graphics.drawable.Icon
import android.os.IBinder
import android.service.quicksettings.Tile
import android.widget.Toast
import androidx.annotation.RequiresApi
import androidx.core.content.ContextCompat
import be.mygod.vpnhotspot.DebugHelper
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.TetheringService
import be.mygod.vpnhotspot.net.TetherType
import be.mygod.vpnhotspot.net.TetheringManager
import be.mygod.vpnhotspot.net.wifi.WifiApManager
import be.mygod.vpnhotspot.util.readableMessage
import be.mygod.vpnhotspot.util.stopAndUnbind
import timber.log.Timber
import java.io.IOException
import java.lang.reflect.InvocationTargetException

@RequiresApi(24)
sealed class TetheringTileService : TetherListeningTileService(), TetheringManager.OnStartTetheringCallback {
    protected val tileOff by lazy { Icon.createWithResource(application, icon) }
    protected val tileOn by lazy { Icon.createWithResource(application, R.drawable.ic_quick_settings_tile_on) }

    protected abstract val labelString: Int
    protected abstract val tetherType: TetherType
    protected open val icon get() = tetherType.icon
    protected val interested get() = tethered.filter { TetherType.ofInterface(it) == tetherType }
    protected var binder: TetheringService.Binder? = null

    protected abstract fun start()
    protected abstract fun stop()

    override fun onStartListening() {
        bindService(Intent(this, TetheringService::class.java), this, Context.BIND_AUTO_CREATE)
        super.onStartListening()
    }

    override fun onStopListening() {
        super.onStopListening()
        stopAndUnbind(this)
    }

    override fun onServiceConnected(name: ComponentName?, service: IBinder?) {
        binder = service as TetheringService.Binder
        service.routingsChanged[this] = { updateTile() }
        super.onServiceConnected(name, service)
    }

    override fun onServiceDisconnected(name: ComponentName?) {
        binder = null
    }

    override fun updateTile() {
        qsTile?.run {
            val interested = interested
            if (interested.isEmpty()) {
                state = Tile.STATE_INACTIVE
                icon = tileOff
            } else {
                val binder = binder ?: return
                state = Tile.STATE_ACTIVE
                icon = if (interested.all(binder::isActive)) tileOn else tileOff
            }
            label = getText(labelString)
            updateTile()
        }
    }

    protected inline fun safeInvoker(func: () -> Unit) = try {
        func()
    } catch (e: IOException) {
        Timber.w(e)
        Toast.makeText(this, e.readableMessage, Toast.LENGTH_LONG).show()
    } catch (e: InvocationTargetException) {
        if (e.targetException !is SecurityException) Timber.w(e)
        var cause: Throwable? = e
        while (cause != null) {
            cause = cause.cause
            if (cause != null && cause !is InvocationTargetException) {
                Toast.makeText(this, cause.readableMessage, Toast.LENGTH_LONG).show()
                break
            }
        }
    }
    override fun onClick() {
        val interested = interested
        if (interested.isEmpty()) safeInvoker { start() } else {
            val binder = binder
            if (binder == null) tapPending = true else {
                val inactive = interested.filterNot(binder::isActive)
                if (inactive.isEmpty()) safeInvoker { stop() }
                else ContextCompat.startForegroundService(this, Intent(this, TetheringService::class.java)
                        .putExtra(TetheringService.EXTRA_ADD_INTERFACES, inactive.toTypedArray()))
            }
        }
    }

    override fun onTetheringStarted() = updateTile()
    override fun onTetheringFailed() {
        DebugHelper.log(javaClass.simpleName, "onTetheringFailed")
        updateTile()
    }

    class Wifi : TetheringTileService() {
        override val labelString get() = R.string.tethering_manage_wifi
        override val tetherType get() = TetherType.WIFI
        override val icon get() = R.drawable.ic_device_wifi_tethering

        override fun start() = TetheringManager.start(TetheringManager.TETHERING_WIFI, true, this)
        override fun stop() = TetheringManager.stop(TetheringManager.TETHERING_WIFI)
    }
    class Usb : TetheringTileService() {
        override val labelString get() = R.string.tethering_manage_usb
        override val tetherType get() = TetherType.USB

        override fun start() = TetheringManager.start(TetheringManager.TETHERING_USB, true, this)
        override fun stop() = TetheringManager.stop(TetheringManager.TETHERING_USB)
    }
    class Bluetooth : TetheringTileService() {
        private var tethering: BluetoothTethering? = null

        override val labelString get() = R.string.tethering_manage_bluetooth
        override val tetherType get() = TetherType.BLUETOOTH

        override fun start() = TetheringManager.start(TetheringManager.TETHERING_BLUETOOTH, true, this)
        override fun stop() {
            TetheringManager.stop(TetheringManager.TETHERING_BLUETOOTH)
            Thread.sleep(1)         // give others a room to breathe
            onTetheringStarted()    // force flush state
        }

        override fun onStartListening() {
            tethering = BluetoothTethering(this)
            super.onStartListening()
        }
        override fun onStopListening() {
            super.onStopListening()
            tethering?.close()
            tethering = null
        }

        override fun updateTile() {
            qsTile?.run {
                when (tethering?.active) {
                    true -> {
                        val binder = binder ?: return
                        state = Tile.STATE_ACTIVE
                        val interested = interested
                        icon = if (interested.isNotEmpty() && interested.all(binder::isActive)) tileOn else tileOff
                    }
                    false -> {
                        state = Tile.STATE_INACTIVE
                        icon = tileOff
                    }
                    null -> return
                }
                label = getText(labelString)
                updateTile()
            }
        }

        override fun onClick() = when (tethering?.active) {
            true -> {
                val binder = binder
                if (binder == null) tapPending = true else {
                    val inactive = interested.filterNot(binder::isActive)
                    if (inactive.isEmpty()) safeInvoker { stop() }
                    else ContextCompat.startForegroundService(this, Intent(this, TetheringService::class.java)
                            .putExtra(TetheringService.EXTRA_ADD_INTERFACES, inactive.toTypedArray()))
                }
            }
            false -> safeInvoker { start() }
            else -> tapPending = true
        }
    }

    @Suppress("DEPRECATION")
    @Deprecated("Not usable since API 25")
    class WifiLegacy : TetheringTileService() {
        override val labelString get() = R.string.tethering_manage_wifi_legacy
        override val tetherType get() = TetherType.WIFI
        override val icon get() = R.drawable.ic_device_wifi_tethering

        override fun start() = WifiApManager.start()
        override fun stop() = WifiApManager.stop()
    }
}
