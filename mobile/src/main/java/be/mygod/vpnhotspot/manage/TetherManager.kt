package be.mygod.vpnhotspot.manage

import android.content.Intent
import android.os.Build
import android.provider.Settings
import android.view.View
import android.widget.Toast
import androidx.annotation.RequiresApi
import androidx.core.net.toUri
import androidx.core.view.updatePaddingRelative
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.LifecycleOwner
import androidx.recyclerview.widget.RecyclerView
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.MainActivity
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.databinding.ListitemInterfaceBinding
import be.mygod.vpnhotspot.net.TetherType
import be.mygod.vpnhotspot.net.TetheringManager
import be.mygod.vpnhotspot.net.wifi.WifiApManager
import be.mygod.vpnhotspot.util.readableMessage
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.launch
import timber.log.Timber
import java.lang.reflect.InvocationTargetException

sealed class TetherManager(protected val parent: TetheringFragment) : Manager(),
        TetheringManager.StartTetheringCallback {
    class ViewHolder(private val binding: ListitemInterfaceBinding) : RecyclerView.ViewHolder(binding.root),
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
                app.logEvent("manage_write_settings") { param("message", e.toString()) }
            }
            if (manager.isStarted) try {
                manager.stop()
            } catch (e: InvocationTargetException) {
                if (e.targetException !is SecurityException) Timber.w(e)
                manager.onException(e)
            } else manager.start()
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
    override fun onTetheringFailed(error: Int?) {
        Timber.d(javaClass.simpleName, "onTetheringFailed: $error")
        error?.let { SmartSnackbar.make("$tetherType: ${TetheringManager.tetherErrorMessage(it)}") }
        data.notifyChange()
    }
    override fun onException(e: Exception) {
        if (e !is InvocationTargetException || e.targetException !is SecurityException) Timber.w(e)
        GlobalScope.launch(Dispatchers.Main.immediate) {
            val context = parent.context ?: app
            Toast.makeText(context, e.readableMessage, Toast.LENGTH_LONG).show()
            ManageBar.start(context)
        }
    }

    override fun bindTo(viewHolder: RecyclerView.ViewHolder) {
        (viewHolder as ViewHolder).manager = this
    }

    private fun getErrorMessage(iface: String): String {
        return TetheringManager.tetherErrorMessage(try {
            TetheringManager.getLastTetherError(iface)
        } catch (e: InvocationTargetException) {
            if (Build.VERSION.SDK_INT !in 24..25 || e.cause !is SecurityException) Timber.w(e) else Timber.d(e)
            return e.readableMessage
        })
    }
    protected open fun makeErrorMessage(errored: List<String>): CharSequence = errored
            .filter { TetherType.ofInterface(it) == tetherType }
            .joinToString("\n") { "$it: ${getErrorMessage(it)}" }
    fun updateErrorMessage(errored: List<String>) {
        data.text = makeErrorMessage(errored)
        data.notifyChange()
    }

    @RequiresApi(24)
    class Wifi(parent: TetheringFragment) : TetherManager(parent) {
        override val title get() = parent.getString(R.string.tethering_manage_wifi)
        override val tetherType get() = TetherType.WIFI
        override val type get() = VIEW_TYPE_WIFI

        override fun start() = TetheringManager.startTethering(TetheringManager.TETHERING_WIFI, true, this)
        override fun stop() = TetheringManager.stopTethering(TetheringManager.TETHERING_WIFI, this::onException)
    }
    @RequiresApi(24)
    class Usb(parent: TetheringFragment) : TetherManager(parent) {
        override val title get() = parent.getString(R.string.tethering_manage_usb)
        override val tetherType get() = TetherType.USB
        override val type get() = VIEW_TYPE_USB

        override fun start() = TetheringManager.startTethering(TetheringManager.TETHERING_USB, true, this)
        override fun stop() = TetheringManager.stopTethering(TetheringManager.TETHERING_USB, this::onException)
    }
    @RequiresApi(24)
    class Bluetooth(parent: TetheringFragment) : TetherManager(parent), DefaultLifecycleObserver {
        private val tethering = BluetoothTethering(parent.requireContext()) {
            data.text = makeErrorMessage()
            data.notifyChange()
        }

        init {
            parent.viewLifecycleOwner.lifecycle.addObserver(this)
        }

        override fun onDestroy(owner: LifecycleOwner) = tethering.close()

        override val title get() = parent.getString(R.string.tethering_manage_bluetooth)
        override val tetherType get() = TetherType.BLUETOOTH
        override val type get() = VIEW_TYPE_BLUETOOTH
        override val isStarted get() = tethering.active == true

        private var baseError: CharSequence? = null
        private fun makeErrorMessage(): CharSequence = listOfNotNull(
                if (tethering.active == null) tethering.activeFailureCause?.readableMessage else null,
                baseError).joinToString("\n")
        override fun makeErrorMessage(errored: List<String>): CharSequence {
            baseError = super.makeErrorMessage(errored).let { if (it.isEmpty()) null else it }
            return makeErrorMessage()
        }

        override fun start() = BluetoothTethering.start(this)
        override fun stop() {
            TetheringManager.stopTethering(TetheringManager.TETHERING_BLUETOOTH, this::onException)
            Thread.sleep(1)         // give others a room to breathe
            onTetheringStarted()    // force flush state
        }
    }
    @RequiresApi(30)
    class Ethernet(parent: TetheringFragment) : TetherManager(parent) {
        override val title get() = parent.getString(R.string.tethering_manage_ethernet)
        override val tetherType get() = TetherType.ETHERNET
        override val type get() = VIEW_TYPE_ETHERNET

        override fun start() = TetheringManager.startTethering(TetheringManager.TETHERING_ETHERNET, true, this)
        override fun stop() = TetheringManager.stopTethering(TetheringManager.TETHERING_ETHERNET, this::onException)
    }
    @RequiresApi(30)
    class Ncm(parent: TetheringFragment) : TetherManager(parent) {
        override val title get() = parent.getString(R.string.tethering_manage_ncm)
        override val tetherType get() = TetherType.NCM
        override val type get() = VIEW_TYPE_NCM

        override fun start() = TetheringManager.startTethering(TetheringManager.TETHERING_NCM, true, this)
        override fun stop() = TetheringManager.stopTethering(TetheringManager.TETHERING_NCM, this::onException)
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
