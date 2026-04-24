package be.mygod.vpnhotspot.net

import android.net.IpPrefix
import android.net.LinkProperties
import android.net.MacAddress
import android.os.Build
import android.os.Process
import android.system.Os
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.net.ipv6.Ipv6NatController
import be.mygod.vpnhotspot.net.ipv6.Ipv6NatProtocol
import be.mygod.vpnhotspot.net.dns.DnsForwarder
import be.mygod.vpnhotspot.net.monitor.FallbackUpstreamMonitor
import be.mygod.vpnhotspot.net.monitor.IpNeighbourMonitor
import be.mygod.vpnhotspot.net.monitor.TrafficRecorder
import be.mygod.vpnhotspot.net.monitor.UpstreamMonitor
import be.mygod.vpnhotspot.net.monitor.VpnMonitor
import be.mygod.vpnhotspot.room.AppDatabase
import be.mygod.vpnhotspot.root.RootManager
import be.mygod.vpnhotspot.root.RoutingCommands
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.RootSession
import be.mygod.vpnhotspot.util.allInterfaceNames
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.runBlocking
import timber.log.Timber
import java.io.BufferedWriter
import java.io.IOException
import java.net.Inet4Address
import java.net.Inet6Address
import java.net.InetAddress
import java.net.NetworkInterface
import java.net.SocketException
import java.security.SecureRandom

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
        private const val RULE_PRIORITY_IPV6_NAT = 17750
        private const val RULE_PRIORITY_IPV6_NAT_REPLY = 17760
        private const val RULE_PRIORITY_UPSTREAM_FALLBACK = 17900
        private const val RULE_PRIORITY_UPSTREAM_DISABLE_SYSTEM = 17980
        private const val IPV6_NAT_TABLE = 19999

        private const val ROOT_DIR = "/system/bin/"
        const val IP = "${ROOT_DIR}ip"
        const val IPTABLES = "iptables -w"
        const val IP6TABLES = "ip6tables -w"

        private fun useLocalnet() = Os.uname().release.split('.', limit = 3).let { version ->
            val major = version[0].toInt()
            // https://github.com/torvalds/linux/commit/d0daebc3d622f95db181601cb0c4a0781f74f758
            major > 3 || major == 3 && version[1].toInt() >= 6
        }

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
            commands.appendLine("while $IP6TABLES -D INPUT -j vpnhotspot_v6_input; do done")
            commands.appendLine("while $IP6TABLES -D FORWARD -j vpnhotspot_v6_forward; do done")
            commands.appendLine("while $IP6TABLES -D OUTPUT -j vpnhotspot_v6_output; do done")
            commands.appendLine("while $IP6TABLES -t mangle -D PREROUTING -j vpnhotspot_v6_tproxy; do done")
            commands.appendLine("$IP6TABLES -F vpnhotspot_v6_input")
            commands.appendLine("$IP6TABLES -X vpnhotspot_v6_input")
            commands.appendLine("$IP6TABLES -F vpnhotspot_v6_forward")
            commands.appendLine("$IP6TABLES -X vpnhotspot_v6_forward")
            commands.appendLine("$IP6TABLES -F vpnhotspot_v6_output")
            commands.appendLine("$IP6TABLES -X vpnhotspot_v6_output")
            commands.appendLine("$IP6TABLES -t mangle -F vpnhotspot_v6_tproxy")
            commands.appendLine("$IP6TABLES -t mangle -X vpnhotspot_v6_tproxy")
            commands.appendLine("while $IP -6 rule del priority $RULE_PRIORITY_IPV6_NAT; do done")
            commands.appendLine("while $IP -6 rule del priority $RULE_PRIORITY_IPV6_NAT_REPLY; do done")
            commands.appendLine("$IP -6 route flush table $IPV6_NAT_TABLE")
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
        private fun RootSession.Transaction.ip6tablesAdd(content: String, table: String = "filter") =
                iptables("$IP6TABLES -t $table -A $content", "$IP6TABLES -t $table -D $content")
        private fun RootSession.Transaction.ip6tablesInsert(content: String, table: String = "filter") =
                iptables("$IP6TABLES -t $table -I $content", "$IP6TABLES -t $table -D $content")

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
    private fun RootSession.Transaction.ip6Rule(action: String, priority: Int, rule: String = "") {
        try {
            exec("$IP -6 rule add $rule iif $downstream $action priority $priority",
                    "$IP -6 rule del $rule iif $downstream $action priority $priority")
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
        Netd,
    }

    class InterfaceNotFoundException(cause: Throwable) : SocketException() {
        init {
            initCause(cause)
        }
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
    lateinit var transaction: RootSession.Transaction

    @Volatile
    private var stopped = false
    private var masqueradeMode = MasqueradeMode.None

    private val upstreams = HashSet<String>()
    private class InterfaceGoneException(upstream: String) : IOException("Interface $upstream not found")
    private open inner class Upstream(val priority: Int) : UpstreamMonitor.Callback {
        inner class Subrouting(priority: Int, val upstream: String) {
            val ifindex = Os.if_nametoindex(upstream).also {
                if (it <= 0) throw InterfaceGoneException(upstream)
            }
            val transaction = RootSession.beginTransaction().safeguard {
                ipRuleLookup(ifindex, priority)
                when (masqueradeMode) {
                    MasqueradeMode.None -> { }  // nothing to be done here
                    // note: specifying -i wouldn't work for POSTROUTING
                    MasqueradeMode.Simple -> iptablesAdd(
                        "vpnhotspot_masquerade -s $hostSubnet -o $upstream -j MASQUERADE", "nat")
                    /**
                     * 0 means that there are no interface addresses coming after, which is unused anyway.
                     * Revert is intentionally omitted because netd tracks forwarding state globally by
                     * interface pair without ownership, so disabling here may tear down system-owned state.
                     *
                     * https://android.googlesource.com/platform/frameworks/base/+/android-5.0.0_r1/services/core/java/com/android/server/NetworkManagementService.java#1251
                     * https://android.googlesource.com/platform/system/netd/+/android-5.0.0_r1/server/CommandListener.cpp#638
                     * https://android.googlesource.com/platform/system/netd/+/e11b8688b1f99292ade06f89f957c1f7e76ceae9/server/TetherController.cpp#652
                     * https://android.googlesource.com/platform/system/netd/+/e11b8688b1f99292ade06f89f957c1f7e76ceae9/server/TetherController.h#40
                     */
                    MasqueradeMode.Netd -> ndc("Nat", "ndc nat enable $downstream $upstream 0")
                }
            }
            init {
                if (Build.VERSION.SDK_INT >= 31) try {
                    runBlocking { RootManager.use { it.execute(IpSecForwardPolicyCommand(upstream)) } }
                } catch (e: Exception) {
                    if (e is CancellationException) throw e
                    SmartSnackbar.make(e).show()
                    Timber.w(e)
                }
            }
        }

        var subrouting = mutableMapOf<String, Subrouting>()

        override fun onAvailable(properties: LinkProperties?): Unit = synchronized(this@Routing) {
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
        }
    }
    private val fallbackUpstream = Upstream(RULE_PRIORITY_UPSTREAM_FALLBACK)
    private val upstream = Upstream(RULE_PRIORITY_UPSTREAM)
    private val emptyCallback = object : UpstreamMonitor.Callback { }
    private var ipv6NatSession: Ipv6NatSession? = null
    private data class Ipv6Prefix(val subnet: String, val gateway: String, val prefixLength: Int)

    @RequiresApi(29)
    private inner class Ipv6NatSession : AutoCloseable {
        private val prefix = generatePrefix()
        private val dnsBindAddress = if (useLocalnet()) "127.0.0.1" else hostAddress.address.hostAddress
        private val interceptMark = 0x6000 + (downstream.hashCode() and 0x1fff)
        private val replyMark = 0x7000 + (downstream.hashCode() and 0x1fff)
        private val mtu = NetworkInterface.getByName(downstream)?.mtu ?: 1500
        private var generationId = 0
        private val mirroredUpstreamTables = mutableSetOf<String>()
        lateinit var ports: Ipv6NatProtocol.SessionPorts

        private val upstreamCallback = object : UpstreamMonitor.Callback {
            override fun onAvailable(properties: LinkProperties?) {
                update()
            }
        }
        private val fallbackCallback = object : UpstreamMonitor.Callback {
            override fun onAvailable(properties: LinkProperties?) {
                update()
            }
        }

        private fun buildUpstream(network: android.net.Network?) = Ipv6NatProtocol.Upstream.from(network,
            network?.let(Services.connectivity::getLinkProperties))
        private fun currentUpstreamTables() = buildSet {
            buildUpstream(UpstreamMonitor.currentNetwork)?.interfaceName?.takeIf(String::isNotEmpty)?.let(::add)
            buildUpstream(FallbackUpstreamMonitor.currentNetwork)?.interfaceName?.takeIf(String::isNotEmpty)?.let(::add)
        }
        private fun syncPrefixRoutes(config: Ipv6NatProtocol.SessionConfig) {
            val current = currentUpstreamTables()
            val subnet = "${prefix.subnet}/${prefix.prefixLength}"
            val staleSubnets = buildSet {
                for (route in config.deprecatedPrefixes) {
                    val address = InetAddress.getByName(route.address)
                    add("${IpPrefix(address, route.prefixLength).address.hostAddress}/${route.prefixLength}")
                }
            }
            for (table in mirroredUpstreamTables + current) {
                for (stale in staleSubnets) transaction.execQuiet("$IP -6 route del $stale dev $downstream table $table")
            }
            for (table in mirroredUpstreamTables - current) {
                transaction.execQuiet("$IP -6 route del $subnet dev $downstream table $table")
            }
            for (table in current - mirroredUpstreamTables) try {
                transaction.exec("$IP -6 route replace $subnet dev $downstream table $table",
                    "$IP -6 route del $subnet dev $downstream table $table")
            } catch (e: RoutingCommands.UnexpectedOutputException) {
                if (!shouldSuppressIpError(e)) throw e
            }
            mirroredUpstreamTables.clear()
            mirroredUpstreamTables.addAll(current)
        }
        private fun nextConfig(): Ipv6NatProtocol.SessionConfig {
            val gateway = InetAddress.getByName(prefix.gateway)
            val interfaceAddresses = NetworkInterface.getByName(downstream)?.interfaceAddresses ?: emptyList()
            val router = interfaceAddresses.firstNotNullOfOrNull { address ->
                val inet = address.address
                if (inet !is Inet6Address || !inet.isLinkLocalAddress || inet.isLoopbackAddress ||
                        inet.isMulticastAddress) null else inet.hostAddress?.substringBefore('%')
            } ?: throw IOException("Missing link-local router address on $downstream")
            val deprecatedPrefixes = interfaceAddresses.mapNotNull { address ->
                val inet = address.address
                if (inet !is Inet6Address || inet.isLinkLocalAddress || inet.isLoopbackAddress ||
                        inet.isMulticastAddress || inet == gateway) null else {
                    val hostAddress = inet.hostAddress ?: return@mapNotNull null
                    Ipv6NatProtocol.Route(hostAddress, address.networkPrefixLength.toInt())
                }
            }
            return Ipv6NatProtocol.SessionConfig(
                sessionId = downstream,
                generationId = ++generationId,
                downstream = downstream,
                router = router,
                gateway = prefix.gateway,
                prefixLength = prefix.prefixLength,
                replyMark = replyMark,
                dnsBindAddress = dnsBindAddress,
                mtu = mtu,
                deprecatedPrefixes = deprecatedPrefixes,
                primary = buildUpstream(UpstreamMonitor.currentNetwork),
                fallback = buildUpstream(FallbackUpstreamMonitor.currentNetwork),
            )
        }

        fun prepare() {
            val config = nextConfig()
            ports = runBlocking { Ipv6NatController.startSession(config) }
            syncPrefixRoutes(config)
            transaction.exec("$IP -6 addr add ${prefix.gateway}/${prefix.prefixLength} dev $downstream",
                "$IP -6 addr del ${prefix.gateway}/${prefix.prefixLength} dev $downstream")
            if (dnsBindAddress == "127.0.0.1") {
                transaction.exec("echo 1 >/proc/sys/net/ipv4/conf/all/route_localnet")
            }
            transaction.iptablesInsert(
                "PREROUTING -i $downstream -p tcp -d ${hostAddress.address.hostAddress} --dport 53 -j DNAT --to-destination $dnsBindAddress:${ports.dnsTcp}",
                "nat")
            transaction.iptablesInsert(
                "PREROUTING -i $downstream -p udp -d ${hostAddress.address.hostAddress} --dport 53 -j DNAT --to-destination $dnsBindAddress:${ports.dnsUdp}",
                "nat")
            transaction.execQuiet("while $IP6TABLES -D INPUT -j vpnhotspot_v6_input; do done")
            transaction.execQuiet("while $IP6TABLES -D FORWARD -j vpnhotspot_v6_forward; do done")
            transaction.execQuiet("while $IP6TABLES -D OUTPUT -j vpnhotspot_v6_output; do done")
            transaction.execQuiet("while $IP6TABLES -t mangle -D PREROUTING -j vpnhotspot_v6_tproxy; do done")
            transaction.execQuiet("$IP6TABLES -F vpnhotspot_v6_input")
            transaction.execQuiet("$IP6TABLES -F vpnhotspot_v6_forward")
            transaction.execQuiet("$IP6TABLES -F vpnhotspot_v6_output")
            transaction.execQuiet("$IP6TABLES -t mangle -F vpnhotspot_v6_tproxy")
            transaction.execQuiet("while $IP -6 rule del priority $RULE_PRIORITY_IPV6_NAT; do done")
            transaction.execQuiet("while $IP -6 rule del priority $RULE_PRIORITY_IPV6_NAT_REPLY; do done")
            transaction.execQuiet("$IP -6 route flush table $IPV6_NAT_TABLE")
            try {
                transaction.exec("$IP -6 route add local ::/0 dev lo table $IPV6_NAT_TABLE",
                    "$IP -6 route del local ::/0 dev lo table $IPV6_NAT_TABLE")
            } catch (e: RoutingCommands.UnexpectedOutputException) {
                if (!shouldSuppressIpError(e)) throw e
            }
            transaction.ip6Rule("lookup $IPV6_NAT_TABLE", RULE_PRIORITY_IPV6_NAT, "fwmark $interceptMark")
            try {
                transaction.exec("$IP -6 rule add to ${prefix.subnet}/${prefix.prefixLength} lookup $downstream priority $RULE_PRIORITY_IPV6_NAT_REPLY",
                    "$IP -6 rule del to ${prefix.subnet}/${prefix.prefixLength} lookup $downstream priority $RULE_PRIORITY_IPV6_NAT_REPLY")
            } catch (e: RoutingCommands.UnexpectedOutputException) {
                if (!shouldSuppressIpError(e)) throw e
            }
            transaction.execQuiet("$IP6TABLES -N vpnhotspot_v6_input")
            transaction.execQuiet("$IP6TABLES -N vpnhotspot_v6_forward")
            transaction.execQuiet("$IP6TABLES -N vpnhotspot_v6_output")
            transaction.execQuiet("$IP6TABLES -t mangle -N vpnhotspot_v6_tproxy")
            transaction.ip6tablesInsert("INPUT -j vpnhotspot_v6_input")
            transaction.ip6tablesInsert("FORWARD -j vpnhotspot_v6_forward")
            transaction.ip6tablesInsert("OUTPUT -j vpnhotspot_v6_output")
            transaction.ip6tablesInsert("PREROUTING -j vpnhotspot_v6_tproxy", "mangle")
            transaction.ip6tablesAdd("vpnhotspot_v6_input -i $downstream -p icmpv6 -j ACCEPT")
            transaction.ip6tablesAdd("vpnhotspot_v6_input -i $downstream -m socket --transparent --nowildcard -j ACCEPT")
            transaction.ip6tablesAdd("vpnhotspot_v6_input -i $downstream -j REJECT")
            transaction.ip6tablesAdd("vpnhotspot_v6_forward -i $downstream -j REJECT")
            transaction.ip6tablesAdd("vpnhotspot_v6_forward -o $downstream -j REJECT")
            transaction.ip6tablesAdd("vpnhotspot_v6_output -o $downstream -p icmpv6 --icmpv6-type 134 -m mark --mark $replyMark -j ACCEPT")
            transaction.ip6tablesAdd("vpnhotspot_v6_output -o $downstream -p icmpv6 --icmpv6-type 134 -j REJECT")
            transaction.ip6tablesAdd(
                "vpnhotspot_v6_tproxy -i $downstream -p tcp -j TPROXY --on-port ${ports.tcp} --tproxy-mark $interceptMark/$interceptMark",
                "mangle")
            transaction.ip6tablesAdd(
                "vpnhotspot_v6_tproxy -i $downstream -p udp -j TPROXY --on-port ${ports.udp} --tproxy-mark $interceptMark/$interceptMark",
                "mangle")
        }

        fun commit() {
            UpstreamMonitor.registerCallback(upstreamCallback)
            FallbackUpstreamMonitor.registerCallback(fallbackCallback)
        }

        private fun update() {
            try {
                val config = nextConfig()
                syncPrefixRoutes(config)
                runBlocking { Ipv6NatController.replaceSession(config) }
            } catch (e: Exception) {
                if (e !is CancellationException) {
                    Timber.w(e)
                    SmartSnackbar.make(e).show()
                }
            }
        }

        override fun close() {
            UpstreamMonitor.unregisterCallback(upstreamCallback)
            FallbackUpstreamMonitor.unregisterCallback(fallbackCallback)
            try {
                runBlocking { Ipv6NatController.removeSession(downstream) }
            } catch (e: Exception) {
                if (e !is CancellationException) Timber.w(e)
            }
        }

        private fun generatePrefix(): Ipv6Prefix {
            val bytes = ByteArray(8)
            SecureRandom().nextBytes(bytes)
            bytes[0] = 0xfd.toByte()
            val prefix = buildString {
                for (index in bytes.indices step 2) {
                    if (index > 0) append(':')
                    append("%02x%02x".format(bytes[index].toInt() and 0xff, bytes[index + 1].toInt() and 0xff))
                }
            }
            return Ipv6Prefix("$prefix::", "$prefix::1", 64)
        }
    }

    private inner class Client(private val ip: Inet4Address, mac: MacAddress) : AutoCloseable {
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
    override fun onIpNeighbourAvailable(neighbours: Collection<IpNeighbour>): Unit = synchronized(this) {
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
        try {
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

    fun ipv6Nat() {
        if (Build.VERSION.SDK_INT < 29) {
            SmartSnackbar.make(R.string.warn_ipv6_nat_fallback).show()
            disableIpv6()
            return
        }
        try {
            ipv6NatSession = Ipv6NatSession().apply { prepare() }
        } catch (e: Exception) {
            if (e is CancellationException) throw e
            Timber.w(e)
            SmartSnackbar.make(R.string.warn_ipv6_nat_fallback).show()
            disableIpv6()
        }
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

    fun stop() {
        synchronized(this) { stopped = true }
        IpNeighbourMonitor.unregisterCallback(this)
        ipv6NatSession?.close()
        ipv6NatSession = null
        DnsForwarder.unregisterClient(this)
        FallbackUpstreamMonitor.unregisterCallback(fallbackUpstream)
        UpstreamMonitor.unregisterCallback(upstream)
        VpnMonitor.unregisterCallback(emptyCallback)
        Timber.i("Stopped routing for $downstream by $caller")
    }

    /**
     * Allow protect UDP sockets which will be used by DnsForwarder. Must call this first.
     */
    fun allowProtect() {
        val command = "ndc network protect allow ${Process.myUid()}"
        val result = transaction.execQuiet(command)
        val suffix = "200 0 success\n"
        result.check(listOf(command), !result.out.endsWith(suffix))
        if (result.out.length > suffix.length) Timber.i(result.message(listOf(command), true))
    }

    fun commit() {
        transaction.ipRule("unreachable", RULE_PRIORITY_UPSTREAM_DISABLE_SYSTEM)
        if (ipv6NatSession == null) {
            val useLocalnet = useLocalnet()
            val forwarder = DnsForwarder.registerClient(this, useLocalnet)
            val hostAddress = hostAddress.address.hostAddress
            val forwarderIp = if (useLocalnet) {
                transaction.exec("echo 1 >/proc/sys/net/ipv4/conf/all/route_localnet")
                "127.0.0.1"
            } else hostAddress
            VpnFirewallManager.setup(transaction)
            transaction.iptablesInsert("PREROUTING -i $downstream -p tcp -d $hostAddress --dport 53 -j DNAT --to-destination $forwarderIp:${forwarder.tcpPort}", "nat")
            transaction.iptablesInsert("PREROUTING -i $downstream -p udp -d $hostAddress --dport 53 -j DNAT --to-destination $forwarderIp:${forwarder.udpPort}", "nat")
        }
        transaction.commit()
        Timber.i("Started routing for $downstream by $caller")
        ipv6NatSession?.commit()
        FallbackUpstreamMonitor.registerCallback(fallbackUpstream)
        UpstreamMonitor.registerCallback(upstream)
        IpNeighbourMonitor.registerCallback(this, true)
        if (VpnFirewallManager.mayBeAffected) VpnMonitor.registerCallback(emptyCallback)
    }
    fun revert() {
        transaction.revert()
        stop()
        TrafficRecorder.update()    // record stats before exiting to prevent stats losing
        synchronized(this) { clients.values.forEach { it.close() } }
        fallbackUpstream.subrouting.values.forEach { it.transaction.revert() }
        upstream.subrouting.values.forEach { it.transaction.revert() }
    }
}
