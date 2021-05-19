package be.mygod.vpnhotspot.client

import android.content.DialogInterface
import android.os.Build
import android.os.Bundle
import android.os.Parcelable
import android.text.format.DateUtils
import android.text.format.Formatter
import android.text.method.LinkMovementMethod
import android.view.LayoutInflater
import android.view.MenuItem
import android.view.View
import android.view.ViewGroup
import android.widget.EditText
import androidx.appcompat.app.AlertDialog
import androidx.appcompat.widget.PopupMenu
import androidx.collection.LongSparseArray
import androidx.databinding.BaseObservable
import androidx.fragment.app.Fragment
import androidx.fragment.app.activityViewModels
import androidx.lifecycle.findViewTreeLifecycleOwner
import androidx.lifecycle.lifecycleScope
import androidx.recyclerview.widget.DefaultItemAnimator
import androidx.recyclerview.widget.LinearLayoutManager
import androidx.recyclerview.widget.ListAdapter
import androidx.recyclerview.widget.RecyclerView
import be.mygod.vpnhotspot.AlertDialogFragment
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.Empty
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.databinding.FragmentClientsBinding
import be.mygod.vpnhotspot.databinding.ListitemClientBinding
import be.mygod.vpnhotspot.net.MacAddressCompat
import be.mygod.vpnhotspot.net.TetherType
import be.mygod.vpnhotspot.net.monitor.IpNeighbourMonitor
import be.mygod.vpnhotspot.net.monitor.TrafficRecorder
import be.mygod.vpnhotspot.room.AppDatabase
import be.mygod.vpnhotspot.room.ClientStats
import be.mygod.vpnhotspot.room.TrafficRecord
import be.mygod.vpnhotspot.util.SpanFormatter
import be.mygod.vpnhotspot.util.showAllowingStateLoss
import be.mygod.vpnhotspot.util.toPluralInt
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.*
import kotlinx.parcelize.Parcelize
import java.text.NumberFormat

class ClientsFragment : Fragment() {
    // FIXME: value class does not work with Parcelize
    @Parcelize
    data class NicknameArg(val mac: Long, val nickname: CharSequence) : Parcelable
    class NicknameDialogFragment : AlertDialogFragment<NicknameArg, Empty>() {
        override fun AlertDialog.Builder.prepare(listener: DialogInterface.OnClickListener) {
            setView(R.layout.dialog_nickname)
            setTitle(getString(R.string.clients_nickname_title, MacAddressCompat(arg.mac).toString()))
            setPositiveButton(android.R.string.ok, listener)
            setNegativeButton(android.R.string.cancel, null)
            setNeutralButton(emojize(getText(R.string.clients_nickname_set_to_vendor)), listener)
        }

        override fun onCreateDialog(savedInstanceState: Bundle?) = super.onCreateDialog(savedInstanceState).apply {
            create()
            findViewById<EditText>(android.R.id.edit)!!.setText(arg.nickname)
        }

        override fun onClick(dialog: DialogInterface?, which: Int) {
            val mac = MacAddressCompat(arg.mac)
            when (which) {
                DialogInterface.BUTTON_POSITIVE -> {
                    val newNickname = this.dialog!!.findViewById<EditText>(android.R.id.edit).text
                    MacLookup.abort(mac)
                    GlobalScope.launch(Dispatchers.Unconfined) {
                        AppDatabase.instance.clientRecordDao.upsert(mac) { nickname = newNickname }
                    }
                }
                DialogInterface.BUTTON_NEUTRAL -> MacLookup.perform(mac, true)
            }
        }
    }

