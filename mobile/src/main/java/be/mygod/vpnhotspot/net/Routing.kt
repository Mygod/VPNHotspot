package be.mygod.vpnhotspot.net

import android.annotation.SuppressLint
import android.net.IpPrefix
import android.net.LinkProperties
import android.net.MacAddress
import android.net.Network
import android.net.RouteInfo
import android.os.Build
import android.provider.Settings
import android.system.Os
import androidx.collection.MutableScatterMap
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.net.monitor.FallbackUpstreamMonitor
import be.mygod.vpnhotspot.net.monitor.IpNeighbourMonitor
import be.mygod.vpnhotspot.net.monitor.TrafficRecorder
import be.mygod.vpnhotspot.net.monitor.UpstreamMonitor
import be.mygod.vpnhotspot.room.AppDatabase
import be.mygod.vpnhotspot.root.RootManager
import be.mygod.vpnhotspot.root.RoutingCommands
import be.mygod.vpnhotspot.root.daemon.DaemonController
import be.mygod.vpnhotspot.root.daemon.DaemonProtocol
import be.mygod.vpnhotspot.util.RootSession
import be.mygod.vpnhotspot.util.allInterfaceNames
import be.mygod.vpnhotspot.util.allRoutes
import be.mygod.vpnhotspot.util.readableMessage
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineExceptionHandler
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.channels.consumeEach
import kotlinx.coroutines.withContext
import timber.log.Timber
import java.io.IOException
import java.io.Writer
import java.net.Inet4Address
import java.net.Inet6Address
import java.net.InetAddress
import java.net.NetworkInterface
import java.net.SocketException
import java.security.MessageDigest
import java.util.Collections

/**
 * A transaction wrapper that helps set up routing environment.
 *
 * Once revert is called, this object no longer serves any purpose.
 */
