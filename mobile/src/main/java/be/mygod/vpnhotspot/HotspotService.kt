package be.mygod.vpnhotspot

import android.app.PendingIntent
import android.app.Service
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.net.wifi.p2p.WifiP2pGroup
import android.net.wifi.p2p.WifiP2pManager
import android.os.Binder
import android.os.Looper
import android.support.v4.app.NotificationCompat
import android.support.v4.content.ContextCompat
import android.util.Log
import android.widget.Toast

class HotspotService : Service(), WifiP2pManager.ChannelListener {
    companion object {
        const val CHANNEL = "hotspot"
        private const val TAG = "HotspotService"
    }

    enum class Status {
        IDLE, STARTING, ACTIVE
    }

    inner class HotspotBinder : Binder() {
        val service get() = this@HotspotService
        val status get() = this@HotspotService.status
        var data: MainActivity.Data? = null

        fun shutdown() {
            p2pManager.removeGroup(channel, object : WifiP2pManager.ActionListener {
                override fun onSuccess() = clean()
                override fun onFailure(reason: Int) {
                    if (reason == WifiP2pManager.BUSY) clean() else {   // assuming it's already gone
                        Toast.makeText(this@HotspotService, "Failed to remove P2P group (reason: $reason)",
                                Toast.LENGTH_SHORT).show()
                        binder.data?.onStatusChanged()
                    }
                }
            })
        }
    }

    private lateinit var p2pManager: WifiP2pManager
    private lateinit var channel: WifiP2pManager.Channel
    private lateinit var group: WifiP2pGroup
    private var receiver: BroadcastReceiver? = null
    private val binder = HotspotBinder()

    // TODO: do something
    private val upstream = "tun0"
    private val downstream = "p2p0"
    private val route = "192.168.49.0/24"
    private val dns = "8.8.8.8:53"

    private var status = Status.IDLE
        set(value) {
            field = value
            binder.data?.onStatusChanged()
        }

    override fun onBind(intent: Intent) = binder

    override fun onCreate() {
        super.onCreate()
        p2pManager = getSystemService(Context.WIFI_P2P_SERVICE) as WifiP2pManager
        onChannelDisconnected()
    }

    override fun onChannelDisconnected() {
        channel = p2pManager.initialize(this, Looper.getMainLooper(), this)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (status != Status.IDLE) return START_NOT_STICKY
        status = Status.STARTING
        initReceiver()
        if (!noisySu("echo 1 >/proc/sys/net/ipv4/ip_forward",
                "ip route add default dev $upstream scope link table 62",
                "ip route add $route dev $downstream scope link table 62",
                "ip route add broadcast 255.255.255.255 dev $downstream scope link table 62",
                "ip rule add from $route lookup 62",
                "iptables -N vpnhotspot_fwd",
                "iptables -A vpnhotspot_fwd -i $upstream -o $downstream -m state --state ESTABLISHED,RELATED -j ACCEPT",
                "iptables -A vpnhotspot_fwd -i $downstream -o $upstream -j ACCEPT",
                "iptables -I FORWARD -j vpnhotspot_fwd",
                "iptables -t nat -A PREROUTING -i $downstream -p tcp --dport 53 -j DNAT --to-destination $dns",
                "iptables -t nat -A PREROUTING -i $downstream -p udp --dport 53 -j DNAT --to-destination $dns")) {
            startFailure("Something went wrong, please check logcat.")
            return START_NOT_STICKY
        }
        p2pManager.requestGroupInfo(channel, {
            when {
                it == null -> doStart()
                it.isGroupOwner -> doStart(it)
                else -> {
                    Log.i(TAG, "Removing old group ($it)")
                    p2pManager.removeGroup(channel, object : WifiP2pManager.ActionListener {
                        override fun onSuccess() = doStart()
                        override fun onFailure(reason: Int) {
                            Toast.makeText(this@HotspotService, "Failed to remove old P2P group (reason: $reason)",
                                    Toast.LENGTH_SHORT).show()
                        }
                    })
                }
            }
        })
        return START_NOT_STICKY
    }

    private fun startFailure(msg: String) {
        Toast.makeText(this@HotspotService, msg, Toast.LENGTH_SHORT).show()
        startForeground(0, NotificationCompat.Builder(this@HotspotService, CHANNEL).build())
        clean()
    }
    private fun doStart() {
        p2pManager.createGroup(channel, object : WifiP2pManager.ActionListener, WifiP2pManager.GroupInfoListener {
            override fun onFailure(reason: Int) = startFailure("Failed to create P2P group (reason: $reason)")

            private var tries = 0
            override fun onSuccess() = p2pManager.requestGroupInfo(channel, this)
            override fun onGroupInfoAvailable(group: WifiP2pGroup?) {
                if (group != null && group.isGroupOwner) doStart(group) else if (tries < 10) {
                    Thread.sleep(30L shl tries++)
                    onSuccess()
                } else startFailure("Unexpected group: $group")
            }
        })
    }
    private fun doStart(group: WifiP2pGroup) {
        status = Status.ACTIVE
        this.group = group
        startForeground(1, NotificationCompat.Builder(this@HotspotService, CHANNEL)
                .setColor(ContextCompat.getColor(this@HotspotService, R.color.colorPrimary))
                .setContentTitle(group.networkName)
                .setSubText(group.passphrase)
                .setSmallIcon(R.drawable.ic_device_wifi_tethering)
                .setContentIntent(PendingIntent.getActivity(this, 0,
                        Intent(this, MainActivity::class.java), PendingIntent.FLAG_UPDATE_CURRENT))
                .build())
    }

    private fun initReceiver() {
        return
        receiver = object : BroadcastReceiver() {
            override fun onReceive(context: Context, intent: Intent) {
                when (intent.action) {
                    // TODO
                }
            }
        }
        registerReceiver(receiver, createIntentFilter(
                WifiP2pManager.WIFI_P2P_STATE_CHANGED_ACTION,
                WifiP2pManager.WIFI_P2P_PEERS_CHANGED_ACTION,
                WifiP2pManager.WIFI_P2P_CONNECTION_CHANGED_ACTION,
                WifiP2pManager.WIFI_P2P_THIS_DEVICE_CHANGED_ACTION))
    }

    private fun clean() {
        if (!noisySu("iptables -t nat -D PREROUTING -i $downstream -p tcp --dport 53 -j DNAT --to-destination $dns",
                "iptables -t nat -D PREROUTING -i $downstream -p udp --dport 53 -j DNAT --to-destination $dns",
                "iptables -D FORWARD -j vpnhotspot_fwd",
                "iptables -F vpnhotspot_fwd",
                "iptables -X vpnhotspot_fwd",
                "ip rule del from $route lookup 62",
                "ip route del broadcast 255.255.255.255 dev $downstream scope link table 62",
                "ip route del $route dev $downstream scope link table 62",
                "ip route del default dev $upstream scope link table 62"))
            Toast.makeText(this, "Something went wrong, please check logcat.", Toast.LENGTH_SHORT).show()
        status = Status.IDLE
        stopForeground(true)
    }

    override fun onDestroy() {
        if (status != Status.IDLE) binder.shutdown()
        if (receiver != null) {
            unregisterReceiver(receiver)
            receiver = null
        }
        super.onDestroy()
    }
}
