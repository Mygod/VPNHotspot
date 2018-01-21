package be.mygod.vpnhotspot

import android.app.Notification
import android.app.PendingIntent
import android.app.Service
import android.content.Intent
import android.os.Binder
import android.support.v4.app.NotificationCompat
import android.support.v4.content.ContextCompat
import android.support.v4.content.LocalBroadcastManager
import android.widget.Toast
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.*

class TetheringService : Service(), VpnMonitor.Callback, IpNeighbourMonitor.Callback {
    companion object {
        const val CHANNEL = "tethering"
        const val CHANNEL_ID = 2
        const val EXTRA_ADD_INTERFACE = "interface.add"
        const val EXTRA_REMOVE_INTERFACE = "interface.remove"
    }

    inner class TetheringBinder : Binder() {
        val active get() = routings.keys
        var fragment: TetheringFragment? = null
    }

    private val binder = TetheringBinder()
    private val routings = HashMap<String, Routing?>()
    private var neighbours = emptyList<IpNeighbour>()
    private var upstream: String? = null
    private var receiverRegistered = false
    private val receiver = broadcastReceiver { _, intent ->
        when (intent.action) {
            NetUtils.ACTION_TETHER_STATE_CHANGED -> {
                val remove = routings.keys - NetUtils.getTetheredIfaces(intent.extras)
                if (remove.isEmpty()) return@broadcastReceiver
                val failed = remove.any { routings.remove(it)?.stop() == false }
                if (failed) Toast.makeText(this, getText(R.string.noisy_su_failure), Toast.LENGTH_SHORT).show()
            }
            App.ACTION_CLEAN_ROUTINGS -> for (iface in routings.keys) routings[iface] = null
        }
        updateRoutings()
    }

    private fun updateRoutings() {
        if (routings.isEmpty()) {
            unregisterReceiver()
            stopForeground(true)
            stopSelf()
        } else {
            val upstream = upstream
            if (upstream != null) {
                var failed = false
                for ((downstream, value) in routings) if (value == null) {
                    val routing = Routing(upstream, downstream).rule().forward().dnsRedirect(app.dns)
                    if (routing.start()) routings[downstream] = routing else {
                        failed = true
                        routing.stop()
                        routings.remove(downstream)
                    }
                }
                if (failed) Toast.makeText(this, getText(R.string.noisy_su_failure), Toast.LENGTH_SHORT).show()
            } else if (!receiverRegistered) {
                registerReceiver(receiver, intentFilter(NetUtils.ACTION_TETHER_STATE_CHANGED))
                LocalBroadcastManager.getInstance(this)
                        .registerReceiver(receiver, intentFilter(App.ACTION_CLEAN_ROUTINGS))
                IpNeighbourMonitor.registerCallback(this)
                VpnMonitor.registerCallback(this)
                receiverRegistered = true
            }
        }
        postIpNeighbourAvailable()
        app.handler.post { binder.fragment?.adapter?.notifyDataSetChanged() }
    }

    override fun onBind(intent: Intent?) = binder

    override fun onStartCommand(intent: Intent, flags: Int, startId: Int): Int {
        val iface = intent.getStringExtra(EXTRA_ADD_INTERFACE)
        if (iface != null) routings[iface] = null
        if (routings.remove(intent.getStringExtra(EXTRA_REMOVE_INTERFACE))?.stop() == false)
            Toast.makeText(this, getText(R.string.noisy_su_failure), Toast.LENGTH_SHORT).show()
        updateRoutings()
        return START_NOT_STICKY
    }

    override fun onAvailable(ifname: String) {
        check(upstream == null || upstream == ifname)
        upstream = ifname
        updateRoutings()
    }

    override fun onLost(ifname: String) {
        check(upstream == null || upstream == ifname)
        upstream = null
        var failed = false
        for ((iface, routing) in routings) {
            if (routing?.stop() == false) failed = true
            routings[iface] = null
        }
        if (failed) Toast.makeText(this, getText(R.string.noisy_su_failure), Toast.LENGTH_SHORT).show()
    }

    override fun onIpNeighbourAvailable(neighbours: Map<String, IpNeighbour>) {
        this.neighbours = neighbours.values.toList()
    }
    override fun postIpNeighbourAvailable() {
        val builder = NotificationCompat.Builder(this, CHANNEL)
                .setWhen(0)
                .setColor(ContextCompat.getColor(this, R.color.colorPrimary))
                .setContentTitle("VPN tethering active")
                .setSmallIcon(R.drawable.ic_device_wifi_tethering)
                .setContentIntent(PendingIntent.getActivity(this, 0,
                        Intent(this, MainActivity::class.java), PendingIntent.FLAG_UPDATE_CURRENT))
                .setVisibility(Notification.VISIBILITY_PUBLIC)
        val content = neighbours.groupBy { it.dev }
                .filter { (dev, _) -> routings.contains(dev) }
                .mapValues { (_, neighbours) -> neighbours.size }
                .toList()
                .joinToString(", ") { (dev, size) ->
                    resources.getQuantityString(R.plurals.notification_connected_devices, size, size, dev)
                }
        if (content.isNotEmpty()) builder.setContentText(content)
        startForeground(CHANNEL_ID, builder.build())
    }

    override fun onDestroy() {
        unregisterReceiver()
        super.onDestroy()
    }

    private fun unregisterReceiver() {
        if (receiverRegistered) {
            unregisterReceiver(receiver)
            LocalBroadcastManager.getInstance(this).unregisterReceiver(receiver)
            VpnMonitor.unregisterCallback(this)
            upstream = null
            receiverRegistered = false
        }
    }
}
