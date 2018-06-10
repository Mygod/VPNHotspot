package be.mygod.vpnhotspot

import android.content.Intent
import android.content.IntentFilter
import android.widget.Toast
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.manage.TetheringFragment
import be.mygod.vpnhotspot.net.IpNeighbourMonitor
import be.mygod.vpnhotspot.net.Routing
import be.mygod.vpnhotspot.net.TetheringManager
import be.mygod.vpnhotspot.net.UpstreamMonitor
import be.mygod.vpnhotspot.util.broadcastReceiver
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
            val failed = (routings.keys - TetheringManager.getTetheredIfaces(intent.extras))
                    .any { routings.remove(it)?.stop() == false }
            if (failed) Toast.makeText(this, getText(R.string.noisy_su_failure), Toast.LENGTH_SHORT).show()
            updateRoutingsLocked()
        }
    }
    override val activeIfaces get() = synchronized(routings) { routings.keys.toList() }

    private fun updateRoutingsLocked() {
        if (routings.isNotEmpty()) {
            val upstream = upstream
            if (upstream != null) {
                var failed = false
                for ((downstream, value) in routings) if (value == null || value.upstream != upstream)
                    try {
                        if (value?.stop() == false) failed = true
                        // system tethering already has working forwarding rules
                        // so it doesn't make sense to add additional forwarding rules
                        val routing = Routing(upstream, downstream).rule().forward().masquerade().dnsRedirect(dns)
                        if (app.pref.getBoolean("service.disableIpv6", false)) routing.disableIpv6()
                        routings[downstream] = routing
                        if (!routing.start()) failed = true
                    } catch (e: SocketException) {
                        e.printStackTrace()
                        Crashlytics.logException(e)
                        routings.remove(downstream)
                        failed = true
                    }
                if (failed) Toast.makeText(this, getText(R.string.noisy_su_failure), Toast.LENGTH_SHORT).show()
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

    override fun onStartCommand(intent: Intent, flags: Int, startId: Int): Int {
        val iface = intent.getStringExtra(EXTRA_ADD_INTERFACE)
        synchronized(routings) {
            if (iface != null) routings[iface] = null
            if (routings.remove(intent.getStringExtra(EXTRA_REMOVE_INTERFACE))?.stop() == false)
                Toast.makeText(this, getText(R.string.noisy_su_failure), Toast.LENGTH_SHORT).show()
            updateRoutingsLocked()
        }
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
        var failed = false
        synchronized(routings) {
            for ((iface, routing) in routings) {
                if (routing?.stop() == false) failed = true
                routings[iface] = null
            }
        }
        if (failed) Toast.makeText(this, getText(R.string.noisy_su_failure), Toast.LENGTH_SHORT).show()
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
