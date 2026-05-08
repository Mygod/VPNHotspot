package be.mygod.vpnhotspot.net

import android.annotation.SuppressLint
import android.net.LinkProperties
import android.net.MacAddress
import android.net.Network
import android.net.RouteInfo
import android.os.Build
import android.provider.Settings
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.monitor.FallbackUpstreamMonitor
import be.mygod.vpnhotspot.net.monitor.NetlinkNeighbours
import be.mygod.vpnhotspot.net.monitor.TrafficRecorder
import be.mygod.vpnhotspot.net.monitor.UpstreamMonitor
import be.mygod.vpnhotspot.room.AppDatabase
import be.mygod.vpnhotspot.root.RootManager
import be.mygod.vpnhotspot.root.daemon.DaemonController
import be.mygod.vpnhotspot.root.daemon.DaemonProtocol
import be.mygod.vpnhotspot.util.allInterfaceNames
import be.mygod.vpnhotspot.util.allRoutes
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.DelicateCoroutinesApi
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.flow.merge
import kotlinx.coroutines.flow.produceIn
import kotlinx.coroutines.flow.receiveAsFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import timber.log.Timber
import java.net.Inet4Address
import java.net.Inet6Address
import java.util.concurrent.atomic.AtomicReference

/**
 * Builds and updates a root routing session for one downstream interface.
 */
class Routing(private val caller: Any, private val downstream: String) {
    companion object {
        private val ipv6NatPrefixSeed: String
            @SuppressLint("HardwareIds")
            get() = "${app.packageName}\u0000${
                Settings.Secure.getString(app.contentResolver, Settings.Secure.ANDROID_ID).orEmpty()
            }"

