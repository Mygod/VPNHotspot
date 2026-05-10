package be.mygod.vpnhotspot

import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.net.wifi.WifiManager
import android.os.Build
import androidx.annotation.RequiresApi
import be.mygod.librootkotlinx.RootServer
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.NetlinkNeighbour
import be.mygod.vpnhotspot.net.TetherStates
import be.mygod.vpnhotspot.net.monitor.TetherTimeoutMonitor
import be.mygod.vpnhotspot.net.wifi.SoftApConfigurationCompat
import be.mygod.vpnhotspot.net.wifi.SoftApConfigurationCompat.Companion.toCompat
import be.mygod.vpnhotspot.net.wifi.WifiApManager
import be.mygod.vpnhotspot.net.wifi.WifiApManager.wifiApState
import be.mygod.vpnhotspot.root.LocalOnlyHotspotCallbacks
import be.mygod.vpnhotspot.root.RootManager
import be.mygod.vpnhotspot.root.WifiApCommands
import be.mygod.vpnhotspot.util.InPlaceExecutor
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.TileServiceDismissHandle
import be.mygod.vpnhotspot.util.broadcastReceiver
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.cancel
import kotlinx.coroutines.currentCoroutineContext
import kotlinx.coroutines.job
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock
import kotlinx.coroutines.withContext
import kotlinx.parcelize.Parcelize
import timber.log.Timber
import java.lang.reflect.InvocationTargetException
import java.util.concurrent.atomic.AtomicInteger

class LocalOnlyHotspotService : NetlinkNeighbourMonitoringService(), TetherStates.Callback {
    companion object {
        const val KEY_USE_SYSTEM = "service.tempHotspot.useSystem"

        var dismissHandle: TileServiceDismissHandle? = null
        private fun dismissIfApplicable() = dismissHandle?.run {
            get()?.dismiss()
            dismissHandle = null
        }
    }

    inner class Binder : android.os.Binder() {
        val iface = this@LocalOnlyHotspotService.iface.asStateFlow()
        val configuration = this@LocalOnlyHotspotService.configuration.asStateFlow()

        fun stop(shouldDisable: Boolean = true, exit: Boolean = false) {
            when (iface.value) {
                null -> if (!exit) return  // stopped
                "" -> WifiApManager.cancelLocalOnlyHotspotRequest()
            }
            reservation?.close()
            stopService(shouldDisable, exit)
        }
    }

    @Parcelize
    class Starter : BootReceiver.Startable {
        override fun start(context: Context) {
            context.startForegroundService(Intent(context, LocalOnlyHotspotService::class.java))
        }
    }

