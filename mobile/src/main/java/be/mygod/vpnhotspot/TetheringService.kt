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
import com.crashlytics.android.Crashlytics
import java.net.InetAddress
import java.net.SocketException

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
                routings.remove(iface)?.stop()
            updateRoutingsLocked()
        }
    }
    override val activeIfaces get() = synchronized(routings) { routings.keys.toList() }

    private fun updateRoutingsLocked() {
        if (routings.isNotEmpty()) {
            val upstream = upstream
            if (upstream != null) {
                var failed = false
                val iterator = routings.iterator()
                while (iterator.hasNext()) {
                    val (downstream, value) = iterator.next()
                    if (value != null && value.upstream == upstream) continue
                    try {
                        routings[downstream] = Routing(upstream, downstream).apply {
                            if (app.dhcpWorkaround) dhcpWorkaround()
                            // system tethering already has working forwarding rules
                            // so it doesn't make sense to add additional forwarding rules
                            rule()
                            forward()
                            if (app.masquerade) masquerade()
                            dnsRedirect(dns)
                            if (app.pref.getBoolean("service.disableIpv6", false)) disableIpv6()
                            if (!start()) failed = true
                        }
                    } catch (e: SocketException) {
                        e.printStackTrace()
                        Crashlytics.logException(e)
                        iterator.remove()
                        failed = true
                    }
                }
                if (failed) SmartSnackbar.make(R.string.noisy_su_failure).show()
            } else if (!receiverRegistered) {
                registerReceiver(receiver, IntentFilter(TetheringManager.ACTION_TETHER_STATE_CHANGED))
                app.cleanRoutings[this] = {
                    synchronized(routings) {
                        for (iface in routings.keys) routings[iface] = null
                        updateRoutingsLocked()
                    }
                }
                IpNeighbourMonitor.registerCallback(this)
                UpstreamMonitor.registerCallback(this)
                receiverRegistered = true
            }
            updateNotification()
        }
        if (routings.isEmpty()) {
            unregisterReceiver()
            ServiceNotification.stopForeground(this)
            stopSelf()
        }
        app.handler.post { binder.fragment?.adapter?.notifyDataSetChanged() }
    }

    override fun onBind(intent: Intent?) = binder

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent != null) {
            val iface = intent.getStringExtra(EXTRA_ADD_INTERFACE)
            synchronized(routings) {
                if (iface != null) routings[iface] = null
                routings.remove(intent.getStringExtra(EXTRA_REMOVE_INTERFACE))?.stop()
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
                routing?.stop()
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
