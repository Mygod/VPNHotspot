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
import android.support.v4.content.ContextCompat
import android.support.v7.app.AppCompatActivity
import android.support.v7.widget.DefaultItemAnimator
import android.support.v7.widget.LinearLayoutManager
import android.support.v7.widget.RecyclerView
import android.view.LayoutInflater
import android.view.ViewGroup
import be.mygod.vpnhotspot.databinding.ClientBinding
import be.mygod.vpnhotspot.databinding.MainActivityBinding

class MainActivity : AppCompatActivity(), ServiceConnection {
    inner class Data : BaseObservable() {
        val switchEnabled: Boolean
            @Bindable get() = when (binder?.status) {
                HotspotService.Status.IDLE -> true
                HotspotService.Status.ACTIVE -> true
                else -> false
            }
        var serviceStarted: Boolean
            @Bindable get() = when (binder?.status) {
                HotspotService.Status.STARTING -> true
                HotspotService.Status.ACTIVE -> true
                else -> false
            }
            set(value) {
                val binder = binder
                when (binder?.status) {
                    HotspotService.Status.IDLE ->
                        ContextCompat.startForegroundService(this@MainActivity,
                                Intent(this@MainActivity, HotspotService::class.java))
                    HotspotService.Status.ACTIVE -> binder.shutdown()
                }
            }

        val running get() = binder?.status == HotspotService.Status.ACTIVE
        val ssid: String @Bindable get() = if (running) binder!!.service.group.networkName else ""
        val password: String @Bindable get() = if (running) binder!!.service.group.passphrase else ""

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
    }

    class ClientViewHolder(val binding: ClientBinding) : RecyclerView.ViewHolder(binding.root)
    inner class ClientAdapter : RecyclerView.Adapter<ClientViewHolder>() {
        private var owner: WifiP2pDevice? = null
        private lateinit var clients: MutableCollection<WifiP2pDevice>
        private lateinit var arpCache: ArpCache

        fun fetchClients() {
            val binder = binder!!
            if (data.running) {
                owner = binder.service.group.owner
                clients = binder.service.group.clientList
                arpCache = ArpCache(binder.service.downstream!!)
            } else owner = null
            notifyDataSetChanged()  // recreate everything
        }

        override fun onCreateViewHolder(parent: ViewGroup, viewType: Int) =
                ClientViewHolder(ClientBinding.inflate(LayoutInflater.from(parent.context)))

        override fun onBindViewHolder(holder: ClientViewHolder, position: Int) {
            val device = when (position) {
                0 -> owner
                else -> clients.elementAt(position - 1)
            }
            holder.binding.device = device
            holder.binding.ipAddress = when (position) {
                0 -> binder?.service?.hostAddress?.hostAddress?.toString()
                else -> arpCache[device?.deviceAddress]
            }
            holder.binding.executePendingBindings()
        }

        override fun getItemCount() = if (owner == null) 0 else 1 + clients.size
    }

    private lateinit var binding: MainActivityBinding
    private val data = Data()
    private val adapter = ClientAdapter()
    private var binder: HotspotService.HotspotBinder? = null

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        binding = DataBindingUtil.setContentView(this, R.layout.main_activity)
        binding.data = data
        binding.clients.layoutManager = LinearLayoutManager(this, LinearLayoutManager.VERTICAL, false)
        val animator = DefaultItemAnimator()
        animator.supportsChangeAnimations = false   // prevent fading-in/out when rebinding
        binding.clients.itemAnimator = animator
        binding.clients.adapter = adapter
    }

    override fun onStart() {
        super.onStart()
        bindService(Intent(this, HotspotService::class.java), this, Context.BIND_AUTO_CREATE)
    }

    override fun onStop() {
        unbindService(this)
        super.onStop()
    }

    override fun onServiceConnected(name: ComponentName?, service: IBinder?) {
        val binder = service as HotspotService.HotspotBinder
        binder.data = data
        this.binder = binder
        data.onStatusChanged()
    }

    override fun onServiceDisconnected(name: ComponentName?) {
        binder?.data = null
        binder = null
        data.onStatusChanged()
    }
}
