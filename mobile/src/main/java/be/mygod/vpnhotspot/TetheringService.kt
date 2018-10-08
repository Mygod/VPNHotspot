package be.mygod.vpnhotspot

import android.content.Intent
import android.content.IntentFilter
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.manage.TetheringFragment
import be.mygod.vpnhotspot.net.Routing
import be.mygod.vpnhotspot.net.TetheringManager
import be.mygod.vpnhotspot.net.monitor.IpNeighbourMonitor
import be.mygod.vpnhotspot.util.broadcastReceiver
import be.mygod.vpnhotspot.widget.SmartSnackbar
import timber.log.Timber

class TetheringService : IpNeighbourMonitoringService() {
    companion object {
        const val EXTRA_ADD_INTERFACE = "interface.add"
        const val EXTRA_REMOVE_INTERFACE = "interface.remove"
    }

    inner class Binder : android.os.Binder() {
        var fragment: TetheringFragment? = null

        fun isActive(iface: String): Boolean = synchronized(routings) { routings.keys.contains(iface) }
    }

    private val binder = Binder()
    private val routings = HashMap<String, Routing?>()
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
        if (routings.isEmpty()) {
            unregisterReceiver()
            ServiceNotification.stopForeground(this)
            stopSelf()
        } else {
            if (!receiverRegistered) {
                receiverRegistered = true
                registerReceiver(receiver, IntentFilter(TetheringManager.ACTION_TETHER_STATE_CHANGED))
                app.cleanRoutings[this] = {
                    synchronized(routings) {
                        for (iface in routings.keys) routings.put(iface, null)?.stop()
                        updateRoutingsLocked()
                    }
                }
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
        app.handler.post { binder.fragment?.adapter?.notifyDataSetChanged() }
    }

    override fun onBind(intent: Intent?) = binder

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent != null) {
            val iface = intent.getStringExtra(EXTRA_ADD_INTERFACE)
            synchronized(routings) {
                if (iface != null) routings[iface] = null
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
            app.cleanRoutings -= this
            IpNeighbourMonitor.unregisterCallback(this)
            receiverRegistered = false
        }
    }
}
