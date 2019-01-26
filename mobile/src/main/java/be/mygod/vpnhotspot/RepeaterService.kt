package be.mygod.vpnhotspot

import android.app.Service
import android.content.Intent
import android.content.SharedPreferences
import android.content.res.Configuration
import android.net.NetworkInfo
import android.net.wifi.WpsInfo
import android.net.wifi.p2p.WifiP2pDevice
import android.net.wifi.p2p.WifiP2pGroup
import android.net.wifi.p2p.WifiP2pInfo
import android.net.wifi.p2p.WifiP2pManager
import android.os.Build
import android.os.Handler
import android.os.Looper
import androidx.annotation.StringRes
import androidx.core.content.getSystemService
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.wifi.WifiDoubleLock
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.deletePersistentGroup
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.netId
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.requestPersistentGroupInfo
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.setWifiP2pChannels
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.startWps
import be.mygod.vpnhotspot.util.StickyEvent0
import be.mygod.vpnhotspot.util.StickyEvent1
import be.mygod.vpnhotspot.util.broadcastReceiver
import be.mygod.vpnhotspot.util.intentFilter
import be.mygod.vpnhotspot.widget.SmartSnackbar
import timber.log.Timber
import java.lang.reflect.InvocationTargetException

/**
 * Service for handling Wi-Fi P2P. `supported` must be checked before this service is started otherwise it would crash.
 */
class RepeaterService : Service(), WifiP2pManager.ChannelListener, SharedPreferences.OnSharedPreferenceChangeListener {
    companion object {
        private const val TAG = "RepeaterService"
        /**
         * This is only a "ServiceConnection" to system service and its impact on system is minimal.
         */
        private val p2pManager: WifiP2pManager? by lazy {
            try {
                app.getSystemService<WifiP2pManager>()
            } catch (e: RuntimeException) {
                Timber.w(e)
                null
            }
        }
        val supported get() = p2pManager != null
        var persistentSupported = false
    }

    enum class Status {
        IDLE, STARTING, ACTIVE, DESTROYED
    }

