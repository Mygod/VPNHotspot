package be.mygod.vpnhotspot

import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.SharedPreferences
import android.net.NetworkInfo
import android.net.wifi.WpsInfo
import android.net.wifi.p2p.WifiP2pGroup
import android.net.wifi.p2p.WifiP2pInfo
import android.net.wifi.p2p.WifiP2pManager
import android.os.Binder
import android.os.Looper
import android.support.annotation.StringRes
import android.support.v4.content.LocalBroadcastManager
import android.util.Log
import android.widget.Toast
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.Routing
import be.mygod.vpnhotspot.net.VpnMonitor
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.deletePersistentGroup
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.netId
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.requestPersistentGroupInfo
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.setWifiP2pChannels
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.startWps
import java.lang.reflect.InvocationTargetException
import java.net.InetAddress
import java.net.SocketException

class RepeaterService : Service(), WifiP2pManager.ChannelListener, VpnMonitor.Callback,
        SharedPreferences.OnSharedPreferenceChangeListener {
    companion object {
        const val ACTION_STATUS_CHANGED = "be.mygod.vpnhotspot.RepeaterService.STATUS_CHANGED"
        private const val TAG = "RepeaterService"
    }

    enum class Status {
        IDLE, STARTING, ACTIVE
    }

    inner class RepeaterBinder : Binder() {
        val service get() = this@RepeaterService
        var data: RepeaterFragment.Data? = null
        val active get() = status == Status.ACTIVE

        val ssid get() = group?.networkName
        val password get() = group?.passphrase

        private var groups: Collection<WifiP2pGroup> = emptyList()

        fun startWps(pin: String? = null) {
            if (!active) return
            val wps = WpsInfo()
            if (pin == null) wps.setup = WpsInfo.PBC else {
                wps.setup = WpsInfo.KEYPAD
                wps.pin = pin
            }
            p2pManager.startWps(channel, wps, object : WifiP2pManager.ActionListener {
                override fun onSuccess() = Toast.makeText(this@RepeaterService,
                        if (pin == null) R.string.repeater_wps_success_pbc else R.string.repeater_wps_success_keypad,
                        Toast.LENGTH_SHORT).show()
                override fun onFailure(reason: Int) = Toast.makeText(this@RepeaterService,
                        formatReason(R.string.repeater_wps_failure, reason), Toast.LENGTH_SHORT).show()
            })
        }

        fun shutdown() {
            if (active) removeGroup()
        }

        fun resetCredentials() = (groups + group).filterNotNull().forEach {
            p2pManager.deletePersistentGroup(channel, it.netId, object : WifiP2pManager.ActionListener {
                override fun onSuccess() = Toast.makeText(this@RepeaterService,
                        R.string.repeater_reset_credentials_success, Toast.LENGTH_SHORT).show()
                override fun onFailure(reason: Int) = Toast.makeText(this@RepeaterService,
                        formatReason(R.string.repeater_reset_credentials_failure, reason), Toast.LENGTH_SHORT).show()
            })
        }

        fun requestGroupUpdate() {
            group = null
            try {
                p2pManager.requestPersistentGroupInfo(channel, {
                    groups = it
                    if (it.size == 1) group = it.single()
                })
            } catch (e: ReflectiveOperationException) {
                e.printStackTrace()
                Toast.makeText(this@RepeaterService, e.message, Toast.LENGTH_LONG).show()
            }
        }
    }

    private lateinit var p2pManager: WifiP2pManager
    private lateinit var channel: WifiP2pManager.Channel
    var group: WifiP2pGroup? = null
        private set(value) {
            field = value
            binder.data?.onGroupChanged(value)
        }
    private val binder = RepeaterBinder()
    private var receiverRegistered = false
    private val receiver = broadcastReceiver { _, intent ->
        when (intent.action) {
            WifiP2pManager.WIFI_P2P_STATE_CHANGED_ACTION ->
                if (intent.getIntExtra(WifiP2pManager.EXTRA_WIFI_STATE, 0) ==
                        WifiP2pManager.WIFI_P2P_STATE_DISABLED) clean() // ignore P2P enabled
            WifiP2pManager.WIFI_P2P_CONNECTION_CHANGED_ACTION -> onP2pConnectionChanged(
                    intent.getParcelableExtra(WifiP2pManager.EXTRA_WIFI_P2P_INFO),
                    intent.getParcelableExtra(WifiP2pManager.EXTRA_NETWORK_INFO),
                    intent.getParcelableExtra(WifiP2pManager.EXTRA_WIFI_P2P_GROUP))
            App.ACTION_CLEAN_ROUTINGS -> if (status == Status.ACTIVE) {
                val routing = routing
                routing!!.started = false
                resetup(routing, upstream, dns)
            }
        }
    }

    private var upstream: String? = null
    private var dns: List<InetAddress> = emptyList()
    private var routing: Routing? = null

    var status = Status.IDLE
        private set(value) {
            if (field == value) return
            field = value
            LocalBroadcastManager.getInstance(this).sendBroadcast(Intent(ACTION_STATUS_CHANGED))
        }

    private fun formatReason(@StringRes resId: Int, reason: Int) = getString(resId, when (reason) {
        WifiP2pManager.ERROR -> getString(R.string.repeater_failure_reason_error)
        WifiP2pManager.P2P_UNSUPPORTED -> getString(R.string.repeater_failure_reason_p2p_unsupported)
        WifiP2pManager.BUSY -> getString(R.string.repeater_failure_reason_busy)
        WifiP2pManager.NO_SERVICE_REQUESTS -> getString(R.string.repeater_failure_reason_no_service_requests)
        else -> getString(R.string.repeater_failure_reason_unknown, reason)
    })

    override fun onCreate() {
        super.onCreate()
        try {
            p2pManager = getSystemService(Context.WIFI_P2P_SERVICE) as WifiP2pManager
            onChannelDisconnected()
            app.pref.registerOnSharedPreferenceChangeListener(this)
        } catch (exc: TypeCastException) {
            exc.printStackTrace()
        }
    }

    override fun onBind(intent: Intent) = binder

    private fun setOperatingChannel(oc: Int = app.operatingChannel) = try {
        // we don't care about listening channel
        p2pManager.setWifiP2pChannels(channel, 0, oc, object : WifiP2pManager.ActionListener {
            override fun onSuccess() { }
            override fun onFailure(reason: Int) {
                Toast.makeText(this@RepeaterService, formatReason(R.string.repeater_set_oc_failure, reason),
                        Toast.LENGTH_SHORT).show()
            }
        })
    } catch (e: InvocationTargetException) {
        if (oc != 0)
            Toast.makeText(this, getString(R.string.repeater_set_oc_failure, e.message), Toast.LENGTH_SHORT).show()
        e.printStackTrace()
    }

    override fun onChannelDisconnected() {
        channel = p2pManager.initialize(this, Looper.getMainLooper(), this)
        setOperatingChannel()
        binder.requestGroupUpdate()
    }

    override fun onSharedPreferenceChanged(sharedPreferences: SharedPreferences?, key: String?) {
        if (key == App.KEY_OPERATING_CHANNEL) setOperatingChannel()
    }

    /**
     * startService 1st stop
     */
    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (status != Status.IDLE) return START_NOT_STICKY
        status = Status.STARTING
        VpnMonitor.registerCallback(this) { setup() }
        return START_NOT_STICKY
    }
    private fun startFailure(msg: CharSequence?, group: WifiP2pGroup? = null) {
        Toast.makeText(this, msg, Toast.LENGTH_SHORT).show()
        showNotification()
        if (group != null) removeGroup() else clean()
    }

    /**
     * startService 2nd stop
     */
    private fun setup(ifname: String? = null, dns: List<InetAddress> = emptyList()) {
        val matcher = WifiP2pManagerHelper.patternNetworkInfo.matcher(
                loggerSu("dumpsys ${Context.WIFI_P2P_SERVICE}") ?: "")
        when {
            !matcher.find() -> startFailure(getString(R.string.root_unavailable))
            matcher.group(2) == "true" -> {
                unregisterReceiver()
                upstream = ifname
                this.dns = dns
                registerReceiver(receiver, intentFilter(WifiP2pManager.WIFI_P2P_STATE_CHANGED_ACTION,
                        WifiP2pManager.WIFI_P2P_CONNECTION_CHANGED_ACTION))
                LocalBroadcastManager.getInstance(this)
                        .registerReceiver(receiver, intentFilter(App.ACTION_CLEAN_ROUTINGS))
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
                                    Toast.makeText(this@RepeaterService,
                                            formatReason(R.string.repeater_remove_old_group_failure, reason),
                                            Toast.LENGTH_SHORT).show()
                                }
                            })
                        }
                    }
                })
            }
            else -> startFailure(getString(R.string.repeater_p2p_unavailable))
        }
    }

    private fun resetup(routing: Routing, ifname: String? = null, dns: List<InetAddress> = emptyList()) =
            initRouting(ifname, routing.downstream, routing.hostAddress, dns)

    override fun onAvailable(ifname: String, dns: List<InetAddress>) = when (status) {
        Status.STARTING -> setup(ifname, dns)
        Status.ACTIVE -> {
            val routing = routing!!
            if (routing.started) {
                routing.stop()
                check(routing.upstream == null)
            }
            resetup(routing, ifname, dns)
            while (false) { }
        }
        else -> throw IllegalStateException("RepeaterService is in unexpected state when receiving onAvailable")
    }
    override fun onLost(ifname: String) {
        if (routing?.stop() == false)
            Toast.makeText(this, getText(R.string.noisy_su_failure), Toast.LENGTH_SHORT).show()
        upstream = null
        if (status == Status.ACTIVE) resetup(routing!!)
    }

    private fun doStart() = p2pManager.createGroup(channel, object : WifiP2pManager.ActionListener {
        override fun onFailure(reason: Int) = startFailure(formatReason(R.string.repeater_create_group_failure, reason))
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
        debugLog(TAG, "P2P connection changed: $info\n$net\n$group")
        if (!info.groupFormed || !info.isGroupOwner || !group.isGroupOwner) {
            if (routing != null) clean()    // P2P shutdown
            return
        }
        if (routing == null) try {
            if (initRouting(upstream, group.`interface` ?: return, info.groupOwnerAddress ?: return, dns))
                doStart(group)
        } catch (e: SocketException) {
            startFailure(e.message, group)
            return
        } else showNotification(group)
        this.group = group
    }
    private fun initRouting(upstream: String?, downstream: String,
                            owner: InetAddress, dns: List<InetAddress>): Boolean {
        val routing = Routing(upstream, downstream, owner)
        this.routing = routing
        this.dns = dns
        val strict = app.pref.getBoolean("service.repeater.strict", false)
        return if (strict && upstream == null ||    // in this case, nothing to be done
                routing.ipForward()                 // Wi-Fi direct doesn't enable ip_forward
                        .rule().forward(strict).masquerade(strict).dnsRedirect(dns).start()) true else {
            routing.stop()
            Toast.makeText(this, getText(R.string.noisy_su_failure), Toast.LENGTH_SHORT).show()
            false
        }
    }

    private fun showNotification(group: WifiP2pGroup? = null) = ServiceNotification.startForeground(this,
            if (group == null) emptyMap() else mapOf(Pair(group.`interface`, group.clientList?.size ?: 0)))

    private fun removeGroup() {
        p2pManager.removeGroup(channel, object : WifiP2pManager.ActionListener {
            override fun onSuccess() = clean()
            override fun onFailure(reason: Int) {
                if (reason == WifiP2pManager.BUSY) clean() else {   // assuming it's already gone
                    Toast.makeText(this@RepeaterService,
                            formatReason(R.string.repeater_remove_group_failure, reason), Toast.LENGTH_SHORT).show()
                    status = Status.ACTIVE
                    LocalBroadcastManager.getInstance(this@RepeaterService).sendBroadcast(Intent(ACTION_STATUS_CHANGED))
                }
            }
        })
    }
    private fun unregisterReceiver() {
        if (receiverRegistered) {
            unregisterReceiver(receiver)
            LocalBroadcastManager.getInstance(this).unregisterReceiver(receiver)
            receiverRegistered = false
        }
    }
    private fun clean() {
        VpnMonitor.unregisterCallback(this)
        unregisterReceiver()
        if (routing?.stop() == false)
            Toast.makeText(this, getText(R.string.noisy_su_failure), Toast.LENGTH_SHORT).show()
        routing = null
        status = Status.IDLE
        ServiceNotification.stopForeground(this)
        stopSelf()
    }

    override fun onDestroy() {
        if (status != Status.IDLE) binder.shutdown()
        clean() // force clean to prevent leakage
        app.pref.unregisterOnSharedPreferenceChangeListener(this)
        super.onDestroy()
    }
}
