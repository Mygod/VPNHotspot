package be.mygod.vpnhotspot.shizuku

import android.annotation.SuppressLint
import android.net.IpPrefix
import android.net.LinkAddress
import android.net.LinkProperties
import android.net.Network
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
import kotlinx.coroutines.Job
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.currentCoroutineContext
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.job
import kotlinx.coroutines.launch
import kotlinx.coroutines.selects.select
import kotlinx.coroutines.withContext
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
     * How long the exact request stays registered while ConnectivityService creates and publishes this
     * session's native network, which is the only thing that bounds it.
     *
     * What is bounded is the request, not a wait. `updateNetworkInfo` returns the moment
     * `createNativeNetwork` fails, so netd or DnsResolver refusing the network leaves an agent that is
     * registered, never created and never matched, and no negative NetworkAgent callback exists to say so -
     * every publication barrier below would wait on a callback nothing will ever send. The timed
     * `requestNetwork` overload is the platform's own answer to exactly that: it removes the unsatisfied
     * request and delivers `onUnavailable`, which is the negative terminal this startup rolls back on.
     *
     * It cannot bound a session that published. `notifyNetworkAvailable` removes the pending timeout
     * message before it dispatches `CALLBACK_AVAILABLE`, so a satisfied request has no lifetime left at all
     * and a committed session can never be taken back by this.
     *
     * The platform states no bound on native network creation, so the value is borrowed rather than derived:
     * one minute is `TetheringManager`'s own `DEFAULT_TIMEOUT_MS`, the bound it puts on its synchronous
     * service-result and initial-callback waits - `RequestDispatcher.waitForResult` and
     * `TetheringCallbackInternal.waitForStarted` - at both ends of the supported range. That is a precedent
     * for the length and nothing more. It is not a bound on this session's own preference call, which
     * deliberately bypasses `TetheringManager` and waits on the connector's direct result with no
     * elapsed-time terminal at all - which is exactly why that call is issued before this lifetime starts.
     *
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/service/src/com/android/server/ConnectivityService.java#9223
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/service/src/com/android/server/ConnectivityService.java#9326
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/Tethering/common/TetheringLib/src/android/net/TetheringManager.java#63
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/Tethering/common/TetheringLib/src/android/net/TetheringManager.java#431
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/service/src/com/android/server/ConnectivityService.java#13527
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/service/src/com/android/server/ConnectivityService.java#13699
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/common/TetheringLib/src/android/net/TetheringManager.java#73
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/common/TetheringLib/src/android/net/TetheringManager.java#546
     */
    private const val EXACT_REQUEST_LIFETIME_MILLIS = 60_000

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
     * that silent mismatch into an explicit failure instead of a publication that never arrives.
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
         */
        RESTART_REQUIRED(R.string.shizuku_state_restart_required),
    }

    /**
     * What one session generation still owes, as a private ledger fact. Never a user-visible state and
     * never a transition to render: a successor is admitted exactly when the generation is finished and
     * cleared, which happens once, as the final committed step of a retirement.
     */
    private enum class Phase {
        /**
         * Resources are being acquired and nothing is committed. A publication that fails leaves this fact
         * behind with its ledger intact, and the lifespan's finalizer runs the one retirement over it.
         */
        ACQUIRING,
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
        /**
         * The lifespan this generation belongs to. Every display state it commits is stamped with this, so
         * whether it may still speak for the row is decided by identity against the accepted intent owner
         * rather than by when a write happened to land.
         */
        val lifespan: Job,
    ) {
        var phase = Phase.ACQUIRING
        /**
         * What this generation currently is, kept here rather than read back out of the display flow: the
         * daemon's config is built from it, and the config must not change because the row stopped showing
         * a label. Null until the first commit and again from the moment a withdrawal begins.
         */
        var state: State? = null
        /** Assigned the moment `createTunInterface` names it, which is also when [descriptor] is recorded. */
        var interfaceName = ""
        /**
         * A committed session has lost the machinery it needs, carrying the operational exception that says
         * so. Completed by whichever observer noticed rather than acted on there, because those observers run
         * inside the scope a withdrawal cancels; signalling is all they do. The one watcher that selects on
         * this reports it and completes [ended], and the withdrawal itself is the lifespan finalizer's alone.
         *
         * The exception rather than a message, because the daemon's own failures are structured: a refused
         * config arrives as a [be.mygod.vpnhotspot.root.daemon.DaemonException] naming the context, the errno
         * and the Rust source line, and flattening it here would leave the one place that reports it with
         * nothing to report. It is turned into text at that boundary and nowhere earlier.
         */
        val failed = CompletableDeferred<Throwable>()
        /**
         * Completed once this session has lost the machinery it needs and its one watcher has said so. What
         * the lifespan awaits: returning from it sends that same lifespan into its own finalizer, which is
         * the only thing that withdraws - so a session that ended on its own and one the user stopped reach
         * exactly the same teardown, in the same place.
         */
        val ended = CompletableDeferred<Unit>()
        /**
         * The retirement in flight, and what makes several stops one withdrawal: the first caller runs the
         * ordered steps and every other awaits this. Written before the first suspension point on the
         * privileged lane, so two observations that both decide the session is over produce one withdrawal.
         * Cleared again when a step could not be confirmed, so the next caller retries what is left instead
         * of returning as though it had finished.
         */
        var retirement: CompletableDeferred<Unit>? = null
        /**
         * Why this generation could not confirm its privileged release, kept so the one owner that reports
         * a start's single cleanup attempt can carry the real cause rather than a bare message.
         */
        var residual: Exception? = null

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

        /** The one live [AppUidDaemon] this session's [child] is speaking, once its start call was ACKed. */
        var daemon: AppUidDaemon? = null
        /**
         * The cleanup-only epoch, authorized only after [epoch] is gone, purely to finish releasing what
         * this session already created. It republishes nothing, and it is revalidated on every retry rather
         * than trusted because it worked once.
         */
        var cleanup: ShizukuEpoch? = null
        /**
         * The upstream generation of every config this session publishes, and the addresses it may never
         * change. The MTU is not among them: the daemon is told it once, on the start call that hands over the
         * TUN, and no config can move it.
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

        /**
         * True while any local resource is still owed at all - live, issuing, released but unconfirmed, or
         * unknown. Wider than [localUnfenced] on purpose, and it is the fence question: `fencing()` moves a
         * child to RELEASE_ISSUED *before* `Child.stop()` can throw, and the descriptor close and the agent
         * barrier have the same shape, so a resource that is merely not-yet-confirmed may still be relaying.
         * Only [localUnfenced] is hopeless; this is everything that is not yet done.
         */
        val localOutstanding get() = arrayOf<SessionResource>(descriptor, child, agent).any { it.outstanding }

        override fun toString() = "session $generation: " + resources.joinToString()
    }

    private var generation = 0L
    /**
     * The one session generation this process owns, whatever [Phase] it is in, and the lock nothing else
     * plays: a successor is admitted exactly when this becomes null, which happens once, as the final
     * committed transition of a retirement. Confined to [privilegedDispatcher], and every mutating entry
     * point below runs inside one lifespan, which is what keeps two of them apart across their
     * suspensions - a single-lane dispatcher orders dispatches, not run-to-completion sections.
     *
     * What serializes the generations themselves is the lifespan barrier: a successor joins its predecessor
     * before it prepares anything, and that barrier is the process's rather than any one component's, so it
     * holds across a recreation too. If Android destroys the service while a lifespan is still finalizing,
     * its scope is deliberately left alive to finish - and that finalizer is exactly the predecessor the
     * recreated instance's first command joins, rather than something it can overtake.
     *
     * This field and [Session.retirement] are the ledger's own half of the same rule, and they answer for
     * what no ordering can: a non-null generation is settled before anything new is created, and a
     * withdrawal already in flight is awaited rather than duplicated, both on this lane. Neither of them is
     * a statement about *ownership*, which is why the one destructive entry point is keyed on the lifespan
     * that published the generation instead of trusting either - see [stop].
     */
    private var current: Session? = null
    private val stateFlow = MutableStateFlow<OwnedState?>(null)
    /**
     * The last committed display state and the lifespan that committed it. Null when none has been.
     *
     * Internal because the carrier is: only this module projects it, through [OwnedState.label].
     */
    internal val state = stateFlow.asStateFlow()
    private val intentFlow = MutableStateFlow<Job?>(null)

    /**
     * Stable user intent as an owner token: the lifespan a start installed, or null for off. On the moment a
     * start is accepted, off the moment a stop is asked for or a lifespan ends on its own. Never a
     * transition, so the row's control is always live and never shows a state the user cannot act on.
     *
     * A token rather than a boolean because the same value answers both questions the row asks - whether
     * this mode is meant to be on, and *whose* committed state may label it. Published here rather than by
     * the lifespan owner because it outlives any one component, while the lifespan itself belongs to the
     * foreground service that holds the process up.
     */
    val intent = intentFlow.asStateFlow()

    /** Accepts [owner] as the lifespan the user asked for. */
    fun publishIntent(owner: Job) {
        intentFlow.value = owner
    }

    /**
     * Withdraws [owner], if it is still the accepted one.
     *
     * Identity-guarded and atomic, so a lifespan finalizing after its successor was already accepted cannot
     * turn the row off under it. Nothing else has to move with it: a committed state carries the lifespan
     * that produced it, so it stops being shown the moment its own owner stops being the intended one.
     */
    fun withdrawIntent(owner: Job) {
        intentFlow.compareAndSet(owner, null)
    }

    /**
     * Whether every local resource this process created has been proven gone.
     *
     * The authoritative answer, read from the ledger rather than remembered beside it: a generation is
     * either absent or owes nothing local at all. A lifespan owner asks this after each cleanup attempt to
     * decide whether it may let the process go, which is why it has to be the ledger's fact and not a flag
     * some component keeps - a flag resets when that component is recreated, and the child it was describing
     * does not.
     *
     * Suspending, and on [privilegedDispatcher], because that is where the ledger lives: reading it from
     * the main thread would be answering a question about state this lane is in the middle of mutating.
     */
    internal suspend fun localResourcesFenced() = withContext(privilegedDispatcher) {
        current?.localOutstanding != true
    }

    /**
     * Settles whatever the previous generation still owed, and creates nothing. Mutating, not mutation-free:
     * it can fence a child, close the TUN, withdraw the agent, release a privileged request and clear the
     * global preference.
     *
     * Reached from a live lifespan as that start's one attempt, and from a cleanup-only one as the whole of
     * what it exists for. Either way it runs *before* anything only a new session needs - the Shizuku
     * authorization, the automatic-upstream support check, [PinnedTetheringConnector.died], the collision
     * scan - because none of those is what a withdrawal is missing: fencing the child, withdrawing the agent
     * and closing the descriptor issue no Shizuku transaction at all, and only the request release and the
     * preference clear do, which is what [Session.cleanupEpoch] authorizes for itself when the session's own
     * epoch is gone. Ordering a successor's gates in front of this would let a Shizuku the user revoked, or a
     * tethering service that died permanently, take away the only retry a recoverable child, descriptor or
     * agent still had.
     *
     * Unkeyed, unlike [stop], and deliberately: what this settles is by definition a generation some *other*
     * lifespan published, which is the whole of inherited debt and which no owner token could name. What
     * makes that sound is the barrier rather than a key - every caller has joined the complete process
     * predecessor, and every successor from any component orders itself behind the caller - so while this
     * runs there is no newer generation for it to reach. What the barrier does not order is anything reached
     * from outside a lifespan, which is what [Session.retirement] is for: [retire] below awaits a withdrawal
     * already in flight instead of starting a second.
     */
    internal suspend fun settle(): Unit = withContext(privilegedDispatcher) {
        val previous = current ?: return@withContext
        // Exactly one attempt, and which one depends on what the previous generation still owes. A
        // withdrawal that threw may have left local resources, so it is retried whole; one that got as far
        // as [Phase.RESIDUAL] owes only the privileged release, which is the one thing a cleanup epoch
        // still reaches.
        //
        // The two are alternatives rather than steps. A full retirement that itself ends RESIDUAL has
        // already made this attempt: issuing its release again here would be the same debt attempted twice
        // in a row, reported twice, by one press.
        if (previous.phase == Phase.RESIDUAL) {
            try {
                releasePrivileged(previous)
            } catch (e: Exception) {
                if (e is CancellationException) throw e
                throw IllegalStateException(app.getString(R.string.shizuku_failure_cleanup_unconfirmed), e)
            }
            finish(previous)
        } else {
            // Throws when the local resources could not be fenced, and finishes the generation itself when
            // it settles everything. Anything it leaves behind - a release it could not confirm, which is
            // [Phase.RESIDUAL] - is the *next* attempt's, not a second one here, so this one reports the
            // failure rather than letting a successor publish over a generation the ledger still owes.
            retire(previous)
            current?.let { still ->
                throw IllegalStateException(
                    app.getString(R.string.shizuku_failure_cleanup_unconfirmed), still.residual)
            }
        }
        Timber.i("Finished the outstanding cleanup of Shizuku session ${previous.generation}")
    }

    /**
     * Everything that has to hold before this generation creates anything *of its own*, and nothing about
     * the previous one: [settle] has already run, on the same lifespan and before this. Cancellable
     * throughout, because Shizuku authorization can sit on the user's own permission dialog for as long as
     * they take. Returns the publication step.
     *
     * Every gate here may refuse this session permanently, and none of that refusal reaches backwards: a
     * revoked Shizuku, an unsupported device or a dead tethering service stops a new generation from
     * existing and leaves the old one already withdrawn.
     */
    @SuppressLint("WrongConstant")
    internal suspend fun prepare(): suspend () -> Unit {
        check(Build.VERSION.SDK_INT >= 33) { "Shizuku mode requires Android 13" }
        // Read here, on the lifespan's own coroutine and before anything switches away from it, so the
        // session's observers can be launched as its structured children rather than as a second root. A
        // `withContext` job further down would be the wrong parent: it completes when its block returns,
        // and a parent with a live child never would.
        val lifespan = currentCoroutineContext().job
        val epoch = ShizukuEpoch.authorize()
        // before anything is created, because a device that never consults the preference cannot run this
        // mode at all and a permanent RESTART_REQUIRED is the worst available outcome
        PinnedTetheringConnector.requireAutomaticUpstream()
        withContext(privilegedDispatcher) {
            // The process-terminal fact, read from where every observation of it lands: a death recipient
            // firing under any session, or a `linkToDeath` that refused an already dead binder. A flag of
            // this file's own would have to be written by one of those observers, and the one that matters
            // most - the connector that could never be linked at all - belongs to no session.
            check(!PinnedTetheringConnector.died.isCompleted) {
                app.getString(R.string.shizuku_failure_tethering_died)
            }
            // The ledger's own half of one-generation-at-a-time, on the lane that owns it. [settle] above
            // either cleared this or threw, so reaching it non-null means the ordering in front of the
            // ledger failed; fail closed rather than publish a second generation over a live one.
            current?.let {
                throw IllegalStateException("Shizuku session ${it.generation} is still outstanding")
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
        return { withContext(privilegedDispatcher) { publish(epoch, lifespan) } }
    }

    /**
     * Publishes one session, recording every step it completes so that whatever it leaves behind is exactly
     * what [ShizukuLifecycle.Session.retire] withdraws.
     *
     * It deliberately does **not** roll itself back. Failing here throws with [current] still naming the
     * session and its ledger still owning every resource that exists, and [ShizukuLifecycle] runs the one
     * retirement - so a rollback that fails is not retried immediately by the start that caused it. What
     * retries it is the next *command*: intent is already off, so the next thing the user can do to this row
     * is turn it on again, and [settle] runs ahead of any new session either way - as a start's first step,
     * or as the whole of an idle stop's. Nothing before the ledger exists can leave anything behind: building
     * the private
     * manager and fetching the TestNetwork service binder issue no transaction that owes a release.
     *
     * Runs inside one lifespan, after its predecessor barrier, so no other publication can interleave.
     * Nothing of root mode's is consulted, withdrawn or rebuilt to get here: whatever root routing exists
     * stays exactly as it is, and Android's tethering decides for itself whether to select what this
     * publishes.
     */
    @SuppressLint("WrongConstant")
    private suspend fun publish(epoch: ShizukuEpoch, lifespan: Job) {
        // Resolved before anything is created, because the direct release is what a retirement needs and
        // discovering that it is unreachable *after* the TUN, the request, the preference and the agent
        // exist would leave a session that cannot be taken back. No transaction, no side effect.
        UnblockCentral.IConnectivityManager_releaseNetworkRequest
        UnblockCentral.NetworkCallback_networkRequest
        // Same reason, for the child: the launched process's pid is what its control connection is
        // authenticated against, and one that never authenticates leaves no peer credentials to take it
        // from.
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
        // A supervisor *child* of the lifespan rather than a root of its own, so these observers end with
        // it and no scope of theirs outlives it. That is structure, not exclusion: cancellation is
        // cooperative and a collector can still finish a write afterwards, which is why what it writes is
        // stamped with its owner rather than trusted to have stopped. Supervisor because one observer
        // failing is not the other's business, and the retirement below is still what joins them before
        // touching a resource.
        val current = Session(++generation, epoch,
            CoroutineScope(privilegedDispatcher + SupervisorJob(lifespan)), lifespan)
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
        // Acquired here, ahead of the observation below and not merely ahead of the preference it is used
        // for further down. Linking death is what makes the tethering process's own ending observable at
        // all - nothing else in this app surfaces it - and the wait below is on the first thing only that
        // process can produce, so without a recipient linked in front of it a crash in that window would
        // leave a startup waiting on an observation nothing can ever make. One acquisition and one recipient
        // for the whole session, reused for the preference rather than linked a second time.
        val connector = current.connector.live(PinnedTetheringConnector.acquire(epoch))
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
            try {
                TetheringManagerCompat.eventFlow.collect { event ->
                    // Level-triggered and repeating its current value, so an observation that changed
                    // nothing is dropped here rather than costing a config round trip.
                    if (event !is TetheringManagerCompat.Event.UpstreamChanged) return@collect
                    snapshot.complete(Unit)
                    if (event.network == current.upstream) return@collect
                    current.upstream = event.network
                    if (current.network == null) return@collect
                    val next = try {
                        current.commit()
                    } catch (e: CollisionException) {
                        // Terminal, and signalled to the one watcher that completes [Session.ended] rather
                        // than withdrawn from here: this collector is inside the scope a withdrawal
                        // cancels, and withdrawing is not an observer's to do.
                        Timber.w(e)
                        current.failed.complete(e)
                        return@collect
                    }
                    // Moves admission, and never a generation, whichever way it went. Only ACTIVE admits
                    // traffic, but closing admission tears nothing down: the daemon drops what it reads from
                    // the TUN, creates nothing and refreshes no lifetime, leaving what exists to end on its
                    // own deadlines and its own protocols. So this is not a pause, and the interval is not
                    // free - a flow whose lifetime stops being refreshed can expire inside it - only that
                    // nothing is retired *for* the transition. Retiring for it would need evidence that a
                    // TUN-visible endpoint changed hands, and there is none to have: Android's conntrack owns
                    // the mapping between one and a physical client.
                    if (current.state == next) return@collect
                    current.state = next
                    // Stamped with the lifespan that produced it rather than filtered by when it lands. This
                    // write is not cancellable, so an observer can reach it after its own lifespan was
                    // cancelled and stopped being the accepted one, but a stamped value can only ever label
                    // the owner it names - so a late write is harmless and the daemon's config above still
                    // gets the truth on the way out.
                    stateFlow.value = OwnedState(current.lifespan, next)
                    Timber.i("Shizuku session ${current.generation} is $next, upstream ${current.upstream}")
                    current.push()
                }
            } catch (e: Throwable) {
                // A cancellation is this lifespan's own stop, and the waiter below is inside the lifespan
                // being cancelled, so there is nothing to hand it and nothing to report.
                if (e is CancellationException) throw e
                // Exactly one owner otherwise. Before readiness that owner is the wait below, and it is the
                // only reader this failure can have: registering the callback is part of the startup, this
                // collector is a supervisor child, and a registration or collection failure isolated here
                // would leave that wait on an observation nothing will ever make. Afterwards the deferred
                // has long since been answered, so there is nobody left to hand it to and the scope's own
                // reporting is all there is - which is exactly what an observer failure had before this
                // reader existed.
                if (!snapshot.completeExceptionally(e)) throw e
            }
        }
        // Ended by the first observation, by that observation having become impossible - the process that
        // would make it has gone, and `TetheringManager` never produces another connector - or by the owner
        // cancelling this lifespan, and by nothing else. Death is raced because it is terminal and not
        // because it is a bound: a level-triggered callback that has not fired yet is waited out however
        // long tethering takes. Biased to the observation, so one already delivered still wins the turn it
        // shares with the death that followed it.
        select<Unit> {
            snapshot.onAwait { }
            PinnedTetheringConnector.died.onAwait {
                throw PinnedTetheringConnector.DiedException(
                    app.getString(R.string.shizuku_failure_tethering_died))
            }
        }
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
        // Ownership begins at `spawn`, not at an acknowledged start call. A failure after the start frame is
        // sent may leave the child holding a TUN descriptor, and the ledger below fences it either way.
        val child = current.child.live(AppUidDaemon.spawn())
        val daemon = AppUidDaemon.connect(child, descriptor, current.interfaceName, TEST_NETWORK_MTU,
            current.publication)
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
        // Recorded as owed from the moment the transaction is issued, not from a successful result; the
        // connector classifies the answer, because only the service's own result code can separate "did
        // not act" from "may have acted".
        //
        // Ahead of the exact request rather than after it, because this wait has no bound of its own - it
        // ends on the service's answer, on that service dying, or on a stop - and running it inside the
        // request's lifetime would let a merely slow tethering service spend the publication's.
        connector.setPreferTestNetworks(true, current.preference)
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
            privileged.manager.requestNetwork(request, exact.callback, Services.mainHandler,
                EXACT_REQUEST_LIFETIME_MILLIS)
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
        current.request.settle(handle)
        registration?.let { throw it }
        check(current.request.state == ResourceState.LIVE) {
            "Registration gave no exact NetworkRequest back for $exact"
        }
        epoch.ensureCurrent()
        val callback = exact.callback
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
        current.agent.settle(agent.published)
        registering?.let { throw it }
        val network = checkNotNull(agent.published) { "Agent registration returned no network" }
        epoch.ensureCurrent()
        agent.markConnected()
        // Every one of these is raced against the request's own expiry, because none of them has a negative
        // of its own: a native network ConnectivityService could not create is announced to nobody, so an
        // agent that never reaches `onNetworkCreated` and a request that never matches it are indistinguishable
        // from a publication still in progress. The expiry is the platform's answer, and it is authoritative
        // for all four alike - it is delivered only to a request that was never satisfied, and a request that
        // was satisfied has already had its expiry cancelled.
        val published: NetworkCapabilities
        val readback: LinkProperties
        try {
            awaitNetworkRequest(agent.created, callback.unavailable)
            check(awaitNetworkRequest(callback.available, callback.unavailable) == network) {
                "Request matched a different network than the one published"
            }
            published = awaitNetworkRequest(callback.capabilities, callback.unavailable)
            readback = awaitNetworkRequest(callback.properties, callback.unavailable)
        } catch (e: NetworkRequestExpiredException) {
            // Both halves of the request are provably gone before `onUnavailable` runs - ConnectivityService
            // removes it, and `ConnectivityManager` drops its `sCallbacks` entry and tombstones the callback -
            // so the debt is discharged on this privileged lane instead of being left to a retirement that
            // would reacquire Shizuku to release what the platform has already taken back.
            current.request.expired()
            throw e
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
        // below is what notices afterwards.
        check(!callback.lost.isCompleted) { "The request lost its network during startup" }
        check(!agent.destroyed.isCompleted) { "The test network was destroyed during startup" }
        current.network = network
        current.phase = Phase.RUNNING
        val committed = current.commit()
        current.state = committed
        stateFlow.value = OwnedState(current.lifespan, committed)
        // The first config to reach the daemon, and so the first thing that can end this session. Applied
        // here rather than through [push] because this one failure is this function's own: the watcher that
        // turns a later one into an ending is installed below this line, so there is nothing yet to signal -
        // and because what a refused config throws *is* the report, which routing it through
        // [Session.failed] and a `check` would replace with a sentence of this file's own invention.
        // Throwing it keeps one retirement in charge exactly as the check did: the caller's rollback runs.
        daemon.apply(current.config())
        Timber.i("Published restricted test network $network on ${current.interfaceName} as " +
                "$committed, upstream ${current.upstream}: $published $readback")
        // Only a committed session is watched for these. Until commit, the retirement [ShizukuLifecycle]
        // runs on a failed publication is the only withdrawal there can be: a second one running
        // concurrently with it would release resources the first has not finished recording, and leak
        // whatever it had not reached. Nothing is lost by waiting, because all five are awaited rather
        // than polled.
        //
        // A committed session ends only when its own machinery is gone, and each of these is that:
        // the network this session exists to run has been removed, or the app can no longer tell
        // what its child is bound to. One watcher rather than five, because one ending is all there
        // is and the first of them to fire is the one reported. It lives in the session's own scope
        // so an ordered stop cancels it before touching anything, and all it does is report and
        // complete [Session.ended] - the lifespan resumes on that and withdraws in its own finalizer.
        current.scope.launch {
            val failure = select {
                PinnedTetheringConnector.died.onAwait {
                    // The network stack has already reset the preference and reselected an ordinary
                    // upstream. That is positive proof rather than an assumption - the flag lives in the
                    // process that just died - so the clear this session owed is discharged here instead
                    // of being retried against a service that has forgotten it. The process-terminal half
                    // of the same fact was latched by the death recipient that completed this, so what is
                    // left to do here is this generation's ledger and nothing wider.
                    current.preference.lostWithService()
                    app.getText(R.string.shizuku_failure_tethering_died)
                }
                // The daemon names its own ending whenever it could: the session call's structured error
                // is what the app shows instead of a generic message. A null cause is the conversation
                // simply stopping - EOF, or a child that died - which has nothing more specific to say.
                daemon.ended.onAwait { it?.readableMessage ?: app.getText(R.string.shizuku_failure_daemon) }
                // Agent or request loss keeps [State.ACTIVE] honest as well as ending the session:
                // [commit] compares netIds, and a netId can only be reissued to an unrelated
                // network once this one is destroyed, which is exactly what these observe.
                agent.destroyed.onAwait { app.getText(R.string.shizuku_failure_network) }
                callback.lost.onAwait { app.getText(R.string.shizuku_failure_network) }
                current.failed.onAwait { it.readableMessage }
            }
            Timber.w("Shizuku session ${current.generation} ended: $failure")
            SmartSnackbar.make(failure).show()
            // Handed to the lifespan rather than acted on here, so the withdrawal has one owner however it
            // was reached: this completes, the lifespan returns from its await and runs the same retirement
            // a user's stop would have cancelled it into. Nothing outside this mode is asked for anything -
            // root routing, if any, is untouched by this session ending.
            current.ended.complete(Unit)
        }
    }

    /**
     * Suspends on the committed session's own terminal, which its watcher completes when the session loses
     * something it needs.
     *
     * Nothing is handed anywhere by that completion: this is the lifespan's own suspension, so the same
     * lifespan resumes here and walks into its own finalizer, and the withdrawal below runs there. A stop
     * arriving instead cancels this wait and reaches that same finalizer, which is why there is one teardown
     * for both endings.
     */
    internal suspend fun awaitEnd() = withContext(privilegedDispatcher) {
        checkNotNull(current) { "A published session is what a lifespan awaits" }.ended.await()
    }

    /**
     * Retires the running session, if any, and returns only once every local resource is gone: the child has
     * exited, the network's destruction is proven, the observers are joined and the descriptor is closed. A
     * privileged release may still be owed after that - that residue forbids another session in this process
     * without keeping the TUN open for it, and this is where the caller hears about it.
     *
     * Called from a lifespan's finalizer, and [owner] is that lifespan. This is the one destructive entry
     * point the ledger exposes, so it is keyed: a generation belongs to the lifespan that published it, and
     * one found under any other owner is left whole. The predecessor barrier means the question should not
     * arise - a successor from any component joins the lifespan it replaces before it prepares anything -
     * and keying it is what makes that an invariant of the ledger rather than a property of whoever happened
     * to call. A finalizer that outlived its own generation has nothing of its own left to withdraw, and a
     * replacement's resources were never its to take.
     *
     * A session already left residual is not retried here: its local resources are gone and only [settle]
     * can finish what is left.
     */
    internal suspend fun stop(owner: Job): Unit = withContext(privilegedDispatcher) {
        val session = current ?: return@withContext
        if (session.lifespan !== owner) return@withContext
        if (session.phase == Phase.RESIDUAL) return@withContext
        retire(session)
        // The withdrawal itself finished - nothing local is left - but it could not confirm the privileged
        // release, and that is this caller's answer rather than a line in the log. [retire] records it
        // without showing it precisely so that the one layer which asked, here the lifespan's finalizer,
        // is the one layer that reports it.
        session.residual?.let { throw it }
    }

    // The global preference is the one piece of system state a session can leave behind. It has no owner
    // token, so app death, force-stop or uninstall can strand it at true with nothing left running to clear
    // it, and it is *not* recovered through an action on an existing screen: the row the user already has is
    // the recovery path. Two cases, and they differ. A session left RESIDUAL inside *this* process is
    // retried by [settle], which is why a command settles what a previous withdrawal still owed. After full
    // process death there is no ledger to retry, so a fresh start does not clear the flag in preparation -
    // what clears it then is that session's own [retire], or a reboot.
    //
    // Stranding it is not immediately harmful: with no TEST network present Android's upstream selection
    // falls through to the ordinary default, so the hotspot keeps working. What the flag does mean is that
    // the *next* test network to appear is preferred - this app's own later session, or another app's - and
    // that is the residue worth naming. Another TestNetwork controller is unsupported, so no previous value
    // is restored: the preference is cleared, never negotiated.

    /**
     * Ordered withdrawal, reached from a lifespan finalizer through [stop] and from a later lifespan's
     * [settle] retrying what a previous generation still owed - never from an observer or the watcher.
     * Unkeyed, unlike [stop], and deliberately: what [settle] passes here is a generation some *other*
     * lifespan published, which is the whole of inherited debt, and it may only do so having joined that
     * lifespan.
     * Idempotent and resumable: the first caller runs the steps and the rest await
     * [Session.retirement], while every step confirms its own ledger entry, so a retry does what is left
     * rather than what is done.
     *
     * Local resources are always released, even when the Shizuku epoch is gone, because fencing the child,
     * withdrawing the agent and closing the descriptor need no Shizuku transaction at all. Only the
     * preference clear and the request release do, and those are the ones that may be left outstanding.
     *
     * [NonCancellable] because of who the ordinary caller is: a lifespan that has *already* been cancelled
     * and is running its finalizer, or a later [settle] retrying inherited debt inside a lifespan a stop can
     * cancel at any moment. Every step below awaits something - the preference clear, the agent's
     * destruction callback, the request release - and without this each would abandon at its first
     * suspension, leaving exactly the system state this exists to remove. Each has to run to its own fence.
     */
    private suspend fun retire(session: Session): Unit = withContext(NonCancellable) {
        // Read and written before the first suspension point on the single privileged lane, so two
        // observations that both decide the session is over produce one withdrawal rather than two.
        session.retirement?.let { return@withContext it.await() }
        val retirement = CompletableDeferred<Unit>()
        session.retirement = retirement
        session.phase = Phase.RETIRING
        // Cleared rather than replaced by a transition: what this flow says is what a *committed* session is
        // doing, and from here there is none. The row's own on-ness is the lifespan's to report.
        session.state = null
        stateFlow.value = null
        try {
            // Joined, not merely cancelled: cancellation is a request and this needs a completion. Until it
            // returns, an observer could still be inside a config round trip, or advancing the upstream
            // generation the next one will carry.
            session.scope.coroutineContext.job.cancelAndJoin()
            // Step 1 of the ordered stop, and the daemon's half of it: closing admission in the app alone
            // would leave Rust admitting new flows, mappings, queries and fragments for the whole of the
            // preference clear below, which waits on the tethering service. It throws when the child could
            // not acknowledge that it stopped admitting, and then the child fence runs first rather than
            // after that wait.
            val daemon = session.daemon
            val admissionClosed = if (daemon == null) {
                // No control connection means nothing can be *told* to stop admitting, and that is only
                // harmless when there is no child to admit anything: a child whose start call failed after
                // the start frame was sent may already hold a duplicate of the TUN. Treating a null daemon
                // as "nothing left to do" would let such a child relay through the preference clear below.
                !session.child.outstanding
            } else try {
                daemon.apply(session.config())
                true
            } catch (e: Exception) {
                if (e is CancellationException) throw e
                Timber.w(e, "Shizuku session ${session.generation} could not close admission")
                false
            }
            // Immediately, and before the clear that waits on the tethering service, whenever admission is
            // not provably shut. The fence is idempotent, so the ordinary one below simply finds nothing
            // left.
            if (!admissionClosed) fenceChild(session)
            // Cleared before the agent is withdrawn: this is the one piece of system state that outlives
            // the session, so it is dropped while there is still a live connector to drop it through.
            session.connector.value?.let { connector ->
                try {
                    if (session.preference.clearable) connector.setPreferTestNetworks(false, session.preference)
                } catch (e: Exception) {
                    if (e is CancellationException) throw e
                    // The ledger already knows which of the two this was: a denied clear proves the flag is
                    // untouched and is still owed, while a replaced epoch leaves it unknown. Either way
                    // the withdrawal continues and [releasePrivileged] retries what is left.
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
            // only remaining release is process death. Intent is already off by then - the user asked and
            // got their answer - so what this refuses is reporting the *withdrawal* finished, which is the
            // foreground component's cue to keep the process rather than anything the row shows.
            check(!session.localUnfenced) {
                app.getString(R.string.shizuku_failure_unfenced, session.toString())
            }
            try {
                releasePrivileged(session)
                finish(session)
            } catch (e: Exception) {
                if (e is CancellationException) throw e
                // Explicit and fail closed: the request or the preference may still be held by a retired
                // generation, so no successor session runs in this process until a retry confirms it or the
                // process ends. The withdrawal itself still finishes, because nothing local is left.
                session.phase = Phase.RESIDUAL
                session.residual = e
                // Recorded rather than shown. Whoever asked for this withdrawal is the one layer that
                // reports it - the lifespan's finalizer for a stop, [settle]'s own failure for an inherited
                // debt - so displaying it here as well would make one attempt two reports.
                Timber.w(e, "Shizuku session ${session.generation} could not confirm its cleanup")
            }
            session.state = null
            stateFlow.value = null
            Timber.i("Retired Shizuku session ${session.generation}")
            retirement.complete(Unit)
        } catch (e: Throwable) {
            // Nothing local is confirmed, so the next caller retries the steps that are left rather than
            // returning as though this one had finished. The session stays in [Phase.RETIRING] and the
            // process keeps holding it: its child may still be relaying. None of that is on the row, which
            // has read off since the stop was taken.
            session.retirement = null
            retirement.completeExceptionally(e)
            throw e
        }
    }

    /**
     * The child exit fence, which everything after it assumes has run: a child that survived still holds the
     * TUN and its upstream sockets, so it would keep relaying clients onto a network this session no longer
     * owns. Failing here therefore stops the withdrawal with the whole session retained, and the next
     * command retries it through [settle] - intent is off already, so a further stop or a fresh start are
     * the only things left for the user to do with it.
     *
     * Reissuable rather than once-only: closing sockets and waiting for observed exit is idempotent, and a
     * fence that failed is exactly what a retry is for. Runs before the agent is withdrawn, so nothing is
     * still reading the TUN when the network it belongs to disappears - and out of order, immediately after a
     * failed admission close, so a child that is still admitting never relays through the preference
     * clear's own wait on the tethering service.
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
        //
        // Awaited exactly, and for as long as the platform takes. Declaring the network gone without its
        // proof would release a request that still names it, and there is nothing to substitute that proof
        // with: a known agent always owes [RestrictedAgent.unwanted], and callback absence is never proof
        // that no remote agent exists. So a barrier that never arrives leaves this withdrawal - and the
        // successor joining it - fenced, still holding the agent that a later command's retry would need.
        agent.unwanted.await()
        if (agent.created.isCompleted) agent.destroyed.await()
        if (callback?.available?.isCompleted == true) callback.lost.await()
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
        if (session.preference.clearable) try {
            val connector = PinnedTetheringConnector.acquire(epoch)
            try {
                connector.setPreferTestNetworks(false, session.preference)
            } finally {
                connector.unlink()
            }
        } catch (e: Exception) {
            // Asked here rather than inside, because the `finally` above is where the last synchronous chance
            // to learn of this death is: `unlinkToDeath` answering false latches it, and that happens while
            // this attempt is still unwinding. Only once it has run does the failure in hand mean anything
            // definite - so a clear that failed *unknown* and then proved the death is settled in this
            // attempt, not the next command's.
            if (!PinnedTetheringConnector.died.isCompleted) throw e
            // The process is gone, so the flag went with it whatever this attempt managed or failed to do -
            // by acquisition being handed a binder `linkToDeath` refuses, by the call, or by the unlink. The
            // debt is discharged here rather than reported as residue, because residue is a debt a later
            // command is supposed to settle and there is nothing left that could: `TetheringManager` caches
            // its connector permanently. Letting it out would also skip the exact request's release below,
            // which owes tethering nothing and is still owed.
            session.preference.lostWithService()
            // A cancellation stays a cancellation. The discharge above is the latch's fact and holds either
            // way, but this caller went away, and that is nobody's operational failure to report.
            if (e is CancellationException) throw e
            // Consumed, not suppressed: what it was about is provably settled, so there is no retry for it to
            // ask for and nothing for the caller to act on. Logged with its own context all the same -
            // [PinnedTetheringConnector.DiedException] is the shape expected here, and anything else is a
            // failure this is the only record of.
            Timber.w(e, "Shizuku session ${session.generation} lost its tethering preference with the service")
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
     * No config carries an MTU. The one the daemon sizes every packet against is [TEST_NETWORK_MTU], sent once
     * on the start call and checked there against the TUN itself: it is the TestNetwork's own contract rather
     * than a measurement - what the TUN was created with, what the agent publishes, and what tethering clamps
     * the downstream MTU it derives from this upstream to. This mode owns no downstream and never asks which
     * interfaces Android is serving behind it, so there is nothing narrower to measure and nothing that could
     * move it within a session.
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
        return publication.build(state == State.ACTIVE, selected?.network?.networkHandle,
            index?.takeIf { it != 0 })
    }

    private suspend fun Session.push() {
        val daemon = daemon ?: return
        try {
            daemon.apply(config())
        } catch (e: Exception) {
            // A cancellation is this session's own withdrawal cancelling the observer that called this, not
            // a daemon failure, and treating it as one would report a session that is already stopping as
            // broken. It is the only shape one can arrive in: a config round trip ends on its answer, on the
            // conversation ending, or on that withdrawal, and never on elapsed time.
            if (e is CancellationException) throw e
            // An update that cannot be carried or confirmed ends the session, and that is not defensive
            // teardown: the app can no longer tell what the child is bound to, and retirement is daemon-side
            // work it has no way to ask for again - a failed frame write desynchronizes the stream, so there is
            // nothing to retry on either. Stop and reapply is the way back.
            //
            // Reported rather than acted on, because this runs inside the scope a withdrawal cancels: the one
            // watcher selecting on [Session.failed] is what turns it into an ending. Startup does not come
            // through here at all - [publish] applies its first config directly and lets the exception
            // propagate, because it runs before that watcher exists. Neither path withdraws; the lifespan
            // finalizer does.
            Timber.w(e, "Shizuku session $generation lost control of its daemon")
            // The exception itself, not a message: a config the daemon refused carries its structured report,
            // and the one watcher that selects on this is where it becomes text.
            failed.complete(e)
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
     * ends the lifespan at once. A `Network` is netId equality, so that last part is what matters: a
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
                // upstream callback decides for real. It closes admission, exactly as any other loss of
                // positive confirmation does, and retires nothing.
                null -> State.VERIFYING
            }
        }
    }
}

/**
 * A committed display state together with the lifespan that committed it, and the one rule that decides
 * whether it may still be shown.
 *
 * Top-level, small and deliberately not a mechanism: it exists because "which generation does this label
 * belong to" cannot be answered by a flow of bare states. Publication onto the display flow is not
 * cancellable, so an observer belonging to a lifespan that was already cancelled - and that therefore
 * stopped being the accepted owner - can still finish a write, and no ordering argument makes that
 * impossible. Sampling the job's liveness does not answer it either: liveness is not identity, so it says
 * nothing about *whose* label this is, and the check can be overtaken by the very write it guards. Stamping
 * removes the question instead: a value names its owner, and an owner that is no longer the accepted one
 * simply is not shown.
 */
internal class OwnedState(val owner: Job, val state: ShizukuTestNetwork.State) {
    companion object {
        /**
         * The row's label: the committed state's, but only when [owner] - the accepted intent owner - is
         * referentially the lifespan that committed it. Off carries no label, and neither does a successor
         * that has not committed anything yet.
         */
        @StringRes
        fun label(owner: Job?, committed: OwnedState?) =
            if (owner != null && committed != null && committed.owner === owner) committed.state.label
            else null
    }
}
