package be.mygod.vpnhotspot.shizuku

import android.annotation.SuppressLint
import android.net.ConnectivityManager
import android.net.IpPrefix
import android.net.LinkAddress
import android.net.LinkProperties
import android.net.Network
import android.net.NetworkAgent
import android.net.NetworkCapabilities
import android.net.`NetworkCapabilities$Builder`
import android.net.NetworkRequest
import android.net.RouteInfo
import android.net.TestNetworkManager
import android.os.Build
import android.system.Os
import androidx.annotation.StringRes
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.net.TetheringManagerCompat
import be.mygod.vpnhotspot.net.monitor.Upstream
import be.mygod.vpnhotspot.net.monitor.Upstreams
import be.mygod.vpnhotspot.root.daemon.ShizukuSessionConfig
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.UnblockCentral
import be.mygod.vpnhotspot.util.readableMessage
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.TimeoutCancellationException
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.job
import kotlinx.coroutines.launch
import kotlinx.coroutines.selects.select
import kotlinx.coroutines.withContext
import kotlinx.coroutines.withTimeout
import kotlinx.coroutines.withTimeoutOrNull
import timber.log.Timber
import okio.ByteString.Companion.toByteString
import java.net.InetAddress
import java.net.NetworkInterface

/**
 * One restricted TestNetwork session: the TUN, the exact foreground request that keeps it wanted, and
 * the app-hosted agent that publishes it.
 *
 * `TestNetworkManager.setupTestNetwork` is never called, on any path. AOSP's setup path adds
 * `NET_CAPABILITY_NOT_RESTRICTED`, which would let any installed app use the network; only
 * `createTunInterface` is used, and publication is this app's own agent with an empty allowed-UID set.
 *
 * This mode never starts or stops tethering. It does set the global `preferTestNetworks` preference,
 * which is the one piece of system state that can outlive a session, and clears it first on the way
 * out; setting it is not itself a reselection trigger, so a hotspot already holding an ordinary
 * upstream stays there until the user cycles it.
 *
 * It is also entirely independent of root mode. Nothing here starts, stops, delays, refuses or rebuilds root
 * routing, and nothing in root mode consults this: what this publishes is one upstream among the ones
 * Android's own tethering may select, while root mode installs its per-interface routing directly. Running
 * both at once is allowed, and root's routing then takes precedence over whatever upstream Android picked,
 * by the ordinary root design and without either side being told.
 *
 * The path is global rather than per-downstream throughout. Which interfaces Android happens to be tethering
 * behind the selected upstream is never read, stored or made policy from - the session is defined by the
 * exact `Network` it published and the fixed [TEST_NETWORK_MTU] contract its TUN and agent carry.
 */
object ShizukuTestNetwork {
    /**
     * Every address this interface occupies is an address clients cannot reach: their traffic arrives
     * over the default route and is re-originated upstream, but addresses inside the connected prefix
     * are delivered locally instead. The interface's address space is therefore a hole punched in the
     * client-reachable internet, which sets both rules: keep it small, and put it where nothing real
     * lives.
     *
     * TEST-NET-1 (RFC 5737 section 3) is guaranteed never to be assigned, so the hole cannot collide
     * with a destination a client legitimately wants, and it is recognizable on sight in a capture or
     * a bug report as the synthetic upstream rather than a real network.
     */
    private const val TUN_IPV4_ADDRESS = "192.0.2.1"
    /**
     * The interface address plus the virtual DNS addresses are the whole requirement, so this is the
     * smallest prefix worth trying. A host route is deliberately not used: tethering's IPv4 path is
     * not qualified against a `/32` upstream, and Android's `MASQUERADE` rewrites client sources to
     * this address, so it must be present and usable. Widening this is the documented recovery if
     * tethering rejects it.
     */
    private const val TUN_IPV4_PREFIX_LENGTH = 30
    /**
     * Classified by exact address match, while sharing [TUN_IPV4_ADDRESS]'s prefix keeps every synthetic
     * IPv4 destination inside the one connected route.
     */
    private const val VIRTUAL_DNS_IPV4 = "192.0.2.2"
    /**
     * Tethering copies only `isGlobalPreferred()` `/64`s from its upstream into the downstream
     * config, and that predicate rejects ULAs by name, so the prefix clients receive has to look
     * global. The IPv6 documentation prefix (RFC 3849) satisfies the predicate and cannot collide,
     * because that space is guaranteed never to be assigned; within a fully translated dataplane
     * these addresses are local and unroutable by design.
     *
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/src/com/android/networkstack/tethering/IPv6TetheringCoordinator.java#239
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/LinkAddress.java#487
     */
    private const val TUN_IPV6_ADDRESS = "2001:db8:1::1"
    private const val TUN_IPV6_PREFIX_LENGTH = 64
    /**
     * A ULA resolver address is fine even though a ULA prefix is not: the same tethering file admits
     * ULAs as DNS servers through a looser predicate. Android synthesizes the RDNSS it advertises
     * from the delegated prefix itself, so this exists to give proxied DNS an upstream target, not to
     * unlock delegation.
     */
    private const val VIRTUAL_DNS_IPV6 = "fd00::53"
    /**
     * Immutable in agent `LinkProperties` and in the daemon. Tethering clamps the downstream IPv6 MTU
     * it derives from its upstream to 1280-1500, so this is the top of what the platform will
     * propagate and it matches the physical downstream link.
     *
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/src/android/net/ip/IpServer.java#894
     */
    private const val TEST_NETWORK_MTU = 1500

    /**
     * `NetworkCapabilities.TRANSPORT_TEST`, blocked and therefore hardcoded. Checked at both ends of the
     * supported range rather than only the current one: 7 on Android 13 and 7 on Android 17, with
     * `MAX_TRANSPORT` equal to it on both, so a release that added a transport would have moved that instead.
     *
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/framework/src/android/net/NetworkCapabilities.java#1799
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/NetworkCapabilities.java#2278
     */
    private const val TRANSPORT_TEST = 7
    /**
     * `NetworkCapabilities.NET_CAPABILITY_NOT_VCN_MANAGED`, reached by value because the builder call is what
     * this app links against rather than the constant. 28 on Android 13 and 28 on Android 17.
     *
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/framework/src/android/net/NetworkCapabilities.java#360
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/NetworkCapabilities.java#434
     */
    private const val NET_CAPABILITY_NOT_VCN_MANAGED = 28
    /**
     * `ConnectivityManager.TYPE_TEST`, blocked and therefore hardcoded, and what AOSP's own test networks
     * report. 18 on Android 13 and 18 on Android 17; it is a deprecated legacy type, so nothing renumbers it.
     *
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/framework/src/android/net/ConnectivityManager.java#345
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/ConnectivityManager.java#369
     */
    private const val TYPE_TEST = 18
    /**
     * The deprecated `String` network-specifier overload only yields a `TestNetworkSpecifier` because
     * `TRANSPORT_TEST` is already set on the builder; otherwise it produces an
     * `EthernetNetworkSpecifier` that can never match this agent. Asserting the resulting type turns
     * that silent mismatch into an explicit failure instead of a publication timeout.
     *
     * The same branch on the transport is present at both ends of the supported range, so the ordering
     * requirement is not a property of one release.
     *
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/framework/src/android/net/NetworkRequest.java#419
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/NetworkRequest.java#559
     */
    private const val TEST_NETWORK_SPECIFIER_CLASS = "android.net.TestNetworkSpecifier"

