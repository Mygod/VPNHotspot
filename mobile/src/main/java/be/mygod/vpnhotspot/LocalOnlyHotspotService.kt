package be.mygod.vpnhotspot

import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.net.wifi.WifiManager
import android.os.Build
import androidx.annotation.RequiresApi
import be.mygod.librootkotlinx.RootServer
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.IpNeighbour
import be.mygod.vpnhotspot.net.monitor.IpNeighbourMonitor
import be.mygod.vpnhotspot.net.monitor.TetherTimeoutMonitor
import be.mygod.vpnhotspot.net.wifi.SoftApConfigurationCompat
import be.mygod.vpnhotspot.net.wifi.SoftApConfigurationCompat.Companion.toCompat
import be.mygod.vpnhotspot.net.wifi.WifiApManager
import be.mygod.vpnhotspot.net.wifi.WifiApManager.wifiApState
import be.mygod.vpnhotspot.root.LocalOnlyHotspotCallbacks
import be.mygod.vpnhotspot.root.RootManager
import be.mygod.vpnhotspot.root.WifiApCommands
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.StickyEvent1
import be.mygod.vpnhotspot.util.TileServiceDismissHandle
import be.mygod.vpnhotspot.util.broadcastReceiver
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.cancel
import kotlinx.coroutines.launch
import kotlinx.parcelize.Parcelize
import timber.log.Timber
import java.net.Inet4Address

class LocalOnlyHotspotService : IpNeighbourMonitoringService(), CoroutineScope {
    companion object {
        const val KEY_USE_SYSTEM = "service.tempHotspot.useSystem"

        var dismissHandle: TileServiceDismissHandle? = null
        private fun dismissIfApplicable() = dismissHandle?.run {
            get()?.dismiss()
            dismissHandle = null
        }
    }

