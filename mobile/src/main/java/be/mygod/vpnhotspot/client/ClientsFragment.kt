package be.mygod.vpnhotspot.client

import android.content.ComponentName
import android.content.DialogInterface
import android.content.ServiceConnection
import android.os.Bundle
import android.os.IBinder
import android.text.format.DateUtils
import android.text.format.Formatter
import android.util.LongSparseArray
import android.view.LayoutInflater
import android.view.MenuItem
import android.view.View
import android.view.ViewGroup
import android.widget.EditText
import androidx.appcompat.app.AlertDialog
import androidx.appcompat.widget.PopupMenu
import androidx.core.os.bundleOf
import androidx.databinding.BaseObservable
import androidx.databinding.DataBindingUtil
import androidx.fragment.app.Fragment
import androidx.recyclerview.widget.DefaultItemAnimator
import androidx.recyclerview.widget.LinearLayoutManager
import androidx.recyclerview.widget.ListAdapter
import androidx.recyclerview.widget.RecyclerView
import be.mygod.vpnhotspot.AlertDialogFragment
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.databinding.FragmentClientsBinding
import be.mygod.vpnhotspot.databinding.ListitemClientBinding
import be.mygod.vpnhotspot.net.monitor.IpNeighbourMonitor
import be.mygod.vpnhotspot.net.monitor.TrafficRecorder
import be.mygod.vpnhotspot.room.*
import be.mygod.vpnhotspot.util.ServiceForegroundConnector
import be.mygod.vpnhotspot.util.computeIfAbsentCompat
import be.mygod.vpnhotspot.util.toPluralInt
import be.mygod.vpnhotspot.widget.SmartSnackbar
import java.text.NumberFormat

class ClientsFragment : Fragment(), ServiceConnection {
    class NicknameDialogFragment : AlertDialogFragment() {
        companion object {
            const val KEY_MAC = "mac"
            const val KEY_NICKNAME = "nickname"
        }

        private val mac by lazy { arguments!!.getString(KEY_MAC)!! }

        override fun AlertDialog.Builder.prepare(listener: DialogInterface.OnClickListener) {
            setView(R.layout.dialog_nickname)
            setTitle(getString(R.string.clients_nickname_title, mac))
            setPositiveButton(android.R.string.ok, listener)
            setNegativeButton(android.R.string.cancel, null)
        }

        override fun onCreateDialog(savedInstanceState: Bundle?) = super.onCreateDialog(savedInstanceState).apply {
            create()
            findViewById<EditText>(android.R.id.edit)!!.setText(arguments!!.getCharSequence(KEY_NICKNAME))
        }

        override fun onClick(di: DialogInterface?, which: Int) {
            AppDatabase.instance.clientRecordDao.lookup(mac.macToLong()).apply {
                nickname = dialog.findViewById<EditText>(android.R.id.edit).text
                AppDatabase.instance.clientRecordDao.update(this)
            }
            IpNeighbourMonitor.instance?.flush()
        }
    }

    class StatsDialogFragment : AlertDialogFragment() {
        companion object {
            const val KEY_TITLE = "title"
            const val KEY_STATS = "stats"
        }

        private val title by lazy { arguments!!.getCharSequence(KEY_TITLE)!! }
        private val stats by lazy { arguments!!.getParcelable<ClientStats>(KEY_STATS)!! }

        override fun AlertDialog.Builder.prepare(listener: DialogInterface.OnClickListener) {
            setTitle(getString(R.string.clients_stats_title, title))
            val context = context
            val resources = resources
            val format = NumberFormat.getIntegerInstance(resources.configuration.locale)
            setMessage("%s\n%s\n%s".format(
                    resources.getQuantityString(R.plurals.clients_stats_message_1, stats.count.toPluralInt(),
                            format.format(stats.count), DateUtils.formatDateTime(context, stats.timestamp,
                            DateUtils.FORMAT_SHOW_TIME or DateUtils.FORMAT_SHOW_YEAR or DateUtils.FORMAT_SHOW_DATE)),
                    resources.getQuantityString(R.plurals.clients_stats_message_2, stats.sentPackets.toPluralInt(),
                            format.format(stats.sentPackets), Formatter.formatFileSize(context, stats.sentBytes)),
                    resources.getQuantityString(R.plurals.clients_stats_message_3, stats.sentPackets.toPluralInt(),
                            format.format(stats.receivedPackets),
                            Formatter.formatFileSize(context, stats.receivedBytes))))
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
            binding.root.setOnClickListener(this)
        }

        override fun onClick(v: View) {
            PopupMenu(binding.root.context, binding.root).apply {
                menuInflater.inflate(R.menu.popup_client, menu)
                menu.removeItem(if (binding.client!!.record.blocked) R.id.block else R.id.unblock)
                setOnMenuItemClickListener(this@ClientViewHolder)
                show()
            }
        }

        override fun onMenuItemClick(item: MenuItem?): Boolean {
            return when (item?.itemId) {
                R.id.nickname -> {
                    val client = binding.client ?: return false
                    NicknameDialogFragment().apply {
                        arguments = bundleOf(Pair(NicknameDialogFragment.KEY_MAC, client.mac),
                                Pair(NicknameDialogFragment.KEY_NICKNAME, client.record.nickname))
                    }.show(fragmentManager, "NicknameDialogFragment")
                    true
                }
                R.id.block, R.id.unblock -> {
                    val client = binding.client ?: return false
                    val wasWorking = TrafficRecorder.isWorking(client.mac.macToLong())
                    client.record.apply {
                        AppDatabase.instance.clientRecordDao.update(ClientRecord(mac, nickname, !blocked))
                    }
                    IpNeighbourMonitor.instance?.flush()
                    if (!wasWorking && item.itemId == R.id.block) {
                        SmartSnackbar.make(R.string.clients_popup_block_service_inactive).show()
                    }
                    true
                }
                R.id.stats -> {
                    val client = binding.client ?: return false
                    StatsDialogFragment().apply {
                        arguments = bundleOf(Pair(StatsDialogFragment.KEY_TITLE, client.title),
                                Pair(StatsDialogFragment.KEY_STATS,
                                        AppDatabase.instance.trafficRecordDao.queryStats(client.mac.macToLong())))
                    }.show(fragmentManager, "StatsDialogFragment")
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
                    rates.computeIfAbsentCompat(Pair(client.iface, client.mac.macToLong())) { TrafficRate() }
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
    private var clients: ClientMonitorService.Binder? = null
    private var rates = HashMap<Pair<String, Long>, TrafficRate>()

    override fun onCreateView(inflater: LayoutInflater, container: ViewGroup?, savedInstanceState: Bundle?): View? {
        binding = DataBindingUtil.inflate(inflater, R.layout.fragment_clients, container, false)
        binding.clients.layoutManager = LinearLayoutManager(context, RecyclerView.VERTICAL, false)
        binding.clients.itemAnimator = DefaultItemAnimator()
        binding.clients.adapter = adapter
        binding.swipeRefresher.setColorSchemeResources(R.color.colorSecondary)
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
}
