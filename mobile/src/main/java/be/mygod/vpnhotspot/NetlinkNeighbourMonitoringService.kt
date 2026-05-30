package be.mygod.vpnhotspot

import android.app.Service
import be.mygod.vpnhotspot.net.NetlinkNeighbour
import be.mygod.vpnhotspot.net.wifi.WifiApManager
import be.mygod.vpnhotspot.net.wifi.apInstanceIdentifierOrNull
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.catch
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.flow.flatMapLatest
import kotlinx.coroutines.flow.flowOf
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.flow.onStart
import kotlinx.coroutines.flow.runningFold
import kotlinx.coroutines.launch
import timber.log.Timber

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

    private fun netlinkSizeLookup(snapshot: NetlinkNeighbour.Snapshot) =
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

    /**
     * The Soft AP callback's latest view, kept as raw AP-instance identifiers (canonicalized lazily against
     * the current bridge topology). [links] are the active AP links from OnInfoChanged — the interfaces the
     * callback is authoritative for — and [clients] are the connected clients. Either is null until its
     * callback first arrives (a null [clients] is "not observed yet", distinct from an observed empty list),
     * keeping counts on netlink until the Soft AP state is fully known.
     */
    private data class SoftApState(val links: List<String?>? = null, val clients: List<String?>? = null) {
        /**
         * Per-interface Soft AP connected-client counts. The authoritative interface set comes from the active
         * AP links (each instance canonicalized through the bridge topology, so bridged links merge onto their
         * bridge), tallied from the connected clients and reporting 0 for a link with no clients. Returns an
         * empty map — falling every interface back to netlink — whenever the callback data can't yield a
         * trustworthy tally, so an authoritative count never undercounts the netlink one.
         */
        fun counts(snapshot: NetlinkNeighbour.Snapshot): Map<String, Int> {
            val links = links ?: return emptyMap()
            // Until the first connected-clients callback the list is unknown (not "zero"), so stay on netlink
            // rather than authoritatively reporting 0 for links whose clients have not been observed yet.
            val clients = clients ?: return emptyMap()
            val ifaces = HashSet<String>(links.size)
            for (link in links) ifaces.add((link ?: continue).let { snapshot.bridgeMasterByMember[it] ?: it })
            if (ifaces.isEmpty()) return emptyMap()
            val counts = HashMap<String, Int>()
            for (client in clients) {
                // A connected client without an AP instance identifier can't be attributed to any interface, so
                // every interface's tally would be an undercount that wrongly overrides netlink — fall back instead.
                val canonical = (client ?: return emptyMap()).let { snapshot.bridgeMasterByMember[it] ?: it }
                counts[canonical] = (counts[canonical] ?: 0) + 1
            }
            return ifaces.associateWith { counts[it] ?: 0 }
        }
    }
    protected fun softApCountsFlow(active: List<String>, events: Flow<WifiApManager.Event>) = combine(
        NetlinkNeighbour.monitorSnapshots,
        events.runningFold(SoftApState()) { state, event ->
            when (event) {
                is WifiApManager.Event.OnInfoChanged ->
                    state.copy(links = event.info.map { it.apInstanceIdentifierOrNull })
                is WifiApManager.Event.OnConnectedClientsChanged ->
                    state.copy(clients = event.clients.map { it.apInstanceIdentifierOrNull })
                else -> state
            }
        }.catch { e ->
            // Soft AP callback unavailable: drop authoritative counts so every interface falls back to netlink.
            if (e is WifiApManager.SoftApCallbackUnavailableException && e.cause == null) Timber.d(e) else Timber.w(e)
            emit(SoftApState())
        },
    ) { snapshot, softAp ->
        val sizeLookup = netlinkSizeLookup(snapshot)
        val authoritative = softAp.counts(snapshot)
        active.associateWith { authoritative[it] ?: sizeLookup[it] ?: 0 }
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
