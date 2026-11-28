package be.mygod.vpnhotspot

import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.net.wifi.WifiManager
import android.os.Build
import android.os.SystemClock
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.TetherStates
import be.mygod.vpnhotspot.net.wifi.SoftApConfigurationCompat
import be.mygod.vpnhotspot.net.wifi.WifiApManager
import be.mygod.vpnhotspot.net.wifi.WifiApManager.LocalOnlyHotspotEvent
import be.mygod.vpnhotspot.net.wifi.WifiApManager.wifiApState
import be.mygod.vpnhotspot.root.RootManager
import be.mygod.vpnhotspot.root.WifiApCommands
import be.mygod.vpnhotspot.ui.softApStartFailureLabel
import be.mygod.vpnhotspot.util.TileServiceDismissHandle
import be.mygod.vpnhotspot.util.broadcastReceiver
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.async
import kotlinx.coroutines.cancel
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.currentCoroutineContext
import kotlinx.coroutines.ensureActive
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.flow.mapNotNull
import kotlinx.coroutines.job
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.parcelize.Parcelize
import timber.log.Timber
import java.lang.reflect.InvocationTargetException

class LocalOnlyHotspotService : NetlinkNeighbourMonitoringService() {
    companion object {
        const val KEY_USE_SYSTEM = "service.tempHotspot.useSystem"

        var dismissHandle: TileServiceDismissHandle? = null
        private fun dismissIfApplicable() = dismissHandle?.run {
            get()?.dismiss()
            dismissHandle = null
        }
    }

    class Binder(owner: LocalOnlyHotspotService) : android.os.Binder() {
        private var service: LocalOnlyHotspotService? = owner
        private val _iface = MutableStateFlow<String?>(null)
        val iface = _iface.asStateFlow()
        private val _configuration = MutableStateFlow<SoftApConfigurationCompat?>(null)
        val configuration = _configuration.asStateFlow()

        fun update(iface: String?, configuration: SoftApConfigurationCompat?) {
            _iface.value = iface
            _configuration.value = configuration
        }

        fun detach() {
            service = null
            update(null, null)
        }

        fun stop() {
            service?.stop()
        }
    }

    @Parcelize
    class Starter : BootReceiver.Startable {
        override fun start(context: Context) {
            context.startForegroundService(Intent(context, LocalOnlyHotspotService::class.java))
        }
    }

    /**
     * A start that the lifespan cannot recover from; [message] (if any) is shown to the user.
     */
    private class StartFailure(message: String? = null) : Exception(message)

    /** The configuration of the reservation currently held by the lifespan, if any. */
    private var configuration: SoftApConfigurationCompat? = null
    private val iface = MutableStateFlow<String?>(null)
    /**
     * Drives the foreground notification off [iface]: counts while connected ("something"), an empty
     * notification while no active interface is owned (""), nothing (no foreground) while idle (null).
     */
    override val interfaces = iface.map { value ->
        when {
            value == null -> null
            value.isEmpty() -> Interfaces()
            else -> Interfaces(active = listOf(value))
        }
    }
    /**
     * null represents IDLE, "" represents a request without an active interface, "something" represents
     * CONNECTED. "" also keeps the foreground slot while a stopped session tears down, or for a start Android has
     * accepted but not delivered yet; the binder reports IDLE for both, see [refreshConfiguration].
     */
    private fun updateIface(value: String?) {
        iface.value = value
        refreshConfiguration()
    }
    /** Publish the binder state, derived from the session: only a [live] one reports [iface] and its configuration. */
    private fun refreshConfiguration() {
        val currentIface = if (live) iface.value else null
        binder.update(currentIface, if (currentIface == null) null else configuration)
    }
    private val binder = Binder(this)

