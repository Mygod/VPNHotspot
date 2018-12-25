package be.mygod.vpnhotspot

import android.content.Intent
import android.content.IntentFilter
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.Routing
import be.mygod.vpnhotspot.net.TetherType
import be.mygod.vpnhotspot.net.TetheringManager
import be.mygod.vpnhotspot.net.monitor.IpNeighbourMonitor
import be.mygod.vpnhotspot.net.wifi.WifiDoubleLock
import be.mygod.vpnhotspot.util.Event0
import be.mygod.vpnhotspot.util.broadcastReceiver
import be.mygod.vpnhotspot.widget.SmartSnackbar
import timber.log.Timber

class TetheringService : IpNeighbourMonitoringService() {
    companion object {
        const val EXTRA_ADD_INTERFACES = "interface.add"
        const val EXTRA_REMOVE_INTERFACE = "interface.remove"
    }

    inner class Binder : android.os.Binder() {
        val routingsChanged = Event0()

        fun isActive(iface: String): Boolean = synchronized(routings) { routings.containsKey(iface) }
    }

    private val binder = Binder()
    private val routings = HashMap<String, Routing?>()
    private var locked = false
    private var receiverRegistered = false
    private val receiver = broadcastReceiver { _, intent ->
        val extras = intent.extras ?: return@broadcastReceiver
        synchronized(routings) {
            for (iface in routings.keys - TetheringManager.getTetheredIfaces(extras))
                routings.remove(iface)?.revert()
            updateRoutingsLocked()
        }
    }
    override val activeIfaces get() = synchronized(routings) { routings.keys.toList() }

    private fun updateRoutingsLocked() {
        if (locked && routings.keys.all { !TetherType.ofInterface(it).isWifi }) {
            WifiDoubleLock.release()
            locked = false
        }
        if (routings.isEmpty()) {
            unregisterReceiver()
            ServiceNotification.stopForeground(this)
            stopSelf()
        } else {
            if (!receiverRegistered) {
                receiverRegistered = true
                registerReceiver(receiver, IntentFilter(TetheringManager.ACTION_TETHER_STATE_CHANGED))
                app.onPreCleanRoutings[this] = {
                    synchronized(routings) { for (iface in routings.keys) routings.put(iface, null)?.stop() }
                }
                app.onRoutingsCleaned[this] = { synchronized(routings) { updateRoutingsLocked() } }
                IpNeighbourMonitor.registerCallback(this)
            }
            val disableIpv6 = app.pref.getBoolean("service.disableIpv6", false)
            val iterator = routings.iterator()
            while (iterator.hasNext()) {
                val (downstream, value) = iterator.next()
                if (value != null) continue
                try {
                    routings[downstream] = Routing(downstream).apply {
                        try {
                            if (app.dhcpWorkaround) dhcpWorkaround()
                            // system tethering already has working forwarding rules
                            // so it doesn't make sense to add additional forwarding rules
                            forward()
                            if (app.masquerade) masquerade()
                            if (disableIpv6) disableIpv6()
                            commit()
                        } catch (e: Exception) {
                            revert()
                            throw e
                        }
                    }
                } catch (e: Exception) {
                    Timber.w(e)
                    SmartSnackbar.make(e).show()
                    iterator.remove()
                }
            }
            if (routings.isEmpty()) {
                updateRoutingsLocked()
                return
            }
            updateNotification()
        }
        app.handler.post { binder.routingsChanged() }
    }

    override fun onBind(intent: Intent?) = binder

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent != null) {
            val ifaces = intent.getStringArrayExtra(EXTRA_ADD_INTERFACES) ?: emptyArray()
            synchronized(routings) {
                for (iface in ifaces) {
                    routings[iface] = null
                    if (TetherType.ofInterface(iface).isWifi && !locked) {
                        WifiDoubleLock.acquire()
                        locked = true
                    }
                }
                routings.remove(intent.getStringExtra(EXTRA_REMOVE_INTERFACE))?.revert()
                updateRoutingsLocked()
            }
        } else if (routings.isEmpty()) stopSelf(startId)
        return START_NOT_STICKY
    }

    override fun onDestroy() {
        unregisterReceiver()
        super.onDestroy()
    }

    private fun unregisterReceiver() {
        if (receiverRegistered) {
            unregisterReceiver(receiver)
            app.onPreCleanRoutings -= this
            app.onRoutingsCleaned -= this
            IpNeighbourMonitor.unregisterCallback(this)
            receiverRegistered = false
        }
    }
}
