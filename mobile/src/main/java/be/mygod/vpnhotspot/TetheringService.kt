package be.mygod.vpnhotspot

import android.content.Intent
import android.content.IntentFilter
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.Routing
import be.mygod.vpnhotspot.net.TetherType
import be.mygod.vpnhotspot.net.TetheringManager
import be.mygod.vpnhotspot.net.monitor.IpNeighbourMonitor
import be.mygod.vpnhotspot.util.Event0
import be.mygod.vpnhotspot.util.broadcastReceiver
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.launch

class TetheringService : IpNeighbourMonitoringService() {
    companion object {
        const val EXTRA_ADD_INTERFACES = "interface.add"
        const val EXTRA_ADD_INTERFACE_MONITOR = "interface.add.monitor"
        const val EXTRA_REMOVE_INTERFACE = "interface.remove"
    }

    inner class Binder : android.os.Binder() {
        val routingsChanged = Event0()
        val monitoredIfaces get() = synchronized(downstreams) {
            downstreams.values.filter { it.monitor }.map { it.downstream }
        }

        fun isActive(iface: String) = synchronized(downstreams) { downstreams.containsKey(iface) }
        fun isInactive(iface: String) = synchronized(downstreams) { downstreams[iface] }?.run { !started && monitor }
        fun monitored(iface: String) = synchronized(downstreams) { downstreams[iface] }?.monitor
    }

    private inner class Downstream(caller: Any, downstream: String, var monitor: Boolean = false) :
            RoutingManager(caller, downstream, TetherType.ofInterface(downstream).isWifi) {
        override fun Routing.configure() {
            forward()
            masquerade(RoutingManager.masqueradeMode)
            if (app.pref.getBoolean("service.disableIpv6", true)) disableIpv6()
            commit()
        }
    }

    private val binder = Binder()
    private val downstreams = mutableMapOf<String, Downstream>()
    private var receiverRegistered = false
    private val receiver = broadcastReceiver { _, intent ->
        val extras = intent.extras ?: return@broadcastReceiver
        synchronized(downstreams) {
            val toRemove = downstreams.toMutableMap()   // make a copy
            for (iface in TetheringManager.getTetheredIfaces(extras)) {
                val downstream = toRemove.remove(iface) ?: continue
                if (downstream.monitor) downstream.start()
            }
            for ((iface, downstream) in toRemove) {
                if (downstream.monitor) downstream.stop() else downstreams.remove(iface)?.destroy()
            }
            onDownstreamsChangedLocked()
        }
    }
    override val activeIfaces get() = synchronized(downstreams) {
        downstreams.values.filter { it.started }.map { it.downstream }
    }
    override val inactiveIfaces get() = synchronized(downstreams) {
        downstreams.values.filter { !it.started }.map { it.downstream }
    }

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
        GlobalScope.launch(Dispatchers.Main) { binder.routingsChanged() }
    }

    override fun onBind(intent: Intent?) = binder

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent != null) synchronized(downstreams) {
            for (iface in intent.getStringArrayExtra(EXTRA_ADD_INTERFACES) ?: emptyArray()) {
                if (downstreams[iface] == null) Downstream(this, iface).apply {
                    if (start()) check(downstreams.put(iface, this) == null) else destroy()
                }
            }
            intent.getStringExtra(EXTRA_ADD_INTERFACE_MONITOR)?.let { iface ->
                val downstream = downstreams[iface]
                if (downstream == null) Downstream(this, iface, true).apply {
                    start()
                    check(downstreams.put(iface, this) == null)
                    downstreams[iface] = this
                } else downstream.monitor = true
            }
            downstreams.remove(intent.getStringExtra(EXTRA_REMOVE_INTERFACE))?.destroy()
            updateNotification()    // call this first just in case we are shutting down immediately
            onDownstreamsChangedLocked()
        } else if (downstreams.isEmpty()) stopSelf(startId)
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        synchronized(downstreams) {
            downstreams.values.forEach { it.destroy() } // force clean to prevent leakage
            unregisterReceiver()
        }
        super.onDestroy()
    }

    private fun unregisterReceiver() {
        if (receiverRegistered) {
            unregisterReceiver(receiver)
            IpNeighbourMonitor.unregisterCallback(this)
            receiverRegistered = false
        }
    }
}