    /**
     * Collect one LOHS request until it completes: the reservation lives exactly as long as the collection (the flow's
     * unregister closes it), and so does the routing for its interface. This runs in the lifespan's own coroutine, and
     * cancelling it (a stop) does not cancel the request outright: the routing is torn down first, then [stopRequest]
     * ends the request, and this returns or throws only once the request has completed with its own cleanup done. A
     * request the framework reports stopped or failed ends by itself, so it is only collected until it completes.
     * [wifi] is the manager an app-process request uses, recorded as [requestWifi] once it starts.
     */
    private suspend fun collectLocalOnlyHotspotEvents(flow: Flow<LocalOnlyHotspotEvent>, wifi: WifiManager? = null,
                                                      stopRequest: suspend (Job) -> Unit = { it.cancel() }) {
        val self = currentCoroutineContext().job
        // a lifespan that has ended (say while a backend failed) must not open another request
        self.ensureActive()
        var failure: StartFailure? = null
        var ended = false
        var routing: RoutingManager? = null
        try {
            coroutineScope {
                var ifaceWait: Job? = null
                val request = async(NonCancellable) {
                    flow.collect { event ->
                        // an ending lifespan only drains the request until it completes
                        if (!self.isActive) return@collect
                        when (event) {
                            is LocalOnlyHotspotEvent.Started -> {
                                // a request that starts has registered, so a leftover of this instance would be on
                                // its manager
                                if (wifi != null) requestWifi = wifi
                                configuration = event.config
                                refreshConfiguration()
                                // attempt to update again
                                registerReceiver(null, IntentFilter(WifiApManager.WIFI_AP_STATE_CHANGED_ACTION))
                                    ?.let(this@LocalOnlyHotspotService::updateState)
                                val state = lastState
                                if (state?.first != WifiApManager.WIFI_AP_STATE_ENABLED) {
                                    failure = StartFailure(if (state?.first == WifiApManager.WIFI_AP_STATE_FAILED) {
                                        getString(R.string.tethering_temp_hotspot_failure,
                                            softApStartFailureLabel(this@LocalOnlyHotspotService, state.third))
                                    } else null)
                                    return@collect stop(self)
                                }
                                unregisterStateReceiver()
                                // Wait for the local-only hotspot interface to appear (the next states with a single
                                // local-only interface), then take over routing. Replaces the old
                                // onLocalOnlyInterfacesChanged callback; cancelled once the request ends.
                                if (ifaceWait == null) ifaceWait = this@coroutineScope.launch {
                                    val interfaceName =
                                        TetherStates.flow.mapNotNull { it.localOnly.singleOrNull() }.first()
                                    updateIface(interfaceName)
                                    BootReceiver.add<LocalOnlyHotspotService>(Starter())
                                    val manager = RoutingManager.LocalOnly(this@LocalOnlyHotspotService, interfaceName)
                                    routing = manager
                                    manager.start()
                                }
                            }
                            // the request flow completes right after this
                            is LocalOnlyHotspotEvent.Stopped -> {
                                ended = true
                                stop(self)
                            }
                            is LocalOnlyHotspotEvent.Failed -> {
                                ended = true
                                failure = StartFailure(getString(
                                    R.string.tethering_temp_hotspot_failure, when (event.reason) {
                                        WifiManager.LocalOnlyHotspotCallback.ERROR_NO_CHANNEL -> {
                                            getString(R.string.tethering_temp_hotspot_failure_no_channel)
                                        }
                                        WifiManager.LocalOnlyHotspotCallback.ERROR_GENERIC -> {
                                            getString(R.string.tethering_temp_hotspot_failure_generic)
                                        }
                                        WifiManager.LocalOnlyHotspotCallback.ERROR_INCOMPATIBLE_MODE -> {
                                            getString(R.string.tethering_temp_hotspot_failure_incompatible_mode)
                                        }
                                        WifiManager.LocalOnlyHotspotCallback.ERROR_TETHERING_DISALLOWED -> {
                                            getString(R.string.tethering_temp_hotspot_failure_tethering_disallowed)
                                        }
                                        else -> getString(R.string.failure_reason_unknown, event.reason)
                                    }))
                                stop(self)
                            }
                        }
                    }
                }
                try {
                    request.await()
                } finally {
                    withContext(NonCancellable) {
                        // a backend failing while the lifespan goes on (falling back to the next one) leaves a request
                        // without an active interface again; an ending lifespan has withdrawn its state in [stop]
                        if (self.isActive) {
                            configuration = null
                            updateIface("")
                        }
                        ifaceWait?.cancelAndJoin()
                        try {
                            routing?.stop()
                        } finally {
                            try {
                                // the framework ends a request it reported stopped or failed by itself
                                if (!ended) stopRequest(request)
                            } finally {
                                // Known issue, inherited from master and deferred: releasing the last request only
                                // queues its AP's shutdown, and the framework fails every request registered by the
                                // time that AP reports DISABLING or DISABLED, so a successor that starts right after
                                // this completes can be failed by it.
                                // https://android.googlesource.com/platform/packages/modules/Wifi/+/android-17.0.0_r1/service/java/com/android/server/wifi/WifiServiceImpl.java#3224
                                try {
                                    request.await()
                                } catch (_: CancellationException) {
                                    // cancelled above; any failure of its own still propagates
                                }
                            }
                        }
                    }
                }
            }
        } catch (e: CancellationException) {
            // a start failure stopped the lifespan: report it rather than the stop it caused
            throw failure ?: e
        }
    }

