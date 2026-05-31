package be.mygod.vpnhotspot

import android.content.Context
import android.content.Intent
import android.os.Build
import androidx.collection.MutableScatterMap
import androidx.collection.MutableScatterSet
import androidx.collection.ScatterSet
import androidx.collection.emptyScatterSet
import androidx.collection.toMutableScatterMap
import androidx.core.content.edit
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.Routing
import be.mygod.vpnhotspot.net.Routing.Ipv6Mode
import be.mygod.vpnhotspot.net.TetherStates
import be.mygod.vpnhotspot.net.TetheringManagerCompat
import be.mygod.vpnhotspot.root.WifiApCommands
import be.mygod.vpnhotspot.root.daemon.RaPreference
import be.mygod.vpnhotspot.util.TileServiceDismissHandle
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.cancel
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.flow.filterIsInstance
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.parcelize.Parcelize
import timber.log.Timber

class TetheringService : NetlinkNeighbourMonitoringService() {
    companion object {
        const val EXTRA_ADD_INTERFACES = "interface.add"
        const val EXTRA_ADD_INTERFACE_MONITOR = "interface.add.monitor"
        const val EXTRA_ADD_INTERFACES_MONITOR = "interface.adds.monitor"
        const val EXTRA_ADD_INTERFACE_GATEWAY = "interface.add.gateway"
        const val EXTRA_ADD_INTERFACES_GATEWAY = "interface.adds.gateway"
        const val EXTRA_REMOVE_INTERFACE = "interface.remove"
        const val EXTRA_REMOVE_INTERFACE_MONITOR = "interface.remove.monitor"
        const val EXTRA_REMOVE_INTERFACE_GATEWAY = "interface.remove.gateway"
        const val EXTRA_RESTART_INTERFACE_GATEWAY = "interface.restart.gateway"

        private const val KEY_GATEWAY_RA_PREFIX = "service.gateway.raPreference."

        fun gatewayRaPreference(iface: String): RaPreference? {
            val value = app.pref.getString(KEY_GATEWAY_RA_PREFIX + iface, null) ?: return null
            return when (value) {
                "High" -> RaPreference.RA_PREFERENCE_HIGH
                "Medium" -> RaPreference.RA_PREFERENCE_MEDIUM
                "Low" -> RaPreference.RA_PREFERENCE_LOW
                else -> null
            }
        }

        fun setGatewayRaPreference(iface: String, pref: RaPreference?) {
            app.pref.edit {
                if (pref == null) {
                    remove(KEY_GATEWAY_RA_PREFIX + iface)
                } else {
                    putString(KEY_GATEWAY_RA_PREFIX + iface, when (pref) {
                        RaPreference.RA_PREFERENCE_HIGH -> "High"
                        RaPreference.RA_PREFERENCE_MEDIUM -> "Medium"
                        RaPreference.RA_PREFERENCE_LOW -> "Low"
                        is RaPreference.Unrecognized -> "Medium"
                    })
                }
            }
        }

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
        val gatewayIfaces = owner.gatewayIfaceSet.asStateFlow()

        fun isActive(iface: String) = managedIfaces.value.contains(iface)
    }

    /**
     * [gateway] marks a single-arm router downstream: a physical interface this device joined as a LAN
     * client. Unlike tether/monitor downstreams, its routing is gated on the interface existing rather
     * than on system tethering state, so it starts immediately and survives [TetherStates] changes.
     */
    private class Downstream(caller: Any, downstream: String, var monitor: Boolean = false,
                             var gateway: Boolean = false) : RoutingManager(caller, downstream) {
        override fun Routing.configure() {
            if (this@Downstream.gateway) {
                ipForward = true   // client interface is not a system tether; enable forwarding
                gateway = true     // request the daemon's single-arm return-path rule
                val pref = gatewayRaPreference(downstream)
                if (pref != null) {
                    ipv6Mode = Ipv6Mode.Nat
                    raPreference = pref
                } else {
                    ipv6Mode = RoutingManager.ipv6Mode
                }
            } else {
                ipv6Mode = RoutingManager.ipv6Mode
            }
        }
    }

