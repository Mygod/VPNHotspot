package be.mygod.vpnhotspot

import android.content.Context
import android.content.Intent
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.Routing
import be.mygod.vpnhotspot.net.TetherStates
import be.mygod.vpnhotspot.net.TetheringManagerCompat
import be.mygod.vpnhotspot.net.Ipv6Mode
import be.mygod.vpnhotspot.net.monitor.IpNeighbourMonitor
import be.mygod.vpnhotspot.util.Event0
import be.mygod.vpnhotspot.util.TileServiceDismissHandle
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch
import kotlinx.parcelize.Parcelize
import timber.log.Timber
import java.util.concurrent.ConcurrentHashMap

class TetheringService : IpNeighbourMonitoringService(), TetherStates.Callback, CoroutineScope {
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
        val routingsChanged = Event0()
        val monitoredIfaces get() = downstreams.values.filter { it.monitor }.map { it.downstream }

        fun isActive(iface: String) = downstreams.containsKey(iface)
        fun isInactive(iface: String) = downstreams[iface]?.run { !started && monitor }
        fun monitored(iface: String) = downstreams[iface]?.monitor
    }

    private class Downstream(caller: Any, downstream: String, var monitor: Boolean = false) :
            RoutingManager(caller, downstream) {
        override suspend fun Routing.configure() {
            forward()
            masquerade(masqueradeMode)
            when (ipv6Mode) {
                Ipv6Mode.Block -> disableIpv6()
                Ipv6Mode.System -> { }
                Ipv6Mode.Nat -> ipv6Nat()
            }
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
    private val binder = Binder()
    private val downstreams = ConcurrentHashMap<String, Downstream>()
    private var callbackRegistered = false
    override val activeIfaces get() = downstreams.values.filter { it.started }.map { it.downstream }
    override val inactiveIfaces get() = downstreams.values.filter { !it.started }.map { it.downstream }

    override fun onTetheredInterfacesChanged(interfaces: List<String?>) {
        launch {
            val toRemove = downstreams.toMutableMap()   // make a copy
            for (iface in interfaces) {
                val downstream = toRemove.remove(iface) ?: continue
                if (downstream.monitor && !downstream.start()) downstream.stop()
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

    private fun onDownstreamsChangedLocked() {
        if (downstreams.isEmpty()) {
            unregisterReceiver()
            ServiceNotification.stopForeground(this)
            stopSelf()
        } else {
            binder.monitoredIfaces.also {
                if (it.isEmpty()) BootReceiver.delete<TetheringService>()
                else BootReceiver.add<TetheringService>(Starter(ArrayList(it)))
            }
            if (!callbackRegistered) {
                callbackRegistered = true
                TetherStates.registerCallback(this)
                IpNeighbourMonitor.registerCallback(this)
            }
            super.updateNotification()
        }
        launch(Dispatchers.Main) {
            binder.routingsChanged()
        }
    }

    override fun onBind(intent: Intent?) = binder

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        BootReceiver.startIfEnabled()
        ServiceNotification.startForeground(this)   // call this first just in case we are shutting down immediately
        launch {
            if (intent != null) {
                for (iface in intent.getStringArrayExtra(EXTRA_ADD_INTERFACES) ?: emptyArray()) {
                    if (downstreams[iface] == null) {
                        val downstream = Downstream(this@TetheringService, iface)
                        check(downstreams.put(iface, downstream) == null)
                        if (!downstream.start()) {
                            dismissIfApplicable()
                            downstreams.remove(iface, downstream)
                            downstream.stop()
                        }
                    }
                }
                val monitorList = intent.getStringArrayListExtra(EXTRA_ADD_INTERFACES_MONITOR) ?:
                    intent.getStringExtra(EXTRA_ADD_INTERFACE_MONITOR)?.let { listOf(it) }
                if (!monitorList.isNullOrEmpty()) for (iface in monitorList) {
                    val downstream = downstreams[iface]
                    if (downstream == null) {
                        val monitored = Downstream(this@TetheringService, iface, true)
                        check(downstreams.put(iface, monitored) == null)
                        if (!monitored.start(true)) {
                            dismissIfApplicable()
                            monitored.stop()
                        }
                    } else downstream.monitor = true
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

    private fun unregisterReceiver() {
        if (callbackRegistered) {
            TetherStates.unregisterCallback(this)
            IpNeighbourMonitor.unregisterCallback(this)
            callbackRegistered = false
        }
    }

    override fun updateNotification() {
        launch { super.updateNotification() }
    }
}
