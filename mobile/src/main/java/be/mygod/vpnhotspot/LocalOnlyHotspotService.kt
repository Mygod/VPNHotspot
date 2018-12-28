package be.mygod.vpnhotspot

import android.content.Intent
import android.content.IntentFilter
import android.content.res.Configuration
import android.net.wifi.WifiManager
import android.os.Build
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.TetheringManager
import be.mygod.vpnhotspot.net.monitor.IpNeighbourMonitor
import be.mygod.vpnhotspot.net.wifi.WifiDoubleLock
import be.mygod.vpnhotspot.util.StickyEvent1
import be.mygod.vpnhotspot.util.broadcastReceiver
import be.mygod.vpnhotspot.widget.SmartSnackbar
import timber.log.Timber

@RequiresApi(26)
class LocalOnlyHotspotService : IpNeighbourMonitoringService() {
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
    private class StartFailure(message: String) : RuntimeException(message)

    private val binder = Binder()
    private var reservation: WifiManager.LocalOnlyHotspotReservation? = null
    private var routingManager: LocalOnlyInterfaceManager? = null
    private var locked = false
    private var receiverRegistered = false
    private val receiver = broadcastReceiver { _, intent ->
        val ifaces = TetheringManager.getLocalOnlyTetheredIfaces(intent.extras ?: return@broadcastReceiver)
        debugLog(TAG, "onTetherStateChangedLocked: $ifaces")
        check(ifaces.size <= 1)
        val iface = ifaces.singleOrNull()
        binder.iface = iface
        if (iface.isNullOrEmpty()) {
            unregisterReceiver()
            ServiceNotification.stopForeground(this)
            stopSelf()
        } else {
            val routingManager = routingManager
            if (routingManager == null) {
                this.routingManager = LocalOnlyInterfaceManager(iface)
                IpNeighbourMonitor.registerCallback(this)
            } else check(iface == routingManager.downstream)
        }
    }
    override val activeIfaces get() = listOfNotNull(binder.iface)

    override fun onBind(intent: Intent?) = binder

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
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
                        check(!locked)
                        WifiDoubleLock.acquire()
                        locked = true
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
                    if (reason != WifiManager.LocalOnlyHotspotCallback.ERROR_INCOMPATIBLE_MODE) {
                        Timber.i(StartFailure(message))
                    }
                    startFailure()
                }
            }, app.handler)
            // assuming IllegalStateException will be thrown only if
            // "Caller already has an active LocalOnlyHotspot request"
        } catch (_: IllegalStateException) { } catch (e: SecurityException) {
            SmartSnackbar.make(e).show()
            Timber.w(e)
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

    override fun onDestroy() {
        unregisterReceiver()
        super.onDestroy()
    }

    private fun unregisterReceiver() {
        routingManager?.stop()
        routingManager = null
        if (locked) {
            WifiDoubleLock.release()
            locked = false
        }
        if (receiverRegistered) {
            unregisterReceiver(receiver)
            IpNeighbourMonitor.unregisterCallback(this)
            receiverRegistered = false
        }
    }
}