    @get:SuppressLint("SoonBlockedPrivateApi")
    private val constructorLinkAddress by lazy {
        LinkAddress::class.java.getDeclaredConstructor(InetAddress::class.java, Int::class.javaPrimitiveType)
    }
    private val constructorRouteInfo by lazy {
        RouteInfo::class.java.getDeclaredConstructor(IpPrefix::class.java, InetAddress::class.java,
            String::class.java, Int::class.javaPrimitiveType)
    }

    /**
     * Tethering selected a TestNetwork this session does not own. Only one TestNetwork controller is
     * supported while this mode is in use, and cycling tethering cannot resolve a second one - whichever
     * network happens to come first in the upstream monitor's map wins - so this is terminal rather than
     * [State.RESTART_REQUIRED], whose remedy is to cycle.
     */
    class CollisionException(interfaceName: String?) : Exception(app.getString(
        R.string.shizuku_failure_collision, interfaceName ?: "?"))

    /**
     * Only [ACTIVE] would admit dataplane traffic. Committing one of these is the last startup step,
     * and the state is recomputed from the global upstream observation for as long as the session runs.
     */
    enum class State(@StringRes val label: Int) {
        /** Tethering names no upstream at all, so there is nothing to carry; resources are prepared. */
        ARMED(R.string.shizuku_state_armed),
        /** Tethering names an upstream this session cannot currently classify. */
        VERIFYING(R.string.shizuku_state_verifying),
        /** Tethering reports the exact owned TestNetwork. */
        ACTIVE(R.string.shizuku_state_active),
        /**
         * Tethering still reports an ordinary upstream. The preference only takes effect on the next
         * reselection, so this clears itself when Android reevaluates or the user independently cycles
         * the separate tethering toggle. This mode never cycles it.
         *
         * Its [label] therefore asks for a cycle and says a single one may not be enough, rather than
         * reading as an error: reselection races the upstream monitor's own map, so the same session and the
         * same build can reach [ACTIVE] on the first cycle or stay here across three.
         */
        RESTART_REQUIRED(R.string.shizuku_state_restart_required),
        /** Admission is closed and ordered cleanup is running. */
        STOPPING(R.string.shizuku_state_stopping),
    }

    /**
     * Where one session generation is in its life. Globally owned and published only by the owner: a
     * successor is admitted exactly when this reaches [Phase.OFF], which happens once - as the final
     * committed transition of a retirement - and never in the middle of one.
     */
    private enum class Phase {
        /**
         * Resources are being acquired and nothing is committed. A publication that fails leaves this phase
         * behind with its ledger intact, and [ShizukuLifecycle] runs the one retirement that withdraws it.
         */
        STARTING,
        /** Committed. The session owns a TUN, a request, an agent, a preference and a child. */
        RUNNING,
        /** Ordered withdrawal is running. Admission is closed and no successor may enter. */
        RETIRING,
        /**
         * Every local resource is gone - child fenced, agent withdrawn, network destroyed, descriptor
         * closed - but a privileged release could not be confirmed. The session itself is over; what is
         * retained is the exact request and preference a retry still owes, which is why a boolean would not
         * do. No successor session runs in this process until a retry confirms it or the process ends.
         */
        RESIDUAL,
    }

    private class Session(
        val generation: Long,
        val epoch: ShizukuEpoch,
        /** Owns tethering and egress observation for the session lifetime, and nothing else. */
        val scope: CoroutineScope,
    ) {
        var phase = Phase.STARTING
        /** Assigned the moment `createTunInterface` names it, which is also when [descriptor] is recorded. */
        var interfaceName = ""
        /**
         * A committed session has lost the machinery it needs, carrying what the user is told. Completed by
         * whichever observer noticed rather than acted on there, because those observers run inside the scope
         * a withdrawal cancels; the one watcher that selects on this owns the withdrawal.
         */
        val failed = CompletableDeferred<CharSequence>()
        /**
         * The retirement in flight, and what makes several stops one withdrawal: the first caller runs the
         * ordered steps and every other awaits this. Written before the first suspension point on the
         * privileged lane, so two observations that both decide the session is over produce one withdrawal.
         * Cleared again when a step could not be confirmed, so the next caller retries what is left instead
         * of returning as though it had finished.
         */
        var retirement: CompletableDeferred<Unit>? = null

        /**
         * Not a ledger entry: building it issues no wrapped transaction, so there is no unknown outcome to
         * remember and nothing to release. Dropped when the generation is given up.
         */
        var privileged: PrivilegedConnectivity? = null
        /**
         * Everything this session created and therefore owes back. Six owners rather than six nullable
         * fields, and six *different* owners rather than one generic one, because they are acquired by
         * different mechanisms and prove themselves with different evidence: a descriptor arrives by return
         * value and is live or was never received, the request and the agent are classified by a handle read
         * back from the framework, the connector is local, and the preference is decided by a result code.
         */
        val descriptor = DescriptorResource()
        val child = ChildResource()
        val request = ExactRequestResource()
        val connector = ConnectorResource()
        val preference = PreferenceResource()
        val agent = AgentResource()

        /** The one live [AppUidDaemon] this session's [child] is speaking, once the handshake completed. */
        var daemon: AppUidDaemon? = null
        /**
         * The cleanup-only epoch, authorized only after [epoch] is gone, purely to finish releasing what
         * this session already created. It republishes nothing, and it is revalidated on every retry rather
         * than trusted because it worked once.
         */
        var cleanup: ShizukuEpoch? = null
        /**
         * The three axes of every config this session publishes, the addresses it may never change, and the
         * fixed MTU its TUN and agent carry. The downstream epoch is advanced on any observation that can
         * break the correspondence between a TUN-visible tuple and a client, which for a global upstream is
         * every loss of positive confirmation that tethering is still carrying this exact network.
         */
        val publication = SessionPublication(
            listOf(VIRTUAL_DNS_IPV4, VIRTUAL_DNS_IPV6).map {
                InetAddress.getByName(it).address.toByteString()
            },
            // The interface's own addresses, which an ICMP error the daemon originates is sourced from: that
            // is the address a router in its position would speak from, and the client matches the error to
            // the socket that caused it by the quoted header rather than by this.
            listOf(TUN_IPV4_ADDRESS, TUN_IPV6_ADDRESS).map {
                InetAddress.getByName(it).address.toByteString()
            },
            TEST_NETWORK_MTU,
        )
        var network: Network? = null
        /** Latest global upstream observation. Confined to [privilegedDispatcher]. */
        var upstream: Network? = null
        /**
         * The egress this session's upstream sockets bind to, which is [Upstreams.appDefault]: the `Network`
         * Android has made this app's own default, so a VPN when one applies to this UID and the ordinary
         * per-UID default when none does. Root mode's `service.upstream`/`service.upstream.fallback`
         * preferences deliberately take no part in it - see that flow for why - so changing them cannot move
         * a rootless session's egress.
         *
         * Null means nothing is selectable, which is not a failure - upstream work fails per operation and
         * the session resumes on the next selection - and this mode never falls back to a second network.
         */
        var selected: Upstream? = null

        private val resources get() = arrayOf<SessionResource>(
            descriptor, child, request, connector, preference, agent)

        /** True while anything is still owed, which is exactly what forbids a successor in this process. */
        val outstanding get() = resources.any { it.outstanding }
        /**
         * True while a local resource - the child, the TUN, the agent and its native network - cannot be
         * fenced by anything this app can still do. Unlike privileged residue, which only forbids a
         * successor session, this is a child that may still be relaying: the withdrawal refuses to report
         * itself finished for it.
         */
        val localUnfenced get() = arrayOf<SessionResource>(descriptor, child, agent).any { it.terminal }

        override fun toString() = "session $generation: " + resources.joinToString()
    }

