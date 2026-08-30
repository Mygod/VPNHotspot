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
import android.os.ParcelFileDescriptor
import androidx.annotation.StringRes
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.net.TetheringManagerCompat
import be.mygod.vpnhotspot.root.daemon.StartShizukuSessionCommand
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.UnblockCentral
import be.mygod.vpnhotspot.util.readableMessage
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.Job
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.async
import kotlinx.coroutines.awaitAll
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.coroutineScope
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
 * Publishes an app-owned restricted TestNetwork without calling `setupTestNetwork`, then feeds its TUN to
 * the app-UID daemon. Tethering itself is never started or cycled; only its global test-network preference
 * is changed and later cleared.
 */
object ShizukuTestNetwork {
    /** RFC 5737 TEST-NET-1 keeps the synthetic IPv4 connected prefix unroutable on the Internet. */
    private const val TUN_IPV4_ADDRESS = "192.0.2.1"
    private const val TUN_IPV4_PREFIX_LENGTH = 30
    private const val VIRTUAL_DNS_IPV4 = "192.0.2.2"
    /**
     * The RFC 3849 documentation prefix is unroutable but passes tethering's `isGlobalPreferred` filter.
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/Tethering/src/com/android/networkstack/tethering/IPv6TetheringCoordinator.java#239
     */
    private const val TUN_IPV6_ADDRESS = "2001:db8:1::1"
    private const val TUN_IPV6_PREFIX_LENGTH = 64
    private const val VIRTUAL_DNS_IPV6 = "fd00::53"
    /** Fixed TUN, agent, and daemon MTU. */
    private const val TEST_NETWORK_MTU = 1500
    /** Exact-request timeout used as the negative publication result. */
    private const val EXACT_REQUEST_LIFETIME_MILLIS = 60_000

    /**
     * `NetworkCapabilities.TRANSPORT_TEST`, blocked and 7 on Android 13 and 17.
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/framework/src/android/net/NetworkCapabilities.java#1799
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/NetworkCapabilities.java#2278
     */
    private const val TRANSPORT_TEST = 7
    /**
     * `NET_CAPABILITY_NOT_VCN_MANAGED`, test API and 28 on Android 13 and 17.
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/framework/src/android/net/NetworkCapabilities.java#360
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/NetworkCapabilities.java#434
     */
    private const val NET_CAPABILITY_NOT_VCN_MANAGED = 28
    /**
     * `ConnectivityManager.TYPE_TEST`, blocked and 18 on Android 13 and 17.
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/framework/src/android/net/ConnectivityManager.java#345
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/ConnectivityManager.java#369
     */
    private const val TYPE_TEST = 18
    /**
     * The deprecated specifier builder produces this hidden type only after `TRANSPORT_TEST` is added.
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/framework/src/android/net/NetworkRequest.java#419
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/NetworkRequest.java#559
     */
    private const val TEST_NETWORK_SPECIFIER_CLASS = "android.net.TestNetworkSpecifier"

    /**
     * `LinkAddress(InetAddress, int)`, test API across Android 13-17.
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/framework/src/android/net/LinkAddress.java#287
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/LinkAddress.java#285
     */
    @get:SuppressLint("SoonBlockedPrivateApi")
    private val constructorLinkAddress by lazy {
        LinkAddress::class.java.getDeclaredConstructor(InetAddress::class.java, Int::class.javaPrimitiveType)
    }
    /**
     * `RouteInfo(IpPrefix, InetAddress, String, int)`, test API across Android 13-17.
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-13.0.0_r1/framework/src/android/net/RouteInfo.java#146
     * https://android.googlesource.com/platform/packages/modules/Connectivity/+/refs/tags/android-17.0.0_r1/framework/src/android/net/RouteInfo.java#146
     */
    private val constructorRouteInfo by lazy {
        RouteInfo::class.java.getDeclaredConstructor(IpPrefix::class.java, InetAddress::class.java,
            String::class.java, Int::class.javaPrimitiveType)
    }

