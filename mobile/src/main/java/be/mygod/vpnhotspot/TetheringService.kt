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
import be.mygod.vpnhotspot.net.Routing
import be.mygod.vpnhotspot.net.TetherStates
import be.mygod.vpnhotspot.net.TetheringManagerCompat
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

    /**
     * Writes and critical reads to downstreams should be protected with this context.
     */
    private val dispatcher = Dispatchers.Default.limitedParallelism(1, "TetheringService")
    override val coroutineContext = dispatcher + Job()
    private val managedIfaces = MutableStateFlow<ScatterSet<String>>(emptyScatterSet())
    private val inactiveIfaceSet = MutableStateFlow<ScatterSet<String>>(emptyScatterSet())
    private val monitoredIfaceSet = MutableStateFlow<ScatterSet<String>>(emptyScatterSet())
    private val binder = Binder(this)
    private var downstreams = MutableScatterMap<String, Downstream>()
    private var tetherStatesJob: Job? = null
    private var tetheredIfaces: ScatterSet<String>? = null
    override val activeIfaces get() = ArrayList<String>(downstreams.size).apply {
        downstreams.forEachValue { if (it.started) add(it.downstream) }
    }
    override val inactiveIfaces get() = ArrayList<String>(downstreams.size).apply {
        downstreams.forEachValue { if (!it.started) add(it.downstream) }
    }

    private fun launchTetherStatesJob() = launch {
        launch {
            TetherStates.flow.map { it.tethered }.distinctUntilChanged().collect { interfaces ->
                val tethered = MutableScatterSet<String>(interfaces.size)
                for (iface in interfaces) tethered.add(iface)
                tetheredIfaces = tethered
                val toRemove = downstreams.toMutableScatterMap()
                tethered.forEach { iface ->
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
    }
    private suspend fun onDownstreamsChangedLocked() {
        val monitoredIfaces = MutableScatterSet<String>()
        val inactiveIfaces = MutableScatterSet<String>()
        val managedIfaces = MutableScatterSet<String>()
        downstreams.forEachValue { downstream ->
            managedIfaces.add(downstream.downstream)
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
            unregisterReceiver()
            ServiceNotification.stopForeground(this)
            stopSelf()
        } else {
            if (tetherStatesJob == null) tetherStatesJob = launchTetherStatesJob()
            if (activeIfaces.isEmpty()) stopNetlinkNeighbours() else startNetlinkNeighbours()
            super.updateNotification()
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
        stopNetlinkNeighbours()
    }

    override fun updateNotification() {
        launch { super.updateNotification() }
    }
}
