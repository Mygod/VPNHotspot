package be.mygod.vpnhotspot.net

import android.net.MacAddress
import be.mygod.vpnhotspot.root.daemon.DaemonController
import be.mygod.vpnhotspot.root.daemon.Neighbour
import be.mygod.vpnhotspot.root.daemon.NeighbourDelta
import be.mygod.vpnhotspot.root.daemon.NeighbourMonitorUpdate
import be.mygod.vpnhotspot.root.daemon.NeighbourState
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.collections.immutable.PersistentMap
import kotlinx.collections.immutable.mutate
import kotlinx.collections.immutable.persistentHashMapOf
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
import java.net.InetAddress

private fun ByteString.toInetAddress() = toByteArray().let { bytes ->
    when (bytes.size) {
        4, 16 -> InetAddress.getByAddress(bytes)
        else -> throw IOException("Invalid IP address length ${bytes.size}")
    }
}

/** Neighbours and bridge membership folded from daemon neighbour monitor updates. */
internal data class NeighbourMonitorState(
    val neighbours: PersistentMap<Pair<ByteString, String>, NetlinkNeighbour> = persistentHashMapOf(),
    val bridgeMasterByMember: PersistentMap<String, String> = persistentHashMapOf(),
) {
    operator fun plus(update: NeighbourMonitorUpdate) = copy(
        neighbours = (if (update.replace_neighbours) persistentHashMapOf() else neighbours).mutate {
            for (entry in update.neighbour_deltas) {
                when (val delta = entry.delta) {
                    is NeighbourDelta.Delta.Upsert -> {
                        val neighbour = NetlinkNeighbour(delta.value)
                        it[delta.value.address to neighbour.dev] = neighbour
                    }
                    is NeighbourDelta.Delta.Delete -> {
                        val address = delta.value.address
                        address.toInetAddress()
                        it.remove(address to delta.value.dev)
                    }
                    null -> throw IOException("Missing neighbour delta")
                }
            }
        },
        bridgeMasterByMember = update.link_topology?.let { topology ->
            persistentHashMapOf<String, String>().mutate {
                for (bridge in topology.bridges) for (member in bridge.members) {
                    if (bridge.name.isNotEmpty() && member.isNotEmpty()) it[member] = bridge.name
                }
            }
        } ?: bridgeMasterByMember,
    )
}

data class NetlinkNeighbour(val proto: Neighbour) {
    val ip = proto.address.toInetAddress()
    inline val dev get() = proto.dev
    val lladdr = proto.lladdr?.toByteArray()?.also {
        if (it.size != 6) throw IOException("Invalid neighbour link-layer address length ${it.size}")
    }?.let(MacAddress::fromBytes)
    inline val state get() = proto.state
    val validClientMac get() = if (state == NeighbourState.NEIGHBOUR_STATE_VALID) lladdr else null

    init {
        if (state is NeighbourState.Unrecognized) throw IOException("Invalid neighbour state ${state.value}")
    }

    data class Snapshot(
        val neighbours: Collection<NetlinkNeighbour> = emptyList(),
        val bridgeMasterByMember: PersistentMap<String, String> = persistentMapOf(),
    )

    companion object {
        val monitorSnapshots = DaemonController.neighbourMonitor()
            .runningFold(NeighbourMonitorState(), NeighbourMonitorState::plus).drop(1).distinctUntilChanged()
            .map { Snapshot(it.neighbours.values, it.bridgeMasterByMember) }.catch { e ->
                if (e is CancellationException) throw e
                Timber.w(e)
                SmartSnackbar.make(e).show()
            }.shareIn(
                CoroutineScope(Dispatchers.Default.limitedParallelism(1, "NetlinkNeighbour") + SupervisorJob()),
                SharingStarted.WhileSubscribed(replayExpirationMillis = 0),
                replay = 1,
            )

        val snapshots = monitorSnapshots.map { it.neighbours }.distinctUntilChanged()
    }
}
