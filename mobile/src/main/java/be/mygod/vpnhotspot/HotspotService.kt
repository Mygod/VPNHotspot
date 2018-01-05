package be.mygod.vpnhotspot

import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.net.*
import android.net.wifi.WifiConfiguration
import android.net.wifi.WifiManager
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
import java.net.InetAddress
import java.util.regex.Pattern

class HotspotService : Service(), WifiP2pManager.ChannelListener {
    companion object {
        const val CHANNEL = "hotspot"
        const val STATUS_CHANGED = "be.mygod.vpnhotspot.HotspotService.STATUS_CHANGED"
        const val KEY_UPSTREAM = "service.upstream"
        const val KEY_WIFI = "service.wifi"
        private const val TAG = "HotspotService"
        // constants from WifiManager
        private const val WIFI_AP_STATE_CHANGED_ACTION = "android.net.wifi.WIFI_AP_STATE_CHANGED"
        private const val WIFI_AP_STATE_ENABLED = 13

        private val upstream get() = app.pref.getString(KEY_UPSTREAM, "tun0")
        private val wifi get() = app.pref.getString(KEY_WIFI, "wlan0")
        private val dns get() = app.pref.getString("service.dns", "8.8.8.8:53")

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

        private val isWifiApEnabledMethod = WifiManager::class.java.getDeclaredMethod("isWifiApEnabled")
        val WifiManager.isWifiApEnabled get() = isWifiApEnabledMethod.invoke(this) as Boolean

        private val request by lazy {
            /* We don't know how to specify the interface we're interested in, so we will listen for everything.
             * However, we need to remove all default capabilities defined in NetworkCapabilities constructor.
             * Also this unfortunately doesn't work for P2P/AP connectivity changes.
             */
            NetworkRequest.Builder()
                    .removeCapability(NetworkCapabilities.NET_CAPABILITY_NOT_RESTRICTED)
                    .removeCapability(NetworkCapabilities.NET_CAPABILITY_TRUSTED)
                    .removeCapability(NetworkCapabilities.NET_CAPABILITY_NOT_VPN)
                    .build()
        }

        init {
            isWifiApEnabledMethod.isAccessible = true
        }
    }

    enum class Status {
        IDLE, STARTING, ACTIVE_P2P, ACTIVE_AP
    }

    inner class HotspotBinder : Binder() {
        val service get() = this@HotspotService
        var data: MainActivity.Data? = null

        fun shutdown() = when (status) {
            Status.ACTIVE_P2P -> removeGroup()
            else -> clean()
        }

        fun reapplyRouting() {
            val routing = routing
            routing?.stop()
            try {
                if (!when (status) {
                    Status.ACTIVE_P2P -> initP2pRouting(routing!!.downstream, routing.hostAddress)
                    Status.ACTIVE_AP -> initApRouting(routing!!.hostAddress)
                    else -> false
                }) Toast.makeText(this@HotspotService, "Something went wrong, please check logcat.",
                        Toast.LENGTH_SHORT).show()
            } catch (e: Exception) {
                Toast.makeText(this@HotspotService, e.message, Toast.LENGTH_SHORT).show()
                return
            }
        }
    }

