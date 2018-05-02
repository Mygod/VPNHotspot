package be.mygod.vpnhotspot

import android.content.Intent
import android.net.wifi.WifiManager
import android.os.Binder
import android.os.Bundle
import android.os.IBinder
import android.support.annotation.RequiresApi
import android.widget.Toast
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.TetheringManager

@RequiresApi(26)
class LocalOnlyHotspotService : BaseTetheringService() {
    companion object {
        private val TAG = "LocalOnlyHotspotService"
    }

    inner class HotspotBinder : Binder() {
        var fragment: TetheringFragment? = null
        var iface: String? = null
        val configuration get() = reservation?.wifiConfiguration

        fun stop() = reservation?.close()
    }

    private val binder = HotspotBinder()
    private var reservation: WifiManager.LocalOnlyHotspotReservation? = null

    override fun onTetherStateChangedLocked(extras: Bundle) {
        val ifaces = TetheringManager.getLocalOnlyTetheredIfaces(extras)
        debugLog(TAG, "onTetherStateChangedLocked: $ifaces")
        check(ifaces.size <= 1)
        val iface = ifaces.singleOrNull()
        binder.iface = iface
        if (iface == null) removeRoutingsLocked(routings.keys) else routings.getOrPut(iface) { null }
    }

    override fun updateRoutingsLocked() {
        super.updateRoutingsLocked()
        app.handler.post { binder.fragment?.adapter?.notifyDataSetChanged() }
    }

    override fun onBind(intent: Intent?): IBinder = binder

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        // throws IllegalStateException if the caller attempts to start the LocalOnlyHotspot while they
        // have an outstanding request.
        // https://android.googlesource.com/platform/frameworks/opt/net/wifi/+/53e0284/service/java/com/android/server/wifi/WifiServiceImpl.java#1192
        try {
            app.wifi.startLocalOnlyHotspot(object : WifiManager.LocalOnlyHotspotCallback() {
                override fun onStarted(reservation: WifiManager.LocalOnlyHotspotReservation?) {
                    if (reservation == null) onFailed(-1) else {
                        this@LocalOnlyHotspotService.reservation = reservation
                        registerReceiver()
                    }
                }

                override fun onStopped() {
                    debugLog(TAG, "LOHCallback.onStopped")
                    reservation = null
                }

                override fun onFailed(reason: Int) {
                    Toast.makeText(this@LocalOnlyHotspotService, "Failed to start hotspot (reason: $reason)", Toast.LENGTH_SHORT).show()
                }
            }, app.handler)
        } catch (e: IllegalStateException) {
            e.printStackTrace()
        }
        return START_STICKY
    }
}
