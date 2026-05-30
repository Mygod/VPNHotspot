package be.mygod.vpnhotspot

import android.content.Context
import android.content.Intent
import android.os.Build
import androidx.annotation.RequiresApi
import androidx.collection.MutableScatterMap
import androidx.collection.MutableScatterSet
import androidx.collection.ScatterSet
import androidx.collection.emptyScatterSet
import androidx.collection.toMutableScatterMap
import be.mygod.vpnhotspot.net.NetlinkNeighbour
import be.mygod.vpnhotspot.net.Routing
import be.mygod.vpnhotspot.net.TetherStates
import be.mygod.vpnhotspot.net.TetheringManagerCompat
import be.mygod.vpnhotspot.net.wifi.WifiApManager
import be.mygod.vpnhotspot.net.wifi.apInstanceIdentifierOrNull
import be.mygod.vpnhotspot.root.WifiApCommands
import be.mygod.vpnhotspot.util.TileServiceDismissHandle
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.cancel
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.catch
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.flow.filterIsInstance
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.flow.runningFold
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

        @RequiresApi(31)
        private val softApStateFlow = WifiApCommands.softApCallbackFlow().runningFold(SoftApState()) { state, event ->
            when (event) {
                is WifiApManager.Event.OnInfoChanged ->
                    state.copy(links = event.info.map { it.apInstanceIdentifierOrNull })
                is WifiApManager.Event.OnConnectedClientsChanged ->
                    state.copy(clients = event.clients.map { it.apInstanceIdentifierOrNull })
                else -> state
            }
        }.catch { e ->
            // Soft AP callback unavailable: drop authoritative counts so every interface falls back to netlink.
            if (e is WifiApManager.SoftApCallbackUnavailableException && e.cause == null) Timber.d(e) else Timber.w(e)
            emit(SoftApState())
        }
    }
    /**
     * The Soft AP callback's latest view, kept as raw AP-instance identifiers (canonicalized lazily against
     * the current bridge topology). [links] are the active AP links from OnInfoChanged — the interfaces the
     * callback is authoritative for — and [clients] are the connected clients. Either is null until its
     * callback first arrives (a null [clients] is "not observed yet", distinct from an observed empty list),
     * keeping counts on netlink until the Soft AP state is fully known.
     */
    private data class SoftApState(val links: List<String?>? = null, val clients: List<String?>? = null)


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

    /**
     * Writes and critical reads to downstreams should be protected with this context.
     */
    private val dispatcher = Dispatchers.Default.limitedParallelism(1, "TetheringService")
    override val coroutineContext = dispatcher + Job()
    private val managedIfaces = MutableStateFlow<ScatterSet<String>>(emptyScatterSet())
    private val inactiveIfaceSet = MutableStateFlow<ScatterSet<String>>(emptyScatterSet())
    private val monitoredIfaceSet = MutableStateFlow<ScatterSet<String>>(emptyScatterSet())
    override val interfaces = MutableStateFlow<Interfaces?>(null)
    private val binder = Binder(this)
    private var downstreams = MutableScatterMap<String, Downstream>()
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
        TetherStates.flow.map { it.tethered }.distinctUntilChanged().collect { tetheredInterfaces ->
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
            onDownstreamsChangedLocked()
        }
    }
    private suspend fun onDownstreamsChangedLocked() {
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
        // Publish to the bound binder before the teardown below: when reached from the TetherStates
        // collector, unregisterReceiver() cancels tetherStatesJob — this coroutine's own parent — which
        // would otherwise skip this withContext and leave stale managedIfaces visible to bound UI.
        withContext(Dispatchers.Main) {
            monitoredIfaceSet.value = monitoredIfaces
            inactiveIfaceSet.value = inactiveIfaces
            this@TetheringService.managedIfaces.value = managedIfaces
        }
        if (managedIfaces.isEmpty()) {
            interfaces.value = null
            unregisterReceiver()
            ServiceNotification.stopForeground(this)
            stopSelf()
        } else {
            if (tetherStatesJob == null) tetherStatesJob = launchTetherStatesJob()
            interfaces.value = Interfaces(active, notificationInactive)
        }
    }

    override fun onBind(intent: Intent?) = binder

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        launch { BootReceiver.startIfEnabled() }
        ServiceNotification.startForeground(this)   // call this first just in case we are shutting down immediately
        launch {
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
                onDownstreamsChangedLocked()
            } else if (downstreams.isEmpty()) stopSelf(startId)
        }
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        interfaces.value = null
        ServiceNotification.stopForeground(this)
        launch {
            unregisterReceiver()
            val oldDownstreams = downstreams
            downstreams = MutableScatterMap()
            oldDownstreams.forEachValue { it.stop() }    // force clean to prevent leakage
            cancel()
        }
        super.onDestroy()
    }

    private fun unregisterReceiver() {
        tetherStatesJob?.cancel()
        tetherStatesJob = null
        tetheredIfaces = null
    }

    /**
     * Per-interface Soft AP connected-client counts. The authoritative interface set comes from the active
     * AP links (each instance canonicalized through the bridge topology, so bridged links merge onto their
     * bridge), tallied from the connected clients and reporting 0 for a link with no clients. Returns an
     * empty map — falling every interface back to netlink — whenever the callback data can't yield a
     * trustworthy tally, so an authoritative count never undercounts the netlink one.
     */
    private fun softApCounts(softAp: SoftApState, snapshot: NetlinkNeighbour.Snapshot): Map<String, Int> {
        val links = softAp.links ?: return emptyMap()
        val ifaces = HashSet<String>(links.size)
        for (link in links) ifaces.add((link ?: continue).let { snapshot.bridgeMasterByMember[it] ?: it })
        if (ifaces.isEmpty()) return emptyMap()
        // Until the first connected-clients callback the list is unknown (not "zero"), so stay on netlink
        // rather than authoritatively reporting 0 for links whose clients have not been observed yet.
        val clients = softAp.clients ?: return emptyMap()
        val counts = HashMap<String, Int>()
        for (client in clients) {
            // A connected client without an AP instance identifier can't be attributed to any interface, so
            // every interface's tally would be an undercount that wrongly overrides netlink — fall back instead.
            val canonical = (client ?: return emptyMap()).let { snapshot.bridgeMasterByMember[it] ?: it }
            counts[canonical] = (counts[canonical] ?: 0) + 1
        }
        return ifaces.associateWith { counts[it] ?: 0 }
    }
    override fun countsFlow(active: List<String>) = if (Build.VERSION.SDK_INT < 31) {
        super.countsFlow(active)
    } else combine(NetlinkNeighbour.monitorSnapshots, softApStateFlow) { snapshot, softAp ->
        val sizeLookup = netlinkSizeLookup(snapshot)
        val authoritative = softApCounts(softAp, snapshot)
        active.associateWith { authoritative[it] ?: sizeLookup[it] ?: 0 }
    }
}
