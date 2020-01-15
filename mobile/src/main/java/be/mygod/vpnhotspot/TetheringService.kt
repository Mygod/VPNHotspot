package be.mygod.vpnhotspot

import android.content.Intent
import android.content.IntentFilter
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.Routing
import be.mygod.vpnhotspot.net.TetherType
import be.mygod.vpnhotspot.net.TetheringManager
import be.mygod.vpnhotspot.net.TetheringManager.tetheredIfaces
import be.mygod.vpnhotspot.net.monitor.IpNeighbourMonitor
import be.mygod.vpnhotspot.util.Event0
import be.mygod.vpnhotspot.util.broadcastReceiver
import be.mygod.vpnhotspot.util.ensureReceiverUnregistered
import kotlinx.coroutines.*
import java.util.concurrent.ConcurrentHashMap

class TetheringService : IpNeighbourMonitoringService(), CoroutineScope {
    companion object {
        const val EXTRA_ADD_INTERFACES = "interface.add"
        const val EXTRA_ADD_INTERFACE_MONITOR = "interface.add.monitor"
        const val EXTRA_REMOVE_INTERFACE = "interface.remove"
    }

    inner class Binder : android.os.Binder() {
        val routingsChanged = Event0()
        val monitoredIfaces get() = downstreams.values.filter { it.monitor }.map { it.downstream }

        fun isActive(iface: String) = downstreams.containsKey(iface)
        fun isInactive(iface: String) = downstreams[iface]?.run { !started && monitor }
        fun monitored(iface: String) = downstreams[iface]?.monitor
    }

    private inner class Downstream(caller: Any, downstream: String, var monitor: Boolean = false) :
            RoutingManager(caller, downstream, TetherType.ofInterface(downstream).isWifi) {
        override fun Routing.configure() {
            forward()
            masquerade(masqueradeMode)
            if (app.pref.getBoolean("service.disableIpv6", true)) disableIpv6()
            commit()
        }
    }

    /**
     * Writes and critical reads to downstreams should be protected with this context.
     */
    private val dispatcher = newSingleThreadContext("TetheringService")
    override val coroutineContext = dispatcher + Job()
    private val binder = Binder()
    private val downstreams = ConcurrentHashMap<String, Downstream>()
    private var receiverRegistered = false
    private val receiver = broadcastReceiver { _, intent ->
        launch {
            val toRemove = downstreams.toMutableMap()   // make a copy
            for (iface in intent.tetheredIfaces ?: return@launch) {
                val downstream = toRemove.remove(iface) ?: continue
                if (downstream.monitor) downstream.start()
            }
            for ((iface, downstream) in toRemove) {
                if (!downstream.monitor) check(downstreams.remove(iface, downstream))
                downstream.stop()
            }
            onDownstreamsChangedLocked()
        }
    }
    override val activeIfaces get() = downstreams.values.filter { it.started }.map { it.downstream }
    override val inactiveIfaces get() = downstreams.values.filter { !it.started }.map { it.downstream }

    private fun onDownstreamsChangedLocked() {
        if (downstreams.isEmpty()) {
            unregisterReceiver()
            ServiceNotification.stopForeground(this)
            stopSelf()
        } else {
            if (!receiverRegistered) {
                receiverRegistered = true
                registerReceiver(receiver, IntentFilter(TetheringManager.ACTION_TETHER_STATE_CHANGED))
                IpNeighbourMonitor.registerCallback(this)
            }
            updateNotification()
        }
        launch(Dispatchers.Main) { binder.routingsChanged() }
    }

    override fun onBind(intent: Intent?) = binder

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        launch {
            if (intent != null) {
                for (iface in intent.getStringArrayExtra(EXTRA_ADD_INTERFACES) ?: emptyArray()) {
                    if (downstreams[iface] == null) Downstream(this@TetheringService, iface).apply {
                        if (start()) check(downstreams.put(iface, this) == null) else stop()
                    }
                }
                intent.getStringExtra(EXTRA_ADD_INTERFACE_MONITOR)?.also { iface ->
                    val downstream = downstreams[iface]
                    if (downstream == null) Downstream(this@TetheringService, iface, true).apply {
                        start()
                        check(downstreams.put(iface, this) == null)
                        downstreams[iface] = this
                    } else downstream.monitor = true
                }
                intent.getStringExtra(EXTRA_REMOVE_INTERFACE)?.also { downstreams.remove(it)?.stop() }
                updateNotification()    // call this first just in case we are shutting down immediately
                onDownstreamsChangedLocked()
            } else if (downstreams.isEmpty()) stopSelf(startId)
        }
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        launch {
            unregisterReceiver()
            downstreams.values.forEach { it.stop() }    // force clean to prevent leakage
            cancel()
            dispatcher.close()
        }
        super.onDestroy()
    }

    private fun unregisterReceiver() {
        if (receiverRegistered) {
            ensureReceiverUnregistered(receiver)
            IpNeighbourMonitor.unregisterCallback(this)
            receiverRegistered = false
        }
    }
}
