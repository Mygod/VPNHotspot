package be.mygod.vpnhotspot.manage

import android.content.Intent
import android.os.Build
import android.provider.Settings
import android.view.View
import android.widget.Toast
import androidx.annotation.RequiresApi
import androidx.core.net.toUri
import androidx.core.os.bundleOf
import androidx.core.view.updatePaddingRelative
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleObserver
import androidx.lifecycle.OnLifecycleEvent
import androidx.recyclerview.widget.RecyclerView
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.DebugHelper
import be.mygod.vpnhotspot.MainActivity
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.databinding.ListitemInterfaceBinding
import be.mygod.vpnhotspot.net.TetherType
import be.mygod.vpnhotspot.net.TetheringManager
import be.mygod.vpnhotspot.net.wifi.WifiApManager
import be.mygod.vpnhotspot.util.readableMessage
import timber.log.Timber
import java.io.IOException
import java.lang.reflect.InvocationTargetException

sealed class TetherManager(protected val parent: TetheringFragment) : Manager(),
        TetheringManager.OnStartTetheringCallback {
    class ViewHolder(val binding: ListitemInterfaceBinding) : RecyclerView.ViewHolder(binding.root),
            View.OnClickListener {
        init {
            itemView.updatePaddingRelative(start = itemView.resources.getDimensionPixelOffset(
                    R.dimen.listitem_manage_tether_padding_start))
            itemView.setOnClickListener(this)
        }

        var manager: TetherManager? = null
            set(value) {
                field = value!!
                binding.data = value.data
            }

        override fun onClick(v: View?) {
            val manager = manager!!
            val mainActivity = manager.parent.activity as MainActivity
            if (Build.VERSION.SDK_INT >= 23 && !Settings.System.canWrite(mainActivity)) try {
                manager.parent.startActivity(Intent(Settings.ACTION_MANAGE_WRITE_SETTINGS,
                        "package:${mainActivity.packageName}".toUri()))
                return
            } catch (e: RuntimeException) {
                DebugHelper.logEvent("manage_write_settings", bundleOf(Pair("message", e.message)))
            }
            val started = manager.isStarted
            try {
                if (started) manager.stop() else manager.start()
            } catch (e: IOException) {
                Timber.w(e)
                Toast.makeText(mainActivity, e.readableMessage, Toast.LENGTH_LONG).show()
                ManageBar.start(itemView.context)
            } catch (e: InvocationTargetException) {
                if (e.targetException !is SecurityException) Timber.w(e)
                var cause: Throwable? = e
                while (cause != null) {
                    cause = cause.cause
                    if (cause != null && cause !is InvocationTargetException) {
                        Toast.makeText(mainActivity, cause.readableMessage, Toast.LENGTH_LONG).show()
                        ManageBar.start(itemView.context)
                        break
                    }
                }
            }
        }
    }

    /**
     * A convenient class to delegate stuff to BaseObservable.
     */
    inner class Data : be.mygod.vpnhotspot.manage.Data() {
        override val icon get() = tetherType.icon
        override val title get() = this@TetherManager.title
        override var text: CharSequence = ""
        override val active get() = isStarted
    }

    val data = Data()
    abstract val title: CharSequence
    abstract val tetherType: TetherType
    open val isStarted get() = parent.enabledTypes.contains(tetherType)

    protected abstract fun start()
    protected abstract fun stop()

    override fun onTetheringStarted() = data.notifyChange()
    override fun onTetheringFailed() {
        DebugHelper.log(javaClass.simpleName, "onTetheringFailed")
        data.notifyChange()
    }

    override fun bindTo(viewHolder: RecyclerView.ViewHolder) {
        (viewHolder as ViewHolder).manager = this
    }

    fun updateErrorMessage(errored: List<String>) {
        data.text = errored.filter { TetherType.ofInterface(it) == tetherType }.joinToString("\n") {
            "$it: " + try {
                val error = TetheringManager.getLastTetherError(it)
                when (error) {
                    TetheringManager.TETHER_ERROR_NO_ERROR -> "TETHER_ERROR_NO_ERROR"
                    TetheringManager.TETHER_ERROR_UNKNOWN_IFACE -> "TETHER_ERROR_UNKNOWN_IFACE"
                    TetheringManager.TETHER_ERROR_SERVICE_UNAVAIL -> "TETHER_ERROR_SERVICE_UNAVAIL"
                    TetheringManager.TETHER_ERROR_UNSUPPORTED -> "TETHER_ERROR_UNSUPPORTED"
                    TetheringManager.TETHER_ERROR_UNAVAIL_IFACE -> "TETHER_ERROR_UNAVAIL_IFACE"
                    TetheringManager.TETHER_ERROR_MASTER_ERROR -> "TETHER_ERROR_MASTER_ERROR"
                    TetheringManager.TETHER_ERROR_TETHER_IFACE_ERROR -> "TETHER_ERROR_TETHER_IFACE_ERROR"
                    TetheringManager.TETHER_ERROR_UNTETHER_IFACE_ERROR -> "TETHER_ERROR_UNTETHER_IFACE_ERROR"
                    TetheringManager.TETHER_ERROR_ENABLE_NAT_ERROR -> "TETHER_ERROR_ENABLE_NAT_ERROR"
                    TetheringManager.TETHER_ERROR_DISABLE_NAT_ERROR -> "TETHER_ERROR_DISABLE_NAT_ERROR"
                    TetheringManager.TETHER_ERROR_IFACE_CFG_ERROR -> "TETHER_ERROR_IFACE_CFG_ERROR"
                    TetheringManager.TETHER_ERROR_PROVISION_FAILED -> "TETHER_ERROR_PROVISION_FAILED"
                    else -> app.getString(R.string.failure_reason_unknown, error)
                }
            } catch (e: InvocationTargetException) {
                if (Build.VERSION.SDK_INT !in 24..25 || e.cause !is SecurityException) Timber.w(e) else Timber.d(e)
                e.readableMessage
            }
        }
        data.notifyChange()
    }

    @RequiresApi(24)
    class Wifi(parent: TetheringFragment) : TetherManager(parent) {
        override val title get() = parent.getString(R.string.tethering_manage_wifi)
        override val tetherType get() = TetherType.WIFI
        override val type get() = VIEW_TYPE_WIFI

        override fun start() = TetheringManager.start(TetheringManager.TETHERING_WIFI, true, this)
        override fun stop() = TetheringManager.stop(TetheringManager.TETHERING_WIFI)
    }
    @RequiresApi(24)
    class Usb(parent: TetheringFragment) : TetherManager(parent) {
        override val title get() = parent.getString(R.string.tethering_manage_usb)
        override val tetherType get() = TetherType.USB
        override val type get() = VIEW_TYPE_USB

        override fun start() = TetheringManager.start(TetheringManager.TETHERING_USB, true, this)
        override fun stop() = TetheringManager.stop(TetheringManager.TETHERING_USB)
    }
    @RequiresApi(24)
    class Bluetooth(parent: TetheringFragment) : TetherManager(parent), LifecycleObserver {
        private val tethering = BluetoothTethering(parent.requireContext())

        init {
            parent.lifecycle.addObserver(this)
        }

        @OnLifecycleEvent(Lifecycle.Event.ON_DESTROY)
        fun onDestroy() = tethering.close()

        override val title get() = parent.getString(R.string.tethering_manage_bluetooth)
        override val tetherType get() = TetherType.BLUETOOTH
        override val type get() = VIEW_TYPE_BLUETOOTH
        override val isStarted get() = tethering.active == true

        override fun start() = TetheringManager.start(TetheringManager.TETHERING_BLUETOOTH, true, this)
        override fun stop() {
            TetheringManager.stop(TetheringManager.TETHERING_BLUETOOTH)
            Thread.sleep(1)         // give others a room to breathe
            onTetheringStarted()    // force flush state
        }
    }

    @Suppress("DEPRECATION")
    @Deprecated("Not usable since API 26, malfunctioning on API 25")
    class WifiLegacy(parent: TetheringFragment) : TetherManager(parent) {
        override val title get() = parent.getString(R.string.tethering_manage_wifi_legacy)
        override val tetherType get() = TetherType.WIFI
        override val type get() = VIEW_TYPE_WIFI_LEGACY

        override fun start() = WifiApManager.start()
        override fun stop() = WifiApManager.stop()
    }
}
