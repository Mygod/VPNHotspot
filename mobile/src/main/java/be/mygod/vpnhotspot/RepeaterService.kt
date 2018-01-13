package be.mygod.vpnhotspot

import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.net.NetworkInfo
import android.net.wifi.WpsInfo
import android.net.wifi.p2p.WifiP2pGroup
import android.net.wifi.p2p.WifiP2pInfo
import android.net.wifi.p2p.WifiP2pManager
import android.os.Binder
import android.os.Handler
import android.os.Looper
import android.support.v4.app.NotificationCompat
import android.support.v4.content.ContextCompat
import android.support.v4.content.LocalBroadcastManager
import android.util.Log
import android.widget.Toast
import be.mygod.vpnhotspot.App.Companion.app
import java.net.InetAddress
import java.util.regex.Pattern

class RepeaterService : Service(), WifiP2pManager.ChannelListener, VpnListener.Callback {
    companion object {
        const val CHANNEL = "repeater"
        const val ACTION_STATUS_CHANGED = "be.mygod.vpnhotspot.RepeaterService.STATUS_CHANGED"
        const val KEY_NET_ID = "netId"
        private const val TAG = "RepeaterService"
        private const val TEMPORARY_NET_ID = -1

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

        /**
         * Available since Android 4.3.
         *
         * Source: https://android.googlesource.com/platform/frameworks/base/+/android-4.3_r0.9/wifi/java/android/net/wifi/p2p/WifiP2pManager.java#958
         */
        private val startWps = WifiP2pManager::class.java.getDeclaredMethod("startWps",
                WifiP2pManager.Channel::class.java, WpsInfo::class.java, WifiP2pManager.ActionListener::class.java)
        private fun WifiP2pManager.startWps(c: WifiP2pManager.Channel, wps: WpsInfo,
                                            listener: WifiP2pManager.ActionListener) {
            startWps.invoke(this, c, wps, listener)
        }

        /**
         * Available since Android 4.2.
         *
         * Source: https://android.googlesource.com/platform/frameworks/base/+/android-4.2_r1/wifi/java/android/net/wifi/p2p/WifiP2pManager.java#1353
         */
        private val deletePersistentGroup = WifiP2pManager::class.java.getDeclaredMethod("deletePersistentGroup",
                WifiP2pManager.Channel::class.java, Int::class.java, WifiP2pManager.ActionListener::class.java)
        private fun WifiP2pManager.deletePersistentGroup(c: WifiP2pManager.Channel, netId: Int,
                                                         listener: WifiP2pManager.ActionListener) {
            deletePersistentGroup.invoke(this, c, netId, listener)
        }

        /**
         * Available since Android 4.2.
         *
         * Source: https://android.googlesource.com/platform/frameworks/base/+/android-4.2_r1/wifi/java/android/net/wifi/p2p/WifiP2pGroup.java#253
         */
        private val getNetworkId = WifiP2pGroup::class.java.getDeclaredMethod("getNetworkId")
        private val WifiP2pGroup.netId get() = getNetworkId.invoke(this) as Int
    }

    enum class Status {
        IDLE, STARTING, ACTIVE
    }

    inner class HotspotBinder : Binder() {
        val service get() = this@RepeaterService
        var data: RepeaterFragment.Data? = null
        val active get() = status == Status.ACTIVE

        fun startWps(pin: String? = null) {
            if (status != Status.ACTIVE) return
            val wps = WpsInfo()
            if (pin == null) wps.setup = WpsInfo.PBC else {
                wps.setup = WpsInfo.KEYPAD
                wps.pin = pin
            }
            p2pManager.startWps(channel, wps, object : WifiP2pManager.ActionListener {
                override fun onSuccess() = Toast.makeText(this@RepeaterService,
                        if (pin == null) "Please use WPS push button within the next 2 minutes to connect your device."
                        else "PIN registered.", Toast.LENGTH_SHORT).show()
                override fun onFailure(reason: Int) = Toast.makeText(this@RepeaterService,
                        "Failed to start WPS (reason: ${formatReason(reason)})", Toast.LENGTH_SHORT).show()
            })
        }

        fun shutdown() {
            if (status == Status.ACTIVE) removeGroup()
        }

        fun resetCredentials() {
            val netId = app.pref.getInt(KEY_NET_ID, TEMPORARY_NET_ID)
            if (netId == TEMPORARY_NET_ID) return
            p2pManager.deletePersistentGroup(channel, netId, object : WifiP2pManager.ActionListener {
                override fun onSuccess() = Toast.makeText(this@RepeaterService,
                        "Credentials reset.", Toast.LENGTH_SHORT).show()
                override fun onFailure(reason: Int) = Toast.makeText(this@RepeaterService,
                        "Failed to reset credentials (reason: ${formatReason(reason)})", Toast.LENGTH_SHORT).show()
            })
        }
    }

