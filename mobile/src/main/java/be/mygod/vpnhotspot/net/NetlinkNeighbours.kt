package be.mygod.vpnhotspot.net

import android.net.MacAddress
import be.mygod.vpnhotspot.root.daemon.DaemonController
import be.mygod.vpnhotspot.root.daemon.DaemonProto
import be.mygod.vpnhotspot.widget.SmartSnackbar
import com.google.protobuf.ByteString
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
import java.io.IOException
import java.net.Inet4Address
import java.net.InetAddress

val netlinkNeighbours = DaemonController.neighbourMonitor()
    .runningFold(persistentMapOf<Pair<ByteString, String>, DaemonProto.Neighbour>()) { neighbours, deltas ->
        neighbours.mutate {
            for (delta in deltas) when (delta.deltaCase) {
                DaemonProto.NeighbourDelta.DeltaCase.UPSERT -> {
                    val neighbour = delta.upsert
                    neighbour.address.toInetAddress()
                    if (neighbour.state == DaemonProto.NeighbourState.UNRECOGNIZED) {
                        throw IOException("Invalid neighbour state ${neighbour.stateValue}")
                    }
                    neighbour.macAddress()
                    it[neighbour.address to neighbour.`interface`] = neighbour
                }
                DaemonProto.NeighbourDelta.DeltaCase.DELETE -> {
                    val delete = delta.delete
                    delete.address.toInetAddress()
                    it.remove(delete.address to delete.`interface`)
                }
                DaemonProto.NeighbourDelta.DeltaCase.DELTA_NOT_SET -> throw IOException("Missing neighbour delta")
            }
        }
    }
    .drop(1)
    .distinctUntilChanged()
    .map { it.values }
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

fun DaemonProto.Neighbour.macAddress(): MacAddress? {
    if (!hasLladdr()) return null
    return lladdr.toByteArray().also {
        if (it.size != 6) throw IOException("Invalid neighbour link-layer address length ${it.size}")
    }.let(MacAddress::fromBytes)
}

fun DaemonProto.Neighbour.validIpv4ClientMac(): MacAddress? {
    if (state != DaemonProto.NeighbourState.NEIGHBOUR_STATE_VALID || address.toInetAddress() !is Inet4Address) {
        return null
    }
    return macAddress()
}

fun ByteString.toInetAddress() = toByteArray().let { bytes ->
    when (bytes.size) {
        4, 16 -> InetAddress.getByAddress(bytes)
        else -> throw IOException("Invalid IP address length ${bytes.size}")
    }
}
