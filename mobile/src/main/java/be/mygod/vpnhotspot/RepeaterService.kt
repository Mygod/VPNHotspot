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
import android.os.Looper
import android.util.Log
import androidx.annotation.StringRes
import androidx.core.content.getSystemService
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.deletePersistentGroup
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.netId
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.requestPersistentGroupInfo
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.setWifiP2pChannels
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.startWps
import be.mygod.vpnhotspot.util.*
import be.mygod.vpnhotspot.widget.SmartSnackbar
import timber.log.Timber
import java.lang.reflect.InvocationTargetException

class RepeaterService : Service(), WifiP2pManager.ChannelListener, SharedPreferences.OnSharedPreferenceChangeListener {
    companion object {
        private const val TAG = "RepeaterService"
    }

    enum class Status {
        IDLE, STARTING, ACTIVE
    }
    private class Failure(message: String, cause: Throwable? = null) : RuntimeException(message, cause)

    inner class Binder : android.os.Binder() {
        val service get() = this@RepeaterService
        val active get() = status == Status.ACTIVE
        val statusChanged = StickyEvent0()
        val groupChanged = StickyEvent1 { group }

        private var groups: Collection<WifiP2pGroup> = emptyList()

        fun startWps(pin: String? = null) {
            if (active) p2pManager.startWps(channel, WpsInfo().apply {
                setup = if (pin == null) WpsInfo.PBC else {
                    this.pin = pin
                    WpsInfo.KEYPAD
                }
            }, object : WifiP2pManager.ActionListener {
                override fun onSuccess() = SmartSnackbar.make(
                        if (pin == null) R.string.repeater_wps_success_pbc else R.string.repeater_wps_success_keypad)
                        .shortToast().show()
                override fun onFailure(reason: Int) = SmartSnackbar.make(
                        formatReason(R.string.repeater_wps_failure, reason)).show()
            })
        }

        fun shutdown() {
            if (active) removeGroup()
        }

        fun resetCredentials() = (groups + group).filterNotNull().forEach {
            p2pManager.deletePersistentGroup(channel, it.netId, object : WifiP2pManager.ActionListener {
                override fun onSuccess() = SmartSnackbar.make(R.string.repeater_reset_credentials_success)
                        .shortToast().show()
                override fun onFailure(reason: Int) = SmartSnackbar.make(
                        formatReason(R.string.repeater_reset_credentials_failure, reason)).show()
            })
        }

        fun requestGroupUpdate() {
            group = null
            try {
                p2pManager.requestPersistentGroupInfo(channel) {
                    groups = it
                    if (it.size == 1) group = it.single()
                }
            } catch (e: ReflectiveOperationException) {
                Timber.w(e)
                SmartSnackbar.make(e.localizedMessage).show()
            }
        }
    }

