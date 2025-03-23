package be.mygod.vpnhotspot

import android.Manifest
import android.content.Context
import android.content.Intent
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.Routing
import be.mygod.vpnhotspot.net.TetherType
import be.mygod.vpnhotspot.net.TetheringManagerCompat
import be.mygod.vpnhotspot.net.monitor.IpNeighbourMonitor
import be.mygod.vpnhotspot.tasker.TaskerPermissionManager
import be.mygod.vpnhotspot.tasker.TetheringEventConfig
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

class TetheringService : IpNeighbourMonitoringService(), TetheringManagerCompat.TetheringEventCallback, CoroutineScope {
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

        var activeTetherTypes: Set<TetherType> = emptySet() // only used for Tasker
            private set
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
        override fun Routing.configure() {
            forward()
            masquerade(masqueradeMode)
            if (app.pref.getBoolean("service.disableIpv6", true)) disableIpv6()
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

    private fun setActiveTetherTypes(value: Set<TetherType>) {
        activeTetherTypes = value
        TaskerPermissionManager.requestQuery(this, TetheringEventConfig::class.java,
            Manifest.permission.ACCESS_NETWORK_STATE)
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
                TetheringManagerCompat.registerTetheringEventCallbackCompat(this, this)
                IpNeighbourMonitor.registerCallback(this)
            }
            super.updateNotification()
        }
        launch(Dispatchers.Main) {
            binder.routingsChanged()
            setActiveTetherTypes(downstreams.keys.mapTo(mutableSetOf()) { TetherType.ofInterface(it) })
        }
    }

    override fun onBind(intent: Intent?) = binder

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        BootReceiver.startIfEnabled()
        ServiceNotification.startForeground(this)   // call this first just in case we are shutting down immediately
        launch {
            if (intent != null) {
                for (iface in intent.getStringArrayExtra(EXTRA_ADD_INTERFACES) ?: emptyArray()) {
                    if (downstreams[iface] == null) Downstream(this@TetheringService, iface).apply {
                        if (start()) check(downstreams.put(iface, this) == null) else {
                            dismissIfApplicable()
                            stop()
                        }
                    }
                }
                val monitorList = intent.getStringArrayListExtra(EXTRA_ADD_INTERFACES_MONITOR) ?:
                    intent.getStringExtra(EXTRA_ADD_INTERFACE_MONITOR)?.let { listOf(it) }
                if (!monitorList.isNullOrEmpty()) for (iface in monitorList) {
                    val downstream = downstreams[iface]
                    if (downstream == null) Downstream(this@TetheringService, iface, true).apply {
                        if (!start(true)) {
                            dismissIfApplicable()
                            stop()
                        }
                        check(downstreams.put(iface, this) == null)
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
            downstreams.values.forEach { it.stop() }    // force clean to prevent leakage
            setActiveTetherTypes(emptySet())
            cancel()
        }
        super.onDestroy()
    }

    private fun unregisterReceiver() {
        if (callbackRegistered) {
            TetheringManagerCompat.unregisterTetheringEventCallbackCompat(this, this)
            IpNeighbourMonitor.unregisterCallback(this)
            callbackRegistered = false
        }
    }

    override fun onCreate() {
        super.onCreate()
        ServiceNotification.startForeground(this)
        if (Build.VERSION.SDK_INT >= 30) {
            val tm = TetheringManagerCompat.getInstance(this)
            tm.registerTetheringEventCallback(mainExecutor, this)
            callbackRegistered = true
        }
        BluetoothTetheringAutoStarter.getInstance(this).start()
        if (Build.VERSION.SDK_INT >= 30) EthernetTetheringAutoStarter.getInstance(this).start()
        WifiTetheringAutoStarter.getInstance(this).start()
        UsbTetheringAutoStarter.getInstance(this).start()

    override fun updateNotification() {
        launch { super.updateNotification() }
    }
}
