package be.mygod.vpnhotspot

import android.content.Context
import android.content.Intent
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.App.Companion.app
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
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.parcelize.Parcelize
import timber.log.Timber
import java.util.concurrent.ConcurrentHashMap

class TetheringService : NetlinkNeighbourMonitoringService(), TetherStates.Callback {
    companion object {
        const val EXTRA_ADD_INTERFACES = "interface.add"
        const val EXTRA_ADD_INTERFACE_MONITOR = "interface.add.monitor"
        const val EXTRA_ADD_INTERFACES_MONITOR = "interface.adds.monitor"
        const val EXTRA_REMOVE_INTERFACE = "interface.remove"

        var dismissHandle: TileServiceDismissHandle? = null
        private fun dismissIfApplicable() = dismissHandle?.run {
            get()?.dismiss()
            dismissHandle = null
        }
    }

    inner class Binder : android.os.Binder() {
        val managedIfaces = this@TetheringService.managedIfaces.asStateFlow()
        val inactiveIfaces = inactiveIfaceSet.asStateFlow()
        val monitoredIfaces = monitoredIfaceSet.asStateFlow()

        fun isActive(iface: String) = this@TetheringService.managedIfaces.value.contains(iface)
        fun isInactive(iface: String) = inactiveIfaceSet.value.contains(iface)
        fun monitored(iface: String) = monitoredIfaceSet.value.contains(iface)
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
    private val managedIfaces = MutableStateFlow(emptySet<String>())
    private val inactiveIfaceSet = MutableStateFlow(emptySet<String>())
    private val monitoredIfaceSet = MutableStateFlow(emptySet<String>())
    private val binder = Binder()
    private val downstreams = ConcurrentHashMap<String, Downstream>()
    private var tetherStatesRegistered = false
    private var tetheredIfaces: Set<String>? = null
    override val activeIfaces get() = downstreams.values.filter { it.started }.map { it.downstream }
    override val inactiveIfaces get() = downstreams.values.filter { !it.started }.map { it.downstream }

    override fun onTetheredInterfacesChanged(interfaces: List<String?>) {
        launch {
            val tethered = interfaces.filterNotNull().toSet()
            tetheredIfaces = tethered
            val toRemove = downstreams.toMutableMap()   // make a copy
            for (iface in tethered) {
                val downstream = toRemove.remove(iface) ?: continue
                if (downstream.monitor && !downstream.start()) dismissIfApplicable()
            }
            for ((iface, downstream) in toRemove) {
                if (!downstream.monitor) check(downstreams.remove(iface, downstream))
                downstream.stop()
            }
            onDownstreamsChangedLocked()
        }
    }

    @RequiresApi(30)
    override fun onOffloadStatusChanged(status: Int) = when (status) {
        TetheringManagerCompat.TETHER_HARDWARE_OFFLOAD_STOPPED,
        TetheringManagerCompat.TETHER_HARDWARE_OFFLOAD_FAILED -> { }
        TetheringManagerCompat.TETHER_HARDWARE_OFFLOAD_STARTED -> {
            Timber.w("TETHER_HARDWARE_OFFLOAD_STARTED")
            SmartSnackbar.make(R.string.tethering_manage_offload_enabled).show()
        }
        else -> Timber.w(IllegalStateException("Unknown onOffloadStatusChanged $status"))
    }

    private suspend fun onDownstreamsChangedLocked() {
        val downstreams = downstreams.values
        val monitoredIfaces = downstreams.filter { it.monitor }.mapTo(mutableSetOf()) { it.downstream }
        val inactiveIfaces = downstreams.filter { !it.started && it.monitor }.mapTo(mutableSetOf()) { it.downstream }
        val managedIfaces = downstreams.mapTo(mutableSetOf()) { it.downstream }
        if (managedIfaces.isEmpty()) {
            unregisterReceiver()
            ServiceNotification.stopForeground(this)
            stopSelf()
        } else {
            monitoredIfaces.also {
                if (it.isEmpty()) BootReceiver.delete<TetheringService>()
                else BootReceiver.add<TetheringService>(Starter(ArrayList(it)))
            }
            if (!tetherStatesRegistered) {
                withContext(Dispatchers.Main.immediate) {
                    TetherStates.registerCallback(this@TetheringService)
                }
                tetherStatesRegistered = true
            }
            if (activeIfaces.isEmpty()) stopNetlinkNeighbours() else startNetlinkNeighbours()
            super.updateNotification()
        }
        withContext(Dispatchers.Main) {
            monitoredIfaceSet.value = monitoredIfaces
            inactiveIfaceSet.value = inactiveIfaces
            this@TetheringService.managedIfaces.value = managedIfaces
        }
    }

    override fun onBind(intent: Intent?) = binder

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        launch { BootReceiver.startIfEnabled() }
        ServiceNotification.startForeground(this)   // call this first just in case we are shutting down immediately
        launch {
            if (intent != null) {
                for (iface in intent.getStringArrayExtra(EXTRA_ADD_INTERFACES) ?: emptyArray()) {
                    if (downstreams[iface] == null) {
                        val downstream = Downstream(this@TetheringService, iface)
                        check(downstreams.put(iface, downstream) == null)
                        if (!downstream.start()) {
                            dismissIfApplicable()
                        }
                    }
                }
                val monitorList = intent.getStringArrayListExtra(EXTRA_ADD_INTERFACES_MONITOR) ?:
                    intent.getStringExtra(EXTRA_ADD_INTERFACE_MONITOR)?.let { listOf(it) }
                if (!monitorList.isNullOrEmpty()) for (iface in monitorList) {
                    val downstream = downstreams[iface]
                    val isTethered = tetheredIfaces?.contains(iface) == true
                    if (downstream == null) {
                        val monitored = Downstream(this@TetheringService, iface, true)
                        check(downstreams.put(iface, monitored) == null)
                        if (isTethered && !monitored.start()) dismissIfApplicable()
                    } else {
                        downstream.monitor = true
                        if (isTethered && !downstream.started && !downstream.start()) dismissIfApplicable()
                    }
                }
                intent.getStringExtra(EXTRA_REMOVE_INTERFACE)?.also { downstreams.remove(it)?.stop() }
                onDownstreamsChangedLocked()
            } else if (downstreams.isEmpty()) stopSelf(startId)
        }
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        launch {
            unregisterReceiver()
            val downstreams = downstreams.values.toList()
            this@TetheringService.downstreams.clear()
            downstreams.forEach { it.stop() }    // force clean to prevent leakage
            cancel()
        }
        super.onDestroy()
    }

    private suspend fun unregisterReceiver() {
        if (tetherStatesRegistered) {
            withContext(Dispatchers.Main.immediate) {
                TetherStates.unregisterCallback(this@TetheringService)
            }
            tetherStatesRegistered = false
        }
        tetheredIfaces = null
        stopNetlinkNeighbours()
    }

    override fun updateNotification() {
        launch { super.updateNotification() }
    }
}