    interface Reservation : AutoCloseable {
        val configuration: SoftApConfigurationCompat?
        val generation: Int
    }
    class Framework(private val reservation: WifiManager.LocalOnlyHotspotReservation,
                    override val generation: Int) : Reservation {
        override val configuration get() = if (Build.VERSION.SDK_INT < 30) @Suppress("DEPRECATION") {
            reservation.wifiConfiguration?.toCompat()
        } else reservation.softApConfiguration.toCompat()
        override fun close() = reservation.close()
    }
    @RequiresApi(30)
    inner class Root(rootServer: RootServer, override val generation: Int) : Reservation {
        private val callbacks = rootServer.flow(WifiApCommands.StartLocalOnlyHotspot())
        private var job: Job? = null
        override var configuration: SoftApConfigurationCompat? = null
            private set
        override fun close() {
            job?.cancel()
        }

        suspend fun work() {
            job = currentCoroutineContext().job
            try {
                callbacks.collect { callback ->
                    when (callback) {
                        is LocalOnlyHotspotCallbacks.OnStarted -> {
                            configuration = callback.config.toCompat()
                            refreshConfiguration()
                            onFrameworkStarted(this)
                        }
                        is LocalOnlyHotspotCallbacks.OnStopped -> onFrameworkStopped(generation)
                        is LocalOnlyHotspotCallbacks.OnFailed -> onFrameworkFailed(callback.reason, generation)
                    }
                }
            } finally {
                job = null
            }
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
     * null represents IDLE, "" represents CONNECTING, "something" represents CONNECTED.
     */
    private fun updateIface(value: String?) {
        iface.value = value
        refreshConfiguration()
    }
    private fun refreshConfiguration() {
        configuration.value = if (iface.value == null) null else reservation?.configuration
    }
    private val binder = Binder()
    private fun lohCallback(generation: Int) = object : WifiManager.LocalOnlyHotspotCallback() {
        override fun onStarted(reservation: WifiManager.LocalOnlyHotspotReservation?) {
            if (reservation == null) onFailed(-2) else {
                val r = Framework(reservation, generation)
                this@LocalOnlyHotspotService.reservation = r
                launch { onFrameworkStarted(r) }
            }
        }
        override fun onStopped() = onFrameworkStopped(generation)
        override fun onFailed(reason: Int) = onFrameworkFailed(reason, generation)
    }
    private suspend fun onFrameworkStarted(reservation: Reservation) {
        val configuration = reservation.configuration
        if (reservation.generation != lifecycleGeneration.get()) {
            if (this@LocalOnlyHotspotService.reservation === reservation) {
                this@LocalOnlyHotspotService.reservation = null
            }
            reservation.close()
            return
        }
        // attempt to update again
        registerReceiver(null, IntentFilter(WifiApManager.WIFI_AP_STATE_CHANGED_ACTION))?.let(this::updateState)
        val state = lastState
        if (state?.first != WifiApManager.WIFI_AP_STATE_ENABLED) {
            if (state?.first == WifiApManager.WIFI_AP_STATE_FAILED) {
                SmartSnackbar.make(getString(R.string.tethering_temp_hotspot_failure,
                    WifiApManager.failureReasonLookup(state.third))).show()
                dismissIfApplicable()
            }
            return stopService(generation = reservation.generation)
        }
        var closeReservation = false
        routingMutex.withLock {
            if (reservation.generation != lifecycleGeneration.get() ||
                    iface.value == null) {
                if (this@LocalOnlyHotspotService.reservation === reservation) {
                    this@LocalOnlyHotspotService.reservation = null
                }
                closeReservation = true
                return@withLock
            }
            if (iface.value != "") return@withLock
            unregisterStateReceiver()
            if (Build.VERSION.SDK_INT < 30 && configuration!!.isAutoShutdownEnabled) {
                timeoutMonitor = TetherTimeoutMonitor(configuration.shutdownTimeoutMillis, coroutineContext) {
                    reservation.close()
                }
            }
            waitingForIface = true
            withContext(Dispatchers.Main.immediate) {
                TetherStates.registerCallback(this@LocalOnlyHotspotService)
            }
        }
        if (closeReservation) reservation.close()
    }

    override fun onLocalOnlyInterfacesChanged(interfaces: List<String?>) {
        val interfaceName = interfaces.singleOrNull() ?: return
        launch {
            routingMutex.withLock {
                if (!waitingForIface || iface.value != "") return@withLock
                waitingForIface = false
                onIfaceAvailable(interfaceName)
            }
        }
    }

    private suspend fun onIfaceAvailable(interfaceName: String) {
        withContext(Dispatchers.Main.immediate) {
            TetherStates.unregisterCallback(this@LocalOnlyHotspotService)
        }
        updateIface(interfaceName)
        BootReceiver.add<LocalOnlyHotspotService>(Starter())
        check(routingManager == null)
        val manager = RoutingManager.LocalOnly(this, interfaceName)
        routingManager = manager
        manager.start()
        if (routingManager === manager && iface.value == interfaceName) startNetlinkNeighbours()
    }
    private fun onFrameworkStopped(generation: Int) {
        if (reservation?.generation == generation) reservation = null
        if (generation != lifecycleGeneration.get()) return
        if (iface.value != null) stopService(generation = generation)
    }
    private fun onFrameworkFailed(reason: Int, generation: Int) {
        if (generation != lifecycleGeneration.get()) return
        SmartSnackbar.make(getString(R.string.tethering_temp_hotspot_failure, when (reason) {
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
            else -> getString(R.string.failure_reason_unknown, reason)
        })).show()
        dismissIfApplicable()
        stopService(generation = generation)
    }

    /**
     * Writes and critical reads to routingManager should be protected with this context.
     */
    private val dispatcher = Dispatchers.Default.limitedParallelism(1, "LocalOnlyHotspotService")
    override val coroutineContext = dispatcher + Job()
    private val routingMutex = Mutex()
    private var routingManager: RoutingManager? = null
    private var timeoutMonitor: TetherTimeoutMonitor? = null
    private val lifecycleGeneration = AtomicInteger()
    private var waitingForIface = false
    override val activeIfaces get() = iface.value.let { if (it.isNullOrEmpty()) emptyList() else listOf(it) }

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
        BootReceiver.startIfEnabled()
        if (iface.value != null) return START_STICKY
        val generation = lifecycleGeneration.incrementAndGet()
        updateIface("")
        ServiceNotification.startForeground(this)   // show invisible foreground notification to avoid being killed
        launch(start = CoroutineStart.UNDISPATCHED) { doStart(generation) }
        return START_STICKY
    }
    private suspend fun doStart(generation: Int) {
        if (!receiverRegistered) {
            receiverRegistered = true
            registerReceiver(receiver, IntentFilter(WifiApManager.WIFI_AP_STATE_CHANGED_ACTION))?.let(this::updateState)
        }
        if (Build.VERSION.SDK_INT >= 30 && app.pref.getBoolean(KEY_USE_SYSTEM, false)) {
            if (Build.VERSION.SDK_INT >= 33) try {
                return Services.wifi.startLocalOnlyHotspotWithConfiguration(WifiApManager.configuration,
                    InPlaceExecutor, lohCallback(generation))
            } catch (e: NoSuchMethodError) {
                if (Build.VERSION.SDK_INT >= 36) Timber.w(e)
            } catch (e: SecurityException) {
                Timber.d(e)
            } catch (e: InvocationTargetException) {
                if (e.targetException !is SecurityException) Timber.w(e)
            }
            try {
                RootManager.use {
                    Root(it, generation).also { root ->
                        reservation = root
                        root.work()
                    }
                }
                return
            } catch (_: CancellationException) {
                return
            } catch (e: Exception) {
                if (generation != lifecycleGeneration.get()) return
                Timber.w(e)
                SmartSnackbar.make(e).show()
            } finally {
                if (reservation?.generation == generation) reservation = null
            }
        }
        try {
            Services.wifi.startLocalOnlyHotspot(lohCallback(generation), null)
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

    override fun onNetlinkNeighboursChanged(neighbours: Collection<NetlinkNeighbour>) {
        super.onNetlinkNeighboursChanged(neighbours)
        timeoutMonitor?.onClientsChanged(neighbours.none { it.validIpv4ClientMac != null })
    }

    override fun onDestroy() {
        binder.stop(false, true)
        super.onDestroy()
    }

    private fun stopService(shouldDisable: Boolean = true, exit: Boolean = false,
            generation: Int = lifecycleGeneration.get()) {
        launch(start = CoroutineStart.UNDISPATCHED) {
            routingMutex.withLock {
                if (!exit && generation != lifecycleGeneration.get()) return@withLock
                if (shouldDisable) BootReceiver.delete<LocalOnlyHotspotService>()
                updateIface(null)
                waitingForIface = false
                withContext(Dispatchers.Main.immediate) {
                    TetherStates.unregisterCallback(this@LocalOnlyHotspotService)
                }
                stopNetlinkNeighbours()
                timeoutMonitor?.close()
                timeoutMonitor = null
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
