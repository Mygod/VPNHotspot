package be.mygod.vpnhotspot.net

import android.os.Build
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.net.monitor.FallbackUpstreamMonitor
import be.mygod.vpnhotspot.net.monitor.IpNeighbourMonitor
import be.mygod.vpnhotspot.net.monitor.TrafficRecorder
import be.mygod.vpnhotspot.net.monitor.UpstreamMonitor
import be.mygod.vpnhotspot.room.AppDatabase
import be.mygod.vpnhotspot.room.lookup
import be.mygod.vpnhotspot.room.macToLong
import be.mygod.vpnhotspot.util.RootSession
import be.mygod.vpnhotspot.util.computeIfAbsentCompat
import be.mygod.vpnhotspot.widget.SmartSnackbar
import timber.log.Timber
import java.lang.RuntimeException
import java.net.*
import java.util.concurrent.atomic.AtomicLong

/**
 * A transaction wrapper that helps set up routing environment.
 *
 * Once revert is called, this object no longer serves any purpose.
 */
class Routing(val downstream: String, ownerAddress: InterfaceAddress? = null) : IpNeighbourMonitor.Callback {
    companion object {
        /**
         * Since Android 5.0, RULE_PRIORITY_TETHERING = 18000.
         * This also works for Wi-Fi direct where there's no rule at 18000.
         *
         * Source: https://android.googlesource.com/platform/system/netd/+/b9baf26/server/RouteController.cpp#65
         */
        private const val RULE_PRIORITY_UPSTREAM = 17800
        private const val RULE_PRIORITY_UPSTREAM_FALLBACK = 17900

        private val dhcpWorkaroundCounter = AtomicLong()

        /**
         * -w <seconds> is not supported on 7.1-.
         * Fortunately there also isn't a time limit for starting a foreground service back in 7.1-.
         *
         * Source: https://android.googlesource.com/platform/external/iptables/+/android-5.0.0_r1/iptables/iptables.c#1574
         */
        val IPTABLES = if (Build.VERSION.SDK_INT >= 26) "iptables -w 1" else "iptables -w"

        fun clean() {
            TrafficRecorder.clean()
            RootSession.use {
                it.execQuiet("$IPTABLES -t nat -F PREROUTING")
                it.execQuiet("while $IPTABLES -D FORWARD -j vpnhotspot_fwd; do done")
                it.execQuiet("$IPTABLES -F vpnhotspot_fwd")
                it.execQuiet("$IPTABLES -X vpnhotspot_fwd")
                it.execQuiet("while $IPTABLES -t nat -D POSTROUTING -j vpnhotspot_masquerade; do done")
                it.execQuiet("$IPTABLES -t nat -F vpnhotspot_masquerade")
                it.execQuiet("$IPTABLES -t nat -X vpnhotspot_masquerade")
                it.execQuiet("while ip rule del priority $RULE_PRIORITY_UPSTREAM; do done")
                it.execQuiet("while ip rule del priority $RULE_PRIORITY_UPSTREAM_FALLBACK; do done")
                it.execQuiet("while ip rule del iif lo uidrange 0-0 lookup local_network priority 11000; do done")
            }
        }

        private fun RootSession.Transaction.iptablesAdd(content: String, table: String = "filter") =
                exec("$IPTABLES -t $table -A $content", "$IPTABLES -t $table -D $content", true)
        private fun RootSession.Transaction.iptablesInsert(content: String, table: String = "filter") =
                exec("$IPTABLES -t $table -I $content", "$IPTABLES -t $table -D $content", true)
    }

    class InterfaceNotFoundException : SocketException() {
        override val message: String get() = app.getString(R.string.exception_interface_not_found)
    }

    val hostAddress = ownerAddress ?: NetworkInterface.getByName(downstream)?.interfaceAddresses?.asSequence()
            ?.singleOrNull { it.address is Inet4Address } ?: throw InterfaceNotFoundException()
    val hostSubnet = "${hostAddress.address.hostAddress}/${hostAddress.networkPrefixLength}"
    private val transaction = RootSession.beginTransaction()

