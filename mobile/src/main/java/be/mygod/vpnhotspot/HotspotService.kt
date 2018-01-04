package be.mygod.vpnhotspot

import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.net.NetworkInfo
import android.net.wifi.p2p.WifiP2pGroup
import android.net.wifi.p2p.WifiP2pInfo
import android.net.wifi.p2p.WifiP2pManager
import android.os.Binder
import android.os.Looper
import android.support.v4.app.NotificationCompat
import android.support.v4.content.ContextCompat
import android.support.v4.content.LocalBroadcastManager
import android.util.Log
import android.widget.Toast
import be.mygod.vpnhotspot.App.Companion.app
import java.net.NetworkInterface
import java.util.regex.Pattern

class HotspotService : Service(), WifiP2pManager.ChannelListener {
    companion object {
        const val CHANNEL = "hotspot"
        const val STATUS_CHANGED = "be.mygod.vpnhotspot.HotspotService.STATUS_CHANGED"
        const val KEY_UPSTREAM = "service.upstream"
        private const val TAG = "HotspotService"

        /**
         * Matches the output of dumpsys wifip2p. This part is available since Android 4.2.
         *
         * Related sources:
         *   https://android.googlesource.com/platform/frameworks/base/+/f0afe4144d09aa9b980cffd444911ab118fa9cbe%5E%21/wifi/java/android/net/wifi/p2p/WifiP2pService.java
         *   https://android.googlesource.com/platform/frameworks/opt/net/wifi/+/a8d5e40/service/java/com/android/server/wifi/p2p/WifiP2pServiceImpl.java#639
         *
         *   https://android.googlesource.com/platform/frameworks/base.git/+/android-5.0.0_r1/core/java/android/net/NetworkInfo.java#433
         *   https://android.googlesource.com/platform/frameworks/base.git/+/220871a/core/java/android/net/NetworkInfo.java#415
         */
        private val patternNetworkInfo = "^mNetworkInfo .* (isA|a)vailable: (true|false)".toPattern(Pattern.MULTILINE)
    }

    enum class Status {
        IDLE, STARTING, ACTIVE
    }

    inner class HotspotBinder : Binder() {
        val service get() = this@HotspotService
        var data: MainActivity.Data? = null

        fun shutdown() {
            p2pManager.removeGroup(channel, object : WifiP2pManager.ActionListener {
                override fun onSuccess() = clean()
                override fun onFailure(reason: Int) {
                    if (reason == WifiP2pManager.BUSY) clean() else {   // assuming it's already gone
                        Toast.makeText(this@HotspotService, "Failed to remove P2P group (${formatReason(reason)})",
                                Toast.LENGTH_SHORT).show()
                        LocalBroadcastManager.getInstance(this@HotspotService).sendBroadcast(Intent(STATUS_CHANGED))
                    }
                }
            })
        }
    }

    private lateinit var p2pManager: WifiP2pManager
    private lateinit var channel: WifiP2pManager.Channel
    lateinit var group: WifiP2pGroup
        private set
    var hostAddress: String? = null
        private set
    private val binder = HotspotBinder()
    private var receiverRegistered = false
    private val receiver = broadcastReceiver { _, intent ->
        when (intent.action) {
            WifiP2pManager.WIFI_P2P_STATE_CHANGED_ACTION ->
                if (intent.getIntExtra(WifiP2pManager.EXTRA_WIFI_STATE, 0) ==
                        WifiP2pManager.WIFI_P2P_STATE_DISABLED) clean() // group may be enabled by other apps
            WifiP2pManager.WIFI_P2P_CONNECTION_CHANGED_ACTION -> {
                val info = intent.getParcelableExtra<WifiP2pInfo>(WifiP2pManager.EXTRA_WIFI_P2P_INFO)
                val net = intent.getParcelableExtra<NetworkInfo>(WifiP2pManager.EXTRA_NETWORK_INFO)
                val group = intent.getParcelableExtra<WifiP2pGroup>(WifiP2pManager.EXTRA_WIFI_P2P_GROUP)
                if (downstream == null) onGroupCreated(info, group)
                this.group = group
                binder.data?.onGroupChanged()
                showNotification(group)
                Log.d(TAG, "${intent.action}: $info, $net, $group")
            }
        }
    }

    var downstream: String? = null
        private set
    private val upstream get() = app.pref.getString(KEY_UPSTREAM, "tun0")
    /**
     * subnetPrefixLength has been the same forever but this option is here anyways. Source:
     *  https://android.googlesource.com/platform/frameworks/base/+/android-4.0.1_r1/wifi/java/android/net/wifi/p2p/WifiP2pService.java#1028
     *  https://android.googlesource.com/platform/frameworks/opt/net/wifi/+/a8d5e40/service/java/com/android/server/wifi/p2p/WifiP2pServiceImpl.java#2547
     */
    private var subnetPrefixLength: Short = 24
    private val dns get() = app.pref.getString("service.dns", "8.8.8.8:53")

    var status = Status.IDLE
        private set(value) {
            if (field == value) return
            field = value
            LocalBroadcastManager.getInstance(this).sendBroadcast(Intent(STATUS_CHANGED))
        }

