package be.mygod.vpnhotspot

import android.content.Context
import android.content.Intent
import android.os.Build
import androidx.collection.MutableScatterMap
import androidx.collection.MutableScatterSet
import androidx.collection.ScatterSet
import androidx.collection.emptyScatterSet
import androidx.collection.toMutableScatterMap
import be.mygod.vpnhotspot.net.Routing
import be.mygod.vpnhotspot.net.TetherStates
import be.mygod.vpnhotspot.net.TetheringManagerCompat
import be.mygod.vpnhotspot.root.WifiApCommands
import be.mygod.vpnhotspot.util.TileServiceDismissHandle
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.cancel
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.flow.filterIsInstance
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.job
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.parcelize.Parcelize
import timber.log.Timber

class TetheringService : NetlinkNeighbourMonitoringService() {
    companion object {
        const val EXTRA_ADD_INTERFACES = "interface.add"
        const val EXTRA_ADD_INTERFACE_MONITOR = "interface.add.monitor"
        const val EXTRA_ADD_INTERFACES_MONITOR = "interface.adds.monitor"
        const val EXTRA_REMOVE_INTERFACE = "interface.remove"
        const val EXTRA_REMOVE_INTERFACE_MONITOR = "interface.remove.monitor"

        var dismissHandle: TileServiceDismissHandle? = null
        private fun dismissIfApplicable() = dismissHandle?.run {
            get()?.dismiss()
            dismissHandle = null
        }
    }

    class Binder(owner: TetheringService) : android.os.Binder() {
        val managedIfaces = owner.managedIfaces.asStateFlow()
        val inactiveIfaces = owner.inactiveIfaceSet.asStateFlow()
        val monitoredIfaces = owner.monitoredIfaceSet.asStateFlow()

        fun isActive(iface: String) = managedIfaces.value.contains(iface)
    }

    private class Downstream(caller: Any, downstream: String, var monitor: Boolean = false) :
            RoutingManager(caller, downstream) {
        override fun Routing.configure() {
            ipv6Mode = RoutingManager.ipv6Mode
        }
    }

    @Parcelize
    data class Starter(val monitored: ArrayList<String>) : BootReceiver.Startable {
        override fun start(context: Context) {
            context.startForegroundService(Intent(context, TetheringService::class.java).apply {
                putStringArrayListExtra(EXTRA_ADD_INTERFACES_MONITOR, monitored)
            })
        }
    }

    override val coroutineContext = Dispatchers.Main.immediate + Job()
    private val managedIfaces = MutableStateFlow<ScatterSet<String>>(emptyScatterSet())
    private val inactiveIfaceSet = MutableStateFlow<ScatterSet<String>>(emptyScatterSet())
    private val monitoredIfaceSet = MutableStateFlow<ScatterSet<String>>(emptyScatterSet())
    override val interfaces = MutableStateFlow<Interfaces?>(null)
    private val binder = Binder(this)
    private var downstreams = MutableScatterMap<String, Downstream>()
    /**
     * Downstream transitions, run to completion one at a time in arrival order by the worker launched in [onCreate]:
     * downstream start/stop suspend, so running intents and tether state changes as independent coroutines would
     * interleave them. Every downstream field is only touched from these transitions, all on the main thread.
     */
    private val transitions = Channel<suspend () -> Unit>(Channel.UNLIMITED)
    private var destroyed = false
    /** The start ID of the latest start whose transition has run, so stopping never drops a start still queued. */
    private var lastStartId = 0
    private var tetherStatesJob: Job? = null
    private var tetheredIfaces: Set<String>? = null