    private val handler = Handler()
    private val p2pManager by lazy { getSystemService(Context.WIFI_P2P_SERVICE) as WifiP2pManager }
    private var _channel: WifiP2pManager.Channel? = null
    private val channel: WifiP2pManager.Channel get() {
        if (_channel == null) onChannelDisconnected()
        return _channel!!
    }
    var group: WifiP2pGroup? = null
        private set(value) {
            field = value
            if (value != null) app.pref.edit().putInt(KEY_NET_ID, value.netId).apply()
        }
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
        }
    }
    private val onVpnUnavailable = Runnable { startFailure("VPN unavailable") }

    val ssid get() = if (status == Status.ACTIVE) group?.networkName else null
    val password get() = if (status == Status.ACTIVE) group?.passphrase else null

    private var upstream: String? = null
    var routing: Routing? = null
        private set

    var status = Status.IDLE
        private set(value) {
            if (field == value) return
            field = value
            LocalBroadcastManager.getInstance(this).sendBroadcast(Intent(ACTION_STATUS_CHANGED))
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

    /**
     * startService 1st stop
     */
    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (status != Status.IDLE) return START_NOT_STICKY
        status = Status.STARTING
        handler.postDelayed(onVpnUnavailable, 4000)
        VpnListener.registerCallback(this)
        return START_NOT_STICKY
    }
    private fun startFailure(msg: CharSequence?, group: WifiP2pGroup? = null) {
        Toast.makeText(this, msg, Toast.LENGTH_SHORT).show()
        showNotification()
        if (group != null) removeGroup() else clean()
    }

    /**
     * startService 2nd stop, also called when VPN re-established
     */
    override fun onAvailable(ifname: String) {
        handler.removeCallbacks(onVpnUnavailable)
        when (status) {
            Status.STARTING -> {
                val matcher = patternNetworkInfo.matcher(loggerSu("dumpsys ${Context.WIFI_P2P_SERVICE}") ?: "")
                when {
                    !matcher.find() -> startFailure("Root unavailable")
                    matcher.group(2) == "true" -> {
                        unregisterReceiver()
                        registerReceiver(receiver, intentFilter(WifiP2pManager.WIFI_P2P_STATE_CHANGED_ACTION,
                                WifiP2pManager.WIFI_P2P_CONNECTION_CHANGED_ACTION))
                        receiverRegistered = true
                        upstream = ifname
                        p2pManager.requestGroupInfo(channel, {
                            when {
                                it == null -> doStart()
                                it.isGroupOwner -> doStart(it)
                                else -> {
                                    Log.i(TAG, "Removing old group ($it)")
                                    p2pManager.removeGroup(channel, object : WifiP2pManager.ActionListener {
                                        override fun onSuccess() = doStart()
                                        override fun onFailure(reason: Int) {
                                            Toast.makeText(this@RepeaterService,
                                                    "Failed to remove old P2P group (${formatReason(reason)})",
                                                    Toast.LENGTH_SHORT).show()
                                        }
                                    })
                                }
                            }
                        })
                    }
                    else -> startFailure("Wi-Fi direct unavailable")
                }
            }
            Status.ACTIVE -> {
                val routing = routing
                check(!routing!!.started)
                if (!initRouting(ifname, routing.downstream, routing.hostAddress))
                    Toast.makeText(this, getText(R.string.noisy_su_failure), Toast.LENGTH_SHORT).show()
            }
            else -> throw RuntimeException("RepeaterService is in unexpected state when receiving onAvailable")
        }
    }
    override fun onLost(ifname: String) {
        if (routing?.stop() == false)
            Toast.makeText(this, getText(R.string.noisy_su_failure), Toast.LENGTH_SHORT).show()
        upstream = null
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

    /**
     * startService 3rd stop (if a group isn't already available), also called when connection changed
     */
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
            if (initRouting(upstream!!, downstream, owner)) doStart(group)
            else startFailure(getText(R.string.noisy_su_failure), group)
        } catch (e: Routing.InterfaceNotFoundException) {
            startFailure(e.message, group)
            return
        }
    }
    private fun initRouting(upstream: String, downstream: String, owner: InetAddress): Boolean {
        val routing = Routing(upstream, downstream, owner)
                .ipForward()   // Wi-Fi direct doesn't enable ip_forward
                .rule().forward().dnsRedirect(app.dns)
        return if (routing.start()) {
            this.routing = routing
            true
        } else {
            routing.stop()
            this.routing = null
            false
        }
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

    private fun removeGroup() {
        p2pManager.removeGroup(channel, object : WifiP2pManager.ActionListener {
            override fun onSuccess() = clean()
            override fun onFailure(reason: Int) {
                if (reason == WifiP2pManager.BUSY) clean() else {   // assuming it's already gone
                    Toast.makeText(this@RepeaterService, "Failed to remove P2P group (${formatReason(reason)})",
                            Toast.LENGTH_SHORT).show()
                    status = Status.ACTIVE
                    LocalBroadcastManager.getInstance(this@RepeaterService).sendBroadcast(Intent(ACTION_STATUS_CHANGED))
                }
            }
        })
    }
    private fun unregisterReceiver() {
        if (receiverRegistered) {
            unregisterReceiver(receiver)
            receiverRegistered = false
        }
    }
    private fun clean() {
        VpnListener.unregisterCallback(this)
        unregisterReceiver()
        if (routing?.stop() == false)
            Toast.makeText(this, getText(R.string.noisy_su_failure), Toast.LENGTH_SHORT).show()
        routing = null
        status = Status.IDLE
        stopForeground(true)
    }

    override fun onDestroy() {
        if (status != Status.IDLE) binder.shutdown()
        super.onDestroy()
    }
}