class Routing(private val caller: Any, private val downstream: String) : IpNeighbourMonitor.Callback {
    companion object {
        /**
         * AOSP local-network/tethering priorities are 20000/21000 since Android 12 and 17000/18000
         * on API 29..30. Keep VPNHotspot rules inside that gap.
         * This also works for Wi-Fi direct where there's no system tethering rule to override.
         *
         * Sources:
         * https://android.googlesource.com/platform/system/netd/+/android-10.0.0_r1/server/RouteController.cpp#65
         * https://android.googlesource.com/platform/system/netd/+/e11b8688b1f99292ade06f89f957c1f7e76ceae9/server/RouteController.h#51
         */
        private val rulePriorityShift = if (Build.VERSION.SDK_INT < 31) -3000 else 0
        private val RULE_PRIORITY_DAEMON = 20600 + rulePriorityShift
        private val RULE_PRIORITY_UPSTREAM = 20700 + rulePriorityShift
        private val RULE_PRIORITY_UPSTREAM_FALLBACK = 20800 + rulePriorityShift
        private val RULE_PRIORITY_UPSTREAM_DISABLE_SYSTEM = 20900 + rulePriorityShift
        /**
         * Android fwmark uses the low bits for netId and platform routing metadata. Keep IPv6 NAT
         * TPROXY marks in the high-bit reserved area and always match through the mask.
         *
         * Sources:
         * https://android.googlesource.com/platform/system/netd/+/android-10.0.0_r1/include/Fwmark.h#24
         * https://android.googlesource.com/platform/system/netd/+/e11b8688b1f99292ade06f89f957c1f7e76ceae9/include/Fwmark.h#24
         */
        private const val DAEMON_INTERCEPT_FWMARK = "0x10000000/0x10000000"

        /**
         * Daemon reply sockets use Android's local-network fwmark so AOSP routes them through
         * local_network before VPN UID rules. This is LOCAL_NET_ID 99 plus explicitlySelected and
         * protectedFromVpn.
         *
         * Sources:
         * https://android.googlesource.com/platform/system/netd/+/android-10.0.0_r1/server/binder/android/net/INetd.aidl#768
         * https://android.googlesource.com/platform/system/netd/+/android-10.0.0_r1/include/Fwmark.h#24
         * https://android.googlesource.com/platform/system/netd/+/android-10.0.0_r1/server/RouteController.cpp#653
         * https://android.googlesource.com/platform/packages/modules/Connectivity/+/android-15.0.0_r1/service-t/src/com/android/server/NsdService.java#1761
         * https://android.googlesource.com/platform/system/netd/+/android-16.0.0_r1/include/Fwmark.h#24
         * https://android.googlesource.com/platform/system/netd/+/android-16.0.0_r1/server/RouteController.cpp#605
         */
        private const val DAEMON_REPLY_MARK_MASK = 0x0003FFFF
        private const val DAEMON_REPLY_MARK = 0x00030063

        /**
         * Android interface route tables start at ifindex + 1000. Use 900 to leave buffer below
         * that range while avoiding kernel-reserved tables and AOSP's fixed 97..99 tables.
         */
        private const val DAEMON_TABLE = 900

        private const val ROOT_DIR = "/system/bin/"
        const val IP = "${ROOT_DIR}ip"
        const val IPTABLES = "iptables -w"
        const val IP6TABLES = "ip6tables -w"
        private const val IPTABLES_RESTORE = "iptables-restore -w --noflush"
        private const val IP6TABLES_RESTORE = "ip6tables-restore -w --noflush"

        /**
         * Uses batch tools for Clean's one-shot deterministic cleanup commands. Android 10's bundled
         * iproute2 supports `ip -force -batch -`; Android 10's bundled iptables-restore supports
         * `-w --noflush`, and with `--noflush`, `:chain - [0:0]` flushes an existing user chain or
         * creates a missing one before the following `-X chain`.
         *
         * Sources:
         * https://android.googlesource.com/platform/external/iproute2/+/android-10.0.0_r1/ip/ip.c#50
         * https://android.googlesource.com/platform/external/iproute2/+/android-10.0.0_r1/ip/ip.c#122
         * https://android.googlesource.com/platform/external/iproute2/+/android-10.0.0_r1/ip/ip.c#251
         * https://android.googlesource.com/platform/external/iptables/+/android-10.0.0_r1/iptables/iptables-restore.c#33
         * https://android.googlesource.com/platform/external/iptables/+/android-10.0.0_r1/iptables/iptables-restore.c#354
         * https://android.googlesource.com/platform/external/iptables/+/android-10.0.0_r1/iptables/ip6tables-restore.c#36
         * https://android.googlesource.com/platform/external/iptables/+/android-10.0.0_r1/iptables/ip6tables-restore.c#355
         */
        private fun Writer.appendIptablesRestore(command: String, blocks: List<String>) {
            append(command)
            appendLine(" <<'EOF_IPTABLES_RESTORE' || true")
            for (block in blocks) {
                append('*')
                appendLine(block)
                appendLine("COMMIT")
            }
            appendLine("EOF_IPTABLES_RESTORE")
        }

        fun appendCleanCommands(commands: Writer, ipv6NatPrefixSeed: String) {
            commands.appendLine("while $IPTABLES -t mangle -D PREROUTING -j vpnhotspot_dns_tproxy; do done")
            commands.appendLine("while $IPTABLES -D FORWARD -j vpnhotspot_acl; do done")
            commands.appendLine("while $IPTABLES -t nat -D POSTROUTING -j vpnhotspot_masquerade; do done")
            commands.appendIptablesRestore(IPTABLES_RESTORE, listOf(
                """mangle
                |:vpnhotspot_dns_tproxy - [0:0]
                |-X vpnhotspot_dns_tproxy""".trimMargin(),
                """filter
                |:vpnhotspot_acl - [0:0]
                |:vpnhotspot_stats - [0:0]
                |-X vpnhotspot_acl
                |-X vpnhotspot_stats""".trimMargin(),
                """nat
                |-F PREROUTING
                |:vpnhotspot_masquerade - [0:0]
                |-X vpnhotspot_masquerade""".trimMargin()))
            commands.appendLine("while $IP6TABLES -D INPUT -j vpnhotspot_filter; do done")
            commands.appendLine("while $IP6TABLES -D FORWARD -j vpnhotspot_filter; do done")
            commands.appendLine("while $IP6TABLES -D OUTPUT -j vpnhotspot_filter; do done")
            commands.appendLine("while $IP6TABLES -D INPUT -j vpnhotspot_v6_input; do done")
            commands.appendLine("while $IP6TABLES -D FORWARD -j vpnhotspot_v6_forward; do done")
            commands.appendLine("while $IP6TABLES -D OUTPUT -j vpnhotspot_v6_output; do done")
            commands.appendLine("while $IP6TABLES -t mangle -D PREROUTING -j vpnhotspot_v6_tproxy; do done")
            commands.appendIptablesRestore(IP6TABLES_RESTORE, listOf(
                """filter
                |:vpnhotspot_filter - [0:0]
                |:vpnhotspot_v6_input - [0:0]
                |:vpnhotspot_v6_forward - [0:0]
                |:vpnhotspot_v6_output - [0:0]
                |-X vpnhotspot_filter
                |-X vpnhotspot_v6_input
                |-X vpnhotspot_v6_forward
                |-X vpnhotspot_v6_output""".trimMargin(),
                """mangle
                |:vpnhotspot_acl - [0:0]
                |:vpnhotspot_v6_tproxy - [0:0]
                |-X vpnhotspot_acl
                |-X vpnhotspot_v6_tproxy""".trimMargin()))
            commands.appendLine("while $IP -6 rule del priority $RULE_PRIORITY_DAEMON; do done")
            val networkInterfaces = Collections.list(NetworkInterface.getNetworkInterfaces())
            commands.appendLine("$IP -force -6 -batch - <<'EOF_IP6_BATCH' || true")
            commands.appendLine("route flush table $DAEMON_TABLE")
            for (networkInterface in networkInterfaces) {
                val prefix = ipv6NatPrefix(ipv6NatPrefixSeed, networkInterface.name)
                commands.appendLine("addr del ${ipv6NatGateway(prefix).hostAddress}/64 dev ${networkInterface.name}")
                commands.appendLine("route del $prefix dev ${networkInterface.name} table local_network")
            }
            commands.appendLine("EOF_IP6_BATCH")
            commands.appendLine("while $IP rule del priority $RULE_PRIORITY_UPSTREAM; do done")
            commands.appendLine("while $IP rule del priority $RULE_PRIORITY_UPSTREAM_FALLBACK; do done")
            commands.appendLine("while $IP rule del priority $RULE_PRIORITY_UPSTREAM_DISABLE_SYSTEM; do done")
        }

        private val ipv6NatPrefixSeed: String
            @SuppressLint("HardwareIds")
            get() = "${app.packageName}\u0000${
                Settings.Secure.getString(app.contentResolver, Settings.Secure.ANDROID_ID).orEmpty()
            }"

