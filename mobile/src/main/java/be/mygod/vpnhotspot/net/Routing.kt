package be.mygod.vpnhotspot.net

import android.annotation.SuppressLint
import android.annotation.TargetApi
import android.net.LinkProperties
import android.net.RouteInfo
import android.os.Build
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.net.monitor.FallbackUpstreamMonitor
import be.mygod.vpnhotspot.net.monitor.IpNeighbourMonitor
import be.mygod.vpnhotspot.net.monitor.TrafficRecorder
import be.mygod.vpnhotspot.net.monitor.UpstreamMonitor
import be.mygod.vpnhotspot.room.AppDatabase
import be.mygod.vpnhotspot.root.RootManager
import be.mygod.vpnhotspot.root.RoutingCommands
import be.mygod.vpnhotspot.util.*
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.CancellationException
import timber.log.Timber
import java.io.BufferedWriter
import java.io.IOException
import java.net.Inet4Address
import java.net.InetAddress
import java.net.NetworkInterface
import java.net.SocketException

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
        private const val RULE_PRIORITY_DNS = 17700
        private const val RULE_PRIORITY_UPSTREAM = 17800
        private const val RULE_PRIORITY_UPSTREAM_FALLBACK = 17900
        private const val RULE_PRIORITY_UPSTREAM_DISABLE_SYSTEM = 17980
        private const val RULE_PRIORITY_TETHERING = 18000

        private const val ROOT_DIR = "/system/bin/"
        const val IP = "${ROOT_DIR}ip"
        const val IPTABLES = "iptables -w"
        const val IP6TABLES = "ip6tables -w"

        fun appendCleanCommands(commands: BufferedWriter) {
            commands.appendLine("$IPTABLES -t nat -F PREROUTING")
            commands.appendLine("while $IPTABLES -D FORWARD -j vpnhotspot_fwd; do done")
            commands.appendLine("$IPTABLES -F vpnhotspot_fwd")
            commands.appendLine("$IPTABLES -X vpnhotspot_fwd")
            commands.appendLine("$IPTABLES -F vpnhotspot_acl")
            commands.appendLine("$IPTABLES -X vpnhotspot_acl")
            commands.appendLine("while $IPTABLES -t nat -D POSTROUTING -j vpnhotspot_masquerade; do done")
            commands.appendLine("$IPTABLES -t nat -F vpnhotspot_masquerade")
            commands.appendLine("$IPTABLES -t nat -X vpnhotspot_masquerade")
            commands.appendLine("while $IP6TABLES -D INPUT -j vpnhotspot_filter; do done")
            commands.appendLine("while $IP6TABLES -D FORWARD -j vpnhotspot_filter; do done")
            commands.appendLine("while $IP6TABLES -D OUTPUT -j vpnhotspot_filter; do done")
            commands.appendLine("$IP6TABLES -F vpnhotspot_filter")
            commands.appendLine("$IP6TABLES -X vpnhotspot_filter")
            commands.appendLine("while $IP rule del priority $RULE_PRIORITY_DNS; do done")
            commands.appendLine("while $IP rule del priority $RULE_PRIORITY_UPSTREAM; do done")
            commands.appendLine("while $IP rule del priority $RULE_PRIORITY_UPSTREAM_FALLBACK; do done")
            commands.appendLine("while $IP rule del priority $RULE_PRIORITY_UPSTREAM_DISABLE_SYSTEM; do done")
        }

        suspend fun clean() {
            TrafficRecorder.clean()
            RootManager.use { it.execute(RoutingCommands.Clean()) }
        }

        private fun RootSession.Transaction.iptables(command: String, revert: String) {
            val result = execQuiet(command, revert)
            val message = result.message(listOf(command), err = false)
            if (result.err.isNotEmpty()) Timber.i(message)  // busy wait message
        }
        private fun RootSession.Transaction.iptablesAdd(content: String, table: String = "filter") =
                iptables("$IPTABLES -t $table -A $content", "$IPTABLES -t $table -D $content")
        private fun RootSession.Transaction.iptablesInsert(content: String, table: String = "filter") =
                iptables("$IPTABLES -t $table -I $content", "$IPTABLES -t $table -D $content")
        private fun RootSession.Transaction.ip6tablesInsert(content: String) =
                iptables("$IP6TABLES -I $content", "$IP6TABLES -D $content")

        private fun RootSession.Transaction.ndc(name: String, command: String, revert: String? = null) {
            val result = execQuiet(command, revert)
            val suffix = "200 0 $name operation succeeded\n"
            result.check(listOf(command), !result.out.endsWith(suffix))
            if (result.out.length > suffix.length) Timber.i(result.message(listOf(command), true))
        }

        fun shouldSuppressIpError(e: RoutingCommands.UnexpectedOutputException, isAdd: Boolean = true) =
                e.result.out.isEmpty() && (e.result.exit == 2 || e.result.exit == 254) && if (isAdd) {
                    "RTNETLINK answers: File exists"
                } else {
                    "RTNETLINK answers: No such file or directory"
                } == e.result.err.trim()
    }

    private fun RootSession.Transaction.ipRule(action: String, priority: Int, rule: String = "") {
        try {
            exec("$IP rule add $rule iif $downstream $action priority $priority",
                    "$IP rule del $rule iif $downstream $action priority $priority")
        } catch (e: RoutingCommands.UnexpectedOutputException) {
            if (!shouldSuppressIpError(e)) throw e
        }
    }
    private fun RootSession.Transaction.ipRuleLookup(ifindex: Int, priority: Int, rule: String = "") =
            // https://android.googlesource.com/platform/system/netd/+/android-5.0.0_r1/server/RouteController.h#37
            ipRule("lookup ${1000 + ifindex}", priority, rule)

    enum class MasqueradeMode {
        None,
        Simple,
        /**
         * Netd does not support multiple tethering upstream below Android 9, which we heavily depend on.
         *
         * Source: https://android.googlesource.com/platform/system/netd/+/3b47c793ff7ade843b1d85a9be8461c3b4dc693e
         */
        @RequiresApi(28)
        Netd,
    }

    class InterfaceNotFoundException(override val cause: Throwable) : SocketException() {
        override val message: String get() = app.getString(R.string.exception_interface_not_found)
    }

    private val hostAddress = try {
        val iface = NetworkInterface.getByName(downstream) ?: error("iface not found")
        val addresses = iface.interfaceAddresses!!.filter { it.address is Inet4Address && it.networkPrefixLength <= 32 }
        if (addresses.size > 1) error("More than one addresses was found: $addresses")
        addresses.first()
    } catch (e: Exception) {
        throw InterfaceNotFoundException(e)
    }
    private val hostSubnet = "${hostAddress.address.hostAddress}/${hostAddress.networkPrefixLength}"
    private val transaction = RootSession.beginTransaction()

    @Volatile
    private var stopped = false
    private var masqueradeMode = MasqueradeMode.None

    private val upstreams = HashSet<String>()
    private class InterfaceGoneException(upstream: String) : IOException("Interface $upstream not found")
    private open inner class Upstream(val priority: Int) : UpstreamMonitor.Callback {
        /**
         * The only case when upstream is null is on API 23- and we are using system default rules.
         */
        inner class Subrouting(priority: Int, val upstream: String) {
            val ifindex = if (upstream.isEmpty()) 0 else if_nametoindex(upstream).also {
                if (it <= 0) throw InterfaceGoneException(upstream)
            }
            val transaction = RootSession.beginTransaction().safeguard {
                if (upstream.isEmpty()) {
                    ipRule("goto $RULE_PRIORITY_TETHERING", priority)   // skip unreachable rule
                } else ipRuleLookup(ifindex, priority)
                @TargetApi(28) when (masqueradeMode) {
                    MasqueradeMode.None -> { }  // nothing to be done here
                    MasqueradeMode.Simple -> {
                        // note: specifying -i wouldn't work for POSTROUTING
                        iptablesAdd(if (upstream.isEmpty()) {
                            "vpnhotspot_masquerade -s $hostSubnet -j MASQUERADE"
                        } else "vpnhotspot_masquerade -s $hostSubnet -o $upstream -j MASQUERADE", "nat")
                    }
                    MasqueradeMode.Netd -> {
                        check(upstream.isNotEmpty())    // fallback is only needed for repeater on API 23 < 28
                        /**
                         * 0 means that there are no interface addresses coming after, which is unused anyway.
                         *
                         * https://android.googlesource.com/platform/frameworks/base/+/android-5.0.0_r1/services/core/java/com/android/server/NetworkManagementService.java#1251
                         * https://android.googlesource.com/platform/system/netd/+/android-5.0.0_r1/server/CommandListener.cpp#638
                         */
                        ndc("Nat", "ndc nat enable $downstream $upstream 0")
                    }
                }
            }
        }

        var subrouting = mutableMapOf<String, Subrouting>()
        var dns = emptyList<Pair<InetAddress, String?>>()

        override fun onAvailable(properties: LinkProperties?) = synchronized(this@Routing) {
            if (stopped) return
            val toRemove = subrouting.keys.toMutableSet()
            for (ifname in properties?.allInterfaceNames ?: emptyList()) {
                if (toRemove.remove(ifname) || !upstreams.add(ifname)) continue
                try {
                    subrouting[ifname] = Subrouting(priority, ifname)
                } catch (e: Exception) {
                    SmartSnackbar.make(e).show()
                    if (e !is CancellationException && e !is InterfaceGoneException) Timber.w(e)
                }
            }
            for (ifname in toRemove) {
                subrouting.remove(ifname)?.transaction?.revert()
                check(upstreams.remove(ifname))
            }
            val routes = properties?.allRoutes
            dns = properties?.dnsServers?.map { dest ->
                // based on:
                // https://cs.android.com/android/platform/superproject/+/master:frameworks/base/packages/Tethering/src/com/android/networkstack/tethering/TetheringInterfaceUtils.java;l=88;drc=master
                // https://cs.android.com/android/platform/superproject/+/master:frameworks/libs/net/common/framework/android/net/util/NetUtils.java;l=44;drc=de5905fe0407a1f5e115423d56c948ee2400683d
                val size = dest.address.size
                var bestRoute: RouteInfo? = null
                for (route in routes!!) {
                    if (route.destination.rawAddress.size == size && (bestRoute == null ||
                                    bestRoute.destination.prefixLength < route.destination.prefixLength)) {
                        try {
                            if (route.matches(dest)) bestRoute = route
                        } catch (e: RuntimeException) {
                            Timber.w(e)
                        }
                    }
                }
                dest to bestRoute?.`interface`
            } ?: emptyList()
            updateDnsRoute()
        }
    }
    private val fallbackUpstream = object : Upstream(RULE_PRIORITY_UPSTREAM_FALLBACK) {
        @SuppressLint("NewApi")
        override fun onFallback() = onAvailable(LinkProperties().apply {
            interfaceName = ""
            setDnsServers(listOf(parseNumericAddress("8.8.8.8")))
        })
    }
    private val upstream = Upstream(RULE_PRIORITY_UPSTREAM)

    private inner class Client(private val ip: Inet4Address, mac: MacAddressCompat) : AutoCloseable {
        private val transaction = RootSession.beginTransaction().safeguard {
            val address = ip.hostAddress
            iptablesInsert("vpnhotspot_acl -i $downstream -s $address -j ACCEPT")
            iptablesInsert("vpnhotspot_acl -o $downstream -d $address -j ACCEPT")
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
    private val clients = mutableMapOf<InetAddress, Client>()
    override fun onIpNeighbourAvailable(neighbours: Collection<IpNeighbour>) = synchronized(this) {
        if (stopped) return
        val toRemove = HashSet(clients.keys)
        for (neighbour in neighbours) {
            if (neighbour.dev != downstream || neighbour.ip !is Inet4Address ||
                    AppDatabase.instance.clientRecordDao.lookupOrDefaultBlocking(neighbour.lladdr).blocked) continue
            toRemove.remove(neighbour.ip)
            try {
                clients.computeIfAbsent(neighbour.ip) { Client(neighbour.ip, neighbour.lladdr) }
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
        } catch (e: RoutingCommands.UnexpectedOutputException) {
            Timber.w(IOException("ndc ipfwd enable failure", e))
        }
        transaction.exec("echo 1 >/proc/sys/net/ipv4/ip_forward")
    }

    fun disableIpv6() {
        transaction.execQuiet("$IP6TABLES -N vpnhotspot_filter")
        transaction.ip6tablesInsert("INPUT -j vpnhotspot_filter")
        transaction.ip6tablesInsert("FORWARD -j vpnhotspot_filter")
        transaction.ip6tablesInsert("OUTPUT -j vpnhotspot_filter")
        transaction.ip6tablesInsert("vpnhotspot_filter -i $downstream -j REJECT")
        transaction.ip6tablesInsert("vpnhotspot_filter -o $downstream -j REJECT")
    }

    fun forward() {
        transaction.execQuiet("$IPTABLES -N vpnhotspot_fwd")
        transaction.execQuiet("$IPTABLES -N vpnhotspot_acl")
        transaction.iptablesInsert("FORWARD -j vpnhotspot_fwd")
        transaction.iptablesInsert("vpnhotspot_fwd -i $downstream -j vpnhotspot_acl")
        transaction.iptablesInsert("vpnhotspot_fwd -o $downstream -m state --state ESTABLISHED,RELATED -j vpnhotspot_acl")
        transaction.iptablesAdd("vpnhotspot_fwd -i $downstream ! -o $downstream -j REJECT") // ensure blocking works
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

    private inner class DnsRoute(val ifindex: Int, val dns: String) {
        val transaction = RootSession.beginTransaction().safeguard {
            val hostAddress = hostAddress.address.hostAddress
            if (ifindex != 0) ipRuleLookup(ifindex, RULE_PRIORITY_DNS, "to $dns")
            iptablesAdd("PREROUTING -i $downstream -p tcp -d $hostAddress --dport 53 -j DNAT --to-destination $dns", "nat")
            iptablesAdd("PREROUTING -i $downstream -p udp -d $hostAddress --dport 53 -j DNAT --to-destination $dns", "nat")
        }
    }
    private var currentDns: DnsRoute? = null
    private fun updateDnsRoute() {
        val selected = sequenceOf(upstream, fallbackUpstream).flatMap { upstream ->
            upstream.dns.asSequence().map { (server, iface) ->
                ((if (iface != null) upstream.subrouting[iface]?.ifindex else null) ?: 0) to server
            }
        }.firstOrNull { it.second is Inet4Address }
        val ifindex = selected?.first ?: 0
        var dns = selected?.second?.hostAddress
        if (dns.isNullOrBlank()) dns = null
        if (ifindex != currentDns?.ifindex || dns != currentDns?.dns) {
            currentDns?.transaction?.revert()
            currentDns = if (dns == null) null else try {
                DnsRoute(ifindex, dns)
            } catch (_: CancellationException) {
                null
            } catch (e: Exception) {
                Timber.w(e)
                SmartSnackbar.make(e).show()
                null
            }
        }
    }

    fun stop() {
        synchronized(this) { stopped = true }
        IpNeighbourMonitor.unregisterCallback(this)
        FallbackUpstreamMonitor.unregisterCallback(fallbackUpstream)
        UpstreamMonitor.unregisterCallback(upstream)
        Timber.i("Stopped routing for $downstream by $caller")
    }

    fun commit() {
        transaction.ipRule("unreachable", RULE_PRIORITY_UPSTREAM_DISABLE_SYSTEM)
        transaction.commit()
        Timber.i("Started routing for $downstream by $caller")
        FallbackUpstreamMonitor.registerCallback(fallbackUpstream)
        UpstreamMonitor.registerCallback(upstream)
        IpNeighbourMonitor.registerCallback(this, true)
    }
    fun revert() {
        stop()
        TrafficRecorder.update()    // record stats before exiting to prevent stats losing
        synchronized(this) { clients.values.forEach { it.close() } }
        currentDns?.transaction?.revert()
        fallbackUpstream.subrouting.values.forEach { it.transaction.revert() }
        upstream.subrouting.values.forEach { it.transaction.revert() }
        transaction.revert()
    }
}
