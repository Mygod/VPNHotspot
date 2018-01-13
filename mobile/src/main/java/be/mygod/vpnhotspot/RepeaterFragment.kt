package be.mygod.vpnhotspot

import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.databinding.BaseObservable
import android.databinding.Bindable
import android.databinding.DataBindingUtil
import android.net.wifi.p2p.WifiP2pDevice
import android.os.Bundle
import android.os.IBinder
import android.support.v4.app.Fragment
import android.support.v4.content.ContextCompat
import android.support.v4.content.LocalBroadcastManager
import android.support.v7.widget.DefaultItemAnimator
import android.support.v7.widget.LinearLayoutManager
import android.support.v7.widget.RecyclerView
import android.view.LayoutInflater
import android.view.View
import android.view.ViewGroup
import be.mygod.vpnhotspot.databinding.FragmentRepeaterBinding
import be.mygod.vpnhotspot.databinding.ListitemClientBinding

class RepeaterFragment : Fragment(), ServiceConnection {
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

        val ssid @Bindable get() = binder?.service?.ssid ?: "Service inactive"
        val password @Bindable get() = binder?.service?.password ?: ""

        fun onStatusChanged() {
            notifyPropertyChanged(BR.switchEnabled)
            notifyPropertyChanged(BR.serviceStarted)
            onGroupChanged()
        }
        fun onGroupChanged() {
            notifyPropertyChanged(BR.ssid)
            notifyPropertyChanged(BR.password)
            adapter.fetchClients()
        }

        val statusListener = broadcastReceiver { _, _ -> onStatusChanged() }
    }

    class ClientViewHolder(val binding: ListitemClientBinding) : RecyclerView.ViewHolder(binding.root)
    inner class ClientAdapter : RecyclerView.Adapter<ClientViewHolder>() {
        private var owner: WifiP2pDevice? = null
        private lateinit var clients: MutableCollection<WifiP2pDevice>
        private lateinit var arpCache: Map<String, String>

        fun fetchClients() {
            val binder = binder
            if (binder?.service?.status == RepeaterService.Status.ACTIVE) {
                owner = binder.service.group.owner
                clients = binder.service.group.clientList
                arpCache = NetUtils.arp(binder.service.routing?.downstream)
            } else owner = null
            notifyDataSetChanged()  // recreate everything
            binding.swipeRefresher.isRefreshing = false
        }

        override fun onCreateViewHolder(parent: ViewGroup, viewType: Int) =
                ClientViewHolder(ListitemClientBinding.inflate(LayoutInflater.from(parent.context)))

        override fun onBindViewHolder(holder: ClientViewHolder, position: Int) {
            val device = when (position) {
                0 -> owner
                else -> clients.elementAt(position - 1)
            }
            holder.binding.device = device
            holder.binding.ipAddress = when (position) {
                0 -> binder?.service?.routing?.hostAddress?.hostAddress
                else -> arpCache[device?.deviceAddress]
            }
            holder.binding.executePendingBindings()
        }

        override fun getItemCount() = if (owner == null) 0 else 1 + clients.size
    }

    private lateinit var binding: FragmentRepeaterBinding
    private val data = Data()
    private val adapter = ClientAdapter()
    private var binder: RepeaterService.HotspotBinder? = null

    override fun onCreateView(inflater: LayoutInflater, container: ViewGroup?, savedInstanceState: Bundle?): View? {
        binding = DataBindingUtil.inflate(inflater, R.layout.fragment_repeater, container, false)
        binding.data = data
        binding.clients.layoutManager = LinearLayoutManager(context, LinearLayoutManager.VERTICAL, false)
        val animator = DefaultItemAnimator()
        animator.supportsChangeAnimations = false   // prevent fading-in/out when rebinding
        binding.clients.itemAnimator = animator
        binding.clients.adapter = adapter
        binding.swipeRefresher.setOnRefreshListener { adapter.fetchClients() }
        return binding.root
    }

    override fun onStart() {
        super.onStart()
        val context = context!!
        context.bindService(Intent(context, RepeaterService::class.java), this, Context.BIND_AUTO_CREATE)
    }

    override fun onStop() {
        onServiceDisconnected(null)
        context!!.unbindService(this)
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
}
