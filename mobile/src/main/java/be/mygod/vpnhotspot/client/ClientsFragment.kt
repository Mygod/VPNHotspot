package be.mygod.vpnhotspot.client

import android.content.DialogInterface
import android.os.Bundle
import android.os.Parcelable
import android.text.format.DateUtils
import android.text.format.Formatter
import android.text.method.LinkMovementMethod
import android.util.LongSparseArray
import android.view.LayoutInflater
import android.view.MenuItem
import android.view.View
import android.view.ViewGroup
import android.widget.EditText
import androidx.appcompat.app.AlertDialog
import androidx.appcompat.widget.PopupMenu
import androidx.databinding.BaseObservable
import androidx.databinding.DataBindingUtil
import androidx.fragment.app.Fragment
import androidx.lifecycle.Observer
import androidx.lifecycle.ViewModelProviders
import androidx.lifecycle.get
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
import be.mygod.vpnhotspot.net.monitor.IpNeighbourMonitor
import be.mygod.vpnhotspot.net.monitor.TrafficRecorder
import be.mygod.vpnhotspot.room.AppDatabase
import be.mygod.vpnhotspot.room.ClientStats
import be.mygod.vpnhotspot.room.TrafficRecord
import be.mygod.vpnhotspot.room.macToString
import be.mygod.vpnhotspot.util.MainScope
import be.mygod.vpnhotspot.util.SpanFormatter
import be.mygod.vpnhotspot.util.computeIfAbsentCompat
import be.mygod.vpnhotspot.util.toPluralInt
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.android.parcel.Parcelize
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.launch
import java.text.NumberFormat

class ClientsFragment : Fragment(), MainScope by MainScope.Supervisor() {
    @Parcelize
    data class NicknameArg(val mac: Long, val nickname: CharSequence) : Parcelable
    class NicknameDialogFragment : AlertDialogFragment<NicknameArg, Empty>() {
        override fun AlertDialog.Builder.prepare(listener: DialogInterface.OnClickListener) {
            setView(R.layout.dialog_nickname)
            setTitle(getString(R.string.clients_nickname_title, arg.mac.macToString()))
            setPositiveButton(android.R.string.ok, listener)
            setNegativeButton(android.R.string.cancel, null)
            setNeutralButton(emojize(getText(R.string.clients_nickname_set_to_vendor)), listener)
        }

        override fun onCreateDialog(savedInstanceState: Bundle?) = super.onCreateDialog(savedInstanceState).apply {
            create()
            findViewById<EditText>(android.R.id.edit)!!.setText(arg.nickname)
        }

        override fun onClick(dialog: DialogInterface?, which: Int) {
            when (which) {
                DialogInterface.BUTTON_POSITIVE -> GlobalScope.launch(Dispatchers.Main, CoroutineStart.UNDISPATCHED) {
                    MacLookup.abort(arg.mac)
                    AppDatabase.instance.clientRecordDao.upsert(arg.mac) {
                        nickname = this@NicknameDialogFragment.dialog!!.findViewById<EditText>(android.R.id.edit).text
                    }
                }
                DialogInterface.BUTTON_NEUTRAL -> MacLookup.perform(arg.mac, true)
            }
        }
    }

    @Parcelize
    data class StatsArg(val title: CharSequence, val stats: ClientStats) : Parcelable
    class StatsDialogFragment : AlertDialogFragment<StatsArg, Empty>() {
        override fun AlertDialog.Builder.prepare(listener: DialogInterface.OnClickListener) {
            setTitle(SpanFormatter.format(getString(R.string.clients_stats_title), arg.title))
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

        override fun toString() = if (send < 0 || receive < 0) "" else
                "▲ ${Formatter.formatFileSize(app, send)}/s\t\t▼ ${Formatter.formatFileSize(app, receive)}/s"
    }

    private inner class ClientViewHolder(val binding: ListitemClientBinding) : RecyclerView.ViewHolder(binding.root),
            View.OnClickListener, PopupMenu.OnMenuItemClickListener {
        init {
            binding.setLifecycleOwner(this@ClientsFragment)
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
                    NicknameDialogFragment().withArg(NicknameArg(client.mac, client.nickname))
                            .show(fragmentManager ?: return false, "NicknameDialogFragment")
                    true
                }
                R.id.block, R.id.unblock -> {
                    val client = binding.client ?: return false
                    val wasWorking = TrafficRecorder.isWorking(client.mac)
                    client.obtainRecord().apply {
                        blocked = !blocked
                        GlobalScope.launch(Dispatchers.Main, CoroutineStart.UNDISPATCHED) {
                            AppDatabase.instance.clientRecordDao.update(this@apply)
                        }
                    }
                    IpNeighbourMonitor.instance?.flush()
                    if (!wasWorking && item.itemId == R.id.block) {
                        SmartSnackbar.make(R.string.clients_popup_block_service_inactive).show()
                    }
                    true
                }
                R.id.stats -> {
                    binding.client?.let { client ->
                        launch(start = CoroutineStart.UNDISPATCHED) {
                            StatsDialogFragment().withArg(StatsArg(client.title.value!!,
                                    AppDatabase.instance.trafficRecordDao.queryStats(client.mac)))
                                    .show(fragmentManager ?: return@launch, "StatsDialogFragment")
                        }
                    }
                    true
                }
                else -> false
            }
        }
    }

    private inner class ClientAdapter : ListAdapter<Client, ClientViewHolder>(Client) {
        override fun submitList(list: MutableList<Client>?) {
            super.submitList(list)
            binding.swipeRefresher.isRefreshing = false
        }

        override fun onCreateViewHolder(parent: ViewGroup, viewType: Int) =
                ClientViewHolder(ListitemClientBinding.inflate(LayoutInflater.from(parent.context), parent, false))

        override fun onBindViewHolder(holder: ClientViewHolder, position: Int) {
            val client = getItem(position)
            holder.binding.client = client
            holder.binding.rate =
                    rates.computeIfAbsentCompat(Pair(client.iface, client.mac)) { TrafficRate() }
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
                    val rate = rates.computeIfAbsentCompat(Pair(newRecord.downstream, newRecord.mac)) { TrafficRate() }
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
    private var rates = HashMap<Pair<String, Long>, TrafficRate>()

    override fun onCreateView(inflater: LayoutInflater, container: ViewGroup?, savedInstanceState: Bundle?): View? {
        binding = DataBindingUtil.inflate(inflater, R.layout.fragment_clients, container, false)
        binding.setLifecycleOwner(this)
        binding.clients.layoutManager = LinearLayoutManager(context, RecyclerView.VERTICAL, false)
        binding.clients.itemAnimator = DefaultItemAnimator()
        binding.clients.adapter = adapter
        binding.swipeRefresher.setColorSchemeResources(R.color.colorSecondary)
        binding.swipeRefresher.setOnRefreshListener {
            IpNeighbourMonitor.instance?.flush()
        }
        ViewModelProviders.of(requireActivity()).get<ClientViewModel>().clients.observe(this,
                Observer { adapter.submitList(it.toMutableList()) })
        return binding.root
    }

    override fun onStart() {
        super.onStart()
        // we just put these two thing together as this is the only place we need to use this event for now
        TrafficRecorder.foregroundListeners[this] = adapter::updateTraffic
        TrafficRecorder.rescheduleUpdate()  // next schedule time might be 1 min, force reschedule to <= 1s
    }

    override fun onStop() {
        TrafficRecorder.foregroundListeners -= this
        super.onStop()
    }

    override fun onDestroy() {
        job.cancel()
        super.onDestroy()
    }
}
