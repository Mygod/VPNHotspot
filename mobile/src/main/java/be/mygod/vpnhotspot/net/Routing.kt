package be.mygod.vpnhotspot.net

import android.os.Build
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.net.monitor.DefaultNetworkMonitor
import be.mygod.vpnhotspot.net.monitor.IpNeighbourMonitor
import be.mygod.vpnhotspot.net.monitor.TrafficRecorder
import be.mygod.vpnhotspot.net.monitor.UpstreamMonitor
import be.mygod.vpnhotspot.room.AppDatabase
import be.mygod.vpnhotspot.util.RootSession
import be.mygod.vpnhotspot.util.computeIfAbsentCompat
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.runBlocking
import timber.log.Timber
import java.net.*

/**
 * A transaction wrapper that helps set up routing environment.
 *
 * Once revert is called, this object no longer serves any purpose.
 */
class Routing(private val caller: Any, private val downstream: String) : IpNeighbourMonitor.Callback {
    companion object {
        /**
         * Since Android 5.0, RULE_PRIORITY_TETHERING = 18000.
         * This also works for Wi-Fi direct where there's no rule at 18000.
         *
         * We override system tethering rules by adding our own rules at higher priority.
         *
         * Source: https://android.googlesource.com/platform/system/netd/+/b9baf26/server/RouteController.cpp#65
         */
        private const val RULE_PRIORITY_UPSTREAM = 17800
        private const val RULE_PRIORITY_UPSTREAM_FALLBACK = 17900

        /**
         * -w <seconds> is not supported on 7.1-.
         * Fortunately there also isn't a time limit for starting a foreground service back in 7.1-.
         *
         * Source: https://android.googlesource.com/platform/external/iptables/+/android-5.0.0_r1/iptables/iptables.c#1574
         */
        val IPTABLES = if (Build.VERSION.SDK_INT >= 26) "iptables -w 1" else "iptables -w"

        /**
         * For debugging: check that we do not start a Routing for the same interface twice.
         */
        private var downstreams = mutableSetOf<String>()

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
            }
            downstreams.clear()
        }

        private fun RootSession.Transaction.iptables(command: String, revert: String) {
            val result = execQuiet(command, revert)
            val message = RootSession.checkOutput(command, result, err = false)
            if (result.err.isNotEmpty()) Timber.i(message)  // busy wait message
        }
        private fun RootSession.Transaction.iptablesAdd(content: String, table: String = "filter") =
                iptables("$IPTABLES -t $table -A $content", "$IPTABLES -t $table -D $content")
        private fun RootSession.Transaction.iptablesInsert(content: String, table: String = "filter") =
                iptables("$IPTABLES -t $table -I $content", "$IPTABLES -t $table -D $content")

        private fun RootSession.Transaction.ndc(name: String, command: String, revert: String) {
            val result = execQuiet(command, revert)
            val log = RootSession.checkOutput(command, result,
                    result.out.lastOrNull() != "200 0 $name operation succeeded")
            if (result.out.size > 1) Timber.i(log)
        }
    }

    enum class MasqueradeMode {
        None, Simple, Netd
    }

    class InterfaceNotFoundException(override val cause: Throwable) : SocketException() {
        override val message: String get() = app.getString(R.string.exception_interface_not_found)
    }

    init {
        check(downstreams.add(downstream)) { "Double routing detected" }
    }

    private val hostAddress = try {
        NetworkInterface.getByName(downstream)!!.interfaceAddresses!!.asSequence().single { it.address is Inet4Address }
    } catch (e: Exception) {
        throw InterfaceNotFoundException(e)
    }
    private val hostSubnet = "${hostAddress.address.hostAddress}/${hostAddress.networkPrefixLength}"
    private val transaction = RootSession.beginTransaction()

    private var masqueradeMode = MasqueradeMode.None

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
                when (masqueradeMode) {
                    MasqueradeMode.None -> { }  // nothing to be done here
                    // note: specifying -i wouldn't work for POSTROUTING
                    MasqueradeMode.Simple -> {
                        iptablesAdd(if (upstream == null) "vpnhotspot_masquerade -s $hostSubnet -j MASQUERADE" else
                            "vpnhotspot_masquerade -s $hostSubnet -o $upstream -j MASQUERADE", "nat")
                    }
                    MasqueradeMode.Netd -> {
                        check(upstream != null)
                        /**
                         * 0 means that there are no interface addresses coming after, which is unused anyway.
                         *
                         * https://android.googlesource.com/platform/frameworks/base/+/android-5.0.0_r1/services/core/java/com/android/server/NetworkManagementService.java#1251
                         * https://android.googlesource.com/platform/system/netd/+/android-5.0.0_r1/server/CommandListener.cpp#638
                         */
                        ndc("Nat", "ndc nat enable $downstream $upstream 0", "ndc nat disable $downstream $upstream 0")
                    }
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

    private inner class Client(private val ip: Inet4Address, mac: Long) : AutoCloseable {
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
                    runBlocking { AppDatabase.instance.clientRecordDao.lookup(neighbour.lladdr) }
                            ?.blocked == true) continue
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

    /**
     * This command is available since API 23 and also handles IPv6 forwarding.
     * https://android.googlesource.com/platform/system/netd/+/android-6.0.0_r1/server/CommandListener.cpp#527
     *
     * `requester` set by system service is assumed to be `tethering`.
     * https://android.googlesource.com/platform/frameworks/base/+/bd249a19bba38a29e617aa849b2f42c3c281eff5/services/core/java/com/android/server/NetworkManagementService.java#1241
     *
     * The fallback approach is consistent with legacy system's IP forwarding approach,
     * but may be broken when system tethering shutdown before local-only interfaces.
     */
    fun ipForward() {
        if (Build.VERSION.SDK_INT >= 23) try {
            transaction.ndc("ipfwd", "ndc ipfwd enable vpnhotspot_$downstream",
                    "ndc ipfwd disable vpnhotspot_$downstream")
            return
        } catch (e: RootSession.UnexpectedOutputException) {
            Timber.w(e)
        }
        transaction.exec("echo 1 >/proc/sys/net/ipv4/ip_forward")
    }

    /**
     * Alternative approach: ndc interface ipv6 $downstream <enable|disable>
     *
     * This approach does the same (up until now) and is easier for parsing error output.
     */
    fun disableIpv6() = transaction.exec("echo 1 >/proc/sys/net/ipv6/conf/$downstream/disable_ipv6",
            "echo 0 >/proc/sys/net/ipv6/conf/$downstream/disable_ipv6")

    fun forward() {
        transaction.execQuiet("$IPTABLES -N vpnhotspot_fwd")
        transaction.iptablesInsert("FORWARD -j vpnhotspot_fwd")
        transaction.iptablesAdd("vpnhotspot_fwd -i $downstream ! -o $downstream -j DROP")   // ensure blocking works
        // the real forwarding filters will be added in Subrouting when clients are connected
    }

    fun masquerade(mode: MasqueradeMode) {
        masqueradeMode = mode
        if (mode == MasqueradeMode.Simple) {
            transaction.execQuiet("$IPTABLES -t nat -N vpnhotspot_masquerade")
            transaction.iptablesInsert("POSTROUTING -j vpnhotspot_masquerade", "nat")
            // further rules are added when upstreams are found
        }
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
        if (dns.isNullOrBlank()) dns = null
        if (dns != currentDns?.dns) {
            currentDns?.transaction?.revert()
            currentDns = if (dns == null) null else try {
                DnsRoute(dns)
            } catch (e: RuntimeException) {
                Timber.w(e)
                SmartSnackbar.make(e).show()
                null
            }
        }
    }

    fun stop() {
        IpNeighbourMonitor.unregisterCallback(this)
        DefaultNetworkMonitor.unregisterCallback(fallbackUpstream)
        UpstreamMonitor.unregisterCallback(upstream)
    }

    fun commit(localOnly: Boolean = false) {
        transaction.commit()
        Timber.i("Started routing for $downstream by $caller")
        if (localOnly || masqueradeMode != MasqueradeMode.Netd) DefaultNetworkMonitor.registerCallback(fallbackUpstream)
        UpstreamMonitor.registerCallback(upstream)
        IpNeighbourMonitor.registerCallback(this)
    }
    fun revert() {
        stop()
        Timber.i("Stopped routing for $downstream by $caller")
        TrafficRecorder.update()    // record stats before exiting to prevent stats losing
        clients.values.forEach { it.close() }
        currentDns?.transaction?.revert()
        fallbackUpstream.subrouting?.transaction?.revert()
        upstream.subrouting?.transaction?.revert()
        transaction.revert()
        check(downstreams.remove(downstream)) { "Double reverting detected" }
    }
}
