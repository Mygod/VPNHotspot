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

data class NetlinkNeighbour(val proto: DaemonProto.Neighbour) {
    val ip = proto.address.toInetAddress()
    inline val dev get() = proto.dev
    val lladdr = if (proto.hasLladdr()) {
        proto.lladdr.toByteArray().also {
            if (it.size != 6) throw IOException("Invalid neighbour link-layer address length ${it.size}")
        }.let(MacAddress::fromBytes)
    } else null
    inline val state get() = proto.state
    val validIpv4ClientMac: MacAddress? get() {
        if (state != DaemonProto.NeighbourState.NEIGHBOUR_STATE_VALID || ip !is Inet4Address) return null
        return lladdr
    }

    init {
        if (state == DaemonProto.NeighbourState.UNRECOGNIZED) {
            throw IOException("Invalid neighbour state ${proto.stateValue}")
        }
    }

    companion object {
        private fun ByteString.toInetAddress() = toByteArray().let { bytes ->
            when (bytes.size) {
                4, 16 -> InetAddress.getByAddress(bytes)
                else -> throw IOException("Invalid IP address length ${bytes.size}")
            }
        }

        val snapshots = DaemonController.neighbourMonitor()
            .runningFold(persistentMapOf<Pair<ByteString, String>, NetlinkNeighbour>()) { neighbours, deltas ->
                neighbours.mutate {
                    for (delta in deltas) when (delta.deltaCase) {
                        DaemonProto.NeighbourDelta.DeltaCase.UPSERT -> {
                            val neighbour = NetlinkNeighbour(delta.upsert)
                            it[delta.upsert.address to neighbour.dev] = neighbour
                        }
                        DaemonProto.NeighbourDelta.DeltaCase.DELETE -> {
                            val address = delta.delete.address
                            address.toInetAddress()
                            it.remove(address to delta.delete.dev)
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
    }
}
