package be.mygod.vpnhotspot

import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.databinding.BaseObservable
import android.databinding.DataBindingUtil
import android.os.Bundle
import android.os.IBinder
import android.support.v4.app.Fragment
import android.support.v4.content.ContextCompat
import android.support.v7.util.SortedList
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

class TetheringFragment : Fragment(), ServiceConnection {
    companion object {
        private const val VIEW_TYPE_INTERFACE = 0
        private const val VIEW_TYPE_MANAGE = 1
    }

    private abstract class BaseSorter<T> : SortedList.Callback<T>() {
        override fun onInserted(position: Int, count: Int) { }
        override fun areContentsTheSame(oldItem: T?, newItem: T?): Boolean = oldItem == newItem
        override fun onMoved(fromPosition: Int, toPosition: Int) { }
        override fun onChanged(position: Int, count: Int) { }
        override fun onRemoved(position: Int, count: Int) { }
        override fun areItemsTheSame(item1: T?, item2: T?): Boolean = item1 == item2
        override fun compare(o1: T?, o2: T?): Int =
                if (o1 == null) if (o2 == null) 0 else 1 else if (o2 == null) -1 else compareNonNull(o1, o2)
        abstract fun compareNonNull(o1: T, o2: T): Int
    }
    private open class DefaultSorter<T : Comparable<T>> : BaseSorter<T>() {
        override fun compareNonNull(o1: T, o2: T): Int = o1.compareTo(o2)
    }
    private object TetheredInterfaceSorter : DefaultSorter<TetheredInterface>()

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

        override fun onClick(v: View?) = itemView.context.startActivity(Intent()
                .setClassName("com.android.settings", "com.android.settings.Settings\$TetherSettingsActivity"))
    }
    class TetheredInterface(val name: String, lookup: Map<String, NetworkInterface>) : Comparable<TetheredInterface> {
        val addresses = lookup[name]?.formatAddresses() ?: ""

        override fun compareTo(other: TetheredInterface) = name.compareTo(other.name)
    }
    inner class TetheringAdapter : RecyclerView.Adapter<RecyclerView.ViewHolder>() {
        private val tethered = SortedList(TetheredInterface::class.java, TetheredInterfaceSorter)

        fun update(data: Set<String>) {
            val lookup = NetworkInterface.getNetworkInterfaces().asSequence().associateBy { it.name }
            tethered.clear()
            tethered.addAll(data.map { TetheredInterface(it, lookup) })
            notifyDataSetChanged()
        }

        override fun getItemCount() = tethered.size() + 1
        override fun getItemViewType(position: Int) =
                if (position == tethered.size()) VIEW_TYPE_MANAGE else VIEW_TYPE_INTERFACE
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
                is InterfaceViewHolder -> holder.binding.data = Data(tethered[position])
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
        val animator = DefaultItemAnimator()
        animator.supportsChangeAnimations = false   // prevent fading-in/out when rebinding
        binding.interfaces.itemAnimator = animator
        binding.interfaces.adapter = adapter
        return binding.root
    }

    override fun onStart() {
        super.onStart()
        val context = context!!
        context.registerReceiver(receiver, intentFilter(ConnectivityManagerHelper.ACTION_TETHER_STATE_CHANGED))
        context.bindService(Intent(context, TetheringService::class.java), this, Context.BIND_AUTO_CREATE)
    }

    override fun onStop() {
        val context = context!!
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
