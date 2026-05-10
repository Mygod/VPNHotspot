package be.mygod.vpnhotspot.net

import android.net.MacAddress
import be.mygod.vpnhotspot.root.daemon.DaemonController
import be.mygod.vpnhotspot.root.daemon.DaemonProtocol
import be.mygod.vpnhotspot.widget.SmartSnackbar
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
import java.net.InetAddress

data class NetlinkNeighbour(val ip: InetAddress, val dev: String, val lladdr: MacAddress?, val state: State) {
    companion object {
        val snapshots = DaemonController.neighbourMonitor()
                .runningFold(persistentMapOf<IpDev, NetlinkNeighbour>()) { neighbours, deltas ->
                    neighbours.mutate {
                        for (delta in deltas) when (delta) {
                            is DaemonProtocol.NeighbourDelta.Upsert ->
                                it[IpDev(delta.neighbour.ip, delta.neighbour.dev)] = delta.neighbour
                            is DaemonProtocol.NeighbourDelta.Delete -> it.remove(IpDev(delta.ip, delta.dev))
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
                .shareIn(CoroutineScope(Dispatchers.Default.limitedParallelism(1, "NetlinkNeighbour") +
                        SupervisorJob()), SharingStarted.WhileSubscribed(replayExpirationMillis = 0), replay = 1)
    }

    enum class State {
        UNSET, INCOMPLETE, VALID, FAILED
    }
}

data class IpDev(val ip: InetAddress, val dev: String) {
    override fun toString() = "$ip%$dev"
}