        suspend fun clean() {
            TrafficRecorder.clean()
            DaemonController.cleanRouting(ipv6NatPrefixSeed)
        }
    }

    enum class Ipv6Mode {
        System,
        Block,
        Nat,
    }

    enum class MasqueradeMode(val protocolValue: Byte) {
        None(0),
        Simple(1),
        Netd(2),
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
    var ipForward = false
    var ipv6Mode = Ipv6Mode.System
    var masqueradeMode = MasqueradeMode.None

    private val updates = Channel<RoutingUpdate>(Channel.UNLIMITED)
    private var daemonSession: DaemonSessionHandle? = null
    private val removeMode = AtomicReference(DaemonProtocol.RemoveMode.PreserveCleanup)

    private val fallbackUpstream = UpstreamTracker(DaemonProtocol.UpstreamRole.Fallback)
    private val primaryUpstream = UpstreamTracker(DaemonProtocol.UpstreamRole.Primary)
    private val clients = linkedMapOf<Inet4Address, MacAddress>()
    private val allowedMacs = linkedSetOf<MacAddress>()

    private object NeighbourUpdateKey

    private sealed class RoutingUpdate(val key: Any) {
        class UpstreamSnapshot(
            val upstream: UpstreamTracker,
            val network: Network?,
            val properties: LinkProperties?,
        ) : RoutingUpdate(upstream)
        class NeighboursSnapshot(val neighbours: Collection<NetlinkNeighbour>) : RoutingUpdate(NeighbourUpdateKey)
    }

    @OptIn(DelicateCoroutinesApi::class)
    private val routingJob = GlobalScope.launch(start = CoroutineStart.LAZY) {
        try {
            val session = DaemonSessionHandle()
            daemonSession = session
            session.prepare()
            FallbackUpstreamMonitor.registerCallback(fallbackUpstream)
            UpstreamMonitor.registerCallback(primaryUpstream)
            Timber.i("Started routing for $downstream by $caller")
            val pendingUpdates = LinkedHashMap<Any, RoutingUpdate>()
            val mergedUpdates = merge(
                updates.receiveAsFlow(),
                NetlinkNeighbours.snapshots.map { RoutingUpdate.NeighboursSnapshot(it) },
            ).produceIn(this)
            for (update in mergedUpdates) {
                pendingUpdates[update.key] = update
                while (true) {
                    val next = mergedUpdates.tryReceive().getOrNull() ?: break
                    pendingUpdates[next.key] = next
                }
                for (pending in pendingUpdates.values) withContext(NonCancellable) {
                    try {
                        when (pending) {
                            is RoutingUpdate.UpstreamSnapshot -> pending.upstream.update(
                                pending.network, pending.properties)
                            is RoutingUpdate.NeighboursSnapshot -> updateNeighbours(pending.neighbours)
                        }
                    } catch (e: Exception) {
                        Timber.w(e)
                        SmartSnackbar.make(e).show()
                    }
                }
                pendingUpdates.clear()
            }
        } catch (_: CancellationException) {
        } catch (e: Exception) {
            Timber.w(e)
            SmartSnackbar.make(e).show()
        } finally {
            withContext(NonCancellable) {
                FallbackUpstreamMonitor.unregisterCallback(fallbackUpstream)
                UpstreamMonitor.unregisterCallback(primaryUpstream)
                updates.cancel()
                if (clients.isNotEmpty()) TrafficRecorder.update()
                daemonSession?.close(removeMode.get())
                daemonSession = null
                Timber.i("Stopped routing for $downstream by $caller")
            }
        }
    }

    private inner class UpstreamTracker(private val role: DaemonProtocol.UpstreamRole) : UpstreamMonitor.Callback {
        val interfaces = linkedSetOf<String>()
        var network: Network? = null
            private set
        var properties: LinkProperties? = null
            private set

        override fun onAvailable(network: Network?, properties: LinkProperties?) {
            updates.trySend(RoutingUpdate.UpstreamSnapshot(this, network, properties))
        }
        suspend fun update(network: Network?, properties: LinkProperties?) {
            this.network = network
            this.properties = properties
            val nextInterfaces = linkedSetOf<String>()
            for (ifname in properties?.allInterfaceNames ?: emptyList()) {
                nextInterfaces += ifname
                if (Build.VERSION.SDK_INT >= 31 && !interfaces.contains(ifname)) try {
                    RootManager.use { it.execute(IpSecForwardPolicyCommand(ifname)) }
                } catch (e: CancellationException) {
                    throw e
                } catch (e: Exception) {
                    SmartSnackbar.make(e).show()
                    Timber.w(e)
                }
            }
            interfaces.clear()
            interfaces.addAll(nextInterfaces)
            daemonSession?.update()
        }

        fun appendConfig(target: MutableList<DaemonProtocol.UpstreamConfig>, seen: MutableSet<String>) {
            for (ifname in interfaces) if (seen.add(ifname)) target += DaemonProtocol.UpstreamConfig(role, ifname)
        }
    }

    private inner class DaemonSessionHandle {
        private fun nextConfig(
            clientSnapshot: Map<Inet4Address, MacAddress> = clients,
            allowedMacSnapshot: Set<MacAddress> = allowedMacs,
        ) = DaemonProtocol.SessionConfig(
            downstream = downstream,
            ipForward = ipForward,
            masquerade = masqueradeMode,
            ipv6Block = ipv6Mode == Ipv6Mode.Block,
            primaryNetwork = primaryUpstream.network,
            primaryRoutes = primaryUpstream.properties?.allRoutes?.mapNotNull { route ->
                val destination = route.destination
                if (route.type == RouteInfo.RTN_UNICAST && destination.address is Inet6Address) destination else null
            } ?: emptyList(),
            fallbackNetwork = fallbackUpstream.network,
            upstreams = buildList {
                val seen = HashSet<String>()
                primaryUpstream.appendConfig(this, seen)
                fallbackUpstream.appendConfig(this, seen)
            },
            clients = buildList {
                for (mac in allowedMacSnapshot) add(DaemonProtocol.ClientConfig(mac,
                    clientSnapshot.filterValues { it == mac }.keys.toList()))
            },
            ipv6Nat = if (ipv6Mode == Ipv6Mode.Nat) DaemonProtocol.Ipv6NatConfig(ipv6NatPrefixSeed) else null,
        )

        suspend fun prepare() = DaemonController.startSession(nextConfig())

        suspend fun update(
            clientSnapshot: Map<Inet4Address, MacAddress> = clients,
            allowedMacSnapshot: Set<MacAddress> = allowedMacs,
        ) = try {
            DaemonController.replaceSession(nextConfig(clientSnapshot, allowedMacSnapshot))
            true
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            Timber.w(e)
            SmartSnackbar.make(e).show()
            false
        }

        suspend fun close(
            removeMode: DaemonProtocol.RemoveMode = DaemonProtocol.RemoveMode.PreserveCleanup,
        ) = try {
            DaemonController.removeSession(downstream, removeMode)
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            Timber.w(e)
        }
    }

    private suspend fun updateNeighbours(neighbours: Collection<NetlinkNeighbour>) {
        val nextClients = linkedMapOf<Inet4Address, MacAddress>()
        val nextAllowedMacs = linkedSetOf<MacAddress>()
        for (neighbour in neighbours) {
            val lladdr = neighbour.lladdr ?: continue
            if (neighbour.dev != downstream || neighbour.state != NetlinkNeighbour.State.VALID ||
                    AppDatabase.instance.clientRecordDao.lookupOrDefault(lladdr).blocked) continue
            nextAllowedMacs.add(lladdr)
            val ip = neighbour.ip
            if (ip is Inet4Address) nextClients[ip] = lladdr
        }
        val removed = clients.keys - nextClients.keys
        if (removed.isNotEmpty()) TrafficRecorder.update()
        val added = nextClients.filterKeys { !clients.containsKey(it) }
        if (daemonSession?.update(nextClients, nextAllowedMacs) == false) return
        val registered = ArrayList<Inet4Address>()
        for ((ip, mac) in added) try {
            TrafficRecorder.register(ip, downstream, mac)
            registered += ip
        } catch (e: Exception) {
            Timber.w(e)
            SmartSnackbar.make(e).show()
            for (rollback in registered) TrafficRecorder.unregister(rollback, downstream)
            return
        }
        for (ip in removed) TrafficRecorder.unregister(ip, downstream)
        clients.clear()
        clients.putAll(nextClients)
        allowedMacs.clear()
        allowedMacs.addAll(nextAllowedMacs)
    }

    fun start() = routingJob.start()

    suspend fun stopForClean() = withContext(NonCancellable) {
        removeMode.set(DaemonProtocol.RemoveMode.WithdrawCleanup)
        routingJob.cancelAndJoin()
    }

    suspend fun revert() = withContext(NonCancellable) {
        routingJob.cancelAndJoin()
        for (ip in clients.keys) TrafficRecorder.unregister(ip, downstream)
        clients.clear()
        allowedMacs.clear()
        fallbackUpstream.interfaces.clear()
        primaryUpstream.interfaces.clear()
    }
}
