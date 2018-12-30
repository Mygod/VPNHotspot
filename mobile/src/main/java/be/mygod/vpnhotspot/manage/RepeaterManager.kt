package be.mygod.vpnhotspot.manage

import android.content.ComponentName
import android.content.DialogInterface
import android.content.Intent
import android.content.ServiceConnection
import android.net.wifi.WifiConfiguration
import android.net.wifi.p2p.WifiP2pGroup
import android.os.Bundle
import android.os.IBinder
import android.view.WindowManager
import android.widget.EditText
import androidx.appcompat.app.AlertDialog
import androidx.core.content.ContextCompat
import androidx.core.os.bundleOf
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
import timber.log.Timber
import java.lang.RuntimeException
import java.net.NetworkInterface
import java.net.SocketException

class RepeaterManager(private val parent: TetheringFragment) : Manager(), ServiceConnection {
    class ViewHolder(val binding: ListitemRepeaterBinding) : RecyclerView.ViewHolder(binding.root)
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
            } catch (e: SocketException) {
                ""
            }
        }
        var oc: CharSequence
            @Bindable get() {
                val oc = app.operatingChannel
                return if (oc in 1..165) oc.toString() else ""
            }
            set(value) = app.pref.edit().putString(App.KEY_OPERATING_CHANNEL, value.toString()).apply()

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
                    ContextCompat.startForegroundService(context, Intent(context, RepeaterService::class.java))
                }
                RepeaterService.Status.ACTIVE -> binder.shutdown()
                else -> { }
            }
        }

        fun wps() {
            if (binder?.active == true) WpsDialogFragment().run {
                setTargetFragment(parent, TetheringFragment.REPEATER_WPS)
                show(parent.fragmentManager, "WpsDialogFragment")
            }
        }

        fun editConfigurations() {
            val group = binder?.group
            if (group != null) try {
                val config = P2pSupplicantConfiguration(group, binder?.thisDevice?.deviceAddress)
                holder.config = config
                WifiP2pDialogFragment().apply {
                    arguments = bundleOf(Pair(WifiP2pDialogFragment.KEY_CONFIGURATION, WifiConfiguration().apply {
                        SSID = group.networkName
                        preSharedKey = config.psk
                    }))
                    setTargetFragment(parent, TetheringFragment.REPEATER_EDIT_CONFIGURATION)
                }.show(parent.fragmentManager, WifiP2pDialogFragment.TAG)
                return
            } catch (e: RuntimeException) {
                Timber.w(e)
            }
            SmartSnackbar.make(R.string.repeater_configure_failure).show()
        }
    }

    class WpsDialogFragment : AlertDialogFragment() {
        companion object {
            const val KEY_PIN = "pin"
        }

        override fun AlertDialog.Builder.prepare(listener: DialogInterface.OnClickListener) {
            setTitle(R.string.repeater_wps_dialog_title)
            setView(R.layout.dialog_wps)
            setPositiveButton(android.R.string.ok, listener)
            setNegativeButton(android.R.string.cancel, null)
            setNeutralButton(R.string.repeater_wps_dialog_pbc, listener)
        }
        override val data: Intent get() = Intent()
                .putExtra(KEY_PIN, dialog.findViewById<EditText>(android.R.id.edit)?.text?.toString())

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

    fun onWpsResult(which: Int, data: Intent) {
        when (which) {
            DialogInterface.BUTTON_POSITIVE -> binder?.startWps(data.getStringExtra(WpsDialogFragment.KEY_PIN))
            DialogInterface.BUTTON_NEUTRAL -> binder?.startWps(null)
        }
    }

    fun onEditResult(which: Int, data: Intent) {
        when (which) {
            DialogInterface.BUTTON_POSITIVE -> try {
                val master = holder.config ?: return
                val config = data.getParcelableExtra<WifiConfiguration>(WifiP2pDialogFragment.KEY_CONFIGURATION)
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