    private var generation = 0L
    /**
     * The one session generation this process owns, whatever [Phase] it is in, and the lock nothing else
     * plays: a successor is admitted exactly when this becomes null, which happens once, as the final
     * committed transition of a retirement. Confined to [privilegedDispatcher], and every mutating entry
     * point below runs inside [lifecycle]'s command lane, which is what keeps two of them apart across their
     * suspensions - a single-lane dispatcher orders dispatches, not run-to-completion sections.
     */
    private var current: Session? = null
    private val stateFlow = MutableStateFlow<State?>(null)
    /** Null when no session is running. */
    val state = stateFlow.asStateFlow()
    /**
     * Outlives every session, because a session's own scope is what retirement cancels. A watcher that
     * asked for the withdrawal would otherwise be cancelled by the withdrawal it asked for.
     */
    private val owned = CoroutineScope(privilegedDispatcher + SupervisorJob())
    /**
     * The tethering service died under a session, so `TetheringManager`'s permanently cached connector in
     * this process is dead and AOSP states that no recovery is possible. Nothing this app can do brings it
     * back, so a further session is refused with that in so many words rather than left to fail obscurely
     * at connector acquisition.
     *
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/common/TetheringLib/src/android/net/TetheringManager.java#467
     */
    private var tetheringDied = false

    /**
     * This mode's own command lane, and the only way in: [ShizukuLifecycle] is what makes a duplicate press
     * share one start, an explicit stop supersede a start that has created nothing yet, and a failed
     * publication retire what it managed to create. It knows about this session and nothing else - root mode
     * is neither consulted nor touched by any transition it drives.
     */
    val lifecycle = ShizukuLifecycle(object : ShizukuLifecycle.Session {
        override suspend fun prepare() = this@ShizukuTestNetwork.prepare()
        override suspend fun retire() = stop()
    })

    /**
     * Everything that has to hold before anything is created, and nothing that mutates anything. Runs
     * cancellable, because Shizuku authorization can sit on the user's own permission dialog for as long as
     * they take. Returns the publication step.
     */
    @SuppressLint("WrongConstant")
    private suspend fun prepare(): suspend () -> Unit {
        check(Build.VERSION.SDK_INT >= 33) { "Shizuku mode requires Android 13" }
        val epoch = ShizukuEpoch.authorize()
        // before anything is created, because a device that never consults the preference cannot run this
        // mode at all and a permanent RESTART_REQUIRED is the worst available outcome
        PinnedTetheringConnector.requireAutomaticUpstream()
        withContext(privilegedDispatcher) {
            check(!tetheringDied) { app.getString(R.string.shizuku_failure_tethering_died) }
            current?.let { previous ->
                // Everything else is the command lane's business: a live or retiring session means it is not
                // OFF, and it would not have called this.
                check(previous.phase == Phase.RESIDUAL) { "A Shizuku session is already running" }
                // Retried here rather than in a path of its own: a fresh epoch has just been authorized,
                // which is the one thing the outstanding release was missing.
                try {
                    releasePrivileged(previous)
                } catch (e: Exception) {
                    if (e is CancellationException && e !is TimeoutCancellationException) throw e
                    throw IllegalStateException(app.getString(R.string.shizuku_failure_cleanup_unconfirmed), e)
                }
                finish(previous)
                Timber.i("Finished the outstanding cleanup of Shizuku session ${previous.generation}")
            }
            // A newly started app process has no old in-memory generation, so a TestNetwork that outlived one
            // is only detectable by asking the platform. Ordinary reads are enough and no privilege is
            // needed: `getAllNetworks` enforces ACCESS_NETWORK_STATE alone and returns every tracked network
            // on Android 13 through 17, and the capability sanitizer never touches transports. Done here, so
            // this session's own agent cannot match itself.
            @Suppress("DEPRECATION")
            for (network in Services.connectivity.allNetworks) {
                if (Services.connectivity.getNetworkCapabilities(network)
                        ?.hasTransport(TRANSPORT_TEST) != true) continue
                throw CollisionException(Services.connectivity.getLinkProperties(network)?.interfaceName)
            }
        }
        return { withContext(privilegedDispatcher) { publish(epoch) } }
    }

