package be.mygod.vpnhotspot.net

import android.annotation.SuppressLint
import android.net.MacAddress
import android.net.RouteInfo
import android.provider.Settings
import androidx.collection.MutableObjectList
import androidx.collection.MutableScatterMap
import androidx.collection.MutableScatterSet
import androidx.collection.ScatterMap
import androidx.collection.ScatterSet
import androidx.collection.toScatterSet
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.monitor.TrafficRecorder
import be.mygod.vpnhotspot.net.monitor.Upstream
import be.mygod.vpnhotspot.net.monitor.Upstreams
import be.mygod.vpnhotspot.room.AppDatabase
import be.mygod.vpnhotspot.root.RootManager
import be.mygod.vpnhotspot.root.daemon.ClientConfig
import be.mygod.vpnhotspot.root.daemon.DaemonController
import be.mygod.vpnhotspot.root.daemon.Ipv6NatConfig
import be.mygod.vpnhotspot.root.daemon.Ipv6Prefix
import be.mygod.vpnhotspot.root.daemon.MasqueradeMode
import be.mygod.vpnhotspot.root.daemon.SessionConfig
import be.mygod.vpnhotspot.util.allInterfaceNames
import be.mygod.vpnhotspot.util.allRoutes
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineExceptionHandler
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.Job
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import okio.ByteString.Companion.toByteString
import timber.log.Timber
import java.io.IOException
import java.net.Inet4Address
import java.net.Inet6Address
import java.util.concurrent.atomic.AtomicBoolean

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
    var masqueradeMode: MasqueradeMode = MasqueradeMode.MASQUERADE_MODE_NONE

    private val fallbackUpstream = UpstreamTracker()
    private val primaryUpstream = UpstreamTracker()
    private val clients = MutableScatterMap<Inet4Address, MacAddress>()
    private val allowedMacs = MutableScatterSet<MacAddress>()
    private val started = AtomicBoolean()
    private val job = Job()

    private sealed class RoutingUpdate(val key: Any) {
        class UpstreamSnapshot(val upstream: UpstreamTracker, val value: Upstream?) : RoutingUpdate(upstream)
        class NeighboursSnapshot(val neighbours: Collection<NetlinkNeighbour>) : RoutingUpdate(NeighboursSnapshot) {
            private companion object
        }
        class BlockedMacsSnapshot(val macs: ScatterSet<MacAddress>) : RoutingUpdate(BlockedMacsSnapshot) {
            private companion object
        }
    }

    fun start(): Boolean {
        if (!started.compareAndSet(false, true)) return false
        CoroutineScope(job + CoroutineExceptionHandler { _, e ->
            Timber.w(e)
            SmartSnackbar.make(e).show()
        }).launch {
            var session: DaemonController.SessionCall? = null
            try {
                val updates = Channel<RoutingUpdate>(Channel.UNLIMITED)
                launch {
                    Upstreams.primary.collect { updates.trySend(RoutingUpdate.UpstreamSnapshot(primaryUpstream, it)) }
                }
                launch {
                    Upstreams.fallback.collect { updates.trySend(RoutingUpdate.UpstreamSnapshot(fallbackUpstream, it)) }
                }
                launch { NetlinkNeighbour.snapshots.collect { updates.trySend(RoutingUpdate.NeighboursSnapshot(it)) } }
                val initialBlockedMacs = CompletableDeferred<ScatterSet<MacAddress>>()
                launch {
                    var first = true
                    AppDatabase.instance.clientRecordDao.observeBlockedMacs().collect {
                        val macs = it.toScatterSet()
                        if (first) {
                            first = false
                            initialBlockedMacs.complete(macs)
                        } else updates.trySend(RoutingUpdate.BlockedMacsSnapshot(macs))
                    }
                }
                session = DaemonController.startSession(nextConfig())
                launch(start = CoroutineStart.UNDISPATCHED) {
                    session.events.collect { event ->
                        event.ipsec_forward_policy?.let {
                            try {
                                RootManager.use { server ->
                                    server.execute(IpSecForwardPolicyCommand(
                                        uid = it.uid,
                                        sourceAddress = it.source_address,
                                        destinationAddress = it.destination_address,
                                        markValue = it.mark_value,
                                        xfrmInterfaceId = it.xfrm_interface_id,
                                    ))
                                }
                            } catch (e: CancellationException) {
                                throw e
                            } catch (e: Exception) {
                                Timber.w(e)
                                SmartSnackbar.make(e).show()
                            }
                        } ?: throw IOException("Unexpected session event $event")
                    }
                }
                var blockedMacs = initialBlockedMacs.await()
                var neighbours: Collection<NetlinkNeighbour> = emptyList()
                Timber.i("Started routing for $downstream by $caller")
                val pendingUpdates = MutableScatterMap<Any, RoutingUpdate>()
                for (update in updates) {
                    pendingUpdates[update.key] = update
                    while (true) {
                        val next = updates.tryReceive().getOrNull() ?: break
                        pendingUpdates[next.key] = next
                    }
                    try {
                        var upstreamChanged = false
                        var clientPolicyChanged = false
                        pendingUpdates.forEachValue { pending ->
                            withContext(NonCancellable) {
                                when (pending) {
                                    is RoutingUpdate.UpstreamSnapshot -> {
                                        upstreamChanged = pending.upstream.update(pending.value) || upstreamChanged
                                    }
                                    is RoutingUpdate.NeighboursSnapshot -> {
                                        neighbours = pending.neighbours
                                        clientPolicyChanged = true
                                    }
                                    is RoutingUpdate.BlockedMacsSnapshot -> if (blockedMacs != pending.macs) {
                                        blockedMacs = pending.macs
                                        clientPolicyChanged = true
                                    }
                                }
                            }
                        }
                        var clientSnapshot: ScatterMap<Inet4Address, MacAddress> = clients
                        var allowedMacSnapshot: ScatterSet<MacAddress> = allowedMacs
                        var nextClients: ScatterMap<Inet4Address, MacAddress>? = null
                        var nextAllowedMacs: ScatterSet<MacAddress>? = null
                        var added = MutableObjectList<Inet4Address>(0)
                        var removed = MutableObjectList<Inet4Address>(0)
                        var addedMacs = MutableObjectList<MacAddress>(0)
                        var removedMacs = MutableObjectList<MacAddress>(0)
                        val clientsChanged = if (clientPolicyChanged) {
                            val candidateClients = MutableScatterMap<Inet4Address, MacAddress>(neighbours.size)
                            val candidateAllowedMacs = MutableScatterSet<MacAddress>()
                            for (neighbour in neighbours) {
                                val lladdr = neighbour.validClientMac ?: continue
                                if (neighbour.dev != downstream || lladdr in blockedMacs) continue
                                candidateAllowedMacs.add(lladdr)
                                if (neighbour.ip is Inet4Address) candidateClients[neighbour.ip] = lladdr
                            }
                            if (candidateClients == clients && candidateAllowedMacs == allowedMacs) false else {
                                removed = MutableObjectList(clients.size)
                                clients.forEach { ip, mac -> if (candidateClients[ip] != mac) removed.add(ip) }
                                removedMacs = MutableObjectList(allowedMacs.size)
                                allowedMacs.forEach { mac -> if (mac !in candidateAllowedMacs) removedMacs.add(mac) }
                                // record stats before removing rules to prevent stats losing
                                if (removed.isNotEmpty() || removedMacs.isNotEmpty()) {
                                    withContext(NonCancellable) { TrafficRecorder.update() }
                                }
                                added = MutableObjectList(candidateClients.size)
                                candidateClients.forEach { ip, mac -> if (clients[ip] != mac) added.add(ip) }
                                addedMacs = MutableObjectList(candidateAllowedMacs.size)
                                candidateAllowedMacs.forEach { mac -> if (mac !in allowedMacs) addedMacs.add(mac) }
                                clientSnapshot = candidateClients
                                allowedMacSnapshot = candidateAllowedMacs
                                nextClients = candidateClients
                                nextAllowedMacs = candidateAllowedMacs
                                true
                            }
                        } else false
                        if (upstreamChanged || clientsChanged) try {
                            withContext(NonCancellable) {
                                DaemonController.replaceSession(
                                    session.id,
                                    nextConfig(clientSnapshot, allowedMacSnapshot),
                                )
                            }
                        } catch (e: CancellationException) {
                            throw e
                        } catch (e: Exception) {
                            Timber.w(e)
                            SmartSnackbar.make(e).show()
                            continue
                        }
                        if (clientsChanged) {
                            val committedClients = nextClients!!
                            withContext(NonCancellable) {
                                removed.forEach { TrafficRecorder.unregister(it, downstream) }
                                removedMacs.forEach { TrafficRecorder.unregister(it, downstream) }
                            }
                            addedMacs.forEach { TrafficRecorder.register(it, downstream) }
                            added.forEach { ip ->
                                try {
                                    TrafficRecorder.register(ip, downstream, committedClients[ip]!!)
                                } catch (e: CancellationException) {
                                    throw e
                                } catch (e: Exception) {
                                    Timber.w(e)
                                    SmartSnackbar.make(e).show()
                                }
                            }
                            clients.clear()
                            clients.putAll(committedClients)
                            allowedMacs.clear()
                            allowedMacs.addAll(nextAllowedMacs!!)
                        }
                    } catch (e: CancellationException) {
                        throw e
                    } catch (e: Exception) {
                        Timber.w(e)
                        SmartSnackbar.make(e).show()
                    } finally {
                        pendingUpdates.clear()
                    }
                }
            } finally {
                withContext(NonCancellable) {
                    // record stats before exiting to prevent stats losing
                    if (clients.isNotEmpty() || allowedMacs.isNotEmpty()) TrafficRecorder.update()
                    session?.close()
                    Timber.i("Stopped routing for $downstream by $caller")
                }
            }
        }
        return true
    }

    private class UpstreamTracker {
        private var interfaces = emptyList<String>()
        var upstream: Upstream? = null
            private set

        fun update(value: Upstream?): Boolean {
            if (upstream == value) return false
            upstream = value
            interfaces = value?.properties?.allInterfaceNames ?: emptyList()
            return true
        }

        fun appendInterfaces(seen: MutableScatterSet<String>) = buildList(interfaces.size) {
            interfaces.forEach { if (seen.add(it)) this += it }
        }
    }

    private fun nextConfig(
        clientSnapshot: ScatterMap<Inet4Address, MacAddress> = clients,
        allowedMacSnapshot: ScatterSet<MacAddress> = allowedMacs,
    ) = MutableScatterSet<String>().let { seen ->
        SessionConfig(
            downstream = downstream,
            ip_forward = ipForward,
            masquerade = masqueradeMode,
            ipv6_block = ipv6Mode == Ipv6Mode.Block,
            primary_network = primaryUpstream.upstream?.network?.networkHandle,
            primary_routes = primaryUpstream.upstream?.properties?.allRoutes?.mapNotNull { route ->
                val destination = route.destination
                val address = destination.address
                if (route.type == RouteInfo.RTN_UNICAST && address is Inet6Address) {
                    Ipv6Prefix(address.address.toByteString(), destination.prefixLength)
                } else null
            } ?: emptyList(),
            fallback_network = fallbackUpstream.upstream?.network?.networkHandle,
            primary_upstream_interfaces = primaryUpstream.appendInterfaces(seen),
            fallback_upstream_interfaces = fallbackUpstream.appendInterfaces(seen),
            clients = buildList(allowedMacSnapshot.size) {
                allowedMacSnapshot.forEach { mac ->
                    add(ClientConfig(
                        mac = mac.toByteArray().toByteString(),
                        ipv4 = buildList(clientSnapshot.size) {
                            clientSnapshot.forEach { ip, clientMac ->
                                if (clientMac == mac) add(ip.address.toByteString())
                            }
                        },
                    ))
                }
            },
            ipv6_nat = if (ipv6Mode == Ipv6Mode.Nat) Ipv6NatConfig(ipv6NatPrefixSeed) else null,
        )
    }

    suspend fun stopForClean() = withContext(NonCancellable) { job.cancelAndJoin() }

    suspend fun revert() = withContext(NonCancellable) {
        job.cancelAndJoin()
        clients.forEach { ip, _ -> TrafficRecorder.unregister(ip, downstream) }
        allowedMacs.forEach { TrafficRecorder.unregister(it, downstream) }
        clients.clear()
        allowedMacs.clear()
    }
}