    private val connectivityManager by lazy { getSystemService(Context.CONNECTIVITY_SERVICE) as ConnectivityManager }
    private val wifiManager by lazy { getSystemService(Context.WIFI_SERVICE) as WifiManager }
    private val p2pManager by lazy { getSystemService(Context.WIFI_P2P_SERVICE) as WifiP2pManager }
    private var _channel: WifiP2pManager.Channel? = null
    private val channel: WifiP2pManager.Channel get() {
        if (_channel == null) onChannelDisconnected()
        return _channel!!
    }
    lateinit var group: WifiP2pGroup
        private set
    private var apConfiguration: WifiConfiguration? = null
    private val binder = HotspotBinder()
    private var receiverRegistered = false
    private val receiver = broadcastReceiver { _, intent ->
        when (intent.action) {
            WifiP2pManager.WIFI_P2P_STATE_CHANGED_ACTION ->
                if (intent.getIntExtra(WifiP2pManager.EXTRA_WIFI_STATE, 0) ==
                        WifiP2pManager.WIFI_P2P_STATE_DISABLED) clean() // ignore P2P enabled
            WifiP2pManager.WIFI_P2P_CONNECTION_CHANGED_ACTION -> {
                onP2pConnectionChanged(intent.getParcelableExtra(WifiP2pManager.EXTRA_WIFI_P2P_INFO),
                        intent.getParcelableExtra(WifiP2pManager.EXTRA_NETWORK_INFO),
                        intent.getParcelableExtra(WifiP2pManager.EXTRA_WIFI_P2P_GROUP))
            }
            WIFI_AP_STATE_CHANGED_ACTION ->
                if (intent.getIntExtra(WifiManager.EXTRA_WIFI_STATE, 0) != WIFI_AP_STATE_ENABLED) clean()
        }
    }
    private var netListenerRegistered = false
    private val netListener = object : ConnectivityManager.NetworkCallback() {
        /**
         * Obtaining ifname in onLost doesn't work so we need to cache it in onAvailable.
         */
        private val ifnameCache = HashMap<Network, String>()
        private val Network.ifname: String? get() {
            var result = ifnameCache[this]
            if (result == null) {
                result = connectivityManager.getLinkProperties(this)?.interfaceName
                if (result != null) ifnameCache.put(this, result)
            }
            return result
        }

        override fun onAvailable(network: Network?) {
            val routing = routing ?: return
            val ifname = network?.ifname
            debugLog(TAG, "onAvailable: $ifname")
            if (ifname == routing.upstream) routing.start()
        }

        override fun onLost(network: Network?) {
            val routing = routing ?: return
            val ifname = network?.ifname
            debugLog(TAG, "onLost: $ifname")
            when (ifname) {
                routing.downstream -> clean()
                routing.upstream -> routing.stop()
            }
        }
    }

    val ssid get() = when (status) {
        HotspotService.Status.ACTIVE_P2P -> group.networkName
        HotspotService.Status.ACTIVE_AP -> apConfiguration?.SSID ?: "Unknown"
        else -> null
    }
    val password get() = when (status) {
        HotspotService.Status.ACTIVE_P2P -> group.passphrase
        HotspotService.Status.ACTIVE_AP -> apConfiguration?.preSharedKey
        else -> null
    }

    var routing: Routing? = null
        private set

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