    enum class State(@StringRes val label: Int) {
        ARMED(R.string.shizuku_state_armed),
        ACTIVE(R.string.shizuku_state_active),
        RESTART_REQUIRED(R.string.shizuku_state_restart_required),
    }

    private class Session(
        val epoch: ShizukuEpoch,
        val scope: CoroutineScope,
        val lifespan: Job,
    ) {
        var state: State? = null
        lateinit var interfaceName: String
        val failed = CompletableDeferred<Throwable>()
        val ended = CompletableDeferred<Unit>()
        var retirement: CompletableDeferred<Unit>? = null
        var residual: Exception? = null

        var privileged: PrivilegedConnectivity? = null
        val descriptor = SimpleResource<ParcelFileDescriptor>("TUN descriptor")
        val child = SimpleResource<AppUidDaemon.Child>("vpnhotspotd")
        val request = ExactRequestResource()
        val connector = SimpleResource<PinnedTetheringConnector>("tethering connector")
        val preference = PreferenceResource()
        val agent = AgentResource()

        var daemon: AppUidDaemon? = null
        var cleanup: ShizukuEpoch? = null
        var network: Network? = null
        var upstream: Network? = null

        private val resources get() = arrayOf<SessionResource>(
            descriptor, child, request, connector, preference, agent)

        val outstanding get() = resources.any { it.outstanding }
        val localOutstanding get() = arrayOf<SessionResource>(descriptor, child, agent).any { it.outstanding }

        override fun toString() = resources.joinToString(prefix = "session(", postfix = ")")
    }

    private var current: Session? = null
    private val stateFlow = MutableStateFlow<OwnedState?>(null)
    internal val state = stateFlow.asStateFlow()
    private val intentFlow = MutableStateFlow<Job?>(null)

    val intent = intentFlow.asStateFlow()

    fun publishIntent(owner: Job) {
        intentFlow.value = owner
    }

    fun withdrawIntent(owner: Job) {
        intentFlow.compareAndSet(owner, null)
    }

    internal suspend fun localResourcesFenced() = withContext(privilegedDispatcher) {
        current?.localOutstanding != true
    }

    internal suspend fun settle(): Unit = withContext(privilegedDispatcher) {
        val previous = current ?: return@withContext
        if (previous.residual != null) {
            try {
                releasePrivileged(previous)
            } catch (e: Exception) {
                if (e is CancellationException) throw e
                throw IllegalStateException(app.getString(R.string.shizuku_failure_cleanup_unconfirmed), e)
            }
            finish(previous)
        } else {
            retire(previous)
            current?.let { still ->
                throw IllegalStateException(
                    app.getString(R.string.shizuku_failure_cleanup_unconfirmed), still.residual)
            }
        }
        Timber.i("Finished outstanding Shizuku cleanup")
    }

    /** Performs non-mutating gates; the returned closure begins publication under the accepted lifespan. */
    internal suspend fun prepare(): suspend () -> Unit {
        check(Build.VERSION.SDK_INT >= 33) { "Shizuku mode requires Android 13" }
        val lifespan = currentCoroutineContext().job
        // Authorization may wait for user input, so run independent gates concurrently.
        lateinit var epoch: ShizukuEpoch
        coroutineScope {
            listOf(
                async<Unit>(start = CoroutineStart.UNDISPATCHED) { epoch = ShizukuEpoch.authorize() },
                async<Unit>(start = CoroutineStart.UNDISPATCHED) {
                    PinnedTetheringConnector.requireAutomaticUpstream()
                    withContext(privilegedDispatcher) {
                        check(!PinnedTetheringConnector.died.isCompleted) {
                            app.getString(R.string.shizuku_failure_tethering_died)
                        }
                        current?.let {
                            throw IllegalStateException("Shizuku session is still outstanding: $it")
                        }
                    }
                },
            ).awaitAll()
        }
        return { withContext(privilegedDispatcher) { publish(epoch, lifespan) } }
    }