    /**
     * Publishes one session, recording every step it completes so that whatever it leaves behind is exactly
     * what [ShizukuLifecycle.Session.retire] withdraws.
     *
     * It deliberately does **not** roll itself back. Failing here throws with [current] still naming the
     * session and its ledger still owning every resource that exists, and [ShizukuLifecycle] runs the one
     * retirement - so a rollback that fails is retried by the next explicit stop rather than immediately by
     * the start that caused it. Nothing before the ledger exists can leave anything behind: building the
     * private manager and fetching the TestNetwork service binder issue no transaction that owes a release.
     *
     * Runs inside [ShizukuLifecycle.State.PUBLISHING], so no other start or stop can interleave with it.
     * Nothing of root mode's is consulted, withdrawn or rebuilt to get here: whatever root routing exists
     * stays exactly as it is, and Android's tethering decides for itself whether to select what this
     * publishes.
     */
    @SuppressLint("WrongConstant")
    private suspend fun publish(epoch: ShizukuEpoch) {
        // Resolved before anything is created, because the direct release is what a retirement needs and
        // discovering that it is unreachable *after* the TUN, the request, the preference and the agent
        // exist would leave a session that cannot be taken back. No transaction, no side effect.
        UnblockCentral.IConnectivityManager_releaseNetworkRequest
        UnblockCentral.NetworkCallback_networkRequest
        // Same reason, for the fence: the launched child's pid is what SIGKILL names, and one that never
        // authenticates leaves no peer credentials to take it from.
        UnblockCentral.UNIXProcess_pid
        val privileged = PrivilegedConnectivity.create(epoch)
        val service = UnblockCentral.ITestNetworkManager_asInterface(null, epoch.wrap(
            checkNotNull(epoch.bracket { privileged.service.startOrGetTestNetworkService() }) {
                "No TestNetwork service binder"
            }))
        val testNetworkManager = UnblockCentral.TestNetworkManager_constructor.newInstance(service)
                as TestNetworkManager
        // Published as the owner of the generation before anything can be created under it, so there is
        // never a completed step that no ledger names.
        val current = Session(++generation, epoch, CoroutineScope(privilegedDispatcher + SupervisorJob()))
        current.privileged = privileged
        this.current = current
        // Recorded live inside the bracket, before its closing epoch check. `createTunInterface` hands
        // the descriptor back by return value, so either this process received it - in which case it
        // exists and is owned from here, whatever the epoch says next - or the reply was lost, in which
        // case there is no app-held descriptor at all and the transaction's own transfer cleanup closed
        // what was sent. A TUN the epoch invalidated is therefore owned by this session's ledger rather
        // than handed to a disposer with nowhere to report its failure.
        val descriptor = epoch.bracket {
            testNetworkManager.createTunInterface(arrayOf(
                constructorLinkAddress.newInstance(
                    InetAddress.getByName(TUN_IPV4_ADDRESS), TUN_IPV4_PREFIX_LENGTH),
                constructorLinkAddress.newInstance(
                    InetAddress.getByName(TUN_IPV6_ADDRESS), TUN_IPV6_PREFIX_LENGTH),
            )).let { tun ->
                current.interfaceName = tun.interfaceName
                current.descriptor.live(tun.fileDescriptor)
            }
        }
        check(current.interfaceName.isNotEmpty()) { "The created TUN has no interface name" }
        // Observation is registered before the preference and the agent, which are the only
        // mutations that can move tethering's upstream, so the snapshot below still predates
        // everything that matters. It is not atomic and does not need to be: the callback is
        // level-triggered and the state below is computed from the latest value of it.
        //
        // The upstream is the whole observation. Which downstreams tethering happens to be serving
        // behind it is deliberately not read: this mode publishes one global upstream and owns no
        // downstream, so a membership change is not its business and its own MTU contract is fixed.
        val snapshot = CompletableDeferred<Unit>()
        current.scope.launch {
            TetheringManagerCompat.eventFlow.collect { event ->
                // Level-triggered and repeating its current value, so an observation that changed
                // nothing is dropped here rather than costing an epoch and a config round trip.
                if (event !is TetheringManagerCompat.Event.UpstreamChanged) return@collect
                snapshot.complete(Unit)
                if (event.network == current.upstream) return@collect
                current.upstream = event.network
                if (current.network == null) return@collect
                val next = try {
                    current.commit()
                } catch (e: CollisionException) {
                    // Terminal, and reported through the one watcher that owns the withdrawal rather
                    // than withdrawn from here: this collector is inside the scope a withdrawal
                    // cancels.
                    Timber.w(e)
                    current.failed.complete(e.readableMessage)
                    return@collect
                }
                // Anything other than arriving at ACTIVE breaks the correspondence between a
                // TUN-visible tuple and a client: tethering may have rebuilt its NAT behind an
                // unchanged Network handle, so continuity has to be established rather than assumed
                // from a short absence.
                if (next != State.ACTIVE) {
                    current.publication.advanceDownstream()
                } else if (stateFlow.value == next) return@collect
                stateFlow.value = next
                Timber.i("Shizuku session ${current.generation} is $next, upstream " +
                        "${current.upstream}, epoch ${current.publication.downstreamEpoch}")
                current.push()
            }
        }
        withTimeout(CONTROL_RESULT_DEADLINE) { snapshot.await() }
        // Not awaited, unlike the tethering snapshot: no selectable network is a legitimate steady
        // state rather than a startup value still to arrive, so there is nothing to wait for and
        // waiting would refuse to start a session that works as soon as a VPN connects.
        current.scope.launch {
            Upstreams.appDefault.collect { upstream ->
                // Rejected by interface name rather than by `Network` identity, because this also has
                // to hold before the agent publishes, when there is no session `Network` to compare
                // against. Selecting the TUN would make the daemon relay to itself. It cannot happen
                // through this source - a network with no INTERNET capability is never anyone's
                // default - so this stands as the assertion that keeps it that way.
                val next = upstream?.takeUnless {
                    it.properties.interfaceName == current.interfaceName
                }
                if (next == null && upstream != null) {
                    Timber.w("Refusing this session's own TUN as its upstream")
                }
                if (next == current.selected) return@collect
                current.selected = next
                Timber.i("Shizuku session ${current.generation} egress is " +
                        "${next?.properties?.interfaceName}, generation " +
                        current.publication.advanceUpstream())
                // Recorded but not sent until the session is committed, exactly as the tethering
                // observation above is: the config is level-triggered, so publication sends whatever
                // the latest observation was, and a push before then could end a session this
                // function is still building.
                if (current.network != null) current.push()
            }
        }
        // The plan launches the daemon before the TUN exists and transfers the descriptor
        // afterwards; this slice couples the two, because splitting them would mean holding a
        // half-finished handshake for no gain until there is a dataplane to configure.
        //
        // Ownership begins at `spawn`, not at a completed handshake: the config frame carries the TUN
        // descriptor, so a child that fails authentication may already hold one, and the ledger below is
        // what fences it either way.
        val child = current.child.live(AppUidDaemon.spawn())
        val daemon = AppUidDaemon.connect(child, descriptor, current.interfaceName, TEST_NETWORK_MTU,
            current.publication, current.selected)
        current.daemon = daemon
        // the deprecated overload is the point: it is what produces a TestNetworkSpecifier
        // without the blocked TestNetworkSpecifier constructor
        @Suppress("DEPRECATION")
        val request = NetworkRequest.Builder()
            .clearCapabilities()
            .addTransportType(TRANSPORT_TEST)
            .setNetworkSpecifier(current.interfaceName)
            .build()
        val specifier = checkNotNull(request.networkSpecifier) { "Request lost its specifier" }
        check(specifier.javaClass.name == TEST_NETWORK_SPECIFIER_CLASS) {
            "Expected $TEST_NETWORK_SPECIFIER_CLASS but built ${specifier.javaClass.name}"
        }
        val capabilities = `NetworkCapabilities$Builder`()
            .addTransportType(TRANSPORT_TEST)
            // a fresh builder starts from NOT_RESTRICTED, TRUSTED and NOT_VPN; AOSP's own test
            // networks carry neither of the first two, and NOT_RESTRICTED is what would make
            // this network usable by ordinary apps
            .removeCapability(NetworkCapabilities.NET_CAPABILITY_NOT_RESTRICTED)
            .removeCapability(NetworkCapabilities.NET_CAPABILITY_TRUSTED)
            .addCapability(NetworkCapabilities.NET_CAPABILITY_NOT_SUSPENDED)
            .addCapability(NET_CAPABILITY_NOT_VCN_MANAGED)
            .addCapability(NetworkCapabilities.NET_CAPABILITY_NOT_METERED)
            .setNetworkSpecifier(specifier)
            .also {
                try {
                    it.addCapability(NetworkCapabilities.NET_CAPABILITY_NOT_BANDWIDTH_CONSTRAINED)
                } catch (e: IllegalArgumentException) {
                    // only present where the installed Connectivity module exposes it
                    if (Build.VERSION.SDK_INT >= 37) Timber.w(e)
                }
            }
            .build()
        val properties = LinkProperties().apply {
            interfaceName = current.interfaceName
            // Re-read what the kernel actually assigned to *this session's own* TUN rather than trusting
            // the request, which is also how the kernel-assigned IPv6 link-local reaches the published
            // properties. Reading `testtunN` is reading a resource this session created and named; it is
            // not downstream knowledge, and no interface Android happens to be tethering is looked at.
            val addresses = checkNotNull(NetworkInterface.getByName(current.interfaceName)) {
                "No such interface ${current.interfaceName}"
            }.interfaceAddresses.map {
                constructorLinkAddress.newInstance(it.address, it.networkPrefixLength.toInt())
            }
            setLinkAddresses(addresses)
            // ConnectivityService adds directly connected routes during registration, so
            // precompute them and require them back
            for (address in addresses) addRoute(constructorRouteInfo.newInstance(
                IpPrefix(address.address, address.prefixLength), null, current.interfaceName,
                RouteInfo.RTN_UNICAST))
            // tethering requires an IPv6 default route alongside the global /64 before it
            // delegates a prefix
            for (any in arrayOf("0.0.0.0", "::")) addRoute(constructorRouteInfo.newInstance(
                IpPrefix(InetAddress.getByName(any), 0), null, current.interfaceName,
                RouteInfo.RTN_UNICAST))
            setDnsServers(listOf(InetAddress.getByName(VIRTUAL_DNS_IPV4),
                InetAddress.getByName(VIRTUAL_DNS_IPV6)))
            mtu = TEST_NETWORK_MTU
        }
        val exact = ExactRequest()
        // The precheck is synchronous and comes *before* the ledger records anything, which is the whole
        // point of splitting them: a stale epoch here proves no Binder call was issued at all, so the
        // session fails owing nothing. Recording first and letting the check throw inside the attempt
        // would classify that as an unknown registration and poison the process for no reason.
        epoch.ensureCurrent()
        // The field has to start empty, or the readback below could not tell what this call produced
        // from what some earlier registration left behind. Reflective failure here is a refusal to start
        // rather than something to discover after the request exists.
        check(exact.readBack() == null) { "$exact was already registered" }
        // Recorded before the transaction, because the callback is the app's half of the request and a
        // request nobody holds is a request nobody can release. `ConnectivityManager` only assigns the
        // service-returned `NetworkRequest` to it after the reply, so the builder's own request object
        // above is not the exact handle and cannot stand in for it.
        current.request.issuing(exact)
        val registration = try {
            privileged.manager.requestNetwork(request, exact.callback, Services.mainHandler)
            null
        } catch (e: Throwable) {
            e
        }
        // Read back on both paths, because an exception is not proof that the service created nothing.
        // A real handle means the request exists and its release is owed even if the closing check below
        // fails; no handle means no answer at all, which is unknown and terminal for this process.
        val handle = try {
            exact.readBack()
        } catch (e: ReflectiveOperationException) {
            // Fail closed rather than downgrade: registration may well have happened, and what is lost
            // is only the ability to name what it made.
            registration?.addSuppressed(e) ?: Timber.w(e, "Cannot read back the exact NetworkRequest")
            null
        }
        current.request.settle(handle, registration)
        registration?.let { throw it }
        check(current.request.state == ResourceState.LIVE) {
            "Registration gave no exact NetworkRequest back for $exact"
        }
        epoch.ensureCurrent()
        val callback = exact.callback
        // acquired before the preference mutation, because connector death is what silently
        // undoes it, and a session never leaves a death recipient linked behind
        val connector = current.connector.live(PinnedTetheringConnector.acquire(epoch))
        // Recorded as owed from the moment the transaction is issued, not from a successful result; the
        // connector classifies the answer, because only the service's own result code can separate "did
        // not act" from "may have acted".
        connector.setPreferTestNetworks(true, current.preference)
        val agent = RestrictedAgent(privileged.context, capabilities, properties, TYPE_TEST)
        // Same split as the request, for the same reason: constructing an agent registers nothing, so a
        // stale epoch caught here proves no registration was issued and leaves nothing owed.
        epoch.ensureCurrent()
        check(agent.published == null) { "$agent was already registered" }
        current.agent.issuing(agent)
        val registering = try {
            agent.register()
            null
        } catch (e: Throwable) {
            e
        }
        // Same classification as the request, and for the same reason: `register()` assigns the agent's
        // `Network` before returning, so reading it back covers the throwing path too, and its absence
        // is the absence of an answer rather than proof that no remote agent exists.
        current.agent.settle(agent.published, registering)
        registering?.let { throw it }
        val network = checkNotNull(agent.published) { "Agent registration returned no network" }
        epoch.ensureCurrent()
        agent.markConnected()
        val (published, readback) = withTimeout(CONTROL_RESULT_DEADLINE) {
            agent.created.await()
            check(callback.available.await() == network) {
                "Request matched a different network than the one published"
            }
            callback.capabilities.await() to callback.properties.await()
        }
        // an asynchronous result is only trustworthy if the epoch still holds when it arrives
        epoch.ensureCurrent()
        check(published.hasTransport(TRANSPORT_TEST)) { "Published network lost TRANSPORT_TEST" }
        check(published.networkSpecifier == specifier) { "Published network lost its specifier" }
        check(!published.hasCapability(NetworkCapabilities.NET_CAPABILITY_NOT_RESTRICTED)) {
            "Published network is not restricted"
        }
        check(!published.hasCapability(NetworkCapabilities.NET_CAPABILITY_INTERNET)) {
            "Published network claims INTERNET"
        }
        check(readback.interfaceName == current.interfaceName) { "Published wrong interface" }
        check(readback.mtu == TEST_NETWORK_MTU) { "Published MTU ${readback.mtu}" }
        check(readback.linkAddresses.containsAll(properties.linkAddresses)) { "Lost link addresses" }
        check(readback.routes.containsAll(properties.routes)) { "Lost routes" }
        check(readback.dnsServers.containsAll(properties.dnsServers)) { "Lost DNS servers" }
        // Both latch, so this covers a loss anywhere between registration and here rather than only
        // one arriving now. Losing either before commit is an ordinary startup failure; the watcher
        // below takes over afterwards.
        check(!callback.lost.isCompleted) { "The request lost its network during startup" }
        check(!agent.destroyed.isCompleted) { "The test network was destroyed during startup" }
        // The read [commit] classifies a non-owned upstream with, aimed at the network this session
        // just published, which is the same case: restricted, inaccessible to the app UID, and
        // carrying no owner or administrator UID, so the sanitizer treats it no differently than
        // somebody else's. Failing it means a release began redacting transports, which costs only
        // the collision distinction - a compatibility finding rather than a startup failure.
        val classifiable = Services.connectivity.getNetworkCapabilities(network)
        if (classifiable?.hasTransport(TRANSPORT_TEST) != true) {
            Timber.w(Exception("Cannot classify $network as a test network unprivileged: $classifiable"))
        }
        current.network = network
        current.phase = Phase.RUNNING
        stateFlow.value = current.commit()
        current.push()
        // The push above is the first one to reach the daemon, so it is also the first thing that
        // can end this session. Turning it into a startup failure keeps one retirement in charge: the
        // watcher below is installed only after this line, so nothing else can be withdrawing yet.
        check(!current.failed.isCompleted) { "The daemon stopped answering during startup" }
        Timber.i("Published restricted test network $network on ${current.interfaceName} as " +
                "${stateFlow.value}, upstream ${current.upstream}: $published $readback")
        // Only a committed session is watched for these. Until commit, the retirement [ShizukuLifecycle]
        // runs on a failed publication is the only withdrawal there can be: a second one running
        // concurrently with it would release resources the first has not finished recording, and leak
        // whatever it had not reached. Nothing is lost by waiting, because all five are awaited rather
        // than polled.
        //
        // A committed session ends only when its own machinery is gone, and each of these is that:
        // the network this session exists to run has been removed, or the app can no longer tell
        // what its child is bound to. One watcher rather than five, because they share one
        // withdrawal and the first of them to fire is the one that owns it. It lives in the
        // session's own scope so an ordered stop cancels it before touching anything, and it hands
        // the withdrawal itself to [owned], which the withdrawal cannot cancel.
        current.scope.launch {
            val failure = select {
                connector.died.onAwait {
                    // The network stack has already reset the preference and reselected an ordinary
                    // upstream. That is positive proof rather than an assumption - the flag lives in the
                    // process that just died - so the clear this session owed is discharged here instead
                    // of being retried against a service that has forgotten it. TetheringManager caches
                    // its connector permanently, so nothing in this process can obtain a working one
                    // again.
                    tetheringDied = true
                    current.preference.lostWithService()
                    app.getText(R.string.shizuku_failure_tethering_died)
                }
                daemon.ended.onAwait { app.getText(R.string.shizuku_failure_daemon) }
                // Agent or request loss keeps [State.ACTIVE] honest as well as ending the session:
                // [commit] compares netIds, and a netId can only be reissued to an unrelated
                // network once this one is destroyed, which is exactly what these observe.
                agent.destroyed.onAwait { app.getText(R.string.shizuku_failure_network) }
                callback.lost.onAwait { app.getText(R.string.shizuku_failure_network) }
                current.failed.onAwait { it }
            }
            Timber.w("Shizuku session ${current.generation} ended: $failure")
            SmartSnackbar.make(failure).show()
            owned.launch {
                // Through the command lane rather than straight into [retire], so a user-initiated stop
                // arriving now becomes the same withdrawal instead of a second one, and so the row stops
                // reporting the mode as on once it finishes. Nothing outside this mode is asked for
                // anything: root routing, if any, is untouched by this session ending.
                try {
                    lifecycle.stop()
                } catch (e: Exception) {
                    if (e is CancellationException && e !is TimeoutCancellationException) throw e
                    // Nothing above this would report it: it is the last thing running for a session
                    // that has already lost the machinery it needed.
                    Timber.w(e)
                    SmartSnackbar.make(e).show()
                }
            }
        }
    }