    override fun onChannelDisconnected() {
        _channel = p2pManager.initialize(this, Looper.getMainLooper(), this)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (status != Status.IDLE) return START_NOT_STICKY
        status = Status.STARTING
        val matcher = patternNetworkInfo.matcher(loggerSu("dumpsys ${Context.WIFI_P2P_SERVICE}"))
        when {
            !matcher.find() -> startFailure("Root unavailable")
            matcher.group(2) == "true" -> {
                unregisterReceiver()
                registerReceiver(receiver, intentFilter(WifiP2pManager.WIFI_P2P_STATE_CHANGED_ACTION,
                        WifiP2pManager.WIFI_P2P_CONNECTION_CHANGED_ACTION))
                receiverRegistered = true
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
                                            "Failed to remove old P2P group (${formatReason(reason)})",
                                            Toast.LENGTH_SHORT).show()
                                }
                            })
                        }
                    }
                })
            }
            wifiManager.isWifiApEnabled -> {
                unregisterReceiver()
                registerReceiver(receiver, intentFilter(WIFI_AP_STATE_CHANGED_ACTION))
                receiverRegistered = true
                try {
                    if (initApRouting()) {
                        connectivityManager.registerNetworkCallback(request, netListener)
                        netListenerRegistered = true
                        apConfiguration = NetUtils.loadApConfiguration()
                        status = Status.ACTIVE_AP
                        showNotification()
                    } else startFailure("Something went wrong, please check logcat.", group)
                } catch (e: Routing.InterfaceNotFoundException) {
                    startFailure(e.message, group)
                }
            }
            else -> startFailure("Wi-Fi direct unavailable and hotspot disabled, please enable either")
        }
        return START_NOT_STICKY
    }
    private fun initApRouting(owner: InetAddress? = null): Boolean {
        val routing = Routing(upstream, wifi, owner).rule().forward().dnsRedirect(dns)
        return if (routing.start()) {
            this.routing = routing
            true
        } else {
            routing.stop()
            this.routing = null
            false
        }
    }

    private fun startFailure(msg: String?, group: WifiP2pGroup? = null) {
        Toast.makeText(this, msg, Toast.LENGTH_SHORT).show()
        showNotification()
        if (group != null) removeGroup() else clean()
    }
    private fun doStart() = p2pManager.createGroup(channel, object : WifiP2pManager.ActionListener {
        override fun onFailure(reason: Int) = startFailure("Failed to create P2P group (${formatReason(reason)})")
        override fun onSuccess() { }    // wait for WIFI_P2P_CONNECTION_CHANGED_ACTION to fire
    })
    private fun doStart(group: WifiP2pGroup) {
        this.group = group
        status = Status.ACTIVE_P2P
        showNotification(group)
    }
    private fun showNotification(group: WifiP2pGroup? = null) {
        val builder = NotificationCompat.Builder(this, CHANNEL)
                .setWhen(0)
                .setColor(ContextCompat.getColor(this, R.color.colorPrimary))
                .setContentTitle(group?.networkName ?: ssid ?: "Connecting...")
                .setSmallIcon(R.drawable.ic_device_wifi_tethering)
                .setContentIntent(PendingIntent.getActivity(this, 0,
                        Intent(this, MainActivity::class.java), PendingIntent.FLAG_UPDATE_CURRENT))
        if (group != null) builder.setContentText(resources.getQuantityString(R.plurals.notification_connected_devices,
                group.clientList.size, group.clientList.size))
        startForeground(1, builder.build())
    }

    private fun onP2pConnectionChanged(info: WifiP2pInfo, net: NetworkInfo?, group: WifiP2pGroup) {
        if (routing == null) onGroupCreated(info, group) else if (!group.isGroupOwner) {    // P2P shutdown
            clean()
            return
        }
        this.group = group
        binder.data?.onGroupChanged()
        showNotification(group)
        Log.d(TAG, "P2P connection changed: $info\n$net\n$group")
    }
    private fun onGroupCreated(info: WifiP2pInfo, group: WifiP2pGroup) {
        val owner = info.groupOwnerAddress
        val downstream = group.`interface`
        if (!info.groupFormed || !info.isGroupOwner || downstream == null || owner == null) return
        receiverRegistered = true
        try {
            if (initP2pRouting(downstream, owner)) {
                connectivityManager.registerNetworkCallback(request, netListener)
                netListenerRegistered = true
                doStart(group)
            } else startFailure("Something went wrong, please check logcat.", group)
        } catch (e: Routing.InterfaceNotFoundException) {
            startFailure(e.message, group)
            return
        }
    }
    private fun initP2pRouting(downstream: String, owner: InetAddress): Boolean {
        val routing = Routing(upstream, downstream, owner)
                .ipForward()   // Wi-Fi direct doesn't enable ip_forward
                .rule().forward().dnsRedirect(dns)
        return if (routing.start()) {
            this.routing = routing
            true
        } else {
            routing.stop()
            this.routing = null
            false
        }
    }

    private fun removeGroup() {
        p2pManager.removeGroup(channel, object : WifiP2pManager.ActionListener {
            override fun onSuccess() = clean()
            override fun onFailure(reason: Int) {
                if (reason == WifiP2pManager.BUSY) clean() else {   // assuming it's already gone
                    Toast.makeText(this@HotspotService, "Failed to remove P2P group (${formatReason(reason)})",
                            Toast.LENGTH_SHORT).show()
                    status = Status.ACTIVE_P2P
                    LocalBroadcastManager.getInstance(this@HotspotService).sendBroadcast(Intent(STATUS_CHANGED))
                }
            }
        })
    }
    private fun unregisterReceiver() {
        if (netListenerRegistered) {
            connectivityManager.unregisterNetworkCallback(netListener)
            netListenerRegistered = false
        }
        if (receiverRegistered) {
            unregisterReceiver(receiver)
            receiverRegistered = false
        }
    }
    private fun clean() {
        unregisterReceiver()
        if (routing?.stop() == false)
            Toast.makeText(this, "Something went wrong, please check logcat.", Toast.LENGTH_SHORT).show()
        routing = null
        status = Status.IDLE
        stopForeground(true)
    }

    override fun onDestroy() {
        if (status != Status.IDLE) binder.shutdown()
        super.onDestroy()
    }
}
