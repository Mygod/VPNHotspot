package be.mygod.vpnhotspot

import android.content.Intent
import android.content.IntentFilter
import android.net.wifi.WifiManager
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.manage.LocalOnlyHotspotManager
import be.mygod.vpnhotspot.net.IpNeighbourMonitor
import be.mygod.vpnhotspot.net.TetheringManager
import be.mygod.vpnhotspot.util.broadcastReceiver
import be.mygod.vpnhotspot.util.debugLog
import be.mygod.vpnhotspot.widget.SmartSnackbar
import com.crashlytics.android.Crashlytics

@RequiresApi(26)
class LocalOnlyHotspotService : IpNeighbourMonitoringService() {
    companion object {
        private const val TAG = "LocalOnlyHotspotService"
    }

    inner class Binder : android.os.Binder() {
        var manager: LocalOnlyHotspotManager? = null
        var iface: String? = null
        val configuration get() = reservation?.wifiConfiguration

        fun stop() = reservation?.close()
    }
    private class StartFailure(message: String) : RuntimeException(message)

    private val binder = Binder()
    private var reservation: WifiManager.LocalOnlyHotspotReservation? = null
    private var routingManager: LocalOnlyInterfaceManager? = null
    private var receiverRegistered = false
    private val receiver = broadcastReceiver { _, intent ->
        val ifaces = TetheringManager.getLocalOnlyTetheredIfaces(intent.extras ?: return@broadcastReceiver)
        debugLog(TAG, "onTetherStateChangedLocked: $ifaces")
        check(ifaces.size <= 1)
        val iface = ifaces.singleOrNull()
        binder.iface = iface
        if (iface == null) {
            unregisterReceiver()
            ServiceNotification.stopForeground(this)
            stopSelf()
        } else {
            val routingManager = routingManager
            if (routingManager == null) {
                this.routingManager = LocalOnlyInterfaceManager(this, iface)
                IpNeighbourMonitor.registerCallback(this)
            } else check(iface == routingManager.downstream)
        }
        app.handler.post { binder.manager?.update() }
    }
    override val activeIfaces get() = listOfNotNull(binder.iface)

    override fun onBind(intent: Intent?) = binder

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        // throws IllegalStateException if the caller attempts to start the LocalOnlyHotspot while they
        // have an outstanding request.
        // https://android.googlesource.com/platform/frameworks/opt/net/wifi/+/53e0284/service/java/com/android/server/wifi/WifiServiceImpl.java#1192
        try {
            app.wifi.startLocalOnlyHotspot(object : WifiManager.LocalOnlyHotspotCallback() {
                override fun onStarted(reservation: WifiManager.LocalOnlyHotspotReservation?) {
                    if (reservation == null) onFailed(-2) else {
                        this@LocalOnlyHotspotService.reservation = reservation
                        if (!receiverRegistered) {
                            registerReceiver(receiver, IntentFilter(TetheringManager.ACTION_TETHER_STATE_CHANGED))
                            receiverRegistered = true
                        }
                    }
                }

                override fun onStopped() {
                    debugLog(TAG, "LOHCallback.onStopped")
                    reservation = null
                }

                override fun onFailed(reason: Int) {
                    val message = getString(R.string.tethering_temp_hotspot_failure,
                            when (reason) {
                                WifiManager.LocalOnlyHotspotCallback.ERROR_NO_CHANNEL ->
                                    getString(R.string.tethering_temp_hotspot_failure_no_channel)
                                WifiManager.LocalOnlyHotspotCallback.ERROR_GENERIC ->
                                    getString(R.string.tethering_temp_hotspot_failure_generic)
                                WifiManager.LocalOnlyHotspotCallback.ERROR_INCOMPATIBLE_MODE ->
                                    getString(R.string.tethering_temp_hotspot_failure_incompatible_mode)
                                WifiManager.LocalOnlyHotspotCallback.ERROR_TETHERING_DISALLOWED ->
                                    getString(R.string.tethering_temp_hotspot_failure_tethering_disallowed)
                                else -> getString(R.string.failure_reason_unknown, reason)
                            })
                    SmartSnackbar.make(message).show()
                    startFailure(if (reason == WifiManager.LocalOnlyHotspotCallback.ERROR_INCOMPATIBLE_MODE) null else
                        StartFailure(message))
                }
            }, app.handler)
        } catch (e: IllegalStateException) {
            e.printStackTrace()
            startFailure(e)
        } catch (e: SecurityException) {
            SmartSnackbar.make(e.localizedMessage).show()
            e.printStackTrace()
            startFailure(e)
        }
        return START_STICKY
    }

    private fun startFailure(e: Exception?) {
        if (e?.message != "Caller already has an active LocalOnlyHotspot request") Crashlytics.logException(e)
        updateNotification()
        ServiceNotification.stopForeground(this@LocalOnlyHotspotService)
        stopSelf()
    }

    override fun onDestroy() {
        unregisterReceiver()
        super.onDestroy()
    }

    private fun unregisterReceiver() {
        routingManager?.stop()
        routingManager = null
        if (receiverRegistered) {
            unregisterReceiver(receiver)
            IpNeighbourMonitor.unregisterCallback(this)
            receiverRegistered = false
        }
    }
}
