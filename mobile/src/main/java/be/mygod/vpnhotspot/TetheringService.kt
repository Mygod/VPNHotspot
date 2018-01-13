package be.mygod.vpnhotspot

import android.app.Service
import android.content.Intent
import android.support.v4.content.LocalBroadcastManager
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.NetUtils.tetheredIfaces

class TetheringService : Service(), VpnListener.Callback {
    companion object {
        const val ACTION_ACTIVE_INTERFACES_CHANGED = "be.mygod.vpnhotspot.TetheringService.ACTIVE_INTERFACES_CHANGED"
        const val EXTRA_ADD_INTERFACE = "interface.add"
        const val EXTRA_REMOVE_INTERFACE = "interface.remove"
        private const val KEY_ACTIVE = "persist.service.tether.active"

        var active: Set<String>?
            get() = app.pref.getStringSet(KEY_ACTIVE, null)
            private set(value) {
                app.pref.edit().putStringSet(KEY_ACTIVE, value).apply()
                LocalBroadcastManager.getInstance(app).sendBroadcast(Intent(ACTION_ACTIVE_INTERFACES_CHANGED))
            }
    }

    private val routings = HashMap<String, Routing?>()
    private var upstream: String? = null
    private var receiverRegistered = false
    private val receiver = broadcastReceiver { _, intent ->
        val remove = routings - intent.extras.getStringArrayList(NetUtils.EXTRA_ACTIVE_TETHER).toSet()
        if (remove.isEmpty()) return@broadcastReceiver
        for ((iface, routing) in remove) {
            routing?.stop()
            routings.remove(iface)
        }
        val upstream = upstream
        if (upstream == null) onLost("") else onAvailable(upstream)
        active = routings.keys
        if (routings.isEmpty()) terminate()
    }

    override fun onBind(intent: Intent?) = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent != null) {   // otw service is recreated after being killed
            var iface = intent.getStringExtra(EXTRA_ADD_INTERFACE)
            if (iface != null && VpnListener.connectivityManager.tetheredIfaces.contains(iface))
                routings.put(iface, null)
            iface = intent.getStringExtra(EXTRA_REMOVE_INTERFACE)
            if (iface != null) routings.remove(iface)?.stop()
            active = routings.keys
        } else active?.forEach { routings.put(it, null) }
        if (routings.isEmpty()) terminate() else {
            if (!receiverRegistered) {
                registerReceiver(receiver, intentFilter(NetUtils.ACTION_TETHER_STATE_CHANGED))
                VpnListener.registerCallback(this)
                receiverRegistered = true
            }
        }
        return START_STICKY
    }

    override fun onAvailable(ifname: String) {
        check(upstream == null || upstream == ifname)
        upstream = ifname
        for ((downstream, value) in routings) if (value == null) {
            val routing = Routing(ifname, downstream).rule().forward().dnsRedirect(app.dns)
            if (routing.start()) routings[downstream] = routing else routing.stop()
        }
    }

    override fun onLost(ifname: String) {
        check(upstream == null || upstream == ifname)
        upstream = null
        for ((iface, routing) in routings) {
            routing?.stop()
            routings[iface] = null
        }
    }

    private fun terminate() {
        if (receiverRegistered) {
            unregisterReceiver(receiver)
            VpnListener.unregisterCallback(this)
            receiverRegistered = false
        }
        stopSelf()
    }
}