        private fun ipv6NatPrefix(seed: String, downstream: String): IpPrefix {
            val raw = ByteArray(16)
            MessageDigest.getInstance("SHA-256").digest("$seed\u0000$downstream".encodeToByteArray())
                .copyInto(raw, 1, endIndex = 7)
            raw[0] = 0xfd.toByte()
            return IpPrefix(InetAddress.getByAddress(raw), 64)
        }

        private fun ipv6NatGateway(prefix: IpPrefix) =
                InetAddress.getByAddress(prefix.rawAddress.apply { this[15] = 1 }) as Inet6Address

        suspend fun clean() {
            TrafficRecorder.clean()
            RootManager.use { it.execute(RoutingCommands.Clean(ipv6NatPrefixSeed)) }
        }

        private suspend fun RootSession.Transaction.iptables(command: String, revert: String) {
            val result = execQuiet(command, revert)
            val message = result.message(listOf(command), err = false)
            if (result.err.isNotEmpty()) Timber.i(message)  // busy wait message
        }
        private suspend fun RootSession.Transaction.iptablesInsert(content: String, table: String = "filter") =
                iptables("$IPTABLES -t $table -I $content", "$IPTABLES -t $table -D $content")
        private suspend fun RootSession.Transaction.ip6tablesInsert(content: String, table: String = "filter") =
                iptables("$IP6TABLES -t $table -I $content", "$IP6TABLES -t $table -D $content")

