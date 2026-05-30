package be.mygod.vpnhotspot

import android.app.Service
import be.mygod.vpnhotspot.net.NetlinkNeighbour
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.flow.flatMapLatest
import kotlinx.coroutines.flow.flowOf
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.flow.onStart
import kotlinx.coroutines.launch

abstract class NetlinkNeighbourMonitoringService : Service(), CoroutineScope {
    protected data class Interfaces(
        val active: List<String> = emptyList(),
        val inactive: List<String> = emptyList(),
    )

    /**
     * Drives the foreground notification: emit [Interfaces] to display counts for [Interfaces.active] (plus
     * the inactive interfaces with no counts), or null to display nothing. The count sources are subscribed
     * only while [Interfaces.active] is non-empty, so this flow alone starts and stops all monitoring.
     *
     * Each refresh is derived from a single emission of this flow — the counts and the inactive list always
     * come from the same [Interfaces] — so a null emission switches the pipeline to "show nothing" (cancelling
     * the in-progress count collection) and can never be paired with a stale prior value. Removing the
     * notification stays the caller's responsibility ([ServiceNotification.stopForeground]); emitting null only
     * stops this service from refreshing it.
     */
    protected abstract val interfaces: Flow<Interfaces?>

    protected fun netlinkSizeLookup(snapshot: NetlinkNeighbour.Snapshot) =
        snapshot.neighbours.groupBy { it.dev }.mapValues { (_, neighbours) ->
            neighbours.mapNotNull { it.validClientMac }.distinct().size
        }
    /**
     * Per-active-interface client counts, collected only while [active] is non-empty. The default counts
     * netlink neighbours; subclasses override to blend in authoritative sources (which fall back to the
     * netlink count for interfaces they do not cover).
     */
    protected open fun countsFlow(active: List<String>) = NetlinkNeighbour.monitorSnapshots.map { snapshot ->
        val sizeLookup = netlinkSizeLookup(snapshot)
        active.associateWith { sizeLookup[it] ?: 0 }
    }

    private data class Content(val counts: Map<String, Int>, val inactive: List<String>)
    @OptIn(ExperimentalCoroutinesApi::class)
    override fun onCreate() {
        super.onCreate()
        launch {
            interfaces.flatMapLatest { ifaces ->
                when {
                    ifaces == null -> flowOf<Content?>(null)
                    ifaces.active.isEmpty() -> flowOf(Content(emptyMap(), ifaces.inactive))
                    else -> countsFlow(ifaces.active)
                        .onStart { emit(ifaces.active.associateWith { 0 }) }
                        .map { Content(it, ifaces.inactive) }
                }
            }.distinctUntilChanged().collect { content ->
                content?.let {
                    ServiceNotification.startForeground(
                        this@NetlinkNeighbourMonitoringService, it.counts, it.inactive, false)
                }
            }
        }
    }
}
