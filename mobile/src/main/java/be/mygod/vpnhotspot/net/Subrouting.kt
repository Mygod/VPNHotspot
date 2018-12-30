package be.mygod.vpnhotspot.net

import be.mygod.vpnhotspot.net.Routing.Companion.iptablesAdd
import be.mygod.vpnhotspot.net.Routing.Companion.iptablesInsert
import be.mygod.vpnhotspot.net.monitor.IpNeighbourMonitor
import be.mygod.vpnhotspot.net.monitor.TrafficRecorder
import be.mygod.vpnhotspot.room.AppDatabase
import be.mygod.vpnhotspot.room.lookup
import be.mygod.vpnhotspot.room.macToLong
import be.mygod.vpnhotspot.util.RootSession
import be.mygod.vpnhotspot.util.computeIfAbsentCompat
import be.mygod.vpnhotspot.widget.SmartSnackbar
import timber.log.Timber
import java.net.Inet4Address
import java.net.InetAddress

/**
 * The only case when upstream is null is on API 23- and we are using system default rules.
 */
class Subrouting(private val parent: Routing, priority: Int, val upstream: String? = null) :
        IpNeighbourMonitor.Callback, AutoCloseable {
    private inner class Subroute(private val ip: Inet4Address, mac: String) : AutoCloseable {
        private val transaction = RootSession.beginTransaction().safeguard {
            val downstream = parent.downstream
            val address = ip.hostAddress
            if (upstream == null) {
                // otw allow downstream packets to be redirected to anywhere
                // because we don't wanna keep track of default network changes
                iptablesInsert("vpnhotspot_fwd -i $downstream -s $address -j ACCEPT")
                iptablesInsert("vpnhotspot_fwd -o $downstream -d $address -m state --state ESTABLISHED,RELATED -j ACCEPT")
            } else {
                iptablesInsert("vpnhotspot_fwd -i $downstream -s $address -o $upstream -j ACCEPT")
                iptablesInsert("vpnhotspot_fwd -i $upstream -o $downstream -d $address -m state --state ESTABLISHED,RELATED -j ACCEPT")
            }
        }

        init {
            try {
                TrafficRecorder.register(ip, upstream, parent.downstream, mac)
            } catch (e: Exception) {
                close()
                throw e
            }
        }

        override fun close() {
            TrafficRecorder.unregister(ip, upstream, parent.downstream)
            transaction.revert()
        }
    }

    private val transaction = RootSession.beginTransaction().safeguard {
        if (upstream != null) {
            val downstream = parent.downstream
            exec("ip rule add from all iif $downstream lookup $upstream priority $priority",
                    // by the time stopScript is called, table entry for upstream may already get removed
                    "ip rule del from all iif $downstream priority $priority")
        }
        // note: specifying -i wouldn't work for POSTROUTING
        if (parent.hasMasquerade) {
            val hostSubnet = parent.hostSubnet
            iptablesAdd(if (upstream == null) "vpnhotspot_masquerade -s $hostSubnet -j MASQUERADE" else
                "vpnhotspot_masquerade -s $hostSubnet -o $upstream -j MASQUERADE", "nat")
        }
    }
    private val subroutes = HashMap<InetAddress, Subroute>()

    init {
        Timber.d("Subrouting initialized from %s to %s", parent.downstream, upstream)
        try {
            IpNeighbourMonitor.registerCallback(this)
        } catch (e: Exception) {
            close()
            revert()
            throw e
        }
    }

    /**
     * Unregister client listener. This should be always called even after clean.
     */
    override fun close() {
        IpNeighbourMonitor.unregisterCallback(this)
        Timber.d("Subrouting closed from %s to %s", parent.downstream, upstream)
    }

    override fun onIpNeighbourAvailable(neighbours: List<IpNeighbour>) = synchronized(parent) {
        val toRemove = HashSet(subroutes.keys)
        for (neighbour in neighbours) {
            if (neighbour.dev != parent.downstream || neighbour.ip !is Inet4Address ||
                    AppDatabase.instance.clientRecordDao.lookup(neighbour.lladdr.macToLong()).blocked) continue
            toRemove.remove(neighbour.ip)
            try {
                subroutes.computeIfAbsentCompat(neighbour.ip) { Subroute(neighbour.ip, neighbour.lladdr) }
            } catch (e: Exception) {
                Timber.w(e)
                SmartSnackbar.make(e).show()
            }
        }
        if (toRemove.isNotEmpty()) {
            TrafficRecorder.update()    // record stats before removing rules to prevent stats losing
            for (address in toRemove) subroutes.remove(address)!!.close()
        }
    }

    fun revert() {
        subroutes.forEach { (_, subroute) -> subroute.close() }
        transaction.revert()
    }
}
