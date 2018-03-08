package be.mygod.vpnhotspot

import android.content.*
import android.databinding.BaseObservable
import android.databinding.DataBindingUtil
import android.os.Bundle
import android.os.IBinder
import android.support.v4.app.Fragment
import android.support.v4.content.ContextCompat
import android.support.v7.recyclerview.extensions.ListAdapter
import android.support.v7.util.DiffUtil
import android.support.v7.widget.DefaultItemAnimator
import android.support.v7.widget.LinearLayoutManager
import android.support.v7.widget.RecyclerView
import android.view.LayoutInflater
import android.view.View
import android.view.ViewGroup
import be.mygod.vpnhotspot.databinding.FragmentTetheringBinding
import be.mygod.vpnhotspot.databinding.ListitemInterfaceBinding
import be.mygod.vpnhotspot.net.ConnectivityManagerHelper
import be.mygod.vpnhotspot.net.TetherType
import java.net.NetworkInterface
import java.net.SocketException
import java.util.*

class TetheringFragment : Fragment(), ServiceConnection {
    companion object {
        private const val VIEW_TYPE_INTERFACE = 0
        private const val VIEW_TYPE_MANAGE = 1
    }

    inner class Data(val iface: TetheredInterface) : BaseObservable() {
        val icon: Int get() = TetherType.ofInterface(iface.name).icon
        val active = binder?.active?.contains(iface.name) == true
    }

    private class InterfaceViewHolder(val binding: ListitemInterfaceBinding) : RecyclerView.ViewHolder(binding.root),
            View.OnClickListener {
        init {
            itemView.setOnClickListener(this)
        }

        override fun onClick(view: View) {
            val context = itemView.context
            val data = binding.data!!
            if (data.active) context.startService(Intent(context, TetheringService::class.java)
                    .putExtra(TetheringService.EXTRA_REMOVE_INTERFACE, data.iface.name))
            else ContextCompat.startForegroundService(context, Intent(context, TetheringService::class.java)
                    .putExtra(TetheringService.EXTRA_ADD_INTERFACE, data.iface.name))
        }
    }
    private class ManageViewHolder(view: View) : RecyclerView.ViewHolder(view), View.OnClickListener {
        init {
            view.setOnClickListener(this)
        }

        override fun onClick(v: View?) = try {
            itemView.context.startActivity(Intent()
                    .setClassName("com.android.settings", "com.android.settings.Settings\$TetherSettingsActivity"))
        } catch (e: ActivityNotFoundException) {
            itemView.context.startActivity(Intent()
                    .setClassName("com.android.settings", "com.android.settings.TetherSettings"))
        }
    }
    class TetheredInterface(val name: String, lookup: Map<String, NetworkInterface>) : Comparable<TetheredInterface> {
        val addresses = lookup[name]?.formatAddresses() ?: ""

        override fun compareTo(other: TetheredInterface) = name.compareTo(other.name)
        override fun equals(other: Any?): Boolean {
            if (this === other) return true
            if (javaClass != other?.javaClass) return false
            other as TetheredInterface
            if (name != other.name) return false
            if (addresses != other.addresses) return false
            return true
        }
        override fun hashCode(): Int = Objects.hash(name, addresses)

        object DiffCallback : DiffUtil.ItemCallback<TetheredInterface>() {
            override fun areItemsTheSame(oldItem: TetheredInterface, newItem: TetheredInterface) =
                    oldItem.name == newItem.name
            override fun areContentsTheSame(oldItem: TetheredInterface, newItem: TetheredInterface) = oldItem == newItem
        }
    }
    inner class TetheringAdapter :
            ListAdapter<TetheredInterface, RecyclerView.ViewHolder>(TetheredInterface.DiffCallback) {
        fun update(data: Set<String>) {
            val lookup = try {
                NetworkInterface.getNetworkInterfaces().asSequence().associateBy { it.name }
            } catch (e: SocketException) {
                e.printStackTrace()
                emptyMap<String, NetworkInterface>()
            }
            submitList(data.map { TetheredInterface(it, lookup) }.sorted())
        }

        override fun getItemCount() = super.getItemCount() + 1
        override fun getItemViewType(position: Int) =
                if (position == super.getItemCount()) VIEW_TYPE_MANAGE else VIEW_TYPE_INTERFACE
        override fun onCreateViewHolder(parent: ViewGroup, viewType: Int): RecyclerView.ViewHolder {
            val inflater = LayoutInflater.from(parent.context)
            return when (viewType) {
                VIEW_TYPE_INTERFACE -> InterfaceViewHolder(ListitemInterfaceBinding.inflate(inflater, parent, false))
                VIEW_TYPE_MANAGE -> ManageViewHolder(inflater.inflate(R.layout.listitem_manage, parent, false))
                else -> throw IllegalArgumentException("Invalid view type")
            }
        }
        override fun onBindViewHolder(holder: RecyclerView.ViewHolder, position: Int) {
            when (holder) {
                is InterfaceViewHolder -> holder.binding.data = Data(getItem(position))
            }
        }
    }

    private lateinit var binding: FragmentTetheringBinding
    private var binder: TetheringService.TetheringBinder? = null
    val adapter = TetheringAdapter()
    private val receiver = broadcastReceiver { _, intent ->
        adapter.update(ConnectivityManagerHelper.getTetheredIfaces(intent.extras))
    }

    override fun onCreateView(inflater: LayoutInflater, container: ViewGroup?, savedInstanceState: Bundle?): View? {
        binding = DataBindingUtil.inflate(inflater, R.layout.fragment_tethering, container, false)
        binding.interfaces.layoutManager = LinearLayoutManager(context, LinearLayoutManager.VERTICAL, false)
        binding.interfaces.itemAnimator = DefaultItemAnimator()
        binding.interfaces.adapter = adapter
        return binding.root
    }

    override fun onStart() {
        super.onStart()
        val context = requireContext()
        context.registerReceiver(receiver, intentFilter(ConnectivityManagerHelper.ACTION_TETHER_STATE_CHANGED))
        context.bindService(Intent(context, TetheringService::class.java), this, Context.BIND_AUTO_CREATE)
    }

    override fun onStop() {
        val context = requireContext()
        context.unbindService(this)
        context.unregisterReceiver(receiver)
        super.onStop()
    }

    override fun onServiceConnected(name: ComponentName?, service: IBinder?) {
        val binder = service as TetheringService.TetheringBinder
        this.binder = binder
        binder.fragment = this
    }

    override fun onServiceDisconnected(name: ComponentName?) {
        binder?.fragment = null
        binder = null
    }
}