    override val coroutineContext = Dispatchers.Main.immediate + Job()
    /**
     * The job owning the current LOHS lifespan, or `null` when idle: the single source of truth for the service's
     * lifecycle. It is created, replaced and cleared only on the main thread; a lifespan winds the service down only
     * while it is still the job [session] points at, so a successor started during teardown takes over seamlessly.
     * A job not cancelled is [live], even while still waiting behind its predecessor; a cancelled one is ending, its
     * intent already withdrawn by [stop], and is kept only for a successor to join.
     */
    private var session: Job? = null
    /** Whether [session] is live rather than ending: the published intent is "on" exactly while this holds. */
    private val live get() = session?.isCancelled == false
    private var destroyed = false
    private var lastStartId = 0
    /**
     * The WifiManager of the latest LOHS request this instance made from its own process (the root backend's live in
     * the root process) that registered, as its Started event proves: a request it left outstanding is on that
     * manager, the only one able to cancel it, while a backend attempt that never registered leaves this alone.
     */
    private var requestWifi: WifiManager? = null
    /**
     * The boot-starter deletion [stop] launched last. Settling waits for it, so that the service is not let go while
     * the persisted starter could still bring back what the user stopped.
     */
    private var bootStarterDeletion: Job? = null

    /**
     * Stop [lifespan] if it is still the live session, withdrawing its intent before any of its teardown can suspend.
     * Cancelling it marks it ending, so the binder reports idle at once (the UI and the tile offer a start, not another
     * stop) and a start from here on is a successor that joins the teardown rather than a duplicate. [iface] keeps
     * the foreground slot with an empty notification until the teardown settles, and the boot starter goes, as no
     * successor exists yet to want it. A lifespan reports its own end through this as well, which does nothing once a
     * stop or a successor has taken over.
     */
    private fun stop(lifespan: Job? = session) {
        if (lifespan == null || lifespan !== session || lifespan.isCancelled) return
        configuration = null
        // Withdraw the active interface from the shared notification before the lifespan unwinds. Its LOHS
        // request cleanup can suspend behind framework callback teardown; leaving [iface] non-empty lets the
        // notification collector keep publishing "0 connected" for this interface, which masks any inactive
        // monitor entry for the same interface.
        updateIface("")
        // detached from the service scope, which onDestroy may end right after this lifespan settles
        bootStarterDeletion = launch(NonCancellable) { BootReceiver.delete<LocalOnlyHotspotService>() }
        // Cancel last: on the main thread, Dispatchers.Main.immediate can resume the lifespan inside cancel(), running
        // its teardown, even its settle, before this returns. The binder state derived after it then reports idle.
        lifespan.cancel()
        refreshConfiguration()
    }

    private var lastState: Triple<Int, String?, Int>? = null
    private val receiver = broadcastReceiver { _, intent -> updateState(intent) }
    private var receiverRegistered = false
    private fun updateState(intent: Intent) {
        // based on: https://android.googlesource.com/platform/packages/services/Car/+/21fa77d/service/src/com/android/car/CarProjectionService.java#193
        lastState = Triple(intent.wifiApState, intent.getStringExtra(WifiApManager.EXTRA_WIFI_AP_INTERFACE_NAME),
            intent.getIntExtra(WifiApManager.EXTRA_WIFI_AP_FAILURE_REASON, 0))
    }
    private fun unregisterStateReceiver() {
        if (!receiverRegistered) return
        receiverRegistered = false
        unregisterReceiver(receiver)
    }

