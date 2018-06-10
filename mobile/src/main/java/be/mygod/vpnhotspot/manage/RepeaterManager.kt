package be.mygod.vpnhotspot.manage

import android.content.ComponentName
import android.content.DialogInterface
import android.content.Intent
import android.content.ServiceConnection
import android.databinding.BaseObservable
import android.databinding.Bindable
import android.net.wifi.WifiConfiguration
import android.net.wifi.p2p.WifiP2pGroup
import android.os.IBinder
import android.support.v4.content.ContextCompat
import android.support.v7.app.AlertDialog
import android.support.v7.app.AppCompatDialog
import android.support.v7.widget.RecyclerView
import android.view.WindowManager
import android.widget.EditText
import android.widget.Toast
import be.mygod.vpnhotspot.App
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.BR
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.RepeaterService
import be.mygod.vpnhotspot.databinding.ListitemRepeaterBinding
import be.mygod.vpnhotspot.net.wifi.P2pSupplicantConfiguration
import be.mygod.vpnhotspot.net.wifi.WifiP2pDialog
import be.mygod.vpnhotspot.util.ServiceForegroundConnector
import be.mygod.vpnhotspot.util.formatAddresses
import com.crashlytics.android.Crashlytics
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

        val ssid @Bindable get() = binder?.service?.group?.networkName ?: ""
        val addresses: CharSequence @Bindable get() {
            return try {
                NetworkInterface.getByName(p2pInterface ?: return "")?.formatAddresses() ?: ""
            } catch (e: SocketException) {
                e.printStackTrace()
                Crashlytics.logException(e)
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
            if (binder?.active != true) return
            val dialog = AlertDialog.Builder(parent.requireContext())
                    .setTitle(R.string.repeater_wps_dialog_title)
                    .setView(R.layout.dialog_wps)
                    .setPositiveButton(android.R.string.ok, { dialog, _ -> binder?.startWps((dialog as AppCompatDialog)
                            .findViewById<EditText>(android.R.id.edit)!!.text.toString()) })
                    .setNegativeButton(android.R.string.cancel, null)
                    .setNeutralButton(R.string.repeater_wps_dialog_pbc, { _, _ -> binder?.startWps(null) })
                    .create()
            dialog.window.setSoftInputMode(WindowManager.LayoutParams.SOFT_INPUT_STATE_VISIBLE)
            dialog.show()
        }

        fun editConfigurations() {
            val binder = binder
            val group = binder?.service?.group
            val ssid = group?.networkName
            val context = parent.requireContext()
            if (ssid != null) {
                val wifi = WifiConfiguration()
                val conf = P2pSupplicantConfiguration()
                wifi.SSID = ssid
                wifi.preSharedKey = group.passphrase
                if (wifi.preSharedKey == null) wifi.preSharedKey = conf.readPsk()
                if (wifi.preSharedKey != null) {
                    var dialog: WifiP2pDialog? = null
                    dialog = WifiP2pDialog(context, DialogInterface.OnClickListener { _, which ->
                        when (which) {
                            DialogInterface.BUTTON_POSITIVE -> when (conf.update(dialog!!.config!!)) {
                                true -> App.app.handler.postDelayed(binder::requestGroupUpdate, 1000)
                                false -> Toast.makeText(context, R.string.noisy_su_failure, Toast.LENGTH_SHORT).show()
                                null -> Toast.makeText(context, R.string.root_unavailable, Toast.LENGTH_SHORT).show()
                            }
                            DialogInterface.BUTTON_NEUTRAL -> binder.resetCredentials()
                        }
                    }, wifi)
                    dialog.show()
                    return
                }
            }
            Toast.makeText(context, R.string.repeater_configure_failure, Toast.LENGTH_LONG).show()
        }
    }

    init {
        ServiceForegroundConnector(parent, this, RepeaterService::class)
    }

    override val type get() = VIEW_TYPE_REPEATER
    private val data = Data()
    private var binder: RepeaterService.Binder? = null
    private var p2pInterface: String? = null

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
}
