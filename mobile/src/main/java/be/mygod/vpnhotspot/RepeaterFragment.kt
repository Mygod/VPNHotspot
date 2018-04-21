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
import android.support.v7.recyclerview.extensions.ListAdapter
import android.support.v7.util.DiffUtil
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
import be.mygod.vpnhotspot.net.TetheringManager
import be.mygod.vpnhotspot.net.TetherType
import java.net.NetworkInterface
import java.net.SocketException
import java.util.*

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
                            val context = requireContext()
                            ContextCompat.startForegroundService(context, Intent(context, RepeaterService::class.java))
                        }
                    RepeaterService.Status.ACTIVE -> if (!value) binder.shutdown()
                    else -> { }
                }
            }

        val ssid @Bindable get() = binder?.service?.ssid ?: getText(R.string.repeater_inactive)
        val password @Bindable get() = binder?.service?.password ?: ""
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
            val binder = binder
            onGroupChanged(if (binder?.active == true) binder.service.group else null)
        }
        fun onGroupChanged(group: WifiP2pGroup?) {
            notifyPropertyChanged(BR.ssid)
            notifyPropertyChanged(BR.password)
            p2pInterface = group?.`interface`
            notifyPropertyChanged(BR.addresses)
            adapter.p2p = group?.clientList ?: emptyList()
            adapter.recreate()
        }

        val statusListener = broadcastReceiver { _, _ -> onStatusChanged() }
    }

    inner class Client(p2p: WifiP2pDevice? = null, neighbour: IpNeighbour? = null) {
        val iface = neighbour?.dev ?: p2pInterface!!
        val mac = p2p?.deviceAddress ?: neighbour?.lladdr!!
        val ip = TreeMap<String, IpNeighbour.State>()

        val icon get() = TetherType.ofInterface(iface, p2pInterface).icon
        val title get() = "$mac%$iface"
        val description get() = ip.entries.joinToString("\n") { (ip, state) ->
            getString(when (state) {
                IpNeighbour.State.INCOMPLETE -> R.string.connected_state_incomplete
                IpNeighbour.State.VALID -> R.string.connected_state_valid
                IpNeighbour.State.FAILED -> R.string.connected_state_failed
                else -> throw IllegalStateException("Invalid IpNeighbour.State: $state")
            }, ip)
        }

        override fun equals(other: Any?): Boolean {
            if (this === other) return true
            if (javaClass != other?.javaClass) return false

            other as Client

            if (iface != other.iface) return false
            if (mac != other.mac) return false
            if (ip != other.ip) return false

            return true
        }
        override fun hashCode() = Objects.hash(iface, mac, ip)
    }
    private object ClientDiffCallback : DiffUtil.ItemCallback<Client>() {
        override fun areItemsTheSame(oldItem: Client, newItem: Client) =
                oldItem.iface == newItem.iface && oldItem.mac == newItem.mac
        override fun areContentsTheSame(oldItem: Client, newItem: Client) = oldItem == newItem
    }
    private class ClientViewHolder(val binding: ListitemClientBinding) : RecyclerView.ViewHolder(binding.root)
    private inner class ClientAdapter : ListAdapter<Client, ClientViewHolder>(ClientDiffCallback) {
        var p2p: Collection<WifiP2pDevice> = emptyList()
        var neighbours = emptyList<IpNeighbour>()

        fun recreate() {
            val p2p = HashMap(p2p.associateBy({ Pair(p2pInterface, it.deviceAddress) }, { Client(it) }))
            for (neighbour in neighbours) {
                val key = Pair(neighbour.dev, neighbour.lladdr)
                var client = p2p[key]
                if (client == null) {
                    if (!tetheredInterfaces.contains(neighbour.dev)) continue
                    client = Client(neighbour = neighbour)
                    p2p[key] = client
                }
                client.ip += Pair(neighbour.ip, neighbour.state)
            }
            submitList(p2p.values.sortedWith(compareBy<Client> { it.iface }.thenBy { it.mac }))
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
    private var binder: RepeaterService.RepeaterBinder? = null
    private var p2pInterface: String? = null
    private var tetheredInterfaces = emptySet<String>()
    private val receiver = broadcastReceiver { _, intent ->
        tetheredInterfaces = TetheringManager.getTetheredIfaces(intent.extras).toSet()
        adapter.recreate()
    }

    override fun onCreateView(inflater: LayoutInflater, container: ViewGroup?, savedInstanceState: Bundle?): View? {
        binding = DataBindingUtil.inflate(inflater, R.layout.fragment_repeater, container, false)
        binding.data = data
        binding.clients.layoutManager = LinearLayoutManager(context, LinearLayoutManager.VERTICAL, false)
        binding.clients.itemAnimator = DefaultItemAnimator()
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
        val context = requireContext()
        context.bindService(Intent(context, RepeaterService::class.java), this, Context.BIND_AUTO_CREATE)
        IpNeighbourMonitor.registerCallback(this)
        context.registerReceiver(receiver, intentFilter(TetheringManager.ACTION_TETHER_STATE_CHANGED))
    }

    override fun onStop() {
        val context = requireContext()
        context.unregisterReceiver(receiver)
        IpNeighbourMonitor.unregisterCallback(this)
        onServiceDisconnected(null)
        context.unbindService(this)
        super.onStop()
    }

    override fun onServiceConnected(name: ComponentName?, service: IBinder?) {
        val binder = service as RepeaterService.RepeaterBinder
        binder.data = data
        this.binder = binder
        data.onStatusChanged()
        LocalBroadcastManager.getInstance(requireContext())
                .registerReceiver(data.statusListener, intentFilter(RepeaterService.ACTION_STATUS_CHANGED))
    }

    override fun onServiceDisconnected(name: ComponentName?) {
        binder?.data = null
        binder = null
        LocalBroadcastManager.getInstance(requireContext()).unregisterReceiver(data.statusListener)
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
        R.id.resetGroup -> {
            AlertDialog.Builder(requireContext())
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