    override fun onBind(intent: Intent?) = binder

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        launch { BootReceiver.startIfEnabled() }
        lastStartId = startId
        if (destroyed || live) return START_NOT_STICKY
        ServiceNotification.startForeground(this)   // show invisible foreground notification to avoid being killed
        val predecessor = session
        val job = launch(start = CoroutineStart.LAZY) {
            val self = coroutineContext.job
            try {
                predecessor?.join()
                runLifespan()
            } catch (_: CancellationException) {
                // a stop, possibly while still waiting behind the barrier: unwind into settle below
            } catch (e: StartFailure) {
                e.message?.let {
                    SmartSnackbar.make(it).show()
                    dismissIfApplicable()
                }
            } finally {
                // a lifespan ending on its own withdraws its intent as a stop would (a no-op once already ending)
                stop(self)
                withContext(NonCancellable) {
                    // a successor joining us must also wait out our whole teardown, so re-join a predecessor whose
                    // cancellable barrier above may have been aborted mid-wait (a no-op once already joined)
                    predecessor?.join()
                    if (session !== self) return@withContext
                    // still the session while waiting, so a start meanwhile becomes a successor that takes over
                    bootStarterDeletion?.join()
                    if (session !== self) return@withContext
                    session = null
                    // stopping by the latest start ID keeps a start delivered meanwhile from being torn down with us;
                    // one Android has accepted but not delivered yet also keeps the foreground slot rather than
                    // having to claim it anew
                    if (destroyed || stopSelfResult(lastStartId)) {
                        updateIface(null)
                        ServiceNotification.stopForeground(this@LocalOnlyHotspotService)
                        if (destroyed) this@LocalOnlyHotspotService.cancel()
                    } else updateIface("")
                }
            }
        }
        session = job
        updateIface("")
        job.start()
        return START_NOT_STICKY
    }

    /**
     * One LOHS lifespan: request a local-only hotspot through the first backend that works, route it while it lasts,
     * and always tear it back down. Cancellation at any suspension point (a stop cancelling this job) unwinds into the
     * same teardown. Exactly one runs at a time — the barrier serializes lifespans — so it owns the request, the
     * receiver and the routing outright without any locking of its own.
     */
    private suspend fun runLifespan() {
        registerReceiver(receiver, IntentFilter(WifiApManager.WIFI_AP_STATE_CHANGED_ACTION))?.let(this::updateState)
        receiverRegistered = true
        try {
            if (Build.VERSION.SDK_INT >= 30 && app.pref.getBoolean(KEY_USE_SYSTEM, false)) {
                if (Build.VERSION.SDK_INT >= 33) try {
                    val wifi = WifiApManager.newLocalOnlyHotspotManager()
                    return collectLocalOnlyHotspotEvents(WifiApManager.startLocalOnlyHotspotWithConfigurationFlow(
                        wifi, WifiApManager.configuration), wifi)
                } catch (e: NoSuchMethodError) {
                    if (Build.VERSION.SDK_INT >= 36) Timber.w(e)
                } catch (e: InvocationTargetException) {
                    if (e.targetException is SecurityException) Timber.d(e) else Timber.w(e)
                } catch (e: SecurityException) {
                    Timber.d(e)
                }
                try {
                    RootManager.use { server ->
                        // the root process ends its request itself, and its flow completes once that cleanup ran; a
                        // dead connection fails the flow instead, which surfaces once it is collected to the end
                        val token = SystemClock.elapsedRealtimeNanos()
                        collectLocalOnlyHotspotEvents(server.flow(WifiApCommands.StartLocalOnlyHotspot(token))) {
                            server.execute(WifiApCommands.StopLocalOnlyHotspot(token))
                        }
                    }
                    return
                } catch (e: CancellationException) {
                    throw e
                } catch (e: StartFailure) {
                    throw e
                } catch (e: Exception) {
                    Timber.w(e)
                    SmartSnackbar.make(e).show()
                }
            }
            try {
                val wifi = WifiApManager.newLocalOnlyHotspotManager()
                collectLocalOnlyHotspotEvents(WifiApManager.startLocalOnlyHotspotFlow(wifi), wifi)
            } catch (e: CancellationException) {
                throw e
            } catch (e: IllegalStateException) {
                // throws IllegalStateException if the caller attempts to start the LocalOnlyHotspot while they
                // have an outstanding request.
                // https://android.googlesource.com/platform/frameworks/opt/net/wifi/+/53e0284/service/java/com/android/server/wifi/WifiServiceImpl.java#1192
                // Only the WifiManager that made it can cancel it: one this instance left is on [requestWifi], while
                // one another service instance left stays until that instance cleans up.
                requestWifi?.let(WifiApManager::cancelLocalOnlyHotspotRequest)
                SmartSnackbar.make(e).show()
                dismissIfApplicable()
            } catch (e: SecurityException) {
                SmartSnackbar.make(e).show()
                dismissIfApplicable()
            }
        } finally {
            unregisterStateReceiver()
        }
    }

    override fun onDestroy() {
        destroyed = true
        binder.detach()
        // the service context drops its receivers right after onDestroy, before a suspended teardown could get to it
        unregisterStateReceiver()
        // Cancel the live lifespan; its settle drops the shared notification and ends the scope once its own teardown
        // finishes. With nothing running, end the scope here instead.
        val current = session
        if (current == null) cancel() else current.cancel()
        super.onDestroy()
    }

    override fun countsFlow(active: List<String>) = if (Build.VERSION.SDK_INT >= 33) {
        softApCountsFlow(active, WifiApCommands.localOnlyHotspotSoftApCallbackFlow())
    } else super.countsFlow(active)
}