    inner class Binder : android.os.Binder() {
        val service get() = this@RepeaterService
        val active get() = status == Status.ACTIVE
        val statusChanged = StickyEvent0()
        var group: WifiP2pGroup? = null
            set(value) {
                field = value
                groupChanged(value)
            }
        val groupChanged = StickyEvent1 { group }
        var thisDevice: WifiP2pDevice? = null

        fun startWps(pin: String? = null) {
            val channel = channel
            if (channel == null) SmartSnackbar.make(R.string.repeater_failure_disconnected).show()
            else if (active) p2pManager.startWps(channel, WpsInfo().apply {
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

        fun resetCredentials() {
            val channel = channel
            if (channel == null) SmartSnackbar.make(R.string.repeater_failure_disconnected).show()
            else p2pManager.deletePersistentGroup(channel, (group ?: return).netId,
                    object : WifiP2pManager.ActionListener {
                        override fun onSuccess() = SmartSnackbar.make(R.string.repeater_reset_credentials_success)
                                .shortToast().show()
                        override fun onFailure(reason: Int) = SmartSnackbar.make(
                                formatReason(R.string.repeater_reset_credentials_failure, reason)).show()
                    })
        }
    }

    private val p2pManager get() = RepeaterService.p2pManager!!
    private var channel: WifiP2pManager.Channel? = null
    private val binder = Binder()
    private val handler = Handler()
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
    private val deviceListener = broadcastReceiver { _, intent ->
        when (intent.action) {
            WifiP2pManager.WIFI_P2P_THIS_DEVICE_CHANGED_ACTION -> binder.thisDevice =
                    intent.getParcelableExtra(WifiP2pManager.EXTRA_WIFI_P2P_DEVICE)
            WifiP2pManagerHelper.WIFI_P2P_PERSISTENT_GROUPS_CHANGED_ACTION -> onPersistentGroupsChanged()
        }
    }
    private var routingManager: LocalOnlyInterfaceManager? = null
    private var locked = false

    var status = Status.IDLE
        private set(value) {
            if (field == value) return
            field = value
            binder.statusChanged()
        }

    private fun formatReason(@StringRes resId: Int, reason: Int) = getString(resId, when (reason) {
        WifiP2pManager.ERROR -> getString(R.string.repeater_failure_reason_error)
        WifiP2pManager.P2P_UNSUPPORTED -> getString(R.string.repeater_failure_reason_p2p_unsupported)
        // we don't ever need to use discovering ever so busy must mean P2pStateMachine is in invalid state
        WifiP2pManager.BUSY -> getString(R.string.repeater_p2p_unavailable)
        // this should never be used
        WifiP2pManager.NO_SERVICE_REQUESTS -> getString(R.string.repeater_failure_reason_no_service_requests)
        WifiP2pManagerHelper.UNSUPPORTED -> getString(R.string.repeater_failure_reason_unsupported_operation)
        else -> getString(R.string.failure_reason_unknown, reason)
    })

    override fun onCreate() {
        super.onCreate()
        onChannelDisconnected()
        registerReceiver(deviceListener, intentFilter(WifiP2pManager.WIFI_P2P_THIS_DEVICE_CHANGED_ACTION,
                WifiP2pManagerHelper.WIFI_P2P_PERSISTENT_GROUPS_CHANGED_ACTION))
        app.pref.registerOnSharedPreferenceChangeListener(this)
    }

    override fun onBind(intent: Intent) = binder

    private fun setOperatingChannel(oc: Int = app.operatingChannel) = try {
        val channel = channel
        if (channel == null) SmartSnackbar.make(R.string.repeater_failure_disconnected).show()
        // we don't care about listening channel
        else p2pManager.setWifiP2pChannels(channel, 0, oc, object : WifiP2pManager.ActionListener {
            override fun onSuccess() { }
            override fun onFailure(reason: Int) {
                SmartSnackbar.make(formatReason(R.string.repeater_set_oc_failure, reason)).show()
            }
        })
    } catch (e: InvocationTargetException) {
        if (oc != 0) {
            val message = getString(R.string.repeater_set_oc_failure, e.message)
            SmartSnackbar.make(message).show()
            Timber.w(RuntimeException("Failed to set operating channel $oc", e))
        } else Timber.w(e)
    }

    override fun onChannelDisconnected() {
        channel = null
        if (status != Status.DESTROYED) try {
            channel = p2pManager.initialize(this, Looper.getMainLooper(), this)
            setOperatingChannel()
        } catch (e: RuntimeException) {
            Timber.w(e)
            handler.postDelayed(this::onChannelDisconnected, 1000)
        }
    }

    override fun onSharedPreferenceChanged(sharedPreferences: SharedPreferences?, key: String?) {
        if (key == App.KEY_OPERATING_CHANNEL) setOperatingChannel()
    }

    private fun onPersistentGroupsChanged() {
        val channel = channel ?: return
        val device = binder.thisDevice ?: return
        try {
            p2pManager.requestPersistentGroupInfo(channel) {
                if (it.isNotEmpty()) persistentSupported = true
                val ownedGroups = it.filter { it.isGroupOwner && it.owner.deviceAddress == device.deviceAddress }
                val main = ownedGroups.minBy { it.netId }
                // do not replace current group if it's better
                if (binder.group?.passphrase == null) binder.group = main
                if (main != null) ownedGroups.filter { it.netId != main.netId }.forEach {
                    p2pManager.deletePersistentGroup(channel, it.netId, object : WifiP2pManager.ActionListener {
                        override fun onSuccess() = Timber.i("Removed redundant owned group: $it")
                        override fun onFailure(reason: Int) = SmartSnackbar.make(
                                formatReason(R.string.repeater_clean_pog_failure, reason)).show()
                    })
                }
            }
        } catch (e: ReflectiveOperationException) {
            Timber.w(e)
            SmartSnackbar.make(e).show()
        }
    }

    /**
     * startService Step 1
     */
    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (status != Status.IDLE) return START_NOT_STICKY
        status = Status.STARTING
        // show invisible foreground notification on television to avoid being killed
        if (Build.VERSION.SDK_INT >= 26 && app.uiMode.currentModeType == Configuration.UI_MODE_TYPE_TELEVISION) {
            showNotification()
        }
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
                        override fun onFailure(reason: Int) =
                                startFailure(formatReason(R.string.repeater_remove_old_group_failure, reason))
                    })
                }
            }
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
        DebugHelper.log(TAG, "P2P connection changed: $info\n$net\n$group")
        when {
            !info.groupFormed || !info.isGroupOwner || !group.isGroupOwner -> {
                if (routingManager != null) clean() // P2P shutdown, else other groups changing before start, ignore
            }
            routingManager != null -> {
                binder.group = group
                showNotification(group)
            }
            else -> doStart(group)
        }
    }
    /**
     * startService Step 3
     */
    private fun doStart(group: WifiP2pGroup) {
        check(!locked)
        WifiDoubleLock.acquire()
        locked = true
        binder.group = group
        check(routingManager == null)
        routingManager = LocalOnlyInterfaceManager(group.`interface`!!)
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
                if (reason != WifiP2pManager.BUSY) {
                    SmartSnackbar.make(formatReason(R.string.repeater_remove_group_failure, reason)).show()
                }   // else assuming it's already gone
                clean()
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
        if (locked) {
            WifiDoubleLock.release()
            locked = false
        }
        status = Status.IDLE
        ServiceNotification.stopForeground(this)
        stopSelf()
    }

    override fun onDestroy() {
        handler.removeCallbacksAndMessages(null)
        if (status != Status.IDLE) binder.shutdown()
        clean() // force clean to prevent leakage
        app.pref.unregisterOnSharedPreferenceChangeListener(this)
        unregisterReceiver(deviceListener)
        status = Status.DESTROYED
        if (Build.VERSION.SDK_INT >= 27) channel?.close()
        super.onDestroy()
    }
}
