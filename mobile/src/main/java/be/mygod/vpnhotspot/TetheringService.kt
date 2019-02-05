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
        const val EXTRA_REMOVE_INTERFACE = "interface.remove"
    }

    inner class Binder : android.os.Binder() {
        val routingsChanged = Event0()

        fun isActive(iface: String): Boolean = synchronized(downstreams) { downstreams.containsKey(iface) }
    }

    inner class Downstream(caller: Any, downstream: String) :
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
            for (iface in downstreams.keys - TetheringManager.getTetheredIfaces(extras))
                downstreams.remove(iface)?.stop()
            updateRoutingsLocked()
        }
    }
    override val activeIfaces get() = synchronized(downstreams) { downstreams.keys.toList() }

    private fun updateRoutingsLocked() {
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
            val iterator = downstreams.iterator()
            while (iterator.hasNext()) {
                val downstream = iterator.next().value
                if (downstream.routing == null && !downstream.initRouting()) iterator.remove()
            }
            if (downstreams.isEmpty()) {
                updateRoutingsLocked()
                return
            }
            updateNotification()
        }
        GlobalScope.launch(Dispatchers.Main) { binder.routingsChanged() }
    }

    override fun onBind(intent: Intent?) = binder

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent != null) {
            val ifaces = intent.getStringArrayExtra(EXTRA_ADD_INTERFACES) ?: emptyArray()
            synchronized(downstreams) {
                for (iface in ifaces) {
                    Downstream(this, iface).run {
                        downstreams[iface] = this
                        initRouting()
                    }
                }
                downstreams.remove(intent.getStringExtra(EXTRA_REMOVE_INTERFACE))?.stop()
                updateRoutingsLocked()
            }
        } else if (downstreams.isEmpty()) stopSelf(startId)
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        downstreams.values.forEach { it.stop() }    // force clean to prevent leakage
        unregisterReceiver()
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
