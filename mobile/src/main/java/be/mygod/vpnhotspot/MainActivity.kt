package be.mygod.vpnhotspot

import android.content.*
import android.databinding.BaseObservable
import android.databinding.Bindable
import android.databinding.DataBindingUtil
import android.net.wifi.p2p.WifiP2pDevice
import android.os.Bundle
import android.os.IBinder
import android.support.v4.content.ContextCompat
import android.support.v4.content.LocalBroadcastManager
import android.support.v7.app.AppCompatActivity
import android.support.v7.widget.DefaultItemAnimator
import android.support.v7.widget.LinearLayoutManager
import android.support.v7.widget.RecyclerView
import android.support.v7.widget.Toolbar
import android.view.LayoutInflater
import android.view.MenuItem
import android.view.ViewGroup
import be.mygod.vpnhotspot.databinding.ActivityMainBinding
import be.mygod.vpnhotspot.databinding.ListitemClientBinding

class MainActivity : AppCompatActivity(), ServiceConnection, Toolbar.OnMenuItemClickListener {
    inner class Data : BaseObservable() {
        val switchEnabled: Boolean
            @Bindable get() = when (binder?.service?.status) {
                HotspotService.Status.IDLE, HotspotService.Status.ACTIVE_P2P, HotspotService.Status.ACTIVE_AP -> true
                else -> false
            }
        var serviceStarted: Boolean
            @Bindable get() = when (binder?.service?.status) {
                HotspotService.Status.STARTING, HotspotService.Status.ACTIVE_P2P, HotspotService.Status.ACTIVE_AP ->
                    true
                else -> false
            }
            set(value) {
                val binder = binder
                when (binder?.service?.status) {
                    HotspotService.Status.IDLE ->
                        if (value) ContextCompat.startForegroundService(this@MainActivity,
                                Intent(this@MainActivity, HotspotService::class.java))
                    HotspotService.Status.ACTIVE_P2P, HotspotService.Status.ACTIVE_AP -> if (!value) binder.shutdown()
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
            if (binder?.service?.status == HotspotService.Status.ACTIVE_P2P) {
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
                0 -> binder?.service?.routing?.hostAddress
                else -> arpCache[device?.deviceAddress]
            }
            holder.binding.executePendingBindings()
        }

        override fun getItemCount() = if (owner == null) 0 else 1 + clients.size
    }

    private lateinit var binding: ActivityMainBinding
    private val data = Data()
    private val adapter = ClientAdapter()
    private var binder: HotspotService.HotspotBinder? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        binding = DataBindingUtil.setContentView(this, R.layout.activity_main)
        binding.data = data
        binding.clients.layoutManager = LinearLayoutManager(this, LinearLayoutManager.VERTICAL, false)
        val animator = DefaultItemAnimator()
        animator.supportsChangeAnimations = false   // prevent fading-in/out when rebinding
        binding.clients.itemAnimator = animator
        binding.clients.adapter = adapter
        binding.toolbar.inflateMenu(R.menu.main)
        binding.toolbar.setOnMenuItemClickListener(this)
        binding.swipeRefresher.setOnRefreshListener { adapter.fetchClients() }
    }

    override fun onMenuItemClick(item: MenuItem): Boolean = when (item.itemId) {
        R.id.settings -> {
            startActivity(Intent(this, SettingsActivity::class.java))
            true
        }
        else -> false
    }

    override fun onStart() {
        super.onStart()
        bindService(Intent(this, HotspotService::class.java), this, Context.BIND_AUTO_CREATE)
    }

    override fun onStop() {
        onServiceDisconnected(null)
        unbindService(this)
        super.onStop()
    }

    override fun onServiceConnected(name: ComponentName?, service: IBinder?) {
        val binder = service as HotspotService.HotspotBinder
        binder.data = data
        this.binder = binder
        data.onStatusChanged()
        LocalBroadcastManager.getInstance(this)
                .registerReceiver(data.statusListener, intentFilter(HotspotService.STATUS_CHANGED))
    }

    override fun onServiceDisconnected(name: ComponentName?) {
        binder?.data = null
        binder = null
        LocalBroadcastManager.getInstance(this).unregisterReceiver(data.statusListener)
        data.onStatusChanged()
    }
}