    /**
     * Retires the running session, if any, and returns only once every local resource is gone: the child has
     * exited, the network's destruction is proven, the observers are joined and the descriptor is closed.
     * A privileged release may still be owed after that - that residue forbids another session in this
     * process without keeping the TUN open for it.
     */
    private suspend fun stop(): Unit = withContext(privilegedDispatcher) {
        current?.let { if (it.phase != Phase.RESIDUAL) retire(it) }
    }

    // The global preference is the one piece of system state a session can leave behind. It has no owner
    // token, so app death, force-stop or uninstall can strand it at true with nothing left running to clear
    // it, and it is *not* recovered through an action on an existing screen: the row the user already has is
    // the recovery path. Two cases, and they differ. A session left RESIDUAL inside *this* process is
    // retried by [prepare], which is why a start settles what a previous withdrawal still owed. After full
    // process death there is no ledger to retry, so a fresh start does not clear the flag in preparation -
    // what clears it then is that session's own [retire], or a reboot.
    //
    // Stranding it is not immediately harmful: with no TEST network present Android's upstream selection
    // falls through to the ordinary default, so the hotspot keeps working. What the flag does mean is that
    // the *next* test network to appear is preferred - this app's own later session, or another app's - and
    // that is the residue worth naming. Another TestNetwork controller is unsupported, so no previous value
    // is restored: the preference is cleared, never negotiated.