    private lateinit var p2pManager: WifiP2pManager
    private lateinit var channel: WifiP2pManager.Channel
    var group: WifiP2pGroup? = null
        private set(value) {
            field = value
            binder.groupChanged(value)
        }
    private val binder = Binder()
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
        }
    }
    private var routingManager: LocalOnlyInterfaceManager? = null

    var status = Status.IDLE
        private set(value) {
            if (field == value) return
            field = value
            binder.statusChanged()
        }

    private fun formatReason(@StringRes resId: Int, reason: Int): String {
        val result = getString(resId, when (reason) {
            WifiP2pManager.ERROR -> getString(R.string.repeater_failure_reason_error)
            WifiP2pManager.P2P_UNSUPPORTED -> getString(R.string.repeater_failure_reason_p2p_unsupported)
            WifiP2pManager.BUSY -> getString(R.string.repeater_failure_reason_busy)
            WifiP2pManager.NO_SERVICE_REQUESTS -> getString(R.string.repeater_failure_reason_no_service_requests)
            WifiP2pManagerHelper.UNSUPPORTED -> getString(R.string.repeater_failure_reason_unsupported_operation)
            else -> getString(R.string.failure_reason_unknown, reason)
        })
        Timber.i(Failure(result))
        return result
    }

    override fun onCreate() {
        super.onCreate()
        try {
            p2pManager = getSystemService()!!
            onChannelDisconnected()
            app.pref.registerOnSharedPreferenceChangeListener(this)
        } catch (e: RuntimeException) {
            Timber.w(e)
        }
    }

    override fun onBind(intent: Intent) = binder

    private fun setOperatingChannel(oc: Int = app.operatingChannel) = try {
        // we don't care about listening channel
        p2pManager.setWifiP2pChannels(channel, 0, oc, object : WifiP2pManager.ActionListener {
            override fun onSuccess() { }
            override fun onFailure(reason: Int) {
                SmartSnackbar.make(formatReason(R.string.repeater_set_oc_failure, reason)).show()
            }
        })
    } catch (e: InvocationTargetException) {
        if (oc != 0) {
            val message = getString(R.string.repeater_set_oc_failure, e.message)
            SmartSnackbar.make(message).show()
            Timber.w(Failure(message, e))
        } else Timber.w(e)
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
     * startService Step 1
     */
    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (status != Status.IDLE) return START_NOT_STICKY
        status = Status.STARTING
        val out = try {
            RootSession.use { it.execOut("dumpsys ${Context.WIFI_P2P_SERVICE}") }
        } catch (e: RuntimeException) {
            Timber.w(e)
            startFailure(e.localizedMessage)
            return START_NOT_STICKY
        }
        val matcher = WifiP2pManagerHelper.patternNetworkInfo.matcher(out)
        when {
            !matcher.find() -> startFailure(out)
            matcher.group(2) == "true" -> {
                unregisterReceiver()
                registerReceiver(receiver, intentFilter(WifiP2pManager.WIFI_P2P_STATE_CHANGED_ACTION,
                        WifiP2pManager.WIFI_P2P_CONNECTION_CHANGED_ACTION))
                receiverRegistered = true
                p2pManager.requestGroupInfo(channel) {
                    when {
                        it == null -> doStart()
                        it.isGroupOwner -> if (routingManager == null) doStart(it)
                        else -> {
                            Timber.i("Removing old group ($it)")
                            p2pManager.removeGroup(channel, object : WifiP2pManager.ActionListener {
                                override fun onSuccess() = doStart()
                                override fun onFailure(reason: Int) {
                                    SmartSnackbar.make(formatReason(R.string.repeater_remove_old_group_failure, reason))
                                            .show()
                                }
                            })
                        }
                    }
                }
            }
            else -> startFailure(getString(R.string.repeater_p2p_unavailable))
        }
        return START_NOT_STICKY
    }
    /**
     * startService Step 2 (if a group isn't already available)
     */
    private fun doStart() = p2pManager.createGroup(channel, object : WifiP2pManager.ActionListener {
        override fun onFailure(reason: Int) = startFailure(formatReason(R.string.repeater_create_group_failure, reason))
        override fun onSuccess() { }    // wait for WIFI_P2P_CONNECTION_CHANGED_ACTION to fire to go to step 3
    })
    /**
     * Used during step 2, also called when connection changed
     */
    private fun onP2pConnectionChanged(info: WifiP2pInfo, net: NetworkInfo?, group: WifiP2pGroup) {
        debugLog(TAG, "P2P connection changed: $info\n$net\n$group")
        if (!info.groupFormed || !info.isGroupOwner || !group.isGroupOwner) {
            if (routingManager != null) clean()    // P2P shutdown
        } else if (routingManager != null) {
            this.group = group
            showNotification(group)
        } else doStart(group)
    }
    /**
     * startService Step 3
     */
    private fun doStart(group: WifiP2pGroup) {
        this.group = group
        check(routingManager == null)
        routingManager = LocalOnlyInterfaceManager(this, group.`interface`!!)
        status = Status.ACTIVE
        showNotification(group)
    }
    private fun startFailure(msg: CharSequence, group: WifiP2pGroup? = null) {
        SmartSnackbar.make(msg).show()
        showNotification()
        if (group != null) removeGroup() else clean()
    }

    private fun showNotification(group: WifiP2pGroup? = null) = ServiceNotification.startForeground(this,
            if (group == null) emptyMap() else mapOf(Pair(group.`interface`, group.clientList?.size ?: 0)))

    private fun removeGroup() {
        p2pManager.removeGroup(channel, object : WifiP2pManager.ActionListener {
            override fun onSuccess() = clean()
            override fun onFailure(reason: Int) {
                if (reason == WifiP2pManager.BUSY) clean() else {   // assuming it's already gone
                    SmartSnackbar.make(formatReason(R.string.repeater_remove_group_failure, reason)).show()
                    status = Status.ACTIVE
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
        unregisterReceiver()
        routingManager?.stop()
        routingManager = null
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
