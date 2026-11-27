package be.mygod.vpnhotspot

import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.net.wifi.WifiManager
import android.os.Build
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
import kotlinx.coroutines.cancel
import kotlinx.coroutines.coroutineScope
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
            val service = service ?: return
            // Withdraw the active interface from the shared notification before the lifespan unwinds. Its LOHS
            // request cleanup can suspend behind framework callback teardown; leaving [iface] non-empty lets the
            // notification collector keep publishing "0 connected" for this interface, which masks any inactive
            // monitor entry for the same interface.
            if (service.iface.value?.isNotEmpty() == true) service.updateIface("")
            service.session?.cancel()
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
     * CONNECTED.
     */
    private fun updateIface(value: String?) {
        iface.value = value
        refreshConfiguration()
    }
    private fun refreshConfiguration() {
        val currentIface = iface.value
        binder.update(currentIface, if (currentIface == null) null else configuration)
    }
    private val binder = Binder(this)

    /**
     * Collect one LOHS request: the reservation lives exactly as long as the collection (the flow's unregister closes
     * it), and so does the routing for its interface, which is torn down before this returns or throws.
     */
    private suspend fun collectLocalOnlyHotspotEvents(flow: Flow<LocalOnlyHotspotEvent>) {
        var routing: RoutingManager? = null
        try {
            coroutineScope {
                var ifaceWait: Job? = null
                flow.collect { event ->
                    when (event) {
                        is LocalOnlyHotspotEvent.Started -> {
                            configuration = event.config
                            refreshConfiguration()
                            // attempt to update again
                            registerReceiver(null, IntentFilter(WifiApManager.WIFI_AP_STATE_CHANGED_ACTION))
                                ?.let(this@LocalOnlyHotspotService::updateState)
                            val state = lastState
                            if (state?.first != WifiApManager.WIFI_AP_STATE_ENABLED) throw StartFailure(
                                if (state?.first == WifiApManager.WIFI_AP_STATE_FAILED) {
                                    getString(R.string.tethering_temp_hotspot_failure,
                                        softApStartFailureLabel(this@LocalOnlyHotspotService, state.third))
                                } else null)
                            unregisterStateReceiver()
                            // Wait for the local-only hotspot interface to appear (the next states with a single
                            // local-only interface), then take over routing. Replaces the old
                            // onLocalOnlyInterfacesChanged callback; cancelled once the request ends.
                            if (ifaceWait == null) ifaceWait = launch {
                                val interfaceName = TetherStates.flow.mapNotNull { it.localOnly.singleOrNull() }.first()
                                updateIface(interfaceName)
                                BootReceiver.add<LocalOnlyHotspotService>(Starter())
                                val manager = RoutingManager.LocalOnly(this@LocalOnlyHotspotService, interfaceName)
                                routing = manager
                                manager.start()
                            }
                        }
                        // the request flow completes right after this
                        is LocalOnlyHotspotEvent.Stopped -> { }
                        is LocalOnlyHotspotEvent.Failed -> throw StartFailure(getString(
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
                    }
                }
                ifaceWait?.cancel()
            }
        } finally {
            configuration = null
            updateIface("")
            routing?.let { withContext(NonCancellable) { it.stop() } }
        }
    }

    override val coroutineContext = Dispatchers.Main.immediate + Job()
    /**
     * The job owning the current LOHS lifespan, or `null` when idle: the single source of truth for the service's
     * lifecycle. It is created, replaced and cleared only on the main thread; a lifespan winds the service down only
     * while it is still the job [session] points at, so a successor started during teardown takes over seamlessly.
     */
    private var session: Job? = null
    private var destroyed = false
    private var lastStartId = 0

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
        if (destroyed || session?.isActive == true) return START_NOT_STICKY
        updateIface("")
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
            }
            withContext(NonCancellable) {
                // a successor joining us must also wait out our whole teardown, so re-join a predecessor whose
                // cancellable barrier above may have been aborted mid-wait (a no-op once already joined)
                predecessor?.join()
                if (session !== self) return@withContext
                if (!destroyed) {
                    BootReceiver.delete<LocalOnlyHotspotService>()
                    if (session !== self) return@withContext
                }
                session = null
                updateIface(null)
                ServiceNotification.stopForeground(this@LocalOnlyHotspotService)
                // stopping by the latest start ID keeps a start delivered meanwhile from being torn down with us
                if (destroyed) this@LocalOnlyHotspotService.cancel() else stopSelf(lastStartId)
            }
        }
        session = job
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
                    return collectLocalOnlyHotspotEvents(
                        WifiApManager.startLocalOnlyHotspotWithConfigurationFlow(WifiApManager.configuration))
                } catch (e: NoSuchMethodError) {
                    if (Build.VERSION.SDK_INT >= 36) Timber.w(e)
                } catch (e: InvocationTargetException) {
                    if (e.targetException is SecurityException) Timber.d(e) else Timber.w(e)
                } catch (e: SecurityException) {
                    Timber.d(e)
                }
                try {
                    RootManager.use {
                        collectLocalOnlyHotspotEvents(it.flow(WifiApCommands.StartLocalOnlyHotspot()))
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
                collectLocalOnlyHotspotEvents(WifiApManager.startLocalOnlyHotspotFlow())
            } catch (e: CancellationException) {
                throw e
            } catch (e: IllegalStateException) {
                // throws IllegalStateException if the caller attempts to start the LocalOnlyHotspot while they
                // have an outstanding request.
                // https://android.googlesource.com/platform/frameworks/opt/net/wifi/+/53e0284/service/java/com/android/server/wifi/WifiServiceImpl.java#1192
                WifiApManager.cancelLocalOnlyHotspotRequest()
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
