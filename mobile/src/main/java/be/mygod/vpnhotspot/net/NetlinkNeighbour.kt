package be.mygod.vpnhotspot.net

import android.net.MacAddress
import be.mygod.vpnhotspot.root.daemon.DaemonController
import be.mygod.vpnhotspot.root.daemon.Neighbour
import be.mygod.vpnhotspot.root.daemon.NeighbourState
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.collections.immutable.PersistentMap
import kotlinx.collections.immutable.mutate
import kotlinx.collections.immutable.persistentMapOf
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.flow.SharingStarted
import kotlinx.coroutines.flow.catch
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.flow.drop
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.flow.runningFold
import kotlinx.coroutines.flow.shareIn
import timber.log.Timber
import okio.ByteString
import java.io.IOException
import java.net.Inet4Address
import java.net.InetAddress

data class NetlinkNeighbour(val proto: Neighbour) {
    val ip = proto.address.toInetAddress()
    inline val dev get() = proto.dev
    val lladdr = proto.lladdr?.toByteArray()?.also {
        if (it.size != 6) throw IOException("Invalid neighbour link-layer address length ${it.size}")
    }?.let(MacAddress::fromBytes)
    inline val state get() = proto.state
    val validIpv4ClientMac: MacAddress? get() {
        if (state != NeighbourState.NEIGHBOUR_STATE_VALID || ip !is Inet4Address) return null
        return lladdr
    }

    init {
        if (state is NeighbourState.Unrecognized) throw IOException("Invalid neighbour state ${state.value}")
    }

    data class Snapshot(
        val neighbours: Collection<NetlinkNeighbour>,
        val bridgeMasterByMember: PersistentMap<String, String>,
    )

    companion object {
        private data class MonitorState(
            val neighbours: PersistentMap<Pair<ByteString, String>, NetlinkNeighbour> = persistentMapOf(),
            val bridgeMasterByMember: PersistentMap<String, String> = persistentMapOf(),
        )

        private fun ByteString.toInetAddress() = toByteArray().let { bytes ->
            when (bytes.size) {
                4, 16 -> InetAddress.getByAddress(bytes)
                else -> throw IOException("Invalid IP address length ${bytes.size}")
            }
        }

        val monitorSnapshots = DaemonController.neighbourMonitor()
            .runningFold(MonitorState()) { state, update ->
                state.copy(
                    neighbours = state.neighbours.mutate {
                        for (delta in update.neighbour_deltas) {
                            when {
                                delta.upsert != null -> {
                                    val neighbour = NetlinkNeighbour(delta.upsert)
                                    it[delta.upsert.address to neighbour.dev] = neighbour
                                }
                                delta.delete != null -> {
                                    val address = delta.delete.address
                                    address.toInetAddress()
                                    it.remove(address to delta.delete.dev)
                                }
                                else -> throw IOException("Missing neighbour delta")
                            }
                        }
                    },
                    bridgeMasterByMember = update.link_topology?.let { topology ->
                        persistentMapOf<String, String>().mutate {
                            for (bridge in topology.bridges) for (member in bridge.members) {
                                if (bridge.name.isNotEmpty() && member.isNotEmpty()) it[member] = bridge.name
                            }
                        }
                    } ?: state.bridgeMasterByMember,
                )
            }
            .drop(1)
            .distinctUntilChanged()
            .map { Snapshot(it.neighbours.values, it.bridgeMasterByMember) }
            .catch { e ->
                if (e is CancellationException) throw e
                Timber.w(e)
                SmartSnackbar.make(e).show()
            }
            .shareIn(
                CoroutineScope(Dispatchers.Default.limitedParallelism(1, "NetlinkNeighbour") + SupervisorJob()),
                SharingStarted.WhileSubscribed(replayExpirationMillis = 0),
                replay = 1,
            )

        val snapshots = monitorSnapshots.map { it.neighbours }.distinctUntilChanged()
    }
}