    private fun launchTetherStatesJob() = launch {
        if (Build.VERSION.SDK_INT >= 30) launch {
            TetheringManagerCompat.eventFlow
                .filterIsInstance<TetheringManagerCompat.Event.OffloadStatusChanged>()
                .collect { (status) ->
                    when (status) {
                        TetheringManagerCompat.TETHER_HARDWARE_OFFLOAD_STOPPED,
                        TetheringManagerCompat.TETHER_HARDWARE_OFFLOAD_FAILED -> { }
                        TetheringManagerCompat.TETHER_HARDWARE_OFFLOAD_STARTED -> {
                            Timber.w("TETHER_HARDWARE_OFFLOAD_STARTED")
                            SmartSnackbar.make(R.string.tethering_manage_offload_enabled).show()
                        }
                        else -> Timber.w(IllegalStateException("Unknown onOffloadStatusChanged $status"))
                    }
                }
        }
        val self = coroutineContext.job
        TetherStates.flow.map { it.tethered }.distinctUntilChanged().collect { tetheredInterfaces ->
            transitions.trySend {
                // a state collected before the transition that stopped this collector ran is stale
                if (tetherStatesJob !== self) return@trySend
                tetheredIfaces = tetheredInterfaces
                val toRemove = downstreams.toMutableScatterMap()
                tetheredInterfaces.forEach { iface ->
                    val downstream = toRemove.remove(iface)
                    if (downstream != null && downstream.monitor && !downstream.start()) dismissIfApplicable()
                }
                toRemove.forEach { iface, downstream ->
                    if (!downstream.monitor) check(downstreams.remove(iface, downstream))
                    downstream.stop()
                }
                onDownstreamsChanged()
            }
        }
    }
    private suspend fun onDownstreamsChanged() {
        val monitoredIfaces = MutableScatterSet<String>()
        val inactiveIfaces = MutableScatterSet<String>()
        val managedIfaces = MutableScatterSet<String>()
        val active = ArrayList<String>(downstreams.size)
        val notificationInactive = ArrayList<String>(downstreams.size)
        downstreams.forEachValue { downstream ->
            managedIfaces.add(downstream.downstream)
            if (downstream.started) active.add(downstream.downstream) else notificationInactive.add(downstream.downstream)
            if (downstream.monitor) {
                monitoredIfaces.add(downstream.downstream)
                if (!downstream.started) inactiveIfaces.add(downstream.downstream)
            }
        }
        monitoredIfaces.also { ifaces ->
            if (ifaces.isEmpty()) {
                BootReceiver.delete<TetheringService>()
            } else BootReceiver.add<TetheringService>(Starter(ArrayList<String>(ifaces.size).apply {
                ifaces.forEach { iface -> add(iface) }
            }))
        }
        monitoredIfaceSet.value = monitoredIfaces
        inactiveIfaceSet.value = inactiveIfaces
        this.managedIfaces.value = managedIfaces
        if (managedIfaces.isEmpty()) {
            interfaces.value = null
            unregisterReceiver()
            ServiceNotification.stopForeground(this)
            stopSelfResult(lastStartId)
        } else {
            if (tetherStatesJob == null) tetherStatesJob = launchTetherStatesJob()
            interfaces.value = Interfaces(active, notificationInactive)
        }
    }

    override fun onCreate() {
        super.onCreate()
        launch {
            try {
                for (transition in transitions) if (!destroyed) transition()
            } finally {
                withContext(NonCancellable) {
                    unregisterReceiver()
                    val oldDownstreams = downstreams
                    downstreams = MutableScatterMap()
                    oldDownstreams.forEachValue { it.stop() }    // force clean to prevent leakage
                }
                this@TetheringService.cancel()
            }
        }
    }

    override fun onBind(intent: Intent?) = binder

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        launch { BootReceiver.startIfEnabled() }
        ServiceNotification.startForeground(this)   // call this first just in case we are shutting down immediately
        transitions.trySend {
            lastStartId = startId
            if (intent != null) {
                for (iface in intent.getStringArrayExtra(EXTRA_ADD_INTERFACES) ?: emptyArray()) {
                    var newDownstream: Downstream? = null
                    downstreams.compute(iface) { _, existing ->
                        existing ?: Downstream(this@TetheringService, iface).also { newDownstream = it }
                    }
                    if (newDownstream?.start() == false) dismissIfApplicable()
                }
                val monitorList = intent.getStringArrayListExtra(EXTRA_ADD_INTERFACES_MONITOR) ?:
                    intent.getStringExtra(EXTRA_ADD_INTERFACE_MONITOR)?.let { listOf(it) }
                if (!monitorList.isNullOrEmpty()) for (iface in monitorList) {
                    val isTethered = tetheredIfaces?.contains(iface) == true
                    var downstreamToStart: Downstream? = null
                    downstreams.compute(iface) { _, downstream ->
                        if (downstream == null) {
                            Downstream(this@TetheringService, iface, true).also {
                                if (isTethered) downstreamToStart = it
                            }
                        } else {
                            downstream.monitor = true
                            if (isTethered && !downstream.started) downstreamToStart = downstream
                            downstream
                        }
                    }
                    if (downstreamToStart?.start() == false) dismissIfApplicable()
                }
                intent.getStringExtra(EXTRA_REMOVE_INTERFACE)?.also { downstreams.remove(it)?.stop() }
                intent.getStringExtra(EXTRA_REMOVE_INTERFACE_MONITOR)?.also { iface ->
                    downstreams[iface]?.also { downstream ->
                        downstream.monitor = false
                        if (!downstream.started) downstreams.remove(iface)?.stop()
                    }
                }
                onDownstreamsChanged()
            } else if (downstreams.isEmpty() && stopSelfResult(startId)) {
                interfaces.value = null
                ServiceNotification.stopForeground(this@TetheringService)
            }
        }
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        destroyed = true
        interfaces.value = null
        ServiceNotification.stopForeground(this)
        transitions.close()     // the worker skips the transitions left and tears every downstream down
        super.onDestroy()
    }

    private fun unregisterReceiver() {
        tetherStatesJob?.cancel()
        tetherStatesJob = null
        tetheredIfaces = null
    }

    override fun countsFlow(active: List<String>) = if (Build.VERSION.SDK_INT >= 31) {
        softApCountsFlow(active, WifiApCommands.softApCallbackFlow())
    } else super.countsFlow(active)
}
