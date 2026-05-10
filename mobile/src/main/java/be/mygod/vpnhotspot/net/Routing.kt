package be.mygod.vpnhotspot.net

import android.annotation.SuppressLint
import android.net.MacAddress
import android.net.RouteInfo
import android.os.Build
import android.provider.Settings
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.monitor.TrafficRecorder
import be.mygod.vpnhotspot.net.monitor.Upstream
import be.mygod.vpnhotspot.net.monitor.Upstreams
import be.mygod.vpnhotspot.room.AppDatabase
import be.mygod.vpnhotspot.root.RootManager
import be.mygod.vpnhotspot.root.daemon.DaemonController
import be.mygod.vpnhotspot.root.daemon.DaemonProto
import be.mygod.vpnhotspot.root.daemon.DaemonProto.MasqueradeMode
import be.mygod.vpnhotspot.util.allInterfaceNames
import be.mygod.vpnhotspot.util.allRoutes
import be.mygod.vpnhotspot.widget.SmartSnackbar
import com.google.protobuf.ByteString
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
import timber.log.Timber
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
    var masqueradeMode = MasqueradeMode.MASQUERADE_MODE_NONE

    private val fallbackUpstream = UpstreamTracker()
    private val primaryUpstream = UpstreamTracker()
    private val clients = linkedMapOf<Inet4Address, MacAddress>()
    private val allowedMacs = linkedSetOf<MacAddress>()
    private val started = AtomicBoolean()
    private val job = Job()

    private sealed class RoutingUpdate(val key: Any) {
        class UpstreamSnapshot(val upstream: UpstreamTracker, val value: Upstream?) : RoutingUpdate(upstream)
        class NeighboursSnapshot(val neighbours: Collection<NetlinkNeighbour>) : RoutingUpdate(NeighboursSnapshot) {
            private companion object
        }
        class BlockedMacsSnapshot(val macs: Set<MacAddress>) : RoutingUpdate(BlockedMacsSnapshot) {
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
                val initialBlockedMacs = CompletableDeferred<Set<MacAddress>>()
                launch {
                    var first = true
                    AppDatabase.instance.clientRecordDao.observeBlockedMacs().collect {
                        val macs = it.toSet()
                        if (first) {
                            first = false
                            initialBlockedMacs.complete(macs)
                        } else updates.trySend(RoutingUpdate.BlockedMacsSnapshot(macs))
                    }
                }
                session = DaemonController.startSession(nextConfig())
                launch(start = CoroutineStart.UNDISPATCHED) { session.closed.collect { } }
                var blockedMacs = initialBlockedMacs.await()
                var neighbours: Collection<NetlinkNeighbour> = emptyList()
                Timber.i("Started routing for $downstream by $caller")
                val pendingUpdates = LinkedHashMap<Any, RoutingUpdate>()
                for (update in updates) {
                    pendingUpdates[update.key] = update
                    while (true) {
                        val next = updates.tryReceive().getOrNull() ?: break
                        pendingUpdates[next.key] = next
                    }
                    try {
                        var upstreamChanged = false
                        var clientPolicyChanged = false
                        for (pending in pendingUpdates.values) withContext(NonCancellable) {
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
                        var clientSnapshot: Map<Inet4Address, MacAddress> = clients
                        var allowedMacSnapshot: Set<MacAddress> = allowedMacs
                        var nextClients: LinkedHashMap<Inet4Address, MacAddress>? = null
                        var nextAllowedMacs: LinkedHashSet<MacAddress>? = null
                        var added = emptyMap<Inet4Address, MacAddress>()
                        var removed = emptySet<Inet4Address>()
                        val clientsChanged = if (clientPolicyChanged) {
                            val candidateClients = linkedMapOf<Inet4Address, MacAddress>()
                            val candidateAllowedMacs = linkedSetOf<MacAddress>()
                            for (neighbour in neighbours) {
                                val lladdr = neighbour.validIpv4ClientMac ?: continue
                                if (neighbour.dev != downstream || lladdr in blockedMacs) continue
                                candidateAllowedMacs.add(lladdr)
                                candidateClients[neighbour.ip as Inet4Address] = lladdr
                            }
                            if (candidateClients == clients && candidateAllowedMacs == allowedMacs) false else {
                                removed = clients.keys - candidateClients.keys
                                // record stats before removing rules to prevent stats losing
                                if (removed.isNotEmpty()) withContext(NonCancellable) { TrafficRecorder.update() }
                                added = candidateClients.filterKeys { !clients.containsKey(it) }
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
                            for ((ip, mac) in added) try {
                                TrafficRecorder.register(ip, downstream, mac)
                            } catch (e: CancellationException) {
                                throw e
                            } catch (e: Exception) {
                                Timber.w(e)
                                SmartSnackbar.make(e).show()
                            }
                            withContext(NonCancellable) {
                                for (ip in removed) TrafficRecorder.unregister(ip, downstream)
                            }
                            clients.clear()
                            clients.putAll(nextClients!!)
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
                    if (clients.isNotEmpty()) TrafficRecorder.update()
                    session?.close()
                    Timber.i("Stopped routing for $downstream by $caller")
                }
            }
        }
        return true
    }

    private class UpstreamTracker {
        private var interfaces = linkedSetOf<String>()
        var upstream: Upstream? = null
            private set

        suspend fun update(value: Upstream?): Boolean {
            if (upstream == value) return false
            upstream = value
            val nextInterfaces = linkedSetOf<String>()
            for (ifname in value?.properties?.allInterfaceNames ?: emptyList()) {
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
            interfaces = nextInterfaces
            return true
        }

        fun appendInterfaces(target: MutableList<String>, seen: MutableSet<String>) {
            for (ifname in interfaces) if (seen.add(ifname)) target += ifname
        }
    }

    private fun nextConfig(
        clientSnapshot: Map<Inet4Address, MacAddress> = clients,
        allowedMacSnapshot: Set<MacAddress> = allowedMacs,
    ): DaemonProto.SessionConfig {
        val seen = HashSet<String>()
        val primaryUpstreamInterfaces = buildList {
            primaryUpstream.appendInterfaces(this, seen)
        }
        val fallbackUpstreamInterfaces = buildList {
            fallbackUpstream.appendInterfaces(this, seen)
        }
        return DaemonProto.SessionConfig.newBuilder().also { config ->
            config.downstream = downstream
            config.ipForward = ipForward
            config.masquerade = masqueradeMode
            config.ipv6Block = ipv6Mode == Ipv6Mode.Block
            primaryUpstream.upstream?.network?.let { config.primaryNetwork = it.networkHandle }
            config.addAllPrimaryRoutes((primaryUpstream.upstream?.properties?.allRoutes?.mapNotNull { route ->
                val destination = route.destination
                val address = destination.address
                if (route.type == RouteInfo.RTN_UNICAST && address is Inet6Address) {
                    DaemonProto.Ipv6Prefix.newBuilder()
                        .setAddress(ByteString.copyFrom(address.address))
                        .setPrefixLength(destination.prefixLength)
                        .build()
                } else null
            } ?: emptyList()))
            fallbackUpstream.upstream?.network?.let { config.fallbackNetwork = it.networkHandle }
            config.addAllPrimaryUpstreamInterfaces(primaryUpstreamInterfaces)
            config.addAllFallbackUpstreamInterfaces(fallbackUpstreamInterfaces)
            config.addAllClients(allowedMacSnapshot.map { mac ->
                DaemonProto.ClientConfig.newBuilder()
                    .setMac(ByteString.copyFrom(mac.toByteArray()))
                    .addAllIpv4(clientSnapshot.filterValues { it == mac }.keys.map { ByteString.copyFrom(it.address) })
                    .build()
            })
            if (ipv6Mode == Ipv6Mode.Nat) {
                config.ipv6Nat = DaemonProto.Ipv6NatConfig.newBuilder().setPrefixSeed(ipv6NatPrefixSeed).build()
            }
        }.build()
    }

    suspend fun stopForClean() = withContext(NonCancellable) { job.cancelAndJoin() }

    suspend fun revert() = withContext(NonCancellable) {
        job.cancelAndJoin()
        for (ip in clients.keys) TrafficRecorder.unregister(ip, downstream)
        clients.clear()
        allowedMacs.clear()
    }
}