    var hasMasquerade = false

    private val upstreams = HashSet<String>()
    private open inner class Upstream(val priority: Int) : UpstreamMonitor.Callback {
        /**
         * The only case when upstream is null is on API 23- and we are using system default rules.
         */
        inner class Subrouting(priority: Int, val upstream: String? = null) {
            val transaction = RootSession.beginTransaction().safeguard {
                if (upstream != null) {
                    exec("ip rule add from all iif $downstream lookup $upstream priority $priority",
                            // by the time stopScript is called, table entry for upstream may already get removed
                            "ip rule del from all iif $downstream priority $priority")
                }
                // note: specifying -i wouldn't work for POSTROUTING
                if (hasMasquerade) {
                    iptablesAdd(if (upstream == null) "vpnhotspot_masquerade -s $hostSubnet -j MASQUERADE" else
                        "vpnhotspot_masquerade -s $hostSubnet -o $upstream -j MASQUERADE", "nat")
                }
            }
        }

        var subrouting: Subrouting? = null
        var dns: List<InetAddress> = emptyList()

        override fun onAvailable(ifname: String, dns: List<InetAddress>) = synchronized(this@Routing) {
            val subrouting = subrouting
            when {
                subrouting != null -> check(subrouting.upstream == ifname)
                !upstreams.add(ifname) -> return
                else -> this.subrouting = try {
                    Subrouting(priority, ifname)
                } catch (e: Exception) {
                    SmartSnackbar.make(e).show()
                    Timber.w(e)
                    null
                }
            }
            this.dns = dns
            updateDnsRoute()
        }

        override fun onLost() = synchronized(this@Routing) {
            val subrouting = subrouting ?: return
            // we could be removing fallback subrouting which no collision could ever happen, check before removing
            if (subrouting.upstream != null) check(upstreams.remove(subrouting.upstream))
            subrouting.transaction.revert()
            this.subrouting = null
            dns = emptyList()
            updateDnsRoute()
        }
    }
    private val fallbackUpstream = object : Upstream(RULE_PRIORITY_UPSTREAM_FALLBACK) {
        override fun onFallback() = synchronized(this@Routing) {
            check(subrouting == null)
            subrouting = try {
                Subrouting(priority)
            } catch (e: Exception) {
                SmartSnackbar.make(e).show()
                Timber.w(e)
                null
            }
            updateDnsRoute()
        }
    }
    private val upstream = Upstream(RULE_PRIORITY_UPSTREAM)

    private inner class Client(private val ip: Inet4Address, mac: String) : AutoCloseable {
        private val transaction = RootSession.beginTransaction().safeguard {
            val address = ip.hostAddress
            iptablesInsert("vpnhotspot_fwd -i $downstream -s $address -j ACCEPT")
            iptablesInsert("vpnhotspot_fwd -o $downstream -d $address -m state --state ESTABLISHED,RELATED -j ACCEPT")
        }

        init {
            try {
                TrafficRecorder.register(ip, downstream, mac)
            } catch (e: Exception) {
                close()
                throw e
            }
        }

        override fun close() {
            TrafficRecorder.unregister(ip, downstream)
            transaction.revert()
        }
    }
    private val clients = HashMap<InetAddress, Client>()
    override fun onIpNeighbourAvailable(neighbours: List<IpNeighbour>) = synchronized(this) {
        val toRemove = HashSet(clients.keys)
        for (neighbour in neighbours) {
            if (neighbour.dev != downstream || neighbour.ip !is Inet4Address ||
                    AppDatabase.instance.clientRecordDao.lookup(neighbour.lladdr.macToLong()).blocked) continue
            toRemove.remove(neighbour.ip)
            try {
                clients.computeIfAbsentCompat(neighbour.ip) { Client(neighbour.ip, neighbour.lladdr) }
            } catch (e: Exception) {
                Timber.w(e)
                SmartSnackbar.make(e).show()
            }
        }
        if (toRemove.isNotEmpty()) {
            TrafficRecorder.update()    // record stats before removing rules to prevent stats losing
            for (address in toRemove) clients.remove(address)!!.close()
        }
    }