    private fun formatReason(reason: Int) = when (reason) {
        WifiP2pManager.ERROR -> "ERROR"
        WifiP2pManager.P2P_UNSUPPORTED -> "P2P_UNSUPPORTED"
        WifiP2pManager.BUSY -> "BUSY"
        WifiP2pManager.NO_SERVICE_REQUESTS -> "NO_SERVICE_REQUESTS"
        else -> "unknown reason: $reason"
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
        val matcher = patternNetworkInfo.matcher(loggerSu("dumpsys ${Context.WIFI_P2P_SERVICE}"))
        if (!matcher.find()) {
            startFailure("Root unavailable")
            return START_NOT_STICKY
        }
        if (matcher.group(2) != "true") {
            startFailure("Wi-Fi direct unavailable")
            return START_NOT_STICKY
        }
        if (!receiverRegistered) {
            registerReceiver(receiver, intentFilter(WifiP2pManager.WIFI_P2P_STATE_CHANGED_ACTION,
                    WifiP2pManager.WIFI_P2P_CONNECTION_CHANGED_ACTION))
            receiverRegistered = true
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
                            Toast.makeText(this@HotspotService,
                                    "Failed to remove old P2P group (${formatReason(reason)})", Toast.LENGTH_SHORT)
                                    .show()
                        }
                    })
                }
            }
        })
        return START_NOT_STICKY
    }

    private fun startFailure(msg: String) {
        Toast.makeText(this@HotspotService, msg, Toast.LENGTH_SHORT).show()
        showNotification()
        clean()
    }
    private fun doStart() = p2pManager.createGroup(channel, object : WifiP2pManager.ActionListener {
        override fun onFailure(reason: Int) = startFailure("Failed to create P2P group (${formatReason(reason)})")
        override fun onSuccess() { }    // wait for WIFI_P2P_CONNECTION_CHANGED_ACTION to fire
    })
    private fun doStart(group: WifiP2pGroup) {
        this.group = group
        status = Status.ACTIVE
        showNotification(group)
    }
    private fun showNotification(group: WifiP2pGroup? = null) {
        val deviceCount = group?.clientList?.size ?: 0
        startForeground(1,
                NotificationCompat.Builder(this@HotspotService, CHANNEL)
                        .setWhen(0)
                        .setColor(ContextCompat.getColor(this@HotspotService, R.color.colorPrimary))
                        .setContentTitle(group?.networkName)
                        .setContentText(group?.passphrase)
                        .setSubText(resources.getQuantityString(R.plurals.notification_connected_devices,
                                deviceCount, deviceCount))
                        .setSmallIcon(R.drawable.ic_device_wifi_tethering)
                        .setContentIntent(PendingIntent.getActivity(this, 0,
                                Intent(this, MainActivity::class.java), PendingIntent.FLAG_UPDATE_CURRENT))
                        .build())
    }

    private fun onGroupCreated(info: WifiP2pInfo, group: WifiP2pGroup) {
        val owner = info.groupOwnerAddress
        val hostAddress = owner?.hostAddress
        val downstream = group.`interface`
        if (!info.groupFormed || !info.isGroupOwner || downstream == null || hostAddress == null) return
        this.downstream = downstream
        this.hostAddress = hostAddress
        var subnetPrefixLength = NetworkInterface.getByName(downstream)?.interfaceAddresses
                ?.singleOrNull { it.address == owner }?.networkPrefixLength
        if (subnetPrefixLength == null) {
            Log.w(TAG, "Unable to find prefix length of interface $downstream, 24 is assumed")
            subnetPrefixLength = 24
        }
        this.subnetPrefixLength = subnetPrefixLength
        if (noisySu("echo 1 >/proc/sys/net/ipv4/ip_forward",
                "ip route add default dev $upstream scope link table 62",
                "ip route add $hostAddress/$subnetPrefixLength dev $downstream scope link table 62",
                "ip route add broadcast 255.255.255.255 dev $downstream scope link table 62",
                "ip rule add iif $downstream lookup 62",
                "iptables -N vpnhotspot_fwd",
                "iptables -A vpnhotspot_fwd -i $upstream -o $downstream -m state --state ESTABLISHED,RELATED -j ACCEPT",
                "iptables -A vpnhotspot_fwd -i $downstream -o $upstream -j ACCEPT",
                "iptables -I FORWARD -j vpnhotspot_fwd",
                "iptables -t nat -A PREROUTING -i $downstream -p tcp -d $hostAddress --dport 53 -j DNAT --to-destination $dns",
                "iptables -t nat -A PREROUTING -i $downstream -p udp -d $hostAddress --dport 53 -j DNAT --to-destination $dns")) {
            doStart(group)
        } else startFailure("Something went wrong, please check logcat.")
    }

    private fun clean() {
        if (receiverRegistered) {
            unregisterReceiver(receiver)
            receiverRegistered = false
        }
        if (downstream != null)
            if (noisySu("iptables -t nat -D PREROUTING -i $downstream -p tcp -d $hostAddress --dport 53 -j DNAT --to-destination $dns",
                    "iptables -t nat -D PREROUTING -i $downstream -p udp -d $hostAddress --dport 53 -j DNAT --to-destination $dns",
                    "iptables -D FORWARD -j vpnhotspot_fwd",
                    "iptables -F vpnhotspot_fwd",
                    "iptables -X vpnhotspot_fwd",
                    "ip rule del iif $downstream lookup 62",
                    "ip route del broadcast 255.255.255.255 dev $downstream scope link table 62",
                    "ip route del $hostAddress/$subnetPrefixLength dev $downstream scope link table 62",
                    "ip route del default dev $upstream scope link table 62")) {
                hostAddress = null
                downstream = null
            } else Toast.makeText(this, "Something went wrong, please check logcat.", Toast.LENGTH_SHORT).show()
        status = Status.IDLE
        stopForeground(true)
    }

    override fun onDestroy() {
        if (status != Status.IDLE) binder.shutdown()
        super.onDestroy()
    }
}