        private suspend fun RootSession.Transaction.ndc(name: String, command: String, revert: String? = null) {
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

    private suspend fun RootSession.Transaction.ipRule(action: String, priority: Int, rule: String = "") {
        try {
            exec("$IP rule add $rule iif $downstream $action priority $priority",
                    "$IP rule del $rule iif $downstream $action priority $priority")
        } catch (e: RoutingCommands.UnexpectedOutputException) {
            if (!shouldSuppressIpError(e)) throw e
        }
    }
    private suspend fun RootSession.Transaction.ip6Rule(action: String, priority: Int, rule: String = "") {
        try {
            exec("$IP -6 rule add $rule iif $downstream $action priority $priority",
                    "$IP -6 rule del $rule iif $downstream $action priority $priority")
        } catch (e: RoutingCommands.UnexpectedOutputException) {
            if (!shouldSuppressIpError(e)) throw e
        }
    }
    private suspend fun RootSession.Transaction.ipRuleLookup(ifindex: Int, priority: Int, rule: String = "") =
            // https://android.googlesource.com/platform/system/netd/+/android-5.0.0_r1/server/RouteController.h#37
            ipRule("lookup ${1000 + ifindex}", priority, rule)

    enum class MasqueradeMode {
        None,
        Simple,
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
    private val hostSubnet = IpPrefix(hostAddress.address, hostAddress.networkPrefixLength.toInt()).toString()
    lateinit var transaction: RootSession.Transaction
    private val scope = CoroutineScope(SupervisorJob() + CoroutineExceptionHandler { _, t -> Timber.w(t) })

    @Volatile
    private var stopped = false
    private var masqueradeMode = MasqueradeMode.None

    private val upstreams = HashSet<String>()

    private sealed class RoutingUpdate {
        class UpstreamSnapshot(
            val upstream: Upstream,
            val network: Network?,
            val properties: LinkProperties?,
        ) : RoutingUpdate()
        class NeighboursSnapshot(val neighbours: Collection<IpNeighbour>) : RoutingUpdate()
    }
    private val updateLock = Any()
    private val neighbourUpdateKey = Any()
    private val pendingUpdates = LinkedHashMap<Any, RoutingUpdate>()
    private var pendingBarrier: CompletableDeferred<Unit>? = null
    private val updateSignal = Channel<Unit>(Channel.CONFLATED)
    private var updateWorker: Job? = null
    private fun enqueue(update: RoutingUpdate) {
        synchronized(updateLock) {
            if (stopped) return
            when (update) {
                is RoutingUpdate.UpstreamSnapshot -> {
                    pendingUpdates.remove(update.upstream)
                    pendingUpdates[update.upstream] = update
                }
                is RoutingUpdate.NeighboursSnapshot -> {
                    pendingUpdates.remove(neighbourUpdateKey)
                    pendingUpdates[neighbourUpdateKey] = update
                }
            }
        }
        val result = updateSignal.trySend(Unit)
        if (result.isFailure) result.exceptionOrNull()?.let { if (it !is CancellationException) Timber.w(it) }
    }
    private class InterfaceGoneException(upstream: String) : IOException("Interface $upstream not found")
    private open inner class Upstream(val priority: Int) : UpstreamMonitor.Callback {
        val subrouting = mutableMapOf<String, RootSession.Transaction>()
        var network: Network? = null
            private set
        var properties: LinkProperties? = null
            private set

        override fun onAvailable(network: Network?, properties: LinkProperties?) {
            enqueue(RoutingUpdate.UpstreamSnapshot(this, network, properties))
        }
        suspend fun update(network: Network?, properties: LinkProperties?) {
            this.network = network
            this.properties = properties
            val toRemove = subrouting.keys.toMutableSet()
            for (ifname in properties?.allInterfaceNames ?: emptyList()) {
                if (toRemove.remove(ifname) || !upstreams.add(ifname)) continue
                try {
                    val ifindex = Os.if_nametoindex(ifname).also {
                        if (it <= 0) throw InterfaceGoneException(ifname)
                    }
                    val transaction = RootSession.beginTransaction().safeguard {
                        ipRuleLookup(ifindex, priority)
                        when (masqueradeMode) {
                            MasqueradeMode.None -> { }  // nothing to be done here
                            // note: specifying -i wouldn't work for POSTROUTING
                            MasqueradeMode.Simple -> iptablesInsert(
                                "vpnhotspot_masquerade -s $hostSubnet -o $ifname -j MASQUERADE", "nat")
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
                            MasqueradeMode.Netd -> ndc("Nat", "ndc nat enable $downstream $ifname 0")
                        }
                    }
                    if (Build.VERSION.SDK_INT >= 31) try {
                        RootManager.use { it.execute(IpSecForwardPolicyCommand(ifname)) }
                    } catch (e: Exception) {
                        if (e is CancellationException) {
                            transaction.revert()
                            throw e
                        }
                        SmartSnackbar.make(e).show()
                        Timber.w(e)
                    }
                    subrouting[ifname] = transaction
                } catch (e: Exception) {
                    SmartSnackbar.make(e).show()
                    if (e !is CancellationException && e !is InterfaceGoneException) Timber.w(e)
                }
            }
            for (ifname in toRemove) {
                subrouting.remove(ifname)?.revert()
                check(upstreams.remove(ifname))
            }
            daemonSession?.update()
        }
    }
    private val fallbackUpstream = Upstream(RULE_PRIORITY_UPSTREAM_FALLBACK)
    private val upstream = Upstream(RULE_PRIORITY_UPSTREAM)
    private var daemonSession: DaemonSession? = null

    private inner class DaemonSession(val ipv6Nat: Boolean) {
        private val prefix by lazy { ipv6NatPrefix(ipv6NatPrefixSeed, downstream) }
        private val gateway by lazy { ipv6NatGateway(prefix) }
        private val mtu by lazy { NetworkInterface.getByName(downstream)?.mtu ?: 1500 }
        lateinit var ports: DaemonProtocol.SessionPorts

        private fun nextConfig(): DaemonProtocol.SessionConfig {
            val ipv6NatConfig = if (ipv6Nat) {
                val interfaceAddresses = NetworkInterface.getByName(downstream)?.interfaceAddresses ?: emptyList()
                DaemonProtocol.Ipv6NatConfig(
                    gateway = gateway,
                    prefixLength = prefix.prefixLength,
                    mtu = mtu,
                    suppressedPrefixes = interfaceAddresses.mapNotNull { address ->
                        val inet = address.address
                        if (inet !is Inet6Address || inet.isLinkLocalAddress || inet.isLoopbackAddress ||
                                inet.isMulticastAddress || inet == gateway) null else {
                            IpPrefix(inet, address.networkPrefixLength.toInt())
                        }
                    },
                    cleanupPrefixes = emptyList(),
                )
            } else null
            return DaemonProtocol.SessionConfig(
                downstream = downstream,
                dnsBindAddress = hostAddress.address as Inet4Address,
                replyMark = DAEMON_REPLY_MARK,
                primaryNetwork = upstream.network,
                primaryRoutes = upstream.properties?.allRoutes?.mapNotNull { route ->
                    val destination = route.destination
                    if (route.type == RouteInfo.RTN_UNICAST && destination.address is Inet6Address) destination else null
                } ?: emptyList(),
                fallbackNetwork = fallbackUpstream.network,
                ipv6Nat = ipv6NatConfig,
            )
        }

        suspend fun prepare() {
            val config = nextConfig()
            ports = DaemonController.startSession(config)
            transaction.iptablesInsert(
                "PREROUTING -i $downstream -p tcp -d ${hostAddress.address.hostAddress} --dport 53 -j DNAT --to-destination :${ports.dnsTcp}",
                "nat")
            transaction.iptablesInsert(
                "PREROUTING -i $downstream -p udp -d ${hostAddress.address.hostAddress} --dport 53 -j DNAT --to-destination :${ports.dnsUdp}",
                "nat")
            if (!ipv6Nat) return
            val ipv6NatPorts = checkNotNull(ports.ipv6Nat)
            val subnet = prefix.toString()
            try {
                transaction.exec("$IP -6 route replace $subnet dev $downstream table local_network",
                    "$IP -6 route del $subnet dev $downstream table local_network")
            } catch (e: RoutingCommands.UnexpectedOutputException) {
                if (!shouldSuppressIpError(e)) throw e
            }
            transaction.exec("$IP -6 addr replace ${gateway.hostAddress}/${prefix.prefixLength} dev $downstream",
                "$IP -6 addr del ${gateway.hostAddress}/${prefix.prefixLength} dev $downstream")
            transaction.execQuiet("$IP6TABLES -N vpnhotspot_v6_input")
            transaction.execQuiet("$IP6TABLES -N vpnhotspot_v6_forward")
            transaction.execQuiet("$IP6TABLES -N vpnhotspot_v6_output")
            transaction.execQuiet("$IP6TABLES -t mangle -N vpnhotspot_acl")
            transaction.execQuiet("$IP6TABLES -t mangle -N vpnhotspot_v6_tproxy")
            try {
                transaction.exec("$IP -6 route replace local ::/0 dev lo table $DAEMON_TABLE")
            } catch (e: RoutingCommands.UnexpectedOutputException) {
                if (!shouldSuppressIpError(e)) throw e
            }
            transaction.ip6Rule("lookup $DAEMON_TABLE", RULE_PRIORITY_DAEMON,
                "fwmark $DAEMON_INTERCEPT_FWMARK")
            transaction.ip6tablesInsert("vpnhotspot_v6_input -i $downstream -j REJECT")
            transaction.ip6tablesInsert("vpnhotspot_v6_input -i $downstream -m socket --transparent --nowildcard -j ACCEPT")
            transaction.ip6tablesInsert("vpnhotspot_v6_input -i $downstream -p icmpv6 -j ACCEPT")
            transaction.ip6tablesInsert("vpnhotspot_v6_forward -o $downstream -j REJECT")
            transaction.ip6tablesInsert("vpnhotspot_v6_forward -i $downstream -j REJECT")
            transaction.ip6tablesInsert("vpnhotspot_v6_output -o $downstream -p icmpv6 --icmpv6-type 134 -j REJECT")
            transaction.ip6tablesInsert("vpnhotspot_v6_output -o $downstream -p icmpv6 --icmpv6-type 134 -m mark --mark $DAEMON_REPLY_MARK/$DAEMON_REPLY_MARK_MASK -j ACCEPT")
            transaction.ip6tablesInsert("vpnhotspot_acl -j DROP", "mangle")
            transaction.ip6tablesInsert(
                "vpnhotspot_v6_tproxy -i $downstream -p tcp -j TPROXY --on-port ${ipv6NatPorts.tcp} --tproxy-mark $DAEMON_INTERCEPT_FWMARK",
                "mangle")
            transaction.ip6tablesInsert(
                "vpnhotspot_v6_tproxy -i $downstream -p udp -j TPROXY --on-port ${ipv6NatPorts.udp} --tproxy-mark $DAEMON_INTERCEPT_FWMARK",
                "mangle")
            transaction.ip6tablesInsert("vpnhotspot_v6_tproxy -i $downstream -j vpnhotspot_acl", "mangle")
            transaction.ip6tablesInsert("INPUT -j vpnhotspot_v6_input")
            transaction.ip6tablesInsert("FORWARD -j vpnhotspot_v6_forward")
            transaction.ip6tablesInsert("OUTPUT -j vpnhotspot_v6_output")
            transaction.ip6tablesInsert("PREROUTING -j vpnhotspot_v6_tproxy", "mangle")
        }

        suspend fun update() {
            try {
                val config = nextConfig()
                DaemonController.replaceSession(config)
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                Timber.w(e)
                SmartSnackbar.make(e).show()
            }
        }

        suspend fun close(
            removeMode: DaemonProtocol.RemoveMode = DaemonProtocol.RemoveMode.PreserveCleanup,
        ) {
            if (ipv6Nat) {
                val subnet = prefix.toString()
                RootSession.use {
                    it.submit("$IP -6 route del $subnet dev $downstream table local_network")
                    it.submit("$IP -6 addr del ${gateway.hostAddress}/${prefix.prefixLength} dev $downstream")
                }
            }
            try {
                DaemonController.removeSession(downstream, removeMode)
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                Timber.w(e)
            }
        }
    }

    private inner class Client(private val ip: Inet4Address, private val transaction: RootSession.Transaction) {
        suspend fun close() {
            TrafficRecorder.unregister(ip, downstream)
            transaction.revert()
        }
    }
    private val clients = MutableScatterMap<InetAddress, Client>()
    private val allowedClients = MutableScatterMap<MacAddress, RootSession.Transaction>()
    override fun onIpNeighbourAvailable(neighbours: Collection<IpNeighbour>) {
        enqueue(RoutingUpdate.NeighboursSnapshot(neighbours))
    }
    private suspend fun updateNeighbours(neighbours: Collection<IpNeighbour>) {
        val toRemove = HashSet<InetAddress>(clients.size)
        clients.forEachKey { toRemove.add(it) }
        val allowedToRemove = HashSet<MacAddress>(allowedClients.size)
        allowedClients.forEachKey { allowedToRemove.add(it) }
        for (neighbour in neighbours) {
            if (neighbour.dev != downstream || neighbour.state != IpNeighbour.State.VALID ||
                    AppDatabase.instance.clientRecordDao.lookupOrDefault(neighbour.lladdr).blocked) continue
            allowedToRemove.remove(neighbour.lladdr)
            try {
                allowedClients.compute(neighbour.lladdr) { mac, transaction ->
                    transaction ?: RootSession.beginTransaction().safeguard {
                        iptablesInsert("vpnhotspot_acl -i $downstream -m mac --mac-source $mac -j ACCEPT")
                        iptablesInsert("vpnhotspot_acl -i $downstream -m mac --mac-source $mac -j vpnhotspot_stats")
                        if (daemonSession?.ipv6Nat == true) {
                            ip6tablesInsert("vpnhotspot_acl -m mac --mac-source $mac -j RETURN", "mangle")
                        }
                    }
                }
            } catch (e: Exception) {
                Timber.w(e)
                SmartSnackbar.make(e).show()
                continue
            }
            val ip = neighbour.ip
            if (ip !is Inet4Address) continue
            toRemove.remove(ip)
            try {
                clients.compute(ip) { address, client ->
                    client ?: run {
                        val transaction = RootSession.beginTransaction().safeguard {
                            iptablesInsert("vpnhotspot_stats -i $downstream -s ${address.hostAddress} -j RETURN")
                            iptablesInsert("vpnhotspot_stats -o $downstream -d ${address.hostAddress} -j RETURN")
                        }
                        try {
                            TrafficRecorder.register(ip, downstream, neighbour.lladdr)
                            Client(ip, transaction)
                        } catch (e: Exception) {
                            TrafficRecorder.unregister(ip, downstream)
                            transaction.revert()
                            throw e
                        }
                    }
                }
            } catch (e: Exception) {
                Timber.w(e)
                SmartSnackbar.make(e).show()
            }
        }
        if (toRemove.isNotEmpty()) {
            TrafficRecorder.update()    // record stats before removing rules to prevent stats losing
            for (address in toRemove) clients.remove(address)!!.close()
        }
        for (mac in allowedToRemove) allowedClients.remove(mac)!!.revert()
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
    suspend fun ipForward() {
        try {
            transaction.ndc("ipfwd", "ndc ipfwd enable vpnhotspot_$downstream",
                "ndc ipfwd disable vpnhotspot_$downstream")
            return
        } catch (e: RoutingCommands.UnexpectedOutputException) {
            Timber.w(IOException("ndc ipfwd enable failure", e))
        }
        transaction.exec("echo 1 >/proc/sys/net/ipv4/ip_forward")
    }

    suspend fun disableIpv6() {
        transaction.execQuiet("$IP6TABLES -N vpnhotspot_filter")
        transaction.ip6tablesInsert("INPUT -j vpnhotspot_filter")
        transaction.ip6tablesInsert("FORWARD -j vpnhotspot_filter")
        transaction.ip6tablesInsert("OUTPUT -j vpnhotspot_filter")
        transaction.ip6tablesInsert("vpnhotspot_filter -i $downstream -j REJECT")
        transaction.ip6tablesInsert("vpnhotspot_filter -o $downstream -j REJECT")
    }

    suspend fun ipv6Nat() {
        val session = DaemonSession(true)
        try {
            session.prepare()
            daemonSession = session
        } catch (e: Exception) {
            withContext(NonCancellable) {
                try {
                    DaemonController.removeSession(downstream)
                } catch (removeError: Exception) {
                    if (removeError !is CancellationException) Timber.w(removeError)
                }
            }
            if (e is CancellationException) throw e
            Timber.w(e)
            SmartSnackbar.make(app.getString(R.string.warn_ipv6_nat_failed, e.readableMessage)).show()
        }
    }

    suspend fun forward() {
        transaction.execQuiet("$IPTABLES -N vpnhotspot_acl")
        transaction.execQuiet("$IPTABLES -N vpnhotspot_stats")
        transaction.iptablesInsert("FORWARD -j vpnhotspot_acl")
        transaction.iptablesInsert("vpnhotspot_acl -i $downstream ! -o $downstream -j REJECT") // ensure blocking works
        transaction.iptablesInsert("vpnhotspot_acl -o $downstream -m state --state ESTABLISHED,RELATED -j ACCEPT")
        transaction.iptablesInsert("vpnhotspot_acl -o $downstream -m state --state ESTABLISHED,RELATED -j vpnhotspot_stats")
        // the real forwarding filters and counters will be added when clients are connected
    }

    suspend fun masquerade(mode: MasqueradeMode) {
        masqueradeMode = mode
        if (mode == MasqueradeMode.Simple) {
            transaction.execQuiet("$IPTABLES -t nat -N vpnhotspot_masquerade")
            transaction.iptablesInsert("POSTROUTING -j vpnhotspot_masquerade", "nat")
            // further rules are added when upstreams are found
        }
    }

    suspend fun stop(withdrawCleanupPrefixes: Boolean = false) {
        val done = synchronized(updateLock) {
            stopped = true
            if (updateWorker?.isActive == true) {
                pendingBarrier ?: CompletableDeferred<Unit>().also { pendingBarrier = it }
            } else null
        }
        IpNeighbourMonitor.unregisterCallback(this)
        FallbackUpstreamMonitor.unregisterCallback(fallbackUpstream)
        UpstreamMonitor.unregisterCallback(upstream)
        if (done != null) {
            val result = updateSignal.trySend(Unit)
            if (result.isSuccess) {
                done.await()
            } else {
                result.exceptionOrNull()?.let { if (it !is CancellationException) Timber.w(it) }
            }
        }
        daemonSession?.close(removeMode = if (withdrawCleanupPrefixes) {
            DaemonProtocol.RemoveMode.WithdrawCleanup
        } else DaemonProtocol.RemoveMode.PreserveCleanup)
        daemonSession = null
        updateSignal.close()
        scope.cancel("Routing stopped")
        Timber.i("Stopped routing for $downstream by $caller")
    }

    suspend fun commit() {
        transaction.ipRule("unreachable", RULE_PRIORITY_UPSTREAM_DISABLE_SYSTEM)
        if (daemonSession == null) {
            val session = DaemonSession(false)
            try {
                session.prepare()
                daemonSession = session
            } catch (e: Exception) {
                withContext(NonCancellable) { session.close() }
                throw e
            }
        }
        transaction.commit()
        Timber.i("Started routing for $downstream by $caller")
        check(updateWorker == null)
        updateWorker = scope.launch {
            updateSignal.consumeEach {
                val batch = ArrayList<RoutingUpdate>()
                val done = synchronized(updateLock) {
                    batch.addAll(pendingUpdates.values)
                    pendingUpdates.clear()
                    pendingBarrier.also { pendingBarrier = null }
                }
                for (next in batch) try {
                    when (next) {
                        is RoutingUpdate.UpstreamSnapshot -> if (!stopped) {
                            next.upstream.update(next.network, next.properties)
                        }
                        is RoutingUpdate.NeighboursSnapshot -> if (!stopped) updateNeighbours(next.neighbours)
                    }
                } catch (e: Exception) {
                    if (e is CancellationException) throw e
                    Timber.w(e)
                }
                done?.complete(Unit)
            }
        }
        FallbackUpstreamMonitor.registerCallback(fallbackUpstream)
        UpstreamMonitor.registerCallback(upstream)
        IpNeighbourMonitor.registerCallback(this, true)
    }
    suspend fun revert() = withContext(NonCancellable) {
        transaction.revert()
        stop()
        TrafficRecorder.update()    // record stats before exiting to prevent stats losing
        clients.forEachValue { it.close() }
        clients.clear()
        allowedClients.forEachValue { it.revert() }
        allowedClients.clear()
        fallbackUpstream.subrouting.values.forEach { it.revert() }
        fallbackUpstream.subrouting.clear()
        upstream.subrouting.values.forEach { it.revert() }
        upstream.subrouting.clear()
    }
}