    inner class Binder : android.os.Binder() {
        /**
         * null represents IDLE, "" represents CONNECTING, "something" represents CONNECTED.
         */
        var iface: String? = null
            set(value) {
                field = value
                ifaceChanged(value)
            }
        val ifaceChanged = StickyEvent1 { iface }
        val configuration get() = reservation?.configuration

        fun stop() {
            when (iface) {
                null -> return  // stopped
                "" -> WifiApManager.cancelLocalOnlyHotspotRequest()
            }
            reservation?.close()
            stopService()
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
    }
    class Framework(private val reservation: WifiManager.LocalOnlyHotspotReservation) : Reservation {
        override val configuration get() = if (Build.VERSION.SDK_INT < 30) @Suppress("DEPRECATION") {
            reservation.wifiConfiguration?.toCompat()
        } else reservation.softApConfiguration.toCompat()
        override fun close() = reservation.close()
    }
    @RequiresApi(30)
    inner class Root(rootServer: RootServer) : Reservation {
        private val channel = rootServer.create(WifiApCommands.StartLocalOnlyHotspot(), this@LocalOnlyHotspotService)
        override var configuration: SoftApConfigurationCompat? = null
            private set
        override fun close() = channel.cancel()

        suspend fun work() {
            for (callback in channel) when (callback) {
                is LocalOnlyHotspotCallbacks.OnStarted -> {
                    configuration = callback.config.toCompat()
                    onFrameworkStarted(this)
                }
                is LocalOnlyHotspotCallbacks.OnStopped -> reservation = null
                is LocalOnlyHotspotCallbacks.OnFailed -> onFrameworkFailed(callback.reason)
            }
        }
    }

    private val binder = Binder()
    private var reservation: Reservation? = null
    private val lohCallback = object : WifiManager.LocalOnlyHotspotCallback() {
        override fun onStarted(reservation: WifiManager.LocalOnlyHotspotReservation?) {
            if (reservation == null) onFailed(-2) else {
                val r = Framework(reservation)
                this@LocalOnlyHotspotService.reservation = r
                launch { onFrameworkStarted(r) }
            }
        }
        override fun onStopped() {
            reservation = null
        }
        override fun onFailed(reason: Int) = onFrameworkFailed(reason)
    }
    private fun onFrameworkStarted(reservation: Reservation) {
        val configuration = reservation.configuration
        if (Build.VERSION.SDK_INT < 30 && configuration!!.isAutoShutdownEnabled) {
            timeoutMonitor = TetherTimeoutMonitor(configuration.shutdownTimeoutMillis, coroutineContext) {
                reservation.close()
            }
        }
        // attempt to update again
        registerReceiver(null, IntentFilter(WifiApManager.WIFI_AP_STATE_CHANGED_ACTION))?.let(this::updateState)
        val state = lastState
        unregisterStateReceiver()
        checkNotNull(state) { "Failed to obtain latest AP state" }
        val iface = state.second
        if (state.first != WifiApManager.WIFI_AP_STATE_ENABLED || iface.isNullOrEmpty()) {
            if (state.first == WifiApManager.WIFI_AP_STATE_FAILED) {
                SmartSnackbar.make(getString(R.string.tethering_temp_hotspot_failure,
                    WifiApManager.failureReasonLookup(state.third))).show()
                dismissIfApplicable()
            }
            return stopService()
        }
        binder.iface = iface
        BootReceiver.add<LocalOnlyHotspotService>(Starter())
        check(routingManager == null)
        routingManager = RoutingManager.LocalOnly(this, iface).apply { start() }
        IpNeighbourMonitor.registerCallback(this)
    }
    private fun onFrameworkFailed(reason: Int) {
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
        stopService()
    }

    /**
     * Writes and critical reads to routingManager should be protected with this context.
     */
    private val dispatcher = Dispatchers.IO.limitedParallelism(1)
    override val coroutineContext = dispatcher + Job()
    private var routingManager: RoutingManager? = null
    private var timeoutMonitor: TetherTimeoutMonitor? = null
    override val activeIfaces get() = binder.iface.let { if (it.isNullOrEmpty()) emptyList() else listOf(it) }

    private var lastState: Triple<Int, String?, Int>? = null
    private val receiver = broadcastReceiver { _, intent -> updateState(intent) }
    private var receiverRegistered = false
    private fun updateState(intent: Intent) {
        // based on: https://android.googlesource.com/platform/packages/services/Car/+/72c71d2/service/src/com/android/car/CarProjectionService.java#160
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
        if (binder.iface != null) return START_STICKY
        binder.iface = ""
        updateNotification()    // show invisible foreground notification to avoid being killed
        launch(start = CoroutineStart.UNDISPATCHED) { doStart() }
        return START_STICKY
    }
    private suspend fun doStart() {
        if (!receiverRegistered) {
            receiverRegistered = true
            registerReceiver(receiver, IntentFilter(WifiApManager.WIFI_AP_STATE_CHANGED_ACTION))
                ?.let(this@LocalOnlyHotspotService::updateState)
        }
        if (Build.VERSION.SDK_INT >= 30 && app.pref.getBoolean(KEY_USE_SYSTEM, false)) try {
            RootManager.use {
                Root(it).apply {
                    reservation = this
                    work()
                }
            }
            return
        } catch (_: CancellationException) {
            return
        } catch (e: Exception) {
            Timber.w(e)
            SmartSnackbar.make(e).show()
        } finally {
            reservation = null
        }
        try {
            Services.wifi.startLocalOnlyHotspot(lohCallback, null)
        } catch (e: IllegalStateException) {
            // throws IllegalStateException if the caller attempts to start the LocalOnlyHotspot while they
            // have an outstanding request.
            // https://android.googlesource.com/platform/frameworks/opt/net/wifi/+/53e0284/service/java/com/android/server/wifi/WifiServiceImpl.java#1192
            WifiApManager.cancelLocalOnlyHotspotRequest()
            SmartSnackbar.make(e).show()
            dismissIfApplicable()
            stopService()
        } catch (e: SecurityException) {
            SmartSnackbar.make(e).show()
            dismissIfApplicable()
            stopService()
        }
    }

    override fun onIpNeighbourAvailable(neighbours: Collection<IpNeighbour>) {
        super.onIpNeighbourAvailable(neighbours)
        timeoutMonitor?.onClientsChanged(neighbours.none {
            it.ip is Inet4Address && it.state == IpNeighbour.State.VALID
        })
    }

    override fun onDestroy() {
        binder.stop()
        unregisterReceiver(true)
        super.onDestroy()
    }

    private fun stopService() {
        BootReceiver.delete<LocalOnlyHotspotService>()
        binder.iface = null
        unregisterReceiver()
        ServiceNotification.stopForeground(this)
        stopSelf()
    }

    private fun unregisterReceiver(exit: Boolean = false) {
        IpNeighbourMonitor.unregisterCallback(this)
        timeoutMonitor?.close()
        timeoutMonitor = null
        launch {
            routingManager?.stop()
            routingManager = null
            unregisterStateReceiver()
            if (exit) cancel()
        }
    }
}
