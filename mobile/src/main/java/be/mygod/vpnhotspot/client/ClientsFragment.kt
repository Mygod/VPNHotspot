package be.mygod.vpnhotspot.client

import android.content.ComponentName
import android.content.ServiceConnection
import android.databinding.DataBindingUtil
import android.os.Bundle
import android.os.IBinder
import android.support.v4.app.Fragment
import android.support.v7.recyclerview.extensions.ListAdapter
import android.support.v7.widget.DefaultItemAnimator
import android.support.v7.widget.LinearLayoutManager
import android.support.v7.widget.RecyclerView
import android.view.LayoutInflater
import android.view.View
import android.view.ViewGroup
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.databinding.FragmentRepeaterBinding
import be.mygod.vpnhotspot.databinding.ListitemClientBinding
import be.mygod.vpnhotspot.net.IpNeighbourMonitor
import be.mygod.vpnhotspot.util.ServiceForegroundConnector

class ClientsFragment : Fragment(), ServiceConnection {
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
    private val adapter = ClientAdapter()
    private var clients: ClientMonitorService.Binder? = null

    override fun onCreateView(inflater: LayoutInflater, container: ViewGroup?, savedInstanceState: Bundle?): View? {
        binding = DataBindingUtil.inflate(inflater, R.layout.fragment_repeater, container, false)
        binding.clients.layoutManager = LinearLayoutManager(context, LinearLayoutManager.VERTICAL, false)
        binding.clients.itemAnimator = DefaultItemAnimator()
        binding.clients.adapter = adapter
        binding.swipeRefresher.setColorSchemeResources(R.color.colorAccent)
        binding.swipeRefresher.setOnRefreshListener {
            IpNeighbourMonitor.instance?.flush()
        }
        ServiceForegroundConnector(this, this, ClientMonitorService::class)
        return binding.root
    }

    override fun onServiceConnected(name: ComponentName?, service: IBinder?) {
        clients = service as ClientMonitorService.Binder
        service.clientsChanged[this] = { adapter.submitList(it.toMutableList()) }
    }

    override fun onServiceDisconnected(name: ComponentName?) {
        val clients = clients ?: return
        clients.clientsChanged -= this
        this.clients = null
    }
}