    @Parcelize
    data class Starter(val monitored: ArrayList<String>, val gateway: ArrayList<String> = ArrayList()) :
            BootReceiver.Startable {
        override fun start(context: Context) {
            context.startForegroundService(Intent(context, TetheringService::class.java).apply {
                if (monitored.isNotEmpty()) putStringArrayListExtra(EXTRA_ADD_INTERFACES_MONITOR, monitored)
                if (gateway.isNotEmpty()) putStringArrayListExtra(EXTRA_ADD_INTERFACES_GATEWAY, gateway)
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
    private val gatewayIfaceSet = MutableStateFlow<ScatterSet<String>>(emptyScatterSet())
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
                if (downstream.gateway) return@forEach   // gateway downstreams are not tether-state gated
                if (!downstream.monitor) check(downstreams.remove(iface, downstream))
                downstream.stop()
            }
            onDownstreamsChangedLocked()
        }
    }
    private suspend fun onDownstreamsChangedLocked() {
        val monitoredIfaces = MutableScatterSet<String>()
        val monitoredGatewayIfaces = MutableScatterSet<String>()
        val gatewayIfaces = MutableScatterSet<String>()
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
            if (downstream.gateway) {
                gatewayIfaces.add(downstream.downstream)
                if (downstream.monitor) monitoredGatewayIfaces.add(downstream.downstream)
            }
        }
        if (monitoredIfaces.isEmpty() && monitoredGatewayIfaces.isEmpty()) {
            BootReceiver.delete<TetheringService>()
        } else BootReceiver.add<TetheringService>(Starter(
            ArrayList<String>(monitoredIfaces.size).apply { monitoredIfaces.forEach { add(it) } },
            ArrayList<String>(monitoredGatewayIfaces.size).apply { monitoredGatewayIfaces.forEach { add(it) } },
        ))
        // Publish to the bound binder before the teardown below: when reached from the TetherStates
        // collector, unregisterReceiver() cancels tetherStatesJob — this coroutine's own parent — which
        // would otherwise skip this withContext and leave stale managedIfaces visible to bound UI.
        withContext(Dispatchers.Main) {
            monitoredIfaceSet.value = monitoredIfaces
            gatewayIfaceSet.value = gatewayIfaces
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
                val gatewayList = intent.getStringArrayListExtra(EXTRA_ADD_INTERFACES_GATEWAY) ?:
                    intent.getStringExtra(EXTRA_ADD_INTERFACE_GATEWAY)?.let { listOf(it) }
                if (!gatewayList.isNullOrEmpty()) for (iface in gatewayList) {
                    var downstreamToStart: Downstream? = null
                    downstreams.compute(iface) { _, downstream ->
                        if (downstream == null) {
                            Downstream(this@TetheringService, iface, gateway = true).also { downstreamToStart = it }
                        } else {
                            downstream.gateway = true
                            if (!downstream.started) downstreamToStart = downstream
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
                intent.getStringExtra(EXTRA_REMOVE_INTERFACE_GATEWAY)?.also { iface ->
                    downstreams[iface]?.also { downstream ->
                        downstream.gateway = false
                        downstream.monitor = false
                        if (tetheredIfaces?.contains(iface) != true) {
                            downstreams.remove(iface)?.stop()
                        }
                    }
                }
                intent.getStringExtra(EXTRA_RESTART_INTERFACE_GATEWAY)?.also { iface ->
                    downstreams[iface]?.also { downstream ->
                        downstream.stop()
                        downstream.start()
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

    override fun countsFlow(active: List<String>) = if (Build.VERSION.SDK_INT >= 31) {
        softApCountsFlow(active, WifiApCommands.softApCallbackFlow())
    } else super.countsFlow(active)
}
