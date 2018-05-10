package be.mygod.vpnhotspot

import android.content.ComponentName
import android.content.DialogInterface
import android.content.Intent
import android.content.ServiceConnection
import android.databinding.BaseObservable
import android.databinding.Bindable
import android.databinding.DataBindingUtil
import android.net.wifi.WifiConfiguration
import android.net.wifi.p2p.WifiP2pGroup
import android.os.Bundle
import android.os.IBinder
import android.support.v4.app.Fragment
import android.support.v4.content.ContextCompat
import android.support.v7.app.AlertDialog
import android.support.v7.app.AppCompatDialog
import android.support.v7.recyclerview.extensions.ListAdapter
import android.support.v7.widget.DefaultItemAnimator
import android.support.v7.widget.LinearLayoutManager
import android.support.v7.widget.RecyclerView
import android.support.v7.widget.Toolbar
import android.view.*
import android.widget.EditText
import android.widget.Toast
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.client.Client
import be.mygod.vpnhotspot.client.ClientMonitorService
import be.mygod.vpnhotspot.databinding.FragmentRepeaterBinding
import be.mygod.vpnhotspot.databinding.ListitemClientBinding
import be.mygod.vpnhotspot.net.IpNeighbourMonitor
import be.mygod.vpnhotspot.net.wifi.P2pSupplicantConfiguration
import be.mygod.vpnhotspot.net.wifi.WifiP2pDialog
import be.mygod.vpnhotspot.util.ServiceForegroundConnector
import be.mygod.vpnhotspot.util.formatAddresses
import java.net.NetworkInterface
import java.net.SocketException

class RepeaterFragment : Fragment(), ServiceConnection, Toolbar.OnMenuItemClickListener {
    inner class Data : BaseObservable() {
        val switchEnabled: Boolean
            @Bindable get() = when (binder?.service?.status) {
                RepeaterService.Status.IDLE, RepeaterService.Status.ACTIVE -> true
                else -> false
            }
        var serviceStarted: Boolean
            @Bindable get() = when (binder?.service?.status) {
                RepeaterService.Status.STARTING, RepeaterService.Status.ACTIVE -> true
                else -> false
            }
            set(value) {
                val binder = binder
                when (binder?.service?.status) {
                    RepeaterService.Status.IDLE ->
                        if (value) {
                            val context = requireContext()
                            ContextCompat.startForegroundService(context, Intent(context, RepeaterService::class.java))
                        }
                    RepeaterService.Status.ACTIVE -> if (!value) binder.shutdown()
                    else -> { }
                }
            }

        val ssid @Bindable get() = binder?.service?.group?.networkName ?: getText(R.string.service_inactive)
        val addresses @Bindable get(): String {
            return try {
                NetworkInterface.getByName(p2pInterface ?: return "")?.formatAddresses() ?: ""
            } catch (e: SocketException) {
                e.printStackTrace()
                ""
            }
        }

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
    }

    private class ClientViewHolder(val binding: ListitemClientBinding) : RecyclerView.ViewHolder(binding.root)
    private inner class ClientAdapter : ListAdapter<Client, ClientViewHolder>(Client) {
        override fun submitList(list: MutableList<Client>?) {
            super.submitList(list)
            binding.swipeRefresher.isRefreshing = false
        }

        override fun onCreateViewHolder(parent: ViewGroup, viewType: Int) =
                ClientViewHolder(ListitemClientBinding.inflate(LayoutInflater.from(parent.context)))

        override fun onBindViewHolder(holder: ClientViewHolder, position: Int) {
            holder.binding.client = getItem(position)
            holder.binding.executePendingBindings()
        }
    }

    private lateinit var binding: FragmentRepeaterBinding
    private val data = Data()
    private val adapter = ClientAdapter()
    private var binder: RepeaterService.Binder? = null
    private var p2pInterface: String? = null
    private var clients: ClientMonitorService.Binder? = null

    override fun onCreateView(inflater: LayoutInflater, container: ViewGroup?, savedInstanceState: Bundle?): View? {
        binding = DataBindingUtil.inflate(inflater, R.layout.fragment_repeater, container, false)
        binding.data = data
        binding.clients.layoutManager = LinearLayoutManager(context, LinearLayoutManager.VERTICAL, false)
        binding.clients.itemAnimator = DefaultItemAnimator()
        binding.clients.adapter = adapter
        binding.swipeRefresher.setColorSchemeResources(R.color.colorAccent)
        binding.swipeRefresher.setOnRefreshListener {
            IpNeighbourMonitor.instance?.flush()
            val binder = binder
            if (binder?.active == false) {
                try {
                    binder.requestGroupUpdate()
                } catch (exc: UninitializedPropertyAccessException) {
                    exc.printStackTrace()
                }
            }
        }
        binding.toolbar.inflateMenu(R.menu.repeater)
        binding.toolbar.setOnMenuItemClickListener(this)
        ServiceForegroundConnector(this, RepeaterService::class, ClientMonitorService::class)
        return binding.root
    }

    override fun onServiceConnected(name: ComponentName?, service: IBinder?) {
        if (service is ClientMonitorService.Binder) {
            clients = service
            service.clientsChanged[this] = { adapter.submitList(it.toMutableList()) }
            return
        }
        val binder = service as RepeaterService.Binder
        this.binder = binder
        binder.statusChanged[this] = data::onStatusChanged
        binder.groupChanged[this] = data::onGroupChanged
    }

    override fun onServiceDisconnected(name: ComponentName?) {
        val clients = clients
        if (clients != null) {
            this.clients = null
            clients.clientsChanged -= this
        }
        val binder = binder ?: return
        this.binder = null
        binder.statusChanged -= this
        binder.groupChanged -= this
        data.onStatusChanged()
    }

    override fun onMenuItemClick(item: MenuItem) = when (item.itemId) {
        R.id.wps -> if (binder?.active == true) {
            val dialog = AlertDialog.Builder(requireContext())
                    .setTitle(R.string.repeater_wps_dialog_title)
                    .setView(R.layout.dialog_wps)
                    .setPositiveButton(android.R.string.ok, { dialog, _ -> binder?.startWps((dialog as AppCompatDialog)
                            .findViewById<EditText>(android.R.id.edit)!!.text.toString()) })
                    .setNegativeButton(android.R.string.cancel, null)
                    .setNeutralButton(R.string.repeater_wps_dialog_pbc, { _, _ -> binder?.startWps(null) })
                    .create()
            dialog.window.setSoftInputMode(WindowManager.LayoutParams.SOFT_INPUT_STATE_VISIBLE)
            dialog.show()
            true
        } else false
        R.id.edit -> {
            editConfigurations()
            true
        }
        else -> false
    }

    private fun editConfigurations() {
        val binder = binder
        val group = binder?.service?.group
        val ssid = group?.networkName
        val context = requireContext()
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
                            true -> app.handler.postDelayed(binder::requestGroupUpdate, 1000)
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
