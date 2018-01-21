package be.mygod.vpnhotspot

import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.databinding.BaseObservable
import android.databinding.Bindable
import android.databinding.DataBindingUtil
import android.net.wifi.p2p.WifiP2pDevice
import android.net.wifi.p2p.WifiP2pGroup
import android.os.Bundle
import android.os.IBinder
import android.support.v4.app.Fragment
import android.support.v4.content.ContextCompat
import android.support.v4.content.LocalBroadcastManager
import android.support.v7.app.AlertDialog
import android.support.v7.app.AppCompatDialog
import android.support.v7.widget.DefaultItemAnimator
import android.support.v7.widget.LinearLayoutManager
import android.support.v7.widget.RecyclerView
import android.support.v7.widget.Toolbar
import android.view.*
import android.widget.EditText
import be.mygod.vpnhotspot.databinding.FragmentRepeaterBinding
import be.mygod.vpnhotspot.databinding.ListitemClientBinding
import be.mygod.vpnhotspot.net.IpNeighbour
import be.mygod.vpnhotspot.net.IpNeighbourMonitor
import be.mygod.vpnhotspot.net.NetUtils
import be.mygod.vpnhotspot.net.TetherType

class RepeaterFragment : Fragment(), ServiceConnection, Toolbar.OnMenuItemClickListener, IpNeighbourMonitor.Callback {
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
                            val context = context!!
                            ContextCompat.startForegroundService(context, Intent(context, RepeaterService::class.java))
                        }
                    RepeaterService.Status.ACTIVE -> if (!value) binder.shutdown()
                }
            }

        val ssid @Bindable get() = binder?.service?.ssid ?: getText(R.string.repeater_inactive)
        val password @Bindable get() = binder?.service?.password ?: ""

        fun onStatusChanged() {
            notifyPropertyChanged(BR.switchEnabled)
            notifyPropertyChanged(BR.serviceStarted)
            val binder = binder
            onGroupChanged(if (binder?.active == true) binder.service.group else null)
        }
        fun onGroupChanged(group: WifiP2pGroup?) {
            notifyPropertyChanged(BR.ssid)
            notifyPropertyChanged(BR.password)
            p2pInterface = group?.`interface`
            adapter.p2p = group?.clientList ?: emptyList()
            adapter.recreate()
        }

        val statusListener = broadcastReceiver { _, _ -> onStatusChanged() }
    }

    inner class Client(p2p: WifiP2pDevice? = null, private val neighbour: IpNeighbour? = null) {
        private val iface = neighbour?.dev ?: p2pInterface!!
        val mac = neighbour?.lladdr ?: p2p!!.deviceAddress!!
        val ip = neighbour?.ip

        val icon get() = TetherType.ofInterface(iface, p2pInterface).icon
        val title get() = listOf(ip, mac).filter { !it.isNullOrEmpty() }.joinToString(", ")
        val description get() = when (neighbour?.state) {
            IpNeighbour.State.INCOMPLETE, null -> "Connecting to $iface"
            IpNeighbour.State.VALID -> "Connected to $iface"
            IpNeighbour.State.VALID_DELAY -> "Connected to $iface (losing)"
            IpNeighbour.State.FAILED -> "Failed to connect to $iface"
            else -> throw IllegalStateException()
        }
    }
    private class ClientViewHolder(val binding: ListitemClientBinding) : RecyclerView.ViewHolder(binding.root)
    private inner class ClientAdapter : RecyclerView.Adapter<ClientViewHolder>() {
        private val clients = ArrayList<Client>()
        var p2p: Collection<WifiP2pDevice> = emptyList()
        var neighbours = emptyList<IpNeighbour>()

        fun recreate() {
            clients.clear()
            val map = HashMap(p2p.associateBy { it.deviceAddress })
            for (neighbour in neighbours) {
                val client = map.remove(neighbour.lladdr)
                if (client != null) clients.add(Client(client, neighbour))
                else if (tetheredInterfaces.contains(neighbour.dev) || neighbour.dev == p2pInterface)
                    clients.add(Client(neighbour = neighbour))
            }
            clients.addAll(map.map { Client(it.value) })
            clients.sortWith(compareBy<Client> { it.ip }.thenBy { it.mac })
            notifyDataSetChanged()  // recreate everything
            binding.swipeRefresher.isRefreshing = false
        }

        override fun onCreateViewHolder(parent: ViewGroup, viewType: Int) =
                ClientViewHolder(ListitemClientBinding.inflate(LayoutInflater.from(parent.context)))

        override fun onBindViewHolder(holder: ClientViewHolder, position: Int) {
            holder.binding.client = clients[position]
            holder.binding.executePendingBindings()
        }

        override fun getItemCount() = clients.size
    }

    private lateinit var binding: FragmentRepeaterBinding
    private val data = Data()
    private val adapter = ClientAdapter()
    private var binder: RepeaterService.HotspotBinder? = null
    private var p2pInterface: String? = null
    private var tetheredInterfaces = emptySet<String>()
    private val receiver = broadcastReceiver { _, intent ->
        tetheredInterfaces = NetUtils.getTetheredIfaces(intent.extras).toSet()
        adapter.recreate()
    }

    override fun onCreateView(inflater: LayoutInflater, container: ViewGroup?, savedInstanceState: Bundle?): View? {
        binding = DataBindingUtil.inflate(inflater, R.layout.fragment_repeater, container, false)
        binding.data = data
        binding.clients.layoutManager = LinearLayoutManager(context, LinearLayoutManager.VERTICAL, false)
        val animator = DefaultItemAnimator()
        animator.supportsChangeAnimations = false   // prevent fading-in/out when rebinding
        binding.clients.itemAnimator = animator
        binding.clients.adapter = adapter
        binding.swipeRefresher.setColorSchemeResources(R.color.colorAccent)
        binding.swipeRefresher.setOnRefreshListener {
            IpNeighbourMonitor.instance?.flush()
            adapter.recreate()
        }
        binding.toolbar.inflateMenu(R.menu.repeater)
        binding.toolbar.setOnMenuItemClickListener(this)
        return binding.root
    }

    override fun onStart() {
        super.onStart()
        val context = context!!
        context.bindService(Intent(context, RepeaterService::class.java), this, Context.BIND_AUTO_CREATE)
        IpNeighbourMonitor.registerCallback(this)
        context.registerReceiver(receiver, intentFilter(NetUtils.ACTION_TETHER_STATE_CHANGED))
    }

    override fun onStop() {
        val context = context!!
        context.unregisterReceiver(receiver)
        IpNeighbourMonitor.unregisterCallback(this)
        onServiceDisconnected(null)
        context.unbindService(this)
        super.onStop()
    }

    override fun onServiceConnected(name: ComponentName?, service: IBinder?) {
        val binder = service as RepeaterService.HotspotBinder
        binder.data = data
        this.binder = binder
        data.onStatusChanged()
        LocalBroadcastManager.getInstance(context!!)
                .registerReceiver(data.statusListener, intentFilter(RepeaterService.ACTION_STATUS_CHANGED))
    }

    override fun onServiceDisconnected(name: ComponentName?) {
        binder?.data = null
        binder = null
        LocalBroadcastManager.getInstance(context!!).unregisterReceiver(data.statusListener)
        data.onStatusChanged()
    }

    override fun onMenuItemClick(item: MenuItem) = when (item.itemId) {
        R.id.wps -> if (binder?.active == true) {
            val dialog = AlertDialog.Builder(context!!)
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
        R.id.resetGroup -> {
            AlertDialog.Builder(context!!)
                    .setTitle(R.string.repeater_reset_credentials)
                    .setMessage(getString(R.string.repeater_reset_credentials_dialog_message))
                    .setPositiveButton(R.string.repeater_reset_credentials_dialog_reset,
                            { _, _ -> binder?.resetCredentials() })
                    .setNegativeButton(android.R.string.cancel, null)
                    .show()
            true
        }
        else -> false
    }

    override fun onIpNeighbourAvailable(neighbours: Map<String, IpNeighbour>) {
        adapter.neighbours = neighbours.values.toList()
    }
    override fun postIpNeighbourAvailable() = adapter.recreate()
}
