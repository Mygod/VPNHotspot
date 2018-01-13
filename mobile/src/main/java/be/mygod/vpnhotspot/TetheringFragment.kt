package be.mygod.vpnhotspot

import android.animation.Animator
import android.animation.AnimatorListenerAdapter
import android.content.Intent
import android.content.res.Resources
import android.databinding.BaseObservable
import android.databinding.DataBindingUtil
import android.os.Bundle
import android.support.v4.app.Fragment
import android.support.v4.content.LocalBroadcastManager
import android.support.v7.util.SortedList
import android.support.v7.widget.DefaultItemAnimator
import android.support.v7.widget.LinearLayoutManager
import android.support.v7.widget.RecyclerView
import android.text.Html
import android.view.LayoutInflater
import android.view.View
import android.view.ViewGroup
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.databinding.FragmentTetheringBinding
import be.mygod.vpnhotspot.databinding.ListitemInterfaceBinding
import be.mygod.vpnhotspot.widget.TextViewLinkHandler

class TetheringFragment : Fragment() {
    companion object {
        /**
         * Source: https://android.googlesource.com/platform/frameworks/base/+/61fa313/core/res/res/values/config.xml#328
         */
        private val usbRegexes = app.resources.getStringArray(Resources.getSystem()
                .getIdentifier("config_tether_usb_regexs", "array", "android"))
                .map { it.toPattern() }
        private val wifiRegexes = app.resources.getStringArray(Resources.getSystem()
                .getIdentifier("config_tether_wifi_regexs", "array", "android"))
                .map { it.toPattern() }
        private val wimaxRegexes = app.resources.getStringArray(Resources.getSystem()
                .getIdentifier("config_tether_wimax_regexs", "array", "android"))
                .map { it.toPattern() }
        private val bluetoothRegexes = app.resources.getStringArray(Resources.getSystem()
                .getIdentifier("config_tether_bluetooth_regexs", "array", "android"))
                .map { it.toPattern() }
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
    private object StringSorter : DefaultSorter<String>()

    class Data(val iface: String) : BaseObservable() {
        val icon: Int get() = when {
            usbRegexes.any { it.matcher(iface).matches() } -> R.drawable.ic_device_usb
            wifiRegexes.any { it.matcher(iface).matches() } -> R.drawable.ic_device_network_wifi
            wimaxRegexes.any { it.matcher(iface).matches() } -> R.drawable.ic_device_network_wifi
            bluetoothRegexes.any { it.matcher(iface).matches() } -> R.drawable.ic_device_bluetooth
            else -> R.drawable.ic_device_wifi_tethering
        }
        var active = TetheringService.active.contains(iface)
    }

    class InterfaceViewHolder(val binding: ListitemInterfaceBinding) : RecyclerView.ViewHolder(binding.root),
            View.OnClickListener {
        init {
            itemView.setOnClickListener(this)
        }

        override fun onClick(view: View) {
            val context = itemView.context
            val data = binding.data!!
            context.startService(Intent(context, TetheringService::class.java).putExtra(if (data.active)
                TetheringService.EXTRA_REMOVE_INTERFACE else TetheringService.EXTRA_ADD_INTERFACE, data.iface))
            data.active = !data.active
        }
    }
    inner class InterfaceAdapter : RecyclerView.Adapter<InterfaceViewHolder>() {
        private val tethered = SortedList(String::class.java, StringSorter)

        fun update(data: Set<String>) {
            val oldEmpty = tethered.size() == 0
            tethered.clear()
            tethered.addAll(data)
            notifyDataSetChanged()
            if (oldEmpty != data.isEmpty())
                if (oldEmpty) crossFade(binding.empty, binding.interfaces)
                else crossFade(binding.interfaces, binding.empty)
        }

        override fun getItemCount() = tethered.size()
        override fun onCreateViewHolder(parent: ViewGroup, viewType: Int) = InterfaceViewHolder(
                ListitemInterfaceBinding.inflate(LayoutInflater.from(parent.context), parent, false))
        override fun onBindViewHolder(holder: InterfaceViewHolder, position: Int) {
            holder.binding.data = Data(tethered[position])
        }
    }

    private lateinit var binding: FragmentTetheringBinding
    private val adapter = InterfaceAdapter()
    private val receiver = broadcastReceiver { _, intent ->
        when (intent.action) {
            TetheringService.ACTION_ACTIVE_INTERFACES_CHANGED -> adapter.notifyDataSetChanged()
            NetUtils.ACTION_TETHER_STATE_CHANGED -> adapter.update(NetUtils.getTetheredIfaces(intent.extras).toSet())
        }
    }
    private var receiverRegistered = false

    override fun onCreateView(inflater: LayoutInflater, container: ViewGroup?, savedInstanceState: Bundle?): View? {
        binding = DataBindingUtil.inflate(inflater, R.layout.fragment_tethering, container, false)
        binding.empty.text = Html.fromHtml(getString(R.string.tethering_no_interfaces))
        binding.empty.movementMethod = TextViewLinkHandler.create {
            startActivity(Intent().setClassName("com.android.settings",
                    "com.android.settings.Settings\$TetherSettingsActivity"))
        }
        binding.interfaces.layoutManager = LinearLayoutManager(context, LinearLayoutManager.VERTICAL, false)
        val animator = DefaultItemAnimator()
        animator.supportsChangeAnimations = false   // prevent fading-in/out when rebinding
        binding.interfaces.itemAnimator = animator
        binding.interfaces.adapter = adapter
        return binding.root
    }

    override fun onStart() {
        super.onStart()
        if (!receiverRegistered) {
            val context = context!!
            context.registerReceiver(receiver, intentFilter(NetUtils.ACTION_TETHER_STATE_CHANGED))
            LocalBroadcastManager.getInstance(context)
                    .registerReceiver(receiver, intentFilter(TetheringService.ACTION_ACTIVE_INTERFACES_CHANGED))
            receiverRegistered = true
        }
    }

    override fun onStop() {
        if (receiverRegistered) {
            val context = context!!
            context.unregisterReceiver(receiver)
            LocalBroadcastManager.getInstance(context).unregisterReceiver(receiver)
            receiverRegistered = false
        }
        super.onStop()
    }

    private fun crossFade(old: View, new: View) {
        val shortAnimTime = resources.getInteger(android.R.integer.config_shortAnimTime).toLong()
        old.animate().alpha(0F).setListener(object : AnimatorListenerAdapter() {
            override fun onAnimationEnd(animation: Animator) {
                old.visibility = View.GONE
            }
        }).duration = shortAnimTime
        new.alpha = 0F
        new.visibility = View.VISIBLE
        new.animate().alpha(1F).setListener(null).duration = shortAnimTime
    }
}