    @Parcelize
    data class StatsArg(val title: CharSequence, val stats: ClientStats) : Parcelable
    class StatsDialogFragment : AlertDialogFragment<StatsArg, Empty>() {
        override fun AlertDialog.Builder.prepare(listener: DialogInterface.OnClickListener) {
            setTitle(SpanFormatter.format(getText(R.string.clients_stats_title), arg.title))
            val context = context
            val resources = resources
            val format = NumberFormat.getIntegerInstance(resources.configuration.locale)
            setMessage("%s\n%s\n%s".format(
                    resources.getQuantityString(R.plurals.clients_stats_message_1, arg.stats.count.toPluralInt(),
                            format.format(arg.stats.count), DateUtils.formatDateTime(context, arg.stats.timestamp,
                            DateUtils.FORMAT_SHOW_TIME or DateUtils.FORMAT_SHOW_YEAR or DateUtils.FORMAT_SHOW_DATE)),
                    resources.getQuantityString(R.plurals.clients_stats_message_2, arg.stats.sentPackets.toPluralInt(),
                            format.format(arg.stats.sentPackets),
                            Formatter.formatFileSize(context, arg.stats.sentBytes)),
                    resources.getQuantityString(R.plurals.clients_stats_message_3, arg.stats.sentPackets.toPluralInt(),
                            format.format(arg.stats.receivedPackets),
                            Formatter.formatFileSize(context, arg.stats.receivedBytes))))
            setPositiveButton(android.R.string.ok, null)
        }
    }

    data class TrafficRate(var send: Long = -1, var receive: Long = -1) : BaseObservable() {
        fun clear() {
            send = -1
            receive = -1
        }

        override fun toString() = if (send < 0 || receive < 0) "" else {
            "▲ ${Formatter.formatFileSize(app, send)}/s\t\t▼ ${Formatter.formatFileSize(app, receive)}/s"
        }
    }

    private inner class ClientViewHolder(parent: ViewGroup, val binding: ListitemClientBinding =
            ListitemClientBinding.inflate(LayoutInflater.from(parent.context), parent, false)) :
            RecyclerView.ViewHolder(binding.root), View.OnClickListener, PopupMenu.OnMenuItemClickListener {
        init {
            binding.lifecycleOwner = parent.findViewTreeLifecycleOwner()!!
            binding.root.setOnClickListener(this)
            binding.description.movementMethod = LinkMovementMethod.getInstance()
        }

        override fun onClick(v: View) {
            PopupMenu(binding.root.context, binding.root).apply {
                menuInflater.inflate(R.menu.popup_client, menu)
                menu.removeItem(if (binding.client!!.blocked) R.id.block else R.id.unblock)
                setOnMenuItemClickListener(this@ClientViewHolder)
                show()
            }
        }

        override fun onMenuItemClick(item: MenuItem?): Boolean {
            return when (item?.itemId) {
                R.id.nickname -> {
                    val client = binding.client ?: return false
                    NicknameDialogFragment().apply {
                        arg(NicknameArg(client.mac.addr, client.nickname))
                    }.showAllowingStateLoss(parentFragmentManager)
                    true
                }
                R.id.block, R.id.unblock -> {
                    val client = binding.client ?: return false
                    val wasWorking = TrafficRecorder.isWorking(client.mac)
                    client.obtainRecord().apply {
                        blocked = !blocked
                        GlobalScope.launch(Dispatchers.Unconfined) {
                            AppDatabase.instance.clientRecordDao.update(this@apply)
                        }
                    }
                    IpNeighbourMonitor.instance?.flushAsync()
                    if (!wasWorking && item.itemId == R.id.block) {
                        SmartSnackbar.make(R.string.clients_popup_block_service_inactive).show()
                    }
                    true
                }
                R.id.stats -> {
                    binding.client?.let { client ->
                        viewLifecycleOwner.lifecycleScope.launchWhenCreated {
                            withContext(Dispatchers.Unconfined) {
                                StatsDialogFragment().apply {
                                    arg(StatsArg(client.title.value ?: return@withContext,
                                            AppDatabase.instance.trafficRecordDao.queryStats(client.mac.addr)))
                                }.showAllowingStateLoss(parentFragmentManager)
                            }
                        }
                    }
                    true
                }
                else -> false
            }
        }
    }

