package be.mygod.vpnhotspot.manage

import android.annotation.SuppressLint
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothProfile
import android.content.ActivityNotFoundException
import android.content.Intent
import android.os.Build
import android.provider.Settings
import android.view.View
import androidx.annotation.RequiresApi
import androidx.core.net.toUri
import androidx.core.view.updatePaddingRelative
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleObserver
import androidx.lifecycle.OnLifecycleEvent
import androidx.recyclerview.widget.RecyclerView
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.MainActivity
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.databinding.ListitemInterfaceBinding
import be.mygod.vpnhotspot.net.TetherType
import be.mygod.vpnhotspot.net.TetheringManager
import be.mygod.vpnhotspot.net.wifi.WifiApManager
import com.crashlytics.android.Crashlytics
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
            } catch (exc: ActivityNotFoundException) {
                exc.printStackTrace()
                Crashlytics.logException(exc)
            }
            val started = manager.isStarted
            try {
                if (started) manager.stop() else manager.start()
            } catch (e: InvocationTargetException) {
                e.printStackTrace()
                Crashlytics.logException(e)
                var cause: Throwable? = e
                while (cause != null) {
                    cause = cause.cause
                    if (cause != null && cause !is InvocationTargetException) {
                        mainActivity.snackbar(cause.message.toString()).show()
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
    override fun onTetheringFailed() =
            (parent.activity as MainActivity).snackbar().setText(R.string.tethering_manage_failed).show()

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
                    else -> app.getString(R.string.failure_reason_unknown, error)
                }
            } catch (e: SecurityException) {
                Crashlytics.logException(e)
                e.localizedMessage
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
    class Bluetooth(parent: TetheringFragment) : TetherManager(parent), LifecycleObserver,
            BluetoothProfile.ServiceListener {
        companion object {
            /**
             * PAN Profile
             * From BluetoothProfile.java.
             */
            private const val PAN = 5
            private val isTetheringOn by lazy @SuppressLint("PrivateApi") {
                Class.forName("android.bluetooth.BluetoothPan").getDeclaredMethod("isTetheringOn")
            }
        }

        private var pan: BluetoothProfile? = null

        init {
            parent.lifecycle.addObserver(this)
            BluetoothAdapter.getDefaultAdapter()?.getProfileProxy(parent.requireContext(), this, PAN)
        }

        override fun onServiceDisconnected(profile: Int) {
            pan = null
        }
        override fun onServiceConnected(profile: Int, proxy: BluetoothProfile) {
            pan = proxy
        }
        @OnLifecycleEvent(Lifecycle.Event.ON_DESTROY)
        fun onDestroy() {
            pan = null
        }

        override val title get() = parent.getString(R.string.tethering_manage_bluetooth)
        override val tetherType get() = TetherType.BLUETOOTH
        override val type get() = VIEW_TYPE_BLUETOOTH
        /**
         * Based on: https://android.googlesource.com/platform/packages/apps/Settings/+/78d5efd/src/com/android/settings/TetherSettings.java
         */
        override val isStarted: Boolean
            get() {
                val pan = pan
                return BluetoothAdapter.getDefaultAdapter()?.state == BluetoothAdapter.STATE_ON && pan != null &&
                        isTetheringOn.invoke(pan) as Boolean
            }

        override fun start() = TetheringManager.start(TetheringManager.TETHERING_BLUETOOTH, true, this)
        override fun stop() {
            TetheringManager.stop(TetheringManager.TETHERING_BLUETOOTH)
            Thread.sleep(1)         // give others a room to breathe
            onTetheringStarted()    // force flush state
        }
    }

    @Suppress("DEPRECATION")
    @Deprecated("Not usable since API 26")
    class WifiLegacy(parent: TetheringFragment) : TetherManager(parent) {
        override val title get() = parent.getString(R.string.tethering_manage_wifi_legacy)
        override val tetherType get() = TetherType.WIFI
        override val type get() = VIEW_TYPE_WIFI_LEGACY

        override fun start() = WifiApManager.start()
        override fun stop() = WifiApManager.stop()
    }
}
