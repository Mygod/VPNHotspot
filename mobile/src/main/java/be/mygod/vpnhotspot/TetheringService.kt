package be.mygod.vpnhotspot

import android.content.Intent
import android.content.IntentFilter
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.manage.TetheringFragment
import be.mygod.vpnhotspot.net.IpNeighbourMonitor
import be.mygod.vpnhotspot.net.Routing
import be.mygod.vpnhotspot.net.TetheringManager
import be.mygod.vpnhotspot.net.UpstreamMonitor
import be.mygod.vpnhotspot.util.broadcastReceiver
import be.mygod.vpnhotspot.widget.SmartSnackbar
import timber.log.Timber
import java.net.InetAddress

class TetheringService : IpNeighbourMonitoringService(), UpstreamMonitor.Callback {
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
    private var upstream: String? = null
    private var dns: List<InetAddress> = emptyList()
    private var receiverRegistered = false
    private val receiver = broadcastReceiver { _, intent ->
        synchronized(routings) {
            for (iface in routings.keys - TetheringManager.getTetheredIfaces(intent.extras!!))
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
                        for (iface in routings.keys) routings[iface] = null
                        updateRoutingsLocked()
                    }
                }
                IpNeighbourMonitor.registerCallback(this)
                UpstreamMonitor.registerCallback(this)
            }
            val upstream = upstream
            val disableIpv6 = app.pref.getBoolean("service.disableIpv6", false)
            if (upstream != null || app.strict || disableIpv6) {
                val iterator = routings.iterator()
                while (iterator.hasNext()) {
                    val (downstream, value) = iterator.next()
                    if (value != null) if (value.upstream == upstream) continue else value.revert()
                    try {
                        routings[downstream] = Routing(upstream, downstream).apply {
                            try {
                                if (app.dhcpWorkaround) dhcpWorkaround()
                                // system tethering already has working forwarding rules
                                // so it doesn't make sense to add additional forwarding rules
                                rule()
                                // here we always enforce strict mode as fallback is handled by system which we disable
                                forward()
                                if (app.strict) overrideSystemRules()
                                if (app.masquerade) masquerade()
                                if (upstream != null) dnsRedirect(dns)
                                if (disableIpv6) disableIpv6()
                            } catch (e: Exception) {
                                revert()
                                throw e
                            } finally {
                                commit()
                            }
                        }
                    } catch (e: Exception) {
                        e.printStackTrace()
                        Timber.e(e)
                        SmartSnackbar.make(e.localizedMessage).show()
                        iterator.remove()
                    }
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

    override fun onAvailable(ifname: String, dns: List<InetAddress>) {
        if (upstream == ifname) return
        upstream = ifname
        this.dns = dns
        synchronized(routings) { updateRoutingsLocked() }
    }

    override fun onLost() {
        upstream = null
        this.dns = emptyList()
        synchronized(routings) {
            for ((iface, routing) in routings) {
                routing?.revert()
                routings[iface] = null
            }
        }
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
            UpstreamMonitor.unregisterCallback(this)
            upstream = null
            receiverRegistered = false
        }
    }
}
