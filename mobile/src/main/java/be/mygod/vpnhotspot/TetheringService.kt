package be.mygod.vpnhotspot

import android.app.Service
import android.content.Intent
import android.support.v4.content.LocalBroadcastManager
import be.mygod.vpnhotspot.App.Companion.app

class TetheringService : Service(), VpnListener.Callback {
    companion object {
        const val ACTION_ACTIVE_INTERFACES_CHANGED = "be.mygod.vpnhotspot.TetheringService.ACTIVE_INTERFACES_CHANGED"
        const val EXTRA_ADD_INTERFACE = "interface.add"
        const val EXTRA_REMOVE_INTERFACE = "interface.remove"
        private const val KEY_ACTIVE = "persist.service.tether.active"

        var active: Set<String>
            get() = app.pref.getStringSet(KEY_ACTIVE, null) ?: emptySet()
            private set(value) {
                app.pref.edit().putStringSet(KEY_ACTIVE, value).apply()
                LocalBroadcastManager.getInstance(app).sendBroadcast(Intent(ACTION_ACTIVE_INTERFACES_CHANGED))
            }
    }

    private val routings = HashMap<String, Routing?>()
    private var upstream: String? = null
    private var receiverRegistered = false
    private val receiver = broadcastReceiver { _, intent ->
        val remove = routings.keys - NetUtils.getTetheredIfaces(intent.extras)
        if (remove.isEmpty()) return@broadcastReceiver
        for (iface in remove) routings.remove(iface)?.stop()
        updateRoutings()
    }

    private fun updateRoutings() {
        active = routings.keys
        if (routings.isEmpty()) {
            unregisterReceiver()
            stopSelf()
        } else {
            val upstream = upstream
            if (upstream != null) {
                for ((downstream, value) in routings) if (value == null) {
                    val routing = Routing(upstream, downstream).rule().forward().dnsRedirect(app.dns)
                    if (routing.start()) routings[downstream] = routing else routing.stop()
                }
            } else if (!receiverRegistered) {
                registerReceiver(receiver, intentFilter(NetUtils.ACTION_TETHER_STATE_CHANGED))
                VpnListener.registerCallback(this)
                receiverRegistered = true
            }
        }
    }

    override fun onBind(intent: Intent?) = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent != null) {   // otw service is recreated after being killed
            val iface = intent.getStringExtra(EXTRA_ADD_INTERFACE)
            if (iface != null) routings.put(iface, null)
            routings.remove(intent.getStringExtra(EXTRA_REMOVE_INTERFACE))?.stop()
        } else active.forEach { routings.put(it, null) }
        updateRoutings()
        return START_STICKY
    }

    override fun onAvailable(ifname: String) {
        check(upstream == null || upstream == ifname)
        upstream = ifname
        updateRoutings()
    }

    override fun onLost(ifname: String) {
        check(upstream == null || upstream == ifname)
        upstream = null
        for ((iface, routing) in routings) {
            routing?.stop()
            routings[iface] = null
        }
    }

    override fun onDestroy() {
        unregisterReceiver()
        super.onDestroy()
    }

    fun unregisterReceiver() {
        if (receiverRegistered) {
            unregisterReceiver(receiver)
            VpnListener.unregisterCallback(this)
            upstream = null
            receiverRegistered = false
        }
    }
}