    /**
     * Ordered withdrawal, shared by a failed publication, the user's own stop and the failure watcher.
     * Idempotent and resumable: the first caller runs the steps and the rest await
     * [Session.retirement], while every step confirms its own ledger entry, so a retry does what is left
     * rather than what is done.
     *
     * Local resources are always released, even when the Shizuku epoch is gone, because fencing the child,
     * withdrawing the agent and closing the descriptor need no Shizuku transaction at all. Only the
     * preference clear and the request release do, and those are the ones that may be left outstanding.
     *
     * [NonCancellable] because the session's own observers reach this: every step below awaits something,
     * and without it the preference clear, the agent's destruction callback and the request release would
     * all abandon immediately, leaving exactly the system state this is here to remove.
     */
    private suspend fun retire(session: Session): Unit = withContext(NonCancellable) {
        // Read and written before the first suspension point on the single privileged lane, so two
        // observations that both decide the session is over produce one withdrawal rather than two.
        session.retirement?.let { return@withContext it.await() }
        val retirement = CompletableDeferred<Unit>()
        session.retirement = retirement
        session.phase = Phase.RETIRING
        stateFlow.value = State.STOPPING
        try {
            // Joined, not merely cancelled: cancellation is a request and this needs a completion. Until it
            // returns, an observer could still be inside a config round trip or an epoch advance.
            session.scope.coroutineContext.job.cancelAndJoin()
            // Step 1 of the ordered stop, and the daemon's half of it: closing admission in the app alone
            // would leave Rust admitting new flows, mappings, queries and fragments for the whole of the
            // potentially 60-second preference clear below. It throws when the child could not acknowledge
            // that it stopped admitting, and then the child fence runs first rather than after that wait.
            val daemon = session.daemon
            val admissionClosed = if (daemon == null) {
                // No control connection means nothing can be *told* to stop admitting, and that is only
                // harmless when there is no child to admit anything: a child spawned whose handshake never
                // completed already holds a duplicate of the TUN, because the descriptor travels with the
                // config frame. Treating a null daemon as "nothing left to do" is what would let such a
                // child relay through the whole preference clear below.
                !session.child.outstanding
            } else try {
                daemon.closeAdmission(session.config())
                true
            } catch (e: Exception) {
                if (e is CancellationException && e !is TimeoutCancellationException) throw e
                Timber.w(e, "Shizuku session ${session.generation} could not close admission")
                false
            }
            // Immediately, and before the clear that can take a minute, whenever admission is not provably
            // shut. The fence is idempotent, so the ordinary one below simply finds nothing left.
            if (!admissionClosed) fenceChild(session)
            // Cleared before the agent is withdrawn: this is the one piece of system state that outlives
            // the session, so it is dropped while there is still a live connector to drop it through.
            session.connector.value?.let { connector ->
                try {
                    if (session.preference.clearable) connector.setPreferTestNetworks(false, session.preference)
                } catch (e: Exception) {
                    if (e is CancellationException && e !is TimeoutCancellationException) throw e
                    // The ledger already knows which of the two this was: a denied clear proves the flag is
                    // untouched and is still owed, while a deadline or a replaced epoch leaves it unknown.
                    // Either way the withdrawal continues and [releasePrivileged] retries what is left.
                    Timber.w(e, "Shizuku session ${session.generation} could not clear the preference")
                } finally {
                    session.connector.unlinking()?.unlink()
                    session.connector.confirm()
                }
            }
            fenceChild(session)
            withdrawAgent(session)
            // Everything local is gone from here on, so the session is over whether or not the two
            // privileged releases below succeed. Closed only now, and never at the top: a successor that
            // inherited this descriptor would be relaying through a TUN ConnectivityService still exposes.
            session.descriptor.closing()?.let {
                it.close()
                session.descriptor.confirm()
            }
            // This withdrawal reports itself finished only once every *local* resource is fenced. An agent
            // or a request whose outcome is unknown leaves a native network this process may still own and
            // cannot name, so there is nothing to unregister it by and no retry that would learn more: the
            // only remaining release is process death, and the row must not read as off beside it.
            check(!session.localUnfenced) {
                app.getString(R.string.shizuku_failure_unfenced, session.toString())
            }
            try {
                releasePrivileged(session)
                finish(session)
            } catch (e: Exception) {
                if (e is CancellationException && e !is TimeoutCancellationException) throw e
                // Explicit and fail closed: the request or the preference may still be held by a retired
                // generation, so no successor session runs in this process until a retry confirms it or the
                // process ends. The withdrawal itself still finishes, because nothing local is left.
                session.phase = Phase.RESIDUAL
                Timber.w(e, "Shizuku session ${session.generation} could not confirm its cleanup")
                SmartSnackbar.make(e).show()
            }
            stateFlow.value = null
            Timber.i("Retired Shizuku session ${session.generation}")
            retirement.complete(Unit)
        } catch (e: Throwable) {
            // Nothing local is confirmed, so the next caller retries the steps that are left rather than
            // returning as though this one had finished. The session stays in [Phase.RETIRING] and the mode
            // keeps reading as on: its child may still be relaying.
            session.retirement = null
            retirement.completeExceptionally(e)
            throw e
        }
    }

