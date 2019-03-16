package be.mygod.vpnhotspot.manage

import android.Manifest
import android.content.ComponentName
import android.content.DialogInterface
import android.content.Intent
import android.content.ServiceConnection
import android.content.pm.PackageManager
import android.net.wifi.WifiConfiguration
import android.net.wifi.p2p.WifiP2pGroup
import android.os.Bundle
import android.os.IBinder
import android.os.Parcelable
import android.text.method.LinkMovementMethod
import android.view.WindowManager
import android.widget.EditText
import androidx.appcompat.app.AlertDialog
import androidx.core.content.ContextCompat
import androidx.core.os.BuildCompat
import androidx.databinding.BaseObservable
import androidx.databinding.Bindable
import androidx.lifecycle.ViewModel
import androidx.lifecycle.ViewModelProviders
import androidx.lifecycle.get
import androidx.recyclerview.widget.RecyclerView
import be.mygod.vpnhotspot.*
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.databinding.ListitemRepeaterBinding
import be.mygod.vpnhotspot.net.wifi.P2pSupplicantConfiguration
import be.mygod.vpnhotspot.net.wifi.WifiP2pDialogFragment
import be.mygod.vpnhotspot.util.ServiceForegroundConnector
import be.mygod.vpnhotspot.util.formatAddresses
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.android.parcel.Parcelize
import timber.log.Timber
import java.net.NetworkInterface
import java.net.SocketException

class RepeaterManager(private val parent: TetheringFragment) : Manager(), ServiceConnection {
    class ViewHolder(val binding: ListitemRepeaterBinding) : RecyclerView.ViewHolder(binding.root) {
        init {
            binding.addresses.movementMethod = LinkMovementMethod.getInstance()
        }
    }
    inner class Data : BaseObservable() {
        val switchEnabled: Boolean
            @Bindable get() = when (binder?.service?.status) {
                RepeaterService.Status.IDLE, RepeaterService.Status.ACTIVE -> true
                else -> false
            }
        val serviceStarted: Boolean
            @Bindable get() = when (binder?.service?.status) {
                RepeaterService.Status.STARTING, RepeaterService.Status.ACTIVE -> true
                else -> false
            }

        val ssid @Bindable get() = binder?.group?.networkName ?: ""
        val addresses: CharSequence @Bindable get() {
            return try {
                NetworkInterface.getByName(p2pInterface ?: return "")?.formatAddresses() ?: ""
            } catch (_: SocketException) {
                ""
            }
        }
        var oc: CharSequence
            @Bindable get() {
                val oc = RepeaterService.operatingChannel
                return if (oc in 1..165) oc.toString() else ""
            }
            set(value) = app.pref.edit().putString(RepeaterService.KEY_OPERATING_CHANNEL, value.toString()).apply()

        fun onStatusChanged() {
            notifyPropertyChanged(BR.switchEnabled)
            notifyPropertyChanged(BR.serviceStarted)
            notifyPropertyChanged(BR.addresses)
        }
        fun onGroupChanged(group: WifiP2pGroup? = null) {
            notifyPropertyChanged(BR.ssid)
            p2pInterface = group?.`interface`
            notifyPropertyChanged(BR.addresses)
        }

        fun toggle() {
            val binder = binder
            when (binder?.service?.status) {
                RepeaterService.Status.IDLE -> {
                    val context = parent.requireContext()
                    if (BuildCompat.isAtLeastQ() && context.checkSelfPermission(
                                    Manifest.permission.ACCESS_FINE_LOCATION) != PackageManager.PERMISSION_GRANTED) {
                        parent.requestPermissions(arrayOf(Manifest.permission.ACCESS_FINE_LOCATION),
                                TetheringFragment.START_REPEATER)
                        return
                    }
                    ContextCompat.startForegroundService(context, Intent(context, RepeaterService::class.java))
                }
                RepeaterService.Status.ACTIVE -> binder.shutdown()
                else -> { }
            }
        }

        fun wps() {
            if (binder?.active == true) WpsDialogFragment().show(parent, TetheringFragment.REPEATER_WPS)
        }

        fun editConfigurations() {
            val group = binder?.group
            if (group != null) try {
                val config = P2pSupplicantConfiguration(group, binder?.thisDevice?.deviceAddress)
                holder.config = config
                WifiP2pDialogFragment().withArg(WifiP2pDialogFragment.Arg(WifiConfiguration().apply {
                    SSID = group.networkName
                    preSharedKey = config.psk
                })).show(parent, TetheringFragment.REPEATER_EDIT_CONFIGURATION)
                return
            } catch (e: RuntimeException) {
                Timber.w(e)
            }
            SmartSnackbar.make(R.string.repeater_configure_failure).show()
        }
    }

    @Parcelize
    data class WpsRet(val pin: String?) : Parcelable
    class WpsDialogFragment : AlertDialogFragment<Empty, WpsRet>() {
        override fun AlertDialog.Builder.prepare(listener: DialogInterface.OnClickListener) {
            setTitle(R.string.repeater_wps_dialog_title)
            setView(R.layout.dialog_wps)
            setPositiveButton(android.R.string.ok, listener)
            setNegativeButton(android.R.string.cancel, null)
            setNeutralButton(R.string.repeater_wps_dialog_pbc, listener)
        }

        override val ret get() = WpsRet(dialog!!.findViewById<EditText>(android.R.id.edit)?.text?.toString())

        override fun onCreateDialog(savedInstanceState: Bundle?) = super.onCreateDialog(savedInstanceState).apply {
            window!!.setSoftInputMode(WindowManager.LayoutParams.SOFT_INPUT_STATE_VISIBLE)
        }
    }

    class ConfigHolder : ViewModel() {
        var config: P2pSupplicantConfiguration? = null
    }

    init {
        ServiceForegroundConnector(parent, this, RepeaterService::class)
    }

    override val type get() = VIEW_TYPE_REPEATER
    private val data = Data()
    internal var binder: RepeaterService.Binder? = null
    private var p2pInterface: String? = null
    private val holder = ViewModelProviders.of(parent).get<ConfigHolder>()

    override fun bindTo(viewHolder: RecyclerView.ViewHolder) {
        (viewHolder as ViewHolder).binding.data = data
    }

    override fun onServiceConnected(name: ComponentName?, service: IBinder?) {
        binder = service as RepeaterService.Binder
        service.statusChanged[this] = data::onStatusChanged
        service.groupChanged[this] = data::onGroupChanged
        data.notifyChange()
    }

    override fun onServiceDisconnected(name: ComponentName?) {
        val binder = binder ?: return
        this.binder = null
        binder.statusChanged -= this
        binder.groupChanged -= this
        data.onStatusChanged()
    }

    fun onWpsResult(which: Int, data: Intent?) {
        when (which) {
            DialogInterface.BUTTON_POSITIVE -> binder!!.startWps(AlertDialogFragment.getRet<WpsRet>(data!!).pin)
            DialogInterface.BUTTON_NEUTRAL -> binder!!.startWps(null)
        }
    }

    fun onEditResult(which: Int, data: Intent?) {
        when (which) {
            DialogInterface.BUTTON_POSITIVE -> try {
                val master = holder.config ?: return
                val config = AlertDialogFragment.getRet<WifiP2pDialogFragment.Arg>(data!!).configuration
                master.update(config.SSID, config.preSharedKey)
                binder!!.group = null
            } catch (e: Exception) {
                Timber.w(e)
                SmartSnackbar.make(e).show()
            }
            DialogInterface.BUTTON_NEUTRAL -> binder!!.resetCredentials()
        }
        holder.config = null
    }
}
