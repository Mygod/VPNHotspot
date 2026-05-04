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
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.net.monitor.FallbackUpstreamMonitor
import be.mygod.vpnhotspot.net.monitor.IpNeighbourMonitor
import be.mygod.vpnhotspot.net.monitor.TrafficRecorder
import be.mygod.vpnhotspot.net.monitor.UpstreamMonitor
import be.mygod.vpnhotspot.room.AppDatabase
import be.mygod.vpnhotspot.root.RootManager
import be.mygod.vpnhotspot.root.daemon.DaemonController
import be.mygod.vpnhotspot.root.daemon.DaemonProtocol
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
import java.net.Inet4Address
import java.net.Inet6Address
import java.net.InetAddress
import java.net.NetworkInterface
import java.net.SocketException
import java.security.MessageDigest
import java.util.Collections

/**
 * Builds and updates a root routing session for one downstream interface.
 */
class Routing(private val caller: Any, private val downstream: String) : IpNeighbourMonitor.Callback {
    companion object {
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
            val cleanups = Collections.list(NetworkInterface.getNetworkInterfaces()).map { networkInterface ->
                val prefix = ipv6NatPrefix(ipv6NatPrefixSeed, networkInterface.name)
                DaemonProtocol.Ipv6Cleanup(networkInterface.name, ipv6NatGateway(prefix), prefix)
            }
            DaemonController.cleanRouting(cleanups)
        }
    }

    enum class Ipv6Mode {
        System,
        Block,
        Nat,
    }

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
    private val hostPrefixLength = hostAddress.networkPrefixLength.toInt()
    private val scope = CoroutineScope(SupervisorJob() + CoroutineExceptionHandler { _, t -> Timber.w(t) })

    @Volatile
    private var stopped = false
    private var ipForward = false
    private var forward = false
    private var ipv6Block = false
    private var ipv6Nat = false
    private var masqueradeMode = MasqueradeMode.None

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
    private open inner class Upstream(private val role: DaemonProtocol.UpstreamRole) : UpstreamMonitor.Callback {
        val interfaces = linkedMapOf<String, Int>()
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
            val nextInterfaces = linkedMapOf<String, Int>()
            for (ifname in properties?.allInterfaceNames ?: emptyList()) {
                try {
                    val ifindex = Os.if_nametoindex(ifname).also {
                        if (it <= 0) throw InterfaceGoneException(ifname)
                    }
                    nextInterfaces[ifname] = ifindex
                    if (Build.VERSION.SDK_INT >= 31 && !interfaces.containsKey(ifname)) try {
                        RootManager.use { it.execute(IpSecForwardPolicyCommand(ifname)) }
                    } catch (e: Exception) {
                        if (e is CancellationException) throw e
                        SmartSnackbar.make(e).show()
                        Timber.w(e)
                    }
                } catch (e: Exception) {
                    SmartSnackbar.make(e).show()
                    if (e !is CancellationException && e !is InterfaceGoneException) Timber.w(e)
                }
            }
            interfaces.clear()
            interfaces.putAll(nextInterfaces)
            daemonSession?.update()
        }

        fun appendConfig(target: MutableList<DaemonProtocol.UpstreamConfig>, seen: MutableSet<String>) {
            for ((ifname, ifindex) in interfaces) if (seen.add(ifname)) {
                target += DaemonProtocol.UpstreamConfig(role, ifname, ifindex)
            }
        }
    }
    private val fallbackUpstream = Upstream(DaemonProtocol.UpstreamRole.Fallback)
    private val upstream = Upstream(DaemonProtocol.UpstreamRole.Primary)
    private var daemonSession: DaemonSession? = null

    private inner class DaemonSession {
        private val prefix by lazy { ipv6NatPrefix(ipv6NatPrefixSeed, downstream) }
        private val gateway by lazy { ipv6NatGateway(prefix) }
        private val mtu by lazy { NetworkInterface.getByName(downstream)?.mtu ?: 1500 }

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
                downstreamPrefixLength = hostPrefixLength,
                ipForward = ipForward,
                forward = forward,
                masquerade = when (masqueradeMode) {
                    MasqueradeMode.None -> DaemonProtocol.MasqueradeMode.None
                    MasqueradeMode.Simple -> DaemonProtocol.MasqueradeMode.Simple
                    MasqueradeMode.Netd -> DaemonProtocol.MasqueradeMode.Netd
                },
                ipv6Block = ipv6Block,
                primaryNetwork = upstream.network,
                primaryRoutes = upstream.properties?.allRoutes?.mapNotNull { route ->
                    val destination = route.destination
                    if (route.type == RouteInfo.RTN_UNICAST && destination.address is Inet6Address) destination else null
                } ?: emptyList(),
                fallbackNetwork = fallbackUpstream.network,
                upstreams = buildList {
                    val seen = HashSet<String>()
                    upstream.appendConfig(this, seen)
                    fallbackUpstream.appendConfig(this, seen)
                },
                clients = buildList {
                    for (mac in allowedMacs) add(DaemonProtocol.ClientConfig(mac,
                        clients.filterValues { it == mac }.keys.toList()))
                },
                ipv6Nat = ipv6NatConfig,
            )
        }

        suspend fun prepare() = DaemonController.startSession(nextConfig())

        suspend fun update() {
            try {
                DaemonController.replaceSession(nextConfig())
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
            try {
                DaemonController.removeSession(downstream, removeMode)
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                Timber.w(e)
            }
        }
    }

    private val clients = linkedMapOf<Inet4Address, MacAddress>()
    private val allowedMacs = linkedSetOf<MacAddress>()
    override fun onIpNeighbourAvailable(neighbours: Collection<IpNeighbour>) {
        enqueue(RoutingUpdate.NeighboursSnapshot(neighbours))
    }
    private suspend fun updateNeighbours(neighbours: Collection<IpNeighbour>) {
        val nextClients = linkedMapOf<Inet4Address, MacAddress>()
        val nextAllowedMacs = linkedSetOf<MacAddress>()
        for (neighbour in neighbours) {
            if (neighbour.dev != downstream || neighbour.state != IpNeighbour.State.VALID ||
                    AppDatabase.instance.clientRecordDao.lookupOrDefault(neighbour.lladdr).blocked) continue
            nextAllowedMacs.add(neighbour.lladdr)
            val ip = neighbour.ip
            if (ip is Inet4Address) nextClients[ip] = neighbour.lladdr
        }
        val removed = clients.keys - nextClients.keys
        if (removed.isNotEmpty()) TrafficRecorder.update()
        val added = nextClients.filterKeys { !clients.containsKey(it) }
        clients.clear()
        clients.putAll(nextClients)
        allowedMacs.clear()
        allowedMacs.addAll(nextAllowedMacs)
        try {
            daemonSession?.update()
        } catch (e: Exception) {
            if (e is CancellationException) throw e
            Timber.w(e)
            SmartSnackbar.make(e).show()
            return
        }
        for ((ip, mac) in added) try {
            TrafficRecorder.register(ip, downstream, mac)
        } catch (e: Exception) {
            Timber.w(e)
            SmartSnackbar.make(e).show()
        }
        for (ip in removed) TrafficRecorder.unregister(ip, downstream)
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
        ipForward = true
    }

    suspend fun disableIpv6() {
        ipv6Block = true
    }

    suspend fun ipv6Nat() {
        ipv6Nat = true
    }

    suspend fun forward() {
        forward = true
    }

    suspend fun masquerade(mode: MasqueradeMode) {
        masqueradeMode = mode
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
        if (clients.isNotEmpty()) TrafficRecorder.update()
        daemonSession?.close(removeMode = if (withdrawCleanupPrefixes) {
            DaemonProtocol.RemoveMode.WithdrawCleanup
        } else DaemonProtocol.RemoveMode.PreserveCleanup)
        daemonSession = null
        updateSignal.close()
        scope.cancel("Routing stopped")
        Timber.i("Stopped routing for $downstream by $caller")
    }

    suspend fun commit() {
        if (daemonSession == null) {
            val session = DaemonSession()
            try {
                session.prepare()
                daemonSession = session
            } catch (e: Exception) {
                if (!ipv6Nat || e is CancellationException) throw e
                Timber.w(e)
                SmartSnackbar.make(app.getString(R.string.warn_ipv6_nat_failed, e.readableMessage)).show()
                ipv6Nat = false
                val fallback = DaemonSession()
                try {
                    fallback.prepare()
                    daemonSession = fallback
                } catch (fallbackError: Exception) {
                    withContext(NonCancellable) { fallback.close() }
                    throw fallbackError
                }
            }
        }
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
        stop()
        for (ip in clients.keys.toList()) TrafficRecorder.unregister(ip, downstream)
        clients.clear()
        allowedMacs.clear()
        fallbackUpstream.interfaces.clear()
        upstream.interfaces.clear()
    }
}