    /**
     * The child exit fence, which everything after it assumes has run: a child that survived still holds the
     * TUN and its upstream sockets, so it would keep relaying clients onto a network this session no longer
     * owns. Failing here therefore stops the withdrawal with the whole session retained, and the next stop
     * retries it.
     *
     * Reissuable rather than once-only: closing sockets and waiting for observed exit is idempotent, and a
     * fence that failed is exactly what a retry is for. Runs before the agent is withdrawn, so nothing is
     * still reading the TUN when the network it belongs to disappears - and out of order, immediately after a
     * failed admission close, so a child that is still admitting never runs through the preference clear's
     * deadline.
     */
    private suspend fun fenceChild(session: Session) {
        session.child.fencing()?.let { child ->
            child.stop()
            session.child.confirm()
            session.daemon = null
        }
    }

    /**
     * Withdraws the agent exactly once and proves the network is gone before anything releases the request.
     *
     * The order is the platform's, not a preference. ConnectivityService emits `CALLBACK_LOST` when the
     * agent disconnects, before it removes and rematches the network and destroys its native state, so
     * unregistering the request's callback first would make framework dispatch drop the queued loss and
     * leave this app unable to prove anything. Releasing the request before its own `onLost` would race it.
     *
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/service/src/com/android/server/ConnectivityService.java#4285
     */
    private suspend fun withdrawAgent(session: Session) {
        // Not bracketed in the pinned epoch: unregister reaches ConnectivityService through the
        // INetworkAgentRegistry that registration returned, which is this process's own Binder handle, so
        // it works after Shizuku death - and refusing it there would strand the very network this
        // withdrawal exists to remove.
        // https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/NetworkAgent.java#1135
        session.agent.unregistering()?.unregister()
        // Retained through the barrier rather than dropped after the call: a retry must not unregister
        // twice, and it must still be able to await what this attempt could not prove.
        val agent = session.agent.awaiting ?: return
        val callback = session.request.value?.callback
        // Every barrier that was actually acquired, and one that always is. `onNetworkUnwanted` is delivered
        // from `NetworkAgentInfo.disconnect()` whether or not a native network was created, so it - not the
        // absence of a callback - is what proves a *known* agent is gone. `onNetworkDestroyed` is owed only
        // once `onNetworkCreated` arrived, and `onLost` only once the request reached `onAvailable`.
        val proven = withTimeoutOrNull(CONTROL_RESULT_DEADLINE) {
            agent.unwanted.await()
            if (agent.created.isCompleted) agent.destroyed.await()
            if (callback?.available?.isCompleted == true) callback.lost.await()
            Unit
        } != null
        // Keep the agent and retry rather than declaring the network gone and releasing a request that still
        // names it. Deliberately unconditional: a known agent always owes [RestrictedAgent.unwanted], so
        // there is no "nothing was owed" case left to excuse - the old one existed only because destruction
        // was the barrier, and callback absence is never proof that no remote agent exists.
        check(proven) {
            "Shizuku session ${session.generation} could not prove its agent was withdrawn: $agent"
        }
        session.agent.confirm()
    }

    /**
     * The two releases that need Shizuku, and the one epoch question a withdrawal has to answer. The pinned
     * epoch is used while it holds; once it is gone a cleanup-only epoch with the same effective UID is
     * authorized instead, and it republishes nothing - a fresh connector for the preference and a fresh
     * private manager for the release, and no agent, TUN or child.
     *
     * Effective-UID equality is what makes that legal rather than Binder continuity: ConnectivityService
     * authorizes a release against the UID stored with the request, asynchronously, so a call under the
     * wrong UID would no-op while `unregisterNetworkCallback` still cleared this process's own bookkeeping -
     * a false confirmation. Both releases are therefore bracketed as one epoch operation each, and nothing
     * is confirmed on a closing check that failed.
     *
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/service/src/com/android/server/ConnectivityService.java#6732
     */
    private suspend fun releasePrivileged(session: Session) {
        // The local half needs no Shizuku at all, so it is finished first when it is all that is left.
        cleanLocalCallback(session)
        if (!session.preference.clearable && !session.request.outstanding) return
        check(!session.request.terminal) {
            "Shizuku session ${session.generation} cannot release ${session.request}"
        }
        val epoch = session.cleanupEpoch()
        if (session.preference.clearable) {
            val connector = PinnedTetheringConnector.acquire(epoch)
            try {
                connector.setPreferTestNetworks(false, session.preference)
            } finally {
                connector.unlink()
            }
        }
        // The direct service call on the retained handle, never `unregisterNetworkCallback`: the release is
        // authorized against the UID stored with the request, so it has to go out under an epoch whose
        // effective UID matches the issuing one, and the app-facing wrapper would destroy the only handle a
        // retry has while releasing nothing. Reissued until a bracket confirms it, because a closing check
        // that failed may mean the call was authorized against the wrong UID; duplicates are harmless, since
        // ConnectivityService treats a request it no longer knows as successful cleanup.
        //
        // The bracket proves correct-UID acceptance and ordering, not that the service's handler has already
        // run. That is the strongest contract this API offers - it is asynchronous by design - and it is
        // enough, because the ordering it does prove is what makes the redundant release below a no-op.
        session.request.releasing()?.let { handle ->
            val privileged = if (epoch === session.epoch) {
                checkNotNull(session.privileged) { "The session lost its privileged manager" }
            } else PrivilegedConnectivity.create(epoch)
            epoch.bracket { privileged.service.releaseNetworkRequest(handle) }
            session.request.released()
        }
        cleanLocalCallback(session)
    }