    fun ipForward() = transaction.exec("echo 1 >/proc/sys/net/ipv4/ip_forward")

    fun disableIpv6() = transaction.exec("echo 1 >/proc/sys/net/ipv6/conf/$downstream/disable_ipv6",
            "echo 0 >/proc/sys/net/ipv6/conf/$downstream/disable_ipv6")

    fun forward() {
        transaction.execQuiet("$IPTABLES -N vpnhotspot_fwd")
        transaction.iptablesInsert("FORWARD -j vpnhotspot_fwd")
        transaction.iptablesAdd("vpnhotspot_fwd -i $downstream ! -o $downstream -j DROP")   // ensure blocking works
        // the real forwarding filters will be added in Subrouting when clients are connected
    }

    fun masquerade() {
        transaction.execQuiet("$IPTABLES -t nat -N vpnhotspot_masquerade")
        transaction.iptablesInsert("POSTROUTING -j vpnhotspot_masquerade", "nat")
        hasMasquerade = true
        // further rules are added when upstreams are found
    }

    private inner class DnsRoute(val dns: String) {
        val transaction = RootSession.beginTransaction().safeguard {
            val hostAddress = hostAddress.address.hostAddress
            iptablesAdd("PREROUTING -i $downstream -p tcp -d $hostAddress --dport 53 -j DNAT --to-destination $dns", "nat")
            iptablesAdd("PREROUTING -i $downstream -p udp -d $hostAddress --dport 53 -j DNAT --to-destination $dns", "nat")
        }
    }
    private var currentDns: DnsRoute? = null
    private fun updateDnsRoute() {
        var dns = (upstream.dns + fallbackUpstream.dns).firstOrNull { it is Inet4Address }?.hostAddress
                ?: app.pref.getString("service.dns", null)
        if (dns.isNullOrBlank()) dns = "8.8.8.8"
        if (dns != currentDns?.dns) {
            currentDns?.transaction?.revert()
            currentDns = try {
                DnsRoute(dns)
            } catch (e: RuntimeException) {
                Timber.w(e)
                SmartSnackbar.make(e).show()
                null
            }
        }
    }

    /**
     * Similarly, assuming RULE_PRIORITY_VPN_OUTPUT_TO_LOCAL = 11000.
     * Normally this is used to forward packets from remote to local, but it works anyways. It just needs to be before
     * RULE_PRIORITY_SECURE_VPN = 12000. It would be great if we can gain better understanding into why this is only
     * needed on some of the devices but not others.
     *
     * Source: https://android.googlesource.com/platform/system/netd/+/b9baf26/server/RouteController.cpp#57
     */
    fun dhcpWorkaround() {
        // workaround for adding multiple exact same rules
        // if somebody decides to do this 1000 times to break this, god bless you
        val priority = 11000 + dhcpWorkaroundCounter.getAndAdd(1) % 1000
        transaction.exec("ip rule add iif lo uidrange 0-0 lookup local_network priority $priority",
                "ip rule del iif lo uidrange 0-0 lookup local_network priority $priority")
    }

    fun stop() {
        IpNeighbourMonitor.unregisterCallback(this)
        FallbackUpstreamMonitor.unregisterCallback(fallbackUpstream)
        UpstreamMonitor.unregisterCallback(upstream)
    }

    fun commit() {
        transaction.commit()
        FallbackUpstreamMonitor.registerCallback(fallbackUpstream)
        UpstreamMonitor.registerCallback(upstream)
        IpNeighbourMonitor.registerCallback(this)
    }
    fun revert() {
        stop()
        TrafficRecorder.update()    // record stats before exiting to prevent stats losing
        clients.forEach { (_, subroute) -> subroute.close() }
        fallbackUpstream.subrouting?.transaction?.revert()
        upstream.subrouting?.transaction?.revert()
        transaction.revert()
    }
}
