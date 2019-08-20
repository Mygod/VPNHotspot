package be.mygod.vpnhotspot

import android.annotation.SuppressLint
import android.app.Service
import android.content.Intent
import android.content.SharedPreferences
import android.content.res.Configuration
import android.net.wifi.WpsInfo
import android.net.wifi.p2p.*
import android.os.Build
import android.os.Handler
import android.os.Looper
import android.provider.Settings
import androidx.annotation.RequiresApi
import androidx.annotation.StringRes
import androidx.core.content.edit
import androidx.core.content.getSystemService
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.monitor.TetherTimeoutMonitor
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.deletePersistentGroup
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.netId
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.requestPersistentGroupInfo
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.setWifiP2pChannels
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.startWps
import be.mygod.vpnhotspot.net.wifi.configuration.channelToFrequency
import be.mygod.vpnhotspot.util.*
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.*
import timber.log.Timber
import java.lang.reflect.InvocationTargetException

/**
 * Service for handling Wi-Fi P2P. `supported` must be checked before this service is started otherwise it would crash.
 */
class RepeaterService : Service(), CoroutineScope, WifiP2pManager.ChannelListener,
        SharedPreferences.OnSharedPreferenceChangeListener {
    companion object {
        private const val TAG = "RepeaterService"
        private const val KEY_NETWORK_NAME = "service.repeater.networkName"
        private const val KEY_PASSPHRASE = "service.repeater.passphrase"
        private const val KEY_OPERATING_BAND = "service.repeater.band"
        private const val KEY_OPERATING_CHANNEL = "service.repeater.oc"
        /**
         * Placeholder for bypassing networkName check.
         */
        private const val PLACEHOLDER_NETWORK_NAME = "DIRECT-00-VPNHotspot"

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
        @Deprecated("Not initialized and no use at all since API 29")
        var persistentSupported = false

        var networkName: String?
            get() = app.pref.getString(KEY_NETWORK_NAME, null)
            set(value) = app.pref.edit { putString(KEY_NETWORK_NAME, value) }
        var passphrase: String?
            get() = app.pref.getString(KEY_PASSPHRASE, null)
            set(value) = app.pref.edit { putString(KEY_PASSPHRASE, value) }
        var operatingBand: Int
            @SuppressLint("InlinedApi") get() = app.pref.getInt(KEY_OPERATING_BAND, WifiP2pConfig.GROUP_OWNER_BAND_AUTO)
            set(value) = app.pref.edit { putInt(KEY_OPERATING_BAND, value) }
        var operatingChannel: Int
            get() {
                val result = app.pref.getString(KEY_OPERATING_CHANNEL, null)?.toIntOrNull() ?: 0
                return if (result in 1..165) result else 0
            }
            set(value) = app.pref.edit { putString(KEY_OPERATING_CHANNEL, value.toString()) }
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
                if (Build.VERSION.SDK_INT >= 28) value?.clientList?.let {
                    timeoutMonitor?.onClientsChanged(it.isEmpty())
                }
            }
        val groupChanged = StickyEvent1 { group }
        @Deprecated("Not initialized and no use at all since API 29")
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
    }

    private val p2pManager get() = RepeaterService.p2pManager!!
    private var channel: WifiP2pManager.Channel? = null
    private val binder = Binder()
    private val handler = Handler()
    @RequiresApi(28)
    private var timeoutMonitor: TetherTimeoutMonitor? = null
    private var receiverRegistered = false
    private val receiver = broadcastReceiver { _, intent ->
        when (intent.action) {
            WifiP2pManager.WIFI_P2P_STATE_CHANGED_ACTION ->
                if (intent.getIntExtra(WifiP2pManager.EXTRA_WIFI_STATE, 0) ==
                        WifiP2pManager.WIFI_P2P_STATE_DISABLED) launch { cleanLocked() }    // ignore P2P enabled
            WifiP2pManager.WIFI_P2P_CONNECTION_CHANGED_ACTION -> onP2pConnectionChanged(
                    intent.getParcelableExtra(WifiP2pManager.EXTRA_WIFI_P2P_INFO)!!,
                    intent.getParcelableExtra(WifiP2pManager.EXTRA_WIFI_P2P_GROUP))
        }
    }
    @Deprecated("No longer used since API 29")
    @Suppress("DEPRECATION")
    private val deviceListener = broadcastReceiver { _, intent ->
        when (intent.action) {
            WifiP2pManager.WIFI_P2P_THIS_DEVICE_CHANGED_ACTION -> binder.thisDevice =
                    intent.getParcelableExtra(WifiP2pManager.EXTRA_WIFI_P2P_DEVICE)
            WifiP2pManagerHelper.WIFI_P2P_PERSISTENT_GROUPS_CHANGED_ACTION -> onPersistentGroupsChanged()
        }
    }
    /**
     * Writes and critical reads to routingManager should be protected with this context.
     */
    private val dispatcher = newSingleThreadContext("RepeaterService")
    override val coroutineContext = dispatcher + Job()
    private var routingManager: RoutingManager? = null
    private var persistNextGroup = false

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
        if (Build.VERSION.SDK_INT < 29) @Suppress("DEPRECATION") {
            registerReceiver(deviceListener, intentFilter(WifiP2pManager.WIFI_P2P_THIS_DEVICE_CHANGED_ACTION,
                    WifiP2pManagerHelper.WIFI_P2P_PERSISTENT_GROUPS_CHANGED_ACTION))
            app.pref.registerOnSharedPreferenceChangeListener(this)
        }
    }

    override fun onBind(intent: Intent) = binder

    @Deprecated("No longer used since API 29")
    @Suppress("DEPRECATION")
    private fun setOperatingChannel(oc: Int = operatingChannel) = try {
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
            if (Build.VERSION.SDK_INT < 29) @Suppress("DEPRECATION") setOperatingChannel()
        } catch (e: RuntimeException) {
            Timber.w(e)
            handler.postDelayed(this::onChannelDisconnected, 1000)
        }
    }

    @Deprecated("No longer used since API 29")
    @Suppress("DEPRECATION")
    override fun onSharedPreferenceChanged(sharedPreferences: SharedPreferences?, key: String?) {
        if (key == KEY_OPERATING_CHANNEL) setOperatingChannel()
    }

    @Deprecated("No longer used since API 29")
    @Suppress("DEPRECATION")
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
        val channel = channel ?: return START_NOT_STICKY.also { stopSelf() }
        status = Status.STARTING
        // show invisible foreground notification on television to avoid being killed
        if (Build.VERSION.SDK_INT >= 26 && app.uiMode.currentModeType == Configuration.UI_MODE_TYPE_TELEVISION) {
            showNotification()
        }
        launch {
            registerReceiver(receiver, intentFilter(WifiP2pManager.WIFI_P2P_STATE_CHANGED_ACTION,
                    WifiP2pManager.WIFI_P2P_CONNECTION_CHANGED_ACTION))
            receiverRegistered = true
            try {
                p2pManager.requestGroupInfo(channel) {
                    when {
                        it == null -> doStart()
                        it.isGroupOwner -> launch { if (routingManager == null) doStartLocked(it) }
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
            } catch (e: SecurityException) {
                Timber.w(e)
                startFailure(e.readableMessage)
            }
        }
        return START_NOT_STICKY
    }
    /**
     * startService Step 2 (if a group isn't already available)
     */
    private fun doStart() {
        val listener = object : WifiP2pManager.ActionListener {
            override fun onFailure(reason: Int) {
                startFailure(formatReason(R.string.repeater_create_group_failure, reason),
                        showWifiEnable = reason == WifiP2pManager.BUSY)
            }
            override fun onSuccess() { }    // wait for WIFI_P2P_CONNECTION_CHANGED_ACTION to fire to go to step 3
        }
        val channel = channel ?: return listener.onFailure(WifiP2pManager.BUSY)
        val networkName = networkName
        val passphrase = passphrase
        try {
            if (Build.VERSION.SDK_INT < 29 || networkName == null || passphrase == null) {
                persistNextGroup = true
                p2pManager.createGroup(channel, listener)
            } else p2pManager.createGroup(channel, WifiP2pConfig.Builder().apply {
                setNetworkName(PLACEHOLDER_NETWORK_NAME)
                setPassphrase(passphrase)
                operatingChannel.let { oc ->
                    if (oc == 0) setGroupOperatingBand(operatingBand)
                    else setGroupOperatingFrequency(channelToFrequency(oc))
                }
            }.build().run {
                useParcel { p ->
                    p.writeParcelable(this, 0)
                    val end = p.dataPosition()
                    p.setDataPosition(0)
                    val creator = p.readString()
                    val deviceAddress = p.readString()
                    val wps = p.readParcelable<WpsInfo>(javaClass.classLoader)
                    val long = p.readLong()
                    check(p.readString() == PLACEHOLDER_NETWORK_NAME)
                    check(p.readString() == passphrase)
                    val int = p.readInt()
                    check(p.dataPosition() == end)
                    p.setDataPosition(0)
                    p.writeString(creator)
                    p.writeString(deviceAddress)
                    p.writeParcelable(wps, 0)
                    p.writeLong(long)
                    p.writeString(networkName)
                    p.writeString(passphrase)
                    p.writeInt(int)
                    p.setDataPosition(0)
                    p.readParcelable<WifiP2pConfig>(javaClass.classLoader)
                }
            }, listener)
        } catch (e: SecurityException) {
            Timber.w(e)
            startFailure(e.readableMessage)
        }
    }
    /**
     * Used during step 2, also called when connection changed
     */
    private fun onP2pConnectionChanged(info: WifiP2pInfo, group: WifiP2pGroup?) = launch {
        DebugHelper.log(TAG, "P2P connection changed: $info\n$group")
        when {
            !info.groupFormed || !info.isGroupOwner || group?.isGroupOwner != true -> {
                if (routingManager != null) cleanLocked()
                // P2P shutdown, else other groups changing before start, ignore
            }
            routingManager != null -> {
                binder.group = group
                showNotification(group)
            }
            else -> doStartLocked(group)
        }
    }
    /**
     * startService Step 3
     */
    private fun doStartLocked(group: WifiP2pGroup) {
        if (Build.VERSION.SDK_INT >= 28) timeoutMonitor = TetherTimeoutMonitor(this, handler, binder::shutdown)
        binder.group = group
        if (persistNextGroup) {
            networkName = group.networkName
            passphrase = group.passphrase
            persistNextGroup = false
        }
        check(routingManager == null)
        routingManager = RoutingManager.LocalOnly(this, group.`interface`!!).apply { start() }
        status = Status.ACTIVE
        showNotification(group)
    }
    private fun startFailure(msg: CharSequence, group: WifiP2pGroup? = null, showWifiEnable: Boolean = false) {
        SmartSnackbar.make(msg).apply {
            if (showWifiEnable) action(R.string.repeater_p2p_unavailable_enable) {
                if (Build.VERSION.SDK_INT >= 29) it.context.startActivity(Intent(Settings.Panel.ACTION_WIFI))
                else @Suppress("DEPRECATION") app.wifi.isWifiEnabled = true
            }
        }.show()
        showNotification()
        if (group != null) removeGroup() else launch { cleanLocked() }
    }

    private fun showNotification(group: WifiP2pGroup? = null) = ServiceNotification.startForeground(this,
            if (group == null) emptyMap() else mapOf(Pair(group.`interface`, group.clientList?.size ?: 0)))

    private fun removeGroup() {
        p2pManager.removeGroup(channel, object : WifiP2pManager.ActionListener {
            override fun onSuccess() {
                launch { cleanLocked() }
            }
            override fun onFailure(reason: Int) {
                if (reason != WifiP2pManager.BUSY) {
                    SmartSnackbar.make(formatReason(R.string.repeater_remove_group_failure, reason)).show()
                }   // else assuming it's already gone
                launch { cleanLocked() }
            }
        })
    }
    private fun cleanLocked() {
        if (receiverRegistered) {
            unregisterReceiver(receiver)
            receiverRegistered = false
        }
        if (Build.VERSION.SDK_INT >= 28) {
            timeoutMonitor?.close()
            timeoutMonitor = null
        }
        routingManager?.destroy()
        routingManager = null
        status = Status.IDLE
        ServiceNotification.stopForeground(this)
        stopSelf()
    }

    override fun onDestroy() {
        handler.removeCallbacksAndMessages(null)
        if (status != Status.IDLE) binder.shutdown()
        launch {    // force clean to prevent leakage
            cleanLocked()
            cancel()
            dispatcher.close()
        }
        if (Build.VERSION.SDK_INT < 29) @Suppress("DEPRECATION") {
            app.pref.unregisterOnSharedPreferenceChangeListener(this)
            unregisterReceiver(deviceListener)
        }
        status = Status.DESTROYED
        if (Build.VERSION.SDK_INT >= 27) channel?.close()
        super.onDestroy()
    }
}