    /**
     * `ConnectivityManager`'s own bookkeeping, cleaned only once the privileged release is confirmed.
     *
     * This issues a second release under the app UID, which ConnectivityService ignores: the correct release
     * was already accepted and enqueued ahead of it, and an event naming a request the service no longer
     * knows - or one owned by another UID - is a no-op. What it does do is remove the process-static
     * `sCallbacks` entry and tombstone the callback, which is why it is preferred to reflecting into that map
     * or to synthesizing the framework's already-unregistered sentinel.
     *
     * Failing here leaves the remote release confirmed and only this owed, so it is retried on its own rather
     * than by reissuing a release that is already proven.
     *
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/ConnectivityManager.java#5578
     */
    private fun cleanLocalCallback(session: Session) {
        val callback = session.request.localCleanup ?: return
        Services.connectivity.unregisterNetworkCallback(callback)
        session.request.localCleaned()
    }

    /**
     * The epoch to finish cleanup under: the session's own while it holds, otherwise a cleanup-only one.
     *
     * Revalidated on every retry rather than trusted because it worked once. A cached epoch can die too,
     * and a dead one would release nothing while looking like a confirmation.
     */
    private suspend fun Session.cleanupEpoch(): ShizukuEpoch {
        for (candidate in arrayOf(epoch, cleanup)) if (candidate != null) try {
            candidate.ensureCurrent()
            return candidate
        } catch (e: ShizukuEpoch.ChangedException) {
            Timber.w(e, "Reauthorizing Shizuku to finish the cleanup of session $generation")
            if (candidate === cleanup) cleanup = null
        }
        return ShizukuEpoch.authorize().also { fresh ->
            check(fresh.uid == epoch.uid) {
                "Cleanup Shizuku identity is uid ${fresh.uid}, the session ran as ${epoch.uid}"
            }
            cleanup = fresh
        }
    }

    /**
     * The one place the generation is given up, and the only transition that admits a successor. Refuses
     * while anything is still owed, because that is exactly what forbids one.
     */
    private fun finish(session: Session) {
        check(!session.outstanding) { "Shizuku session ${session.generation} still owes $session" }
        session.privileged = null
        if (current === session) current = null
    }

    /**
     * Sends the current session config. Level-triggered, so this is safe to call on any observation: the
     * daemon only needs the newest one, and [AppUidDaemon.apply] coalesces the rest away.
     *
     * The MTU the daemon sizes relayed IPv4 output against is [TEST_NETWORK_MTU], which is the TestNetwork's
     * own contract rather than a measurement: it is what the TUN was created with, what the agent publishes,
     * and what tethering clamps the downstream MTU it derives from this upstream to. This mode owns no
     * downstream and never asks which interfaces Android is serving behind it, so there is nothing narrower
     * to measure and nothing that could move it within a session.
     */
    private fun Session.config(): ShizukuSessionConfig {
        // Zero means the name is unknown, which the daemon treats as having no *relay* upstream at all rather
        // than relaying unchecked: inbound UDP and ICMP demultiplex on local address and port alone, so the
        // arrival interface is the only thing separating a late reply to a retired mapping from a reply to the
        // mapping that reused its port. The resolver and terminated TCP still work, because both connect.
        // Resolved per config rather than cached beside the selection, so the index and the handle can never
        // disagree.
        val index = selected?.properties?.interfaceName?.let { Os.if_nametoindex(it) }
        if (index == 0) Timber.w("Cannot resolve the upstream interface index of " +
                selected?.properties?.interfaceName)
        return publication.build(stateFlow.value == State.ACTIVE, selected?.network?.networkHandle,
            index?.takeIf { it != 0 })
    }

    private suspend fun Session.push() {
        val daemon = daemon ?: return
        try {
            daemon.apply(config())
        } catch (e: Exception) {
            // A plain cancellation is this session's own withdrawal cancelling the observer that called this,
            // not a daemon failure, and treating it as one would report a session that is already stopping as
            // broken. The acknowledgement deadline is a `CancellationException` too, and that one is exactly
            // the daemon failing to answer.
            if (e is CancellationException && e !is TimeoutCancellationException) throw e
            // An update that cannot be carried or confirmed ends the session, and that is not defensive
            // teardown: the app can no longer tell what the child is bound to, and retirement is daemon-side
            // work it has no way to ask for again - a failed frame write desynchronizes the stream, so there is
            // nothing to retry on either. Stop and reapply is the way back.
            //
            // Reported rather than acted on, because this runs inside the scope a withdrawal cancels: the one
            // watcher selecting on [Session.failed] owns it, and during startup [publish] checks it directly.
            Timber.w(e, "Shizuku session $generation lost control of its daemon")
            failed.complete(app.getText(R.string.shizuku_failure_daemon))
        }
    }

    /**
     * Preference success is never proof of upstream selection; only tethering naming the exact owned
     * network grants [State.ACTIVE], and only [State.ACTIVE] admits traffic.
     *
     * Ownership is decided by identity against the exact `Network` [start] published, and that is a positive
     * proof rather than a convention. The registered agent returned it; the exact request, whose specifier is
     * this session's own `testtunN` name, resolved to the same value, and no other agent can carry that
     * specifier; and the agent stays registered for as long as this comparison is made, because losing it
     * enters [State.STOPPING] at once. A `Network` is netId equality, so that last part is what matters: a
     * netId is only reissued after the network holding it is destroyed.
     *
     * Classifying a *non*-owned upstream needs the platform, and it needs no privilege to ask.
     * `getNetworkCapabilities` enforces `ACCESS_NETWORK_STATE` and nothing else - there is no ownership or
     * restricted-network check on the way in - and it sanitizes the allowed-UID set, the specifier, the
     * owner/administrator UIDs, the SSID and the underlying networks, but never the transports. So
     * `TRANSPORT_TEST` is readable for any live network, including a restricted one this app has no access to,
     * on Android 13 through 17 alike.
     *
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/service/src/com/android/server/ConnectivityService.java#3288
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/service/src/com/android/server/ConnectivityService.java#2224
     */
    @SuppressLint("WrongConstant")
    private fun Session.commit(): State {
        val upstream = upstream
        return when {
            network != null && upstream == network -> State.ACTIVE
            // Tethering names no upstream at all, so there is nothing carrying anything and nothing to
            // classify. Deliberately not read as a downstream question: whether a hotspot is up is this
            // mode's business only insofar as tethering selected an upstream for it.
            upstream == null -> State.ARMED
            // Asked only here, where the alternative would be to tell the user to cycle tethering: an
            // upstream that is not this session's network but is somebody's test network is a collision no
            // cycle can fix.
            else -> when (Services.connectivity.getNetworkCapabilities(upstream)
                    ?.hasTransport(TRANSPORT_TEST)) {
                // Also unredacted for a network this app does not own, and the only readable name for it.
                // https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/service/src/com/android/server/ConnectivityService.java#3245
                true -> throw CollisionException(
                    Services.connectivity.getLinkProperties(upstream)?.interfaceName)
                false -> State.RESTART_REQUIRED
                // The network went away between tethering naming it and this read, so there is nothing to
                // classify. Staying unconfirmed is the conservative answer: telling the user to cycle their
                // hotspot over a network that no longer exists would be advice about nothing, and the next
                // upstream callback decides for real. It also advances the epoch, exactly as any other loss
                // of positive confirmation does.
                null -> State.VERIFYING
            }
        }
    }
}
