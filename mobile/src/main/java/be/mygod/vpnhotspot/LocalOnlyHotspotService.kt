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
import kotlinx.coroutines.cancel
import kotlinx.coroutines.job
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.flow.mapNotNull
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.parcelize.Parcelize
import timber.log.Timber
import java.lang.reflect.InvocationTargetException
import java.util.concurrent.atomic.AtomicInteger

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
        @Volatile
        private var service: LocalOnlyHotspotService? = owner
        val iface = owner.iface.asStateFlow()
        val configuration = owner.configuration.asStateFlow()

        fun detach() {
            service = null
        }

        fun stop(shouldDisable: Boolean = true, exit: Boolean = false) {
            val service = service ?: return
            when (iface.value) {
                null -> if (!exit) return  // stopped
                "" -> service.localOnlyHotspotJob?.cancel()
            }
            service.reservation?.close()
            service.stopService(shouldDisable, exit)
        }
    }

    @Parcelize
    class Starter : BootReceiver.Startable {
        override fun start(context: Context) {
            context.startForegroundService(Intent(context, LocalOnlyHotspotService::class.java))
        }
    }

    private class Reservation(
        val configuration: SoftApConfigurationCompat?,
        val generation: Int,
        private val job: Job,
    ) : AutoCloseable {
        override fun close() {
            job.cancel()
        }
    }

    private var reservation: Reservation? = null
        set(value) {
            field = value
            refreshConfiguration()
        }
    private val iface = MutableStateFlow<String?>(null)
    private val configuration = MutableStateFlow<SoftApConfigurationCompat?>(null)
    /**
     * Drives the foreground notification off [iface]: counts while connected ("something"), an empty
     * notification while connecting (""), nothing (no foreground) while idle (null).
     */
    override val interfaces = iface.map { value ->
        when {
            value == null -> null
            value.isEmpty() -> Interfaces()
            else -> Interfaces(active = listOf(value))
        }
    }
    /**
     * null represents IDLE, "" represents CONNECTING, "something" represents CONNECTED.
     */
    private fun updateIface(value: String?) {
        iface.value = value
        refreshConfiguration()
    }
    private fun refreshConfiguration() {
        configuration.value = if (iface.value == null) null else reservation?.configuration
    }
    private val binder = Binder(this)
    private suspend fun collectLocalOnlyHotspotEvents(
        flow: Flow<LocalOnlyHotspotEvent>,
        generation: Int,
        requestJob: Job,
    ) = flow.collect { event ->
        when (event) {
            is LocalOnlyHotspotEvent.Started -> {
                val reservation = Reservation(event.config, generation, requestJob)
                this@LocalOnlyHotspotService.reservation = reservation

                if (reservation.generation != lifecycleGeneration.get()) {
                    if (this@LocalOnlyHotspotService.reservation === reservation) {
                        this@LocalOnlyHotspotService.reservation = null
                    }
                    reservation.close()
                    return@collect
                }
                // attempt to update again
                registerReceiver(null, IntentFilter(WifiApManager.WIFI_AP_STATE_CHANGED_ACTION))
                    ?.let(this::updateState)
                val state = lastState
                if (state?.first != WifiApManager.WIFI_AP_STATE_ENABLED) {
                    if (state?.first == WifiApManager.WIFI_AP_STATE_FAILED) {
                        SmartSnackbar.make(getString(R.string.tethering_temp_hotspot_failure,
                            softApStartFailureLabel(this, state.third))).show()
                        dismissIfApplicable()
                    }
                    reservation.close()
                    stopService(generation = reservation.generation)
                    return@collect
                }
                var closeReservation = false
                routingMutex.withLock {
                    if (reservation.generation != lifecycleGeneration.get() || iface.value == null) {
                        if (this@LocalOnlyHotspotService.reservation === reservation) {
                            this@LocalOnlyHotspotService.reservation = null
                        }
                        closeReservation = true
                        return@withLock
                    }
                    if (iface.value != "") return@withLock
                    unregisterStateReceiver()
                    ifaceWaitJob?.cancel()
                    ifaceWaitJob = launch { awaitLocalOnlyIface() }
                }
                if (closeReservation) reservation.close()
            }
            is LocalOnlyHotspotEvent.Stopped -> {
                if (reservation?.generation == generation) reservation = null
                if (generation != lifecycleGeneration.get()) return@collect
                if (iface.value != null) stopService(generation = generation)
            }
            is LocalOnlyHotspotEvent.Failed -> {
                if (generation != lifecycleGeneration.get()) return@collect
                SmartSnackbar.make(getString(R.string.tethering_temp_hotspot_failure, when (event.reason) {
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
                })).show()
                dismissIfApplicable()
                stopService(generation = generation)
            }
        }
    }

    /**
     * Wait for the local-only hotspot interface to appear (the next states with a single local-only
     * interface), then take over routing. Replaces the old onLocalOnlyInterfacesChanged callback;
     * cancelled by [stopService] / a re-entered wait.
     */
    private suspend fun awaitLocalOnlyIface() {
        val interfaceName = TetherStates.flow.mapNotNull { it.localOnly.singleOrNull() }.first()
        routingMutex.withLock {
            if (iface.value != "") return
            updateIface(interfaceName)
            BootReceiver.add<LocalOnlyHotspotService>(Starter())
            check(routingManager == null)
            val manager = RoutingManager.LocalOnly(this@LocalOnlyHotspotService, interfaceName)
            routingManager = manager
            manager.start()
        }
    }

    /**
     * Writes and critical reads to routingManager should be protected with this context.
     */
    private val dispatcher = Dispatchers.Default.limitedParallelism(1, "LocalOnlyHotspotService")
    override val coroutineContext = dispatcher + Job()
    private val routingMutex = Mutex()
    private var routingManager: RoutingManager? = null
    private var localOnlyHotspotJob: Job? = null
    private val lifecycleGeneration = AtomicInteger()
    private var ifaceWaitJob: Job? = null

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
        if (iface.value != null) return START_STICKY
        val generation = lifecycleGeneration.incrementAndGet()
        updateIface("")
        ServiceNotification.startForeground(this)   // show invisible foreground notification to avoid being killed
        launch(start = CoroutineStart.UNDISPATCHED) {
            val requestJob = coroutineContext.job
            localOnlyHotspotJob = requestJob
            try {
                doStart(generation, requestJob)
            } finally {
                if (localOnlyHotspotJob === requestJob) localOnlyHotspotJob = null
            }
        }
        return START_STICKY
    }
    private suspend fun doStart(generation: Int, requestJob: Job) {
        if (!receiverRegistered) {
            receiverRegistered = true
            registerReceiver(receiver, IntentFilter(WifiApManager.WIFI_AP_STATE_CHANGED_ACTION))?.let(this::updateState)
        }
        if (Build.VERSION.SDK_INT >= 30 && app.pref.getBoolean(KEY_USE_SYSTEM, false)) {
            if (Build.VERSION.SDK_INT >= 33) try {
                return collectLocalOnlyHotspotEvents(
                    WifiApManager.startLocalOnlyHotspotWithConfigurationFlow(WifiApManager.configuration),
                    generation,
                    requestJob,
                )
            } catch (e: NoSuchMethodError) {
                if (Build.VERSION.SDK_INT >= 36) Timber.w(e)
            } catch (e: InvocationTargetException) {
                if (e.targetException is SecurityException) Timber.d(e) else Timber.w(e)
            } catch (e: SecurityException) {
                Timber.d(e)
            }
            try {
                RootManager.use {
                    collectLocalOnlyHotspotEvents(
                        it.flow(WifiApCommands.StartLocalOnlyHotspot()),
                        generation,
                        requestJob,
                    )
                }
                return
            } catch (e: CancellationException) {
                throw e
            } catch (e: Exception) {
                if (generation != lifecycleGeneration.get()) return
                Timber.w(e)
                SmartSnackbar.make(e).show()
            } finally {
                if (reservation?.generation == generation) reservation = null
            }
        }
        try {
            collectLocalOnlyHotspotEvents(WifiApManager.startLocalOnlyHotspotFlow(), generation, requestJob)
        } catch (e: IllegalStateException) {
            if (generation != lifecycleGeneration.get()) return
            // throws IllegalStateException if the caller attempts to start the LocalOnlyHotspot while they
            // have an outstanding request.
            // https://android.googlesource.com/platform/frameworks/opt/net/wifi/+/53e0284/service/java/com/android/server/wifi/WifiServiceImpl.java#1192
            WifiApManager.cancelLocalOnlyHotspotRequest()
            SmartSnackbar.make(e).show()
            dismissIfApplicable()
            stopService(generation = generation)
        } catch (e: SecurityException) {
            if (generation != lifecycleGeneration.get()) return
            SmartSnackbar.make(e).show()
            dismissIfApplicable()
            stopService(generation = generation)
        }
    }

    override fun onDestroy() {
        binder.stop(false, true)
        binder.detach()
        super.onDestroy()
    }

    private fun stopService(shouldDisable: Boolean = true, exit: Boolean = false,
            generation: Int = lifecycleGeneration.get()) {
        val requestJob = localOnlyHotspotJob
        launch(start = CoroutineStart.UNDISPATCHED) {
            if (!exit && generation != lifecycleGeneration.get()) return@launch
            requestJob?.join()
            routingMutex.withLock {
                if (!exit && generation != lifecycleGeneration.get()) return@withLock
                if (shouldDisable) BootReceiver.delete<LocalOnlyHotspotService>()
                updateIface(null)
                ifaceWaitJob?.cancel()
                val manager = routingManager
                manager?.stop()
                if (routingManager === manager) routingManager = null
                if (!exit && generation != lifecycleGeneration.get()) return@withLock
                unregisterStateReceiver()
                ServiceNotification.stopForeground(this@LocalOnlyHotspotService)
                stopSelf()
            }
            if (exit) cancel()
        }
    }
}
