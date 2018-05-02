package be.mygod.vpnhotspot

import android.app.Service
import android.os.Bundle
import android.support.v4.content.LocalBroadcastManager
import android.widget.Toast
import be.mygod.vpnhotspot.net.*
import java.net.InetAddress
import java.net.SocketException

abstract class BaseTetheringService : Service(), VpnMonitor.Callback, IpNeighbourMonitor.Callback {
    protected val routings = HashMap<String, Routing?>()
    private var neighbours = emptyList<IpNeighbour>()
    private var upstream: String? = null
    private var dns: List<InetAddress> = emptyList()
    private var receiverRegistered = false
    private val receiver = broadcastReceiver { _, intent ->
        synchronized(routings) {
            when (intent.action) {
                TetheringManager.ACTION_TETHER_STATE_CHANGED -> onTetherStateChangedLocked(intent.extras)
                App.ACTION_CLEAN_ROUTINGS -> for (iface in routings.keys) routings[iface] = null
            }
            updateRoutingsLocked()
        }
    }

    protected abstract fun onTetherStateChangedLocked(extras: Bundle)

    protected fun removeRoutingsLocked(ifaces: Set<String>) {
        val failed = ifaces.any { routings.remove(it)?.stop() == false }
        if (failed) Toast.makeText(this, getText(R.string.noisy_su_failure), Toast.LENGTH_SHORT).show()
    }

    protected open fun updateRoutingsLocked() {
        if (routings.isNotEmpty()) {
            val upstream = upstream
            if (upstream != null) {
                var failed = false
                for ((downstream, value) in routings) if (value == null) try {
                    // system tethering already has working forwarding rules
                    // so it doesn't make sense to add additional forwarding rules
                    val routing = Routing(upstream, downstream).rule().forward().masquerade().dnsRedirect(dns)
                    routings[downstream] = routing
                    if (!routing.start()) failed = true
                } catch (e: SocketException) {
                    e.printStackTrace()
                    routings.remove(downstream)
                    failed = true
                }
                if (failed) Toast.makeText(this, getText(R.string.noisy_su_failure), Toast.LENGTH_SHORT).show()
            } else registerReceiver()
            postIpNeighbourAvailable()
        }
        if (routings.isEmpty()) {
            unregisterReceiver()
            ServiceNotification.stopForeground(this)
            stopSelf()
        }
    }

    override fun onAvailable(ifname: String, dns: List<InetAddress>) {
        check(upstream == null || upstream == ifname)
        upstream = ifname
        this.dns = dns
        synchronized(routings) { updateRoutingsLocked() }
    }

    override fun onLost(ifname: String) {
        check(upstream == null || upstream == ifname)
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

    override fun onIpNeighbourAvailable(neighbours: Map<String, IpNeighbour>) {
        this.neighbours = neighbours.values.toList()
    }
    override fun postIpNeighbourAvailable() {
        val sizeLookup = neighbours.groupBy { it.dev }.mapValues { (_, neighbours) ->
            neighbours
                    .filter { it.state != IpNeighbour.State.FAILED }
                    .distinctBy { it.lladdr }
                    .size
        }
        ServiceNotification.startForeground(this, synchronized(routings) {
            routings.keys.associate { Pair(it, sizeLookup[it] ?: 0) }
        })
    }

    override fun onDestroy() {
        unregisterReceiver()
        super.onDestroy()
    }

    protected fun registerReceiver() {
        if (!receiverRegistered) {
            registerReceiver(receiver, intentFilter(TetheringManager.ACTION_TETHER_STATE_CHANGED))
            LocalBroadcastManager.getInstance(this)
                    .registerReceiver(receiver, intentFilter(App.ACTION_CLEAN_ROUTINGS))
            IpNeighbourMonitor.registerCallback(this)
            VpnMonitor.registerCallback(this)
            receiverRegistered = true
        }
    }
    private fun unregisterReceiver() {
        if (receiverRegistered) {
            unregisterReceiver(receiver)
            LocalBroadcastManager.getInstance(this).unregisterReceiver(receiver)
            IpNeighbourMonitor.unregisterCallback(this)
            VpnMonitor.unregisterCallback(this)
            upstream = null
            receiverRegistered = false
        }
    }
}