    private inner class ClientAdapter : ListAdapter<Client, ClientViewHolder>(Client) {
        var size = CompletableDeferred(0)

        override fun submitList(list: MutableList<Client>?) {
            val deferred = CompletableDeferred<Int>()
            size = deferred
            super.submitList(list) { deferred.complete(list?.size ?: 0) }
            binding.swipeRefresher.isRefreshing = false
        }

        override fun onCreateViewHolder(parent: ViewGroup, viewType: Int) = ClientViewHolder(parent)
        override fun onBindViewHolder(holder: ClientViewHolder, position: Int) {
            val client = getItem(position)
            holder.binding.client = client
            holder.binding.rate = rates.computeIfAbsent(client.iface to client.mac) { TrafficRate() }
            holder.binding.executePendingBindings()
        }

        fun updateTraffic(newRecords: Collection<TrafficRecord>, oldRecords: LongSparseArray<TrafficRecord>) {
            for (rate in rates.values) rate.clear()
            for (newRecord in newRecords) {
                val oldRecord = oldRecords[newRecord.previousId ?: continue] ?: continue
                val elapsed = newRecord.timestamp - oldRecord.timestamp
                if (elapsed == 0L) {
                    check(newRecord.sentPackets == oldRecord.sentPackets)
                    check(newRecord.sentBytes == oldRecord.sentBytes)
                    check(newRecord.receivedPackets == oldRecord.receivedPackets)
                    check(newRecord.receivedBytes == oldRecord.receivedBytes)
                } else {
                    val rate = rates.computeIfAbsent(newRecord.downstream to MacAddressCompat(newRecord.mac)) {
                        TrafficRate()
                    }
                    if (rate.send < 0 || rate.receive < 0) {
                        rate.send = 0
                        rate.receive = 0
                    }
                    rate.send += (newRecord.sentBytes - oldRecord.sentBytes) * 1000 / elapsed
                    rate.receive += (newRecord.receivedBytes - oldRecord.receivedBytes) * 1000 / elapsed
                }
            }
            for (rate in rates.values) rate.notifyChange()
        }
    }

    private lateinit var binding: FragmentClientsBinding
    private val adapter = ClientAdapter()
    private var rates = mutableMapOf<Pair<String, MacAddressCompat>, TrafficRate>()

    override fun onCreateView(inflater: LayoutInflater, container: ViewGroup?, savedInstanceState: Bundle?): View {
        binding = FragmentClientsBinding.inflate(inflater, container, false)
        binding.clients.layoutManager = LinearLayoutManager(context, RecyclerView.VERTICAL, false)
        binding.clients.itemAnimator = DefaultItemAnimator()
        binding.clients.adapter = adapter
        binding.swipeRefresher.setColorSchemeResources(R.color.colorSecondary)
        binding.swipeRefresher.setOnRefreshListener { IpNeighbourMonitor.instance?.flushAsync() }
        activityViewModels<ClientViewModel>().value.apply {
            lifecycle.addObserver(fullMode)
            clients.observe(viewLifecycleOwner) { adapter.submitList(it.toMutableList()) }
        }
        return binding.root
    }

    override fun onStart() {
        // icon might be changed due to TetherType changes
        if (Build.VERSION.SDK_INT >= 30) TetherType.listener[this] = {
            lifecycleScope.launchWhenStarted { adapter.notifyItemRangeChanged(0, adapter.size.await()) }
        }
        super.onStart()
        // we just put these two thing together as this is the only place we need to use this event for now
        TrafficRecorder.foregroundListeners[this] = { newRecords, oldRecords ->
            lifecycleScope.launchWhenStarted { adapter.updateTraffic(newRecords, oldRecords) }
        }
        lifecycleScope.launchWhenStarted {
            withContext(Dispatchers.Default) {
                TrafficRecorder.rescheduleUpdate()  // next schedule time might be 1 min, force reschedule to <= 1s
            }
        }
    }

    override fun onStop() {
        TrafficRecorder.foregroundListeners -= this
        super.onStop()
        if (Build.VERSION.SDK_INT >= 30) TetherType.listener -= this
    }
}
