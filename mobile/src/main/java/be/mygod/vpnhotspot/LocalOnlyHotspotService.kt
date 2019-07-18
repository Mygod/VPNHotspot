package be.mygod.vpnhotspot

import android.content.Intent
import android.content.IntentFilter
import android.content.res.Configuration
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Handler
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.IpNeighbour
import be.mygod.vpnhotspot.net.TetheringManager
import be.mygod.vpnhotspot.net.TetheringManager.localOnlyTetheredIfaces
import be.mygod.vpnhotspot.net.monitor.IpNeighbourMonitor
import be.mygod.vpnhotspot.net.monitor.TetherTimeoutMonitor
import be.mygod.vpnhotspot.util.StickyEvent1
import be.mygod.vpnhotspot.util.broadcastReceiver
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.*
import timber.log.Timber

@RequiresApi(26)
class LocalOnlyHotspotService : IpNeighbourMonitoringService(), CoroutineScope {
    companion object {
        private const val TAG = "LocalOnlyHotspotService"
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

        val configuration get() = reservation?.wifiConfiguration

        fun stop() = reservation?.close()
    }

    private val binder = Binder()
    private var reservation: WifiManager.LocalOnlyHotspotReservation? = null
    /**
     * Writes and critical reads to routingManager should be protected with this context.
     */
    override val coroutineContext = newSingleThreadContext("LocalOnlyHotspotService") + Job()
    private var routingManager: RoutingManager? = null
    private val handler = Handler()
    @RequiresApi(28)
    private var timeoutMonitor: TetherTimeoutMonitor? = null
    private var receiverRegistered = false
    private val receiver = broadcastReceiver { _, intent ->
        val ifaces = intent.localOnlyTetheredIfaces ?: return@broadcastReceiver
        DebugHelper.log(TAG, "onTetherStateChangedLocked: $ifaces")
        check(ifaces.size <= 1)
        val iface = ifaces.singleOrNull()
        binder.iface = iface
        if (iface.isNullOrEmpty()) {
            unregisterReceiver()
            ServiceNotification.stopForeground(this)
            stopSelf()
        } else launch {
            val routingManager = routingManager
            if (routingManager == null) {
                this@LocalOnlyHotspotService.routingManager = RoutingManager.LocalOnly(this, iface).apply { start() }
                IpNeighbourMonitor.registerCallback(this@LocalOnlyHotspotService)
            } else check(iface == routingManager.downstream)
        }
    }
    override val activeIfaces get() = listOfNotNull(binder.iface)

    override fun onBind(intent: Intent?) = binder

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (binder.iface != null) return START_STICKY
        binder.iface = ""
        // show invisible foreground notification on television to avoid being killed
        if (app.uiMode.currentModeType == Configuration.UI_MODE_TYPE_TELEVISION) updateNotification()
        // throws IllegalStateException if the caller attempts to start the LocalOnlyHotspot while they
        // have an outstanding request.
        // https://android.googlesource.com/platform/frameworks/opt/net/wifi/+/53e0284/service/java/com/android/server/wifi/WifiServiceImpl.java#1192
        try {
            app.wifi.startLocalOnlyHotspot(object : WifiManager.LocalOnlyHotspotCallback() {
                override fun onStarted(reservation: WifiManager.LocalOnlyHotspotReservation?) {
                    if (reservation == null) onFailed(-2) else {
                        this@LocalOnlyHotspotService.reservation = reservation
                        if (!receiverRegistered) {
                            if (Build.VERSION.SDK_INT >= 28) timeoutMonitor = TetherTimeoutMonitor(
                                    this@LocalOnlyHotspotService, handler, reservation::close)
                            registerReceiver(receiver, IntentFilter(TetheringManager.ACTION_TETHER_STATE_CHANGED))
                            receiverRegistered = true
                        }
                    }
                }

                override fun onStopped() {
                    DebugHelper.log(TAG, "LOHCallback.onStopped")
                    reservation = null
                }

                override fun onFailed(reason: Int) {
                    SmartSnackbar.make(getString(R.string.tethering_temp_hotspot_failure, when (reason) {
                        ERROR_NO_CHANNEL -> getString(R.string.tethering_temp_hotspot_failure_no_channel)
                        ERROR_GENERIC -> getString(R.string.tethering_temp_hotspot_failure_generic)
                        ERROR_INCOMPATIBLE_MODE -> getString(R.string.tethering_temp_hotspot_failure_incompatible_mode)
                        ERROR_TETHERING_DISALLOWED -> {
                            getString(R.string.tethering_temp_hotspot_failure_tethering_disallowed)
                        }
                        else -> getString(R.string.failure_reason_unknown, reason)
                    })).show()
                    startFailure()
                }
            }, null)
        } catch (e: IllegalStateException) {
            Timber.w(e)
            SmartSnackbar.make(e).show()
            startFailure()
        } catch (e: SecurityException) {
            SmartSnackbar.make(e).show()
            startFailure()
        }
        return START_STICKY
    }

    private fun startFailure() {
        binder.iface = null
        updateNotification()
        ServiceNotification.stopForeground(this@LocalOnlyHotspotService)
        stopSelf()
    }

    override fun onIpNeighbourAvailable(neighbours: List<IpNeighbour>) {
        super.onIpNeighbourAvailable(neighbours)
        if (Build.VERSION.SDK_INT >= 28) timeoutMonitor?.onClientsChanged(neighbours.isEmpty())
    }

    override fun onDestroy() {
        binder.stop()
        unregisterReceiver(true)
        super.onDestroy()
    }

    private fun unregisterReceiver(exit: Boolean = false) {
        if (receiverRegistered) {
            unregisterReceiver(receiver)
            IpNeighbourMonitor.unregisterCallback(this)
            if (Build.VERSION.SDK_INT >= 28) {
                timeoutMonitor?.close()
                timeoutMonitor = null
            }
            receiverRegistered = false
        }
        launch {
            routingManager?.destroy()
            routingManager = null
            if (exit) cancel()
        }
    }
}