    @SuppressLint("WrongConstant")
    private suspend fun publish(epoch: ShizukuEpoch, lifespan: Job) {
        // Resolve cleanup and child-PID reflection before the first external mutation.
        UnblockCentral.IConnectivityManager_releaseNetworkRequest
        UnblockCentral.NetworkCallback_networkRequest
        UnblockCentral.UNIXProcess_pid
        val privileged = PrivilegedConnectivity.create(epoch)
        val service = UnblockCentral.ITestNetworkManager_asInterface(null, epoch.wrap(
            checkNotNull(epoch.bracket { privileged.service.startOrGetTestNetworkService() }) {
                "No TestNetwork service binder"
            }))
        val testNetworkManager = UnblockCentral.TestNetworkManager_constructor.newInstance(service)
                as TestNetworkManager
        val current = Session(epoch,
            CoroutineScope(privilegedDispatcher + SupervisorJob(lifespan)), lifespan)
        current.privileged = privileged
        this.current = current
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
        val snapshot = CompletableDeferred<Unit>()
        // `UNDISPATCHED` starts every independent branch before any await. Each records what it
        // acquires before completion so rollback owns partial startup.
        val (request, capabilities, properties) = coroutineScope {
            val prerequisites = listOf(
                async<Unit>(start = CoroutineStart.UNDISPATCHED) {
                    current.connector.live(PinnedTetheringConnector.acquire(epoch))
                },
                async<Unit>(start = CoroutineStart.UNDISPATCHED) {
                    current.child.live(AppUidDaemon.spawn()).connect(descriptor,
                        StartShizukuSessionCommand(
                            interface_name = current.interfaceName,
                            mtu = TEST_NETWORK_MTU,
                            virtual_addresses = listOf(VIRTUAL_DNS_IPV4, VIRTUAL_DNS_IPV6).map {
                                InetAddress.getByName(it).address.toByteString()
                            },
                            gateway_addresses = listOf(TUN_IPV4_ADDRESS, TUN_IPV6_ADDRESS).map {
                                InetAddress.getByName(it).address.toByteString()
                            },
                        )).also { current.daemon = it }
                },
                async<Unit>(start = CoroutineStart.UNDISPATCHED) {
                    select<Unit> {
                        snapshot.onAwait { }
                        PinnedTetheringConnector.died.onAwait {
                            throw PinnedTetheringConnector.DiedException(
                                app.getString(R.string.shizuku_failure_tethering_died))
                        }
                    }
                },
            )
            current.scope.launch(start = CoroutineStart.UNDISPATCHED) {
                try {
                    TetheringManagerCompat.eventFlow.collect { event ->
                        if (event !is TetheringManagerCompat.Event.UpstreamChanged) return@collect
                        snapshot.complete(Unit)
                        if (event.network == current.upstream) return@collect
                        current.upstream = event.network
                        if (current.network == null) return@collect
                        val next = current.commit()
                        if (current.state == next) return@collect
                        current.state = next
                        stateFlow.value = OwnedState(current.lifespan, next)
                        Timber.i("Shizuku session is $next, upstream ${current.upstream}")
                        val daemon = current.daemon ?: return@collect
                        try {
                            daemon.apply(next == State.ACTIVE)
                        } catch (e: Exception) {
                            if (e is CancellationException) throw e
                            Timber.w(e, "Shizuku session lost control of its daemon")
                            current.failed.complete(e)
                        }
                    }
                } catch (e: Throwable) {
                    if (e is CancellationException) throw e
                    if (!snapshot.completeExceptionally(e)) throw e
                }
            }
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
                        if (Build.VERSION.SDK_INT >= 37) Timber.w(e)
                    }
                }
                .build()
            val properties = LinkProperties().apply {
                interfaceName = current.interfaceName
                val addresses = checkNotNull(NetworkInterface.getByName(current.interfaceName)) {
                    "No such interface ${current.interfaceName}"
                }.interfaceAddresses.map {
                    constructorLinkAddress.newInstance(it.address, it.networkPrefixLength.toInt())
                }
                setLinkAddresses(addresses)
                for (address in addresses) addRoute(constructorRouteInfo.newInstance(
                    IpPrefix(address.address, address.prefixLength), null, current.interfaceName,
                    RouteInfo.RTN_UNICAST))
                for (any in arrayOf("0.0.0.0", "::")) addRoute(constructorRouteInfo.newInstance(
                    IpPrefix(InetAddress.getByName(any), 0), null, current.interfaceName,
                    RouteInfo.RTN_UNICAST))
                setDnsServers(listOf(InetAddress.getByName(VIRTUAL_DNS_IPV4),
                    InetAddress.getByName(VIRTUAL_DNS_IPV6)))
                mtu = TEST_NETWORK_MTU
            }
            prerequisites.awaitAll()
            Triple(request, capabilities, properties)
        }
        checkNotNull(current.connector.value) { "The session lost its tethering connector" }
            .setPreferTestNetworks(true, current.preference)
        val exact = ExactRequest()
        epoch.ensureCurrent()
        check(exact.readBack() == null) { "$exact was already registered" }
        current.request.issuing(exact)
        val registration = try {
            privileged.manager.requestNetwork(request, exact.callback, Services.mainHandler,
                EXACT_REQUEST_LIFETIME_MILLIS)
            null
        } catch (e: Throwable) {
            e
        }
        val handle = try {
            exact.readBack()
        } catch (e: ReflectiveOperationException) {
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
        epoch.ensureCurrent()
        check(agent.published == null) { "$agent was already registered" }
        current.agent.issuing(agent)
        val registering = try {
            agent.register()
            null
        } catch (e: Throwable) {
            e
        }
        current.agent.settle(agent.published)
        registering?.let { throw it }
        val network = checkNotNull(agent.published) { "Agent registration returned no network" }
        epoch.ensureCurrent()
        agent.markConnected()
        // Arm all independent publication callbacks before awaiting any of them.
        lateinit var published: NetworkCapabilities
        lateinit var readback: LinkProperties
        try {
            coroutineScope {
                listOf(
                    async<Unit>(start = CoroutineStart.UNDISPATCHED) {
                        awaitNetworkRequest(agent.created, callback.unavailable)
                    },
                    async<Unit>(start = CoroutineStart.UNDISPATCHED) {
                        check(awaitNetworkRequest(callback.available, callback.unavailable) == network) {
                            "Request matched a different network than the one published"
                        }
                    },
                    async<Unit>(start = CoroutineStart.UNDISPATCHED) {
                        published = awaitNetworkRequest(callback.capabilities, callback.unavailable)
                    },
                    async<Unit>(start = CoroutineStart.UNDISPATCHED) {
                        readback = awaitNetworkRequest(callback.properties, callback.unavailable)
                    },
                ).awaitAll()
            }
        } catch (e: NetworkRequestExpiredException) {
            current.request.expired()
            throw e
        }
        epoch.ensureCurrent()
        check(published.hasTransport(TRANSPORT_TEST)) { "Published network lost TRANSPORT_TEST" }
        check(published.networkSpecifier == request.networkSpecifier) {
            "Published network lost its specifier"
        }
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
        check(!callback.lost.isCompleted) { "The request lost its network during startup" }
        check(!agent.destroyed.isCompleted) { "The test network was destroyed during startup" }
        current.network = network
        val committed = current.commit()
        current.state = committed
        stateFlow.value = OwnedState(current.lifespan, committed)
        val daemon = checkNotNull(current.daemon) { "The session lost its daemon" }
        daemon.apply(committed == State.ACTIVE)
        Timber.i("Published restricted test network $network on ${current.interfaceName} as " +
                "$committed, upstream ${current.upstream}: $published $readback")
        current.scope.launch {
            val failure = select {
                PinnedTetheringConnector.died.onAwait {
                    current.preference.lostWithService()
                    app.getText(R.string.shizuku_failure_tethering_died)
                }
                daemon.ended.onAwait { it?.readableMessage ?: app.getText(R.string.shizuku_failure_daemon) }
                agent.destroyed.onAwait { app.getText(R.string.shizuku_failure_network) }
                callback.lost.onAwait { app.getText(R.string.shizuku_failure_network) }
                current.failed.onAwait { it.readableMessage }
            }
            Timber.w("Shizuku session ended: $failure")
            SmartSnackbar.make(failure).show()
            current.ended.complete(Unit)
        }
    }

    internal suspend fun awaitEnd() = withContext(privilegedDispatcher) {
        checkNotNull(current) { "A published session is what a lifespan awaits" }.ended.await()
    }

    internal suspend fun stop(owner: Job): Unit = withContext(privilegedDispatcher) {
        val session = current ?: return@withContext
        if (session.lifespan !== owner) return@withContext
        if (session.residual != null) return@withContext
        retire(session)
        session.residual?.let { throw it }
    }


    /** Barriered, joinable teardown; every local resource fences before privileged residue is retried. */
    private suspend fun retire(session: Session): Unit = withContext(NonCancellable) {
        session.retirement?.let { return@withContext it.await() }
        val retirement = CompletableDeferred<Unit>()
        session.retirement = retirement
        session.state = null
        stateFlow.value = null
        try {
            session.scope.coroutineContext.job.cancelAndJoin()
            // Join observers first so none can reopen admission. If the update fails, the child
            // fence below remains the barrier.
            session.daemon?.let { daemon ->
                try {
                    daemon.apply(false)
                } catch (e: Exception) {
                    if (e is CancellationException) throw e
                    Timber.w(e, "Shizuku session could not close admission")
                }
            }
            // Return failures as values so both lanes settle; rethrow the first in issue order,
            // with later failures suppressed, before closing the TUN.
            val fenced = coroutineScope {
                listOf(
                    async(start = CoroutineStart.UNDISPATCHED) {
                        try {
                            fenceChild(session)
                            null
                        } catch (e: Throwable) {
                            e
                        }
                    },
                    // Clear the preference before unregistering the network it names.
                    async(start = CoroutineStart.UNDISPATCHED) {
                        val cleared = try {
                            session.connector.value?.let { connector ->
                                try {
                                    if (session.preference.clearable) {
                                        connector.setPreferTestNetworks(false, session.preference)
                                    }
                                } catch (e: Exception) {
                                    if (e is CancellationException) throw e
                                    Timber.w(e, "Shizuku session could not clear the preference")
                                } finally {
                                    session.connector.releasing()?.unlink()
                                    session.connector.confirm()
                                }
                            }
                            null
                        } catch (e: Throwable) {
                            e
                        }
                        val unregistered = try {
                            session.agent.unregistering()?.unregister()
                            session.agent.awaiting?.let { agent ->
                                // Agent-Binder ordering makes the post-`unwanted` `created` check exact.
                                // Request callbacks use another Binder, so `lost` is best-effort;
                                // `destroyed` fences TUN closure.
                                agent.unwanted.await()
                                if (agent.created.isCompleted) agent.destroyed.await()
                                session.request.value?.callback?.let { callback ->
                                    if (callback.available.isCompleted) callback.lost.await()
                                }
                                session.agent.confirm()
                            }
                            null
                        } catch (e: Throwable) {
                            e
                        }
                        unregistered?.let { cleared?.addSuppressed(it) }
                        cleared ?: unregistered
                    },
                ).awaitAll()
            }
            var failure: Throwable? = null
            for (ending in fenced) {
                if (ending == null) continue
                val first = failure
                if (first == null) failure = ending else first.addSuppressed(ending)
            }
            failure?.let { throw it }
            session.descriptor.releasing()?.let {
                it.close()
                session.descriptor.confirm()
            }
            check(!session.agent.terminal) {
                app.getString(R.string.shizuku_failure_unfenced, session.toString())
            }
            try {
                releasePrivileged(session)
                finish(session)
            } catch (e: Exception) {
                if (e is CancellationException) throw e
                session.residual = e
                Timber.w(e, "Shizuku session could not confirm its cleanup")
            }
            Timber.i("Retired Shizuku session")
            retirement.complete(Unit)
        } catch (e: Throwable) {
            session.retirement = null
            retirement.completeExceptionally(e)
            throw e
        }
    }

    private suspend fun fenceChild(session: Session) {
        session.child.releasing()?.let { child ->
            child.stop()
            session.child.confirm()
            session.daemon = null
        }
    }

    private suspend fun releasePrivileged(session: Session) {
        cleanLocalCallback(session)
        if (!session.preference.clearable && !session.request.outstanding) return
        check(!session.request.terminal) {
            "Shizuku session cannot release ${session.request}"
        }
        val epoch = session.cleanupEpoch()
        // Attempt both remote repairs even if one fails. Only a confirmed request release
        // transfers its callback to the local cleanup below.
        val repaired = coroutineScope {
            listOf(
                async(start = CoroutineStart.UNDISPATCHED) {
                    try {
                        if (session.preference.clearable) try {
                            val connector = PinnedTetheringConnector.acquire(epoch)
                            try {
                                connector.setPreferTestNetworks(false, session.preference)
                            } finally {
                                connector.unlink()
                            }
                        } catch (e: Exception) {
                            if (!PinnedTetheringConnector.died.isCompleted) throw e
                            session.preference.lostWithService()
                            if (e is CancellationException) throw e
                            Timber.w(e, "Shizuku session lost its tethering preference with the service")
                        }
                        null
                    } catch (e: Throwable) {
                        e
                    }
                },
                async(start = CoroutineStart.UNDISPATCHED) {
                    try {
                        session.request.releasing()?.let { handle ->
                            val privileged = if (epoch === session.epoch) {
                                checkNotNull(session.privileged) {
                                    "The session lost its privileged manager"
                                }
                            } else PrivilegedConnectivity.create(epoch)
                            epoch.bracket { privileged.service.releaseNetworkRequest(handle) }
                            session.request.released()
                        }
                        null
                    } catch (e: Throwable) {
                        e
                    }
                },
            ).awaitAll()
        }
        val cleaned = try {
            cleanLocalCallback(session)
            null
        } catch (e: Throwable) {
            e
        }
        var failure: Throwable? = null
        for (ending in repaired + cleaned) {
            if (ending == null) continue
            val first = failure
            if (first == null) failure = ending else first.addSuppressed(ending)
        }
        failure?.let { throw it }
    }

    private fun cleanLocalCallback(session: Session) {
        val callback = session.request.localCleanup ?: return
        Services.connectivity.unregisterNetworkCallback(callback)
        session.request.localCleaned()
    }

    /** Reuses a current epoch or reauthorizes only the same effective UID that created the resources. */
    private suspend fun Session.cleanupEpoch(): ShizukuEpoch {
        for (candidate in arrayOf(epoch, cleanup)) if (candidate != null) try {
            candidate.ensureCurrent()
            return candidate
        } catch (e: ShizukuEpoch.ChangedException) {
            Timber.w(e, "Reauthorizing Shizuku to finish cleanup")
            if (candidate === cleanup) cleanup = null
        }
        return ShizukuEpoch.authorize().also { fresh ->
            check(fresh.uid == epoch.uid) {
                "Cleanup Shizuku identity is uid ${fresh.uid}, the session ran as ${epoch.uid}"
            }
            cleanup = fresh
        }
    }

    private fun finish(session: Session) {
        check(!session.outstanding) { "Shizuku session still owes $session" }
        session.privileged = null
        if (current === session) current = null
    }

    private fun Session.commit() = when {
        network != null && upstream == network -> State.ACTIVE
        upstream == null -> State.ARMED
        else -> State.RESTART_REQUIRED
    }
}

/** Pairs committed state with its lifespan so a predecessor cannot label its successor. */
internal class OwnedState(val owner: Job, val state: ShizukuTestNetwork.State) {
    companion object {
        @StringRes
        fun label(owner: Job?, committed: OwnedState?) =
            if (owner != null && committed != null && committed.owner === owner) committed.state.label
            else null
    }
}
