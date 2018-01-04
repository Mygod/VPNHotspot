package be.mygod.vpnhotspot

import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.net.NetworkInfo
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

        fun shutdown() {
            when (status) {
                Status.ACTIVE_P2P -> p2pManager.removeGroup(channel, object : WifiP2pManager.ActionListener {
                    override fun onSuccess() = clean()
                    override fun onFailure(reason: Int) {
                        if (reason == WifiP2pManager.BUSY) clean() else {   // assuming it's already gone
                            Toast.makeText(this@HotspotService, "Failed to remove P2P group (${formatReason(reason)})",
                                    Toast.LENGTH_SHORT).show()
                            LocalBroadcastManager.getInstance(this@HotspotService)
                                    .sendBroadcast(Intent(STATUS_CHANGED))
                        }
                    }
                })
                else -> clean()
            }
        }
    }

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
                        WifiP2pManager.WIFI_P2P_STATE_DISABLED) clean() // group may be enabled by other apps
            WifiP2pManager.WIFI_P2P_CONNECTION_CHANGED_ACTION -> {
                val info = intent.getParcelableExtra<WifiP2pInfo>(WifiP2pManager.EXTRA_WIFI_P2P_INFO)
                val net = intent.getParcelableExtra<NetworkInfo>(WifiP2pManager.EXTRA_NETWORK_INFO)
                val group = intent.getParcelableExtra<WifiP2pGroup>(WifiP2pManager.EXTRA_WIFI_P2P_GROUP)
                if (routing == null) onGroupCreated(info, group)
                this.group = group
                binder.data?.onGroupChanged()
                showNotification(group)
                Log.d(TAG, "${intent.action}: $info, $net, $group")
            }
            WIFI_AP_STATE_CHANGED_ACTION ->
                if (intent.getIntExtra(WifiManager.EXTRA_WIFI_STATE, 0) != WIFI_AP_STATE_ENABLED) clean()
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
                val routing = try {
                    Routing(upstream, wifi)
                } catch (_: Routing.InterfaceNotFoundException) {
                    startFailure(getString(R.string.exception_interface_not_found))
                    return START_NOT_STICKY
                }.apRule().forward().dnsRedirect(dns)
                if (routing.start()) {
                    this.routing = routing
                    apConfiguration = NetUtils.loadApConfiguration()
                    status = Status.ACTIVE_AP
                    showNotification()
                } else startFailure("Something went wrong, please check logcat.")
            }
            else -> startFailure("Wi-Fi direct unavailable and hotspot disabled, please enable either")
        }
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

    private fun onGroupCreated(info: WifiP2pInfo, group: WifiP2pGroup) {
        val owner = info.groupOwnerAddress
        val downstream = group.`interface`
        if (!info.groupFormed || !info.isGroupOwner || downstream == null || owner == null) return
        receiverRegistered = true
        val routing = try {
            Routing(upstream, downstream, owner)
        } catch (_: Routing.InterfaceNotFoundException) {
            startFailure(getString(R.string.exception_interface_not_found))
            return
        }.p2pRule().forward().dnsRedirect(dns)
        if (routing.start()) {
            this.routing = routing
            doStart(group)
        } else startFailure("Something went wrong, please check logcat.")
    }

    private fun unregisterReceiver() {
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
