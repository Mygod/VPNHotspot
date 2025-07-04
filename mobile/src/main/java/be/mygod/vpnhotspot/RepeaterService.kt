package be.mygod.vpnhotspot

import android.annotation.SuppressLint
import android.annotation.TargetApi
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.SharedPreferences
import android.content.pm.PackageManager
import android.location.LocationManager
import android.net.MacAddress
import android.net.wifi.ScanResult
import android.net.wifi.SoftApConfiguration
import android.net.wifi.WpsInfo
import android.net.wifi.p2p.WifiP2pConfig
import android.net.wifi.p2p.WifiP2pDevice
import android.net.wifi.p2p.WifiP2pGroup
import android.net.wifi.p2p.WifiP2pInfo
import android.net.wifi.p2p.WifiP2pManager
import android.os.Build
import android.os.Looper
import android.provider.Settings
import androidx.annotation.RequiresApi
import androidx.annotation.StringRes
import androidx.core.content.edit
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.MacAddressCompat
import be.mygod.vpnhotspot.net.MacAddressCompat.Companion.toLong
import be.mygod.vpnhotspot.net.monitor.TetherTimeoutMonitor
import be.mygod.vpnhotspot.net.wifi.SoftApConfigurationCompat
import be.mygod.vpnhotspot.net.wifi.VendorElements
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.deletePersistentGroup
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.requestConnectionInfo
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.requestDeviceAddress
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.requestGroupInfo
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.requestP2pState
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.requestPersistentGroupInfo
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.setVendorElements
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.setWifiP2pChannels
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.startWps
import be.mygod.vpnhotspot.net.wifi.WifiSsidCompat
import be.mygod.vpnhotspot.root.RepeaterCommands
import be.mygod.vpnhotspot.root.RootManager
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.StickyEvent0
import be.mygod.vpnhotspot.util.StickyEvent1
import be.mygod.vpnhotspot.util.TileServiceDismissHandle
import be.mygod.vpnhotspot.util.UnblockCentral
import be.mygod.vpnhotspot.util.broadcastReceiver
import be.mygod.vpnhotspot.util.ensureReceiverUnregistered
import be.mygod.vpnhotspot.util.intentFilter
import be.mygod.vpnhotspot.util.readableMessage
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.async
import kotlinx.coroutines.cancel
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.parcelize.Parcelize
import timber.log.Timber
import java.lang.ref.WeakReference
import java.lang.reflect.InvocationTargetException
import java.util.concurrent.atomic.AtomicBoolean

/**
 * Service for handling Wi-Fi P2P. `supported` must be checked before this service is started otherwise it would crash.
 */
class RepeaterService : Service(), CoroutineScope, SharedPreferences.OnSharedPreferenceChangeListener {
    companion object {
        const val KEY_SAFE_MODE = "service.repeater.safeMode"

        private const val KEY_NETWORK_NAME = "service.repeater.networkName"
        private const val KEY_NETWORK_NAME_HEX = "service.repeater.networkNameHex"
        private const val KEY_PASSPHRASE = "service.repeater.passphrase"
        private const val KEY_OPERATING_BAND = "service.repeater.band.v4"
        private const val KEY_OPERATING_CHANNEL = "service.repeater.oc.v3"
        private const val KEY_AUTO_SHUTDOWN = "service.repeater.autoShutdown"
        private const val KEY_SHUTDOWN_TIMEOUT = "service.repeater.shutdownTimeout"
        private const val KEY_DEVICE_ADDRESS = "service.repeater.mac"
        private const val KEY_VENDOR_ELEMENTS = "service.repeater.vendorElements"
        private const val KEY_PCC_MODE_CONNECTION_TYPE = "service.repeater.pccModeConnectionType"

        var persistentSupported = false

        @get:RequiresApi(29)
        private val hasP2pValidateName by lazy {
            val array = Build.VERSION.SECURITY_PATCH.split('-', limit = 3)
            val y = array.getOrNull(0)?.toIntOrNull()
            val m = array.getOrNull(1)?.toIntOrNull()
            y == null || y > 2020 || y == 2020 && (m == null || m >= 3)
        }
        val safeModeConfigurable get() = Build.VERSION.SDK_INT >= 29 && hasP2pValidateName
        val safeMode get() = Build.VERSION.SDK_INT >= 29 &&
                (!hasP2pValidateName || app.pref.getBoolean(KEY_SAFE_MODE, true))

        var networkName: WifiSsidCompat?
            get() = app.pref.getString(KEY_NETWORK_NAME, null).let { legacy ->
                if (legacy != null) WifiSsidCompat.fromUtf8Text(legacy).also {
                    app.pref.edit {
                        putString(KEY_NETWORK_NAME_HEX, it!!.hex)
                        remove(KEY_NETWORK_NAME)
                    }
                } else WifiSsidCompat.fromHex(app.pref.getString(KEY_NETWORK_NAME_HEX, null))
            }
            set(value) = app.pref.edit { putString(KEY_NETWORK_NAME_HEX, value?.hex) }
        var passphrase: String?
            get() = app.pref.getString(KEY_PASSPHRASE, null)
            set(value) = app.pref.edit { putString(KEY_PASSPHRASE, value) }
        var operatingBand: Int
            @SuppressLint("InlinedApi")
            get() = app.pref.getInt(KEY_OPERATING_BAND, if (Build.VERSION.SDK_INT >= 36) {
                SoftApConfigurationCompat.BAND_ANY_30
            } else SoftApConfigurationCompat.BAND_LEGACY) and SoftApConfigurationCompat.BAND_ANY_30
            set(value) = app.pref.edit { putInt(KEY_OPERATING_BAND, value) }
        var operatingChannel: Int
            get() {
                val result = app.pref.getString(KEY_OPERATING_CHANNEL, null)?.toIntOrNull() ?: 0
                return if (result > 0) result else 0
            }
            set(value) = app.pref.edit { putString(KEY_OPERATING_CHANNEL, value.toString()) }
        var isAutoShutdownEnabled: Boolean
            get() = app.pref.getBoolean(KEY_AUTO_SHUTDOWN, false)
            set(value) = app.pref.edit { putBoolean(KEY_AUTO_SHUTDOWN, value) }
        var shutdownTimeoutMillis: Long
            get() = app.pref.getLong(KEY_SHUTDOWN_TIMEOUT, 0)
            set(value) = app.pref.edit { putLong(KEY_SHUTDOWN_TIMEOUT, value) }
        var deviceAddress: MacAddress?
            get() = try {
                MacAddressCompat(app.pref.getLong(KEY_DEVICE_ADDRESS, 2)).run {
                    require(addr and ((1L shl 48) - 1).inv() == 0L)
                    if (addr == 2L) null else toPlatform()
                }
            } catch (e: IllegalArgumentException) {
                Timber.w(e)
                null
            }
            set(value) = app.pref.edit {
                putLong(KEY_DEVICE_ADDRESS, (value ?: MacAddressCompat.ANY_ADDRESS).toLong())
            }
        @get:RequiresApi(33)
        @set:RequiresApi(33)
        var vendorElements: List<ScanResult.InformationElement>
            get() = VendorElements.deserialize(app.pref.getString(KEY_VENDOR_ELEMENTS, null))
            set(value) = app.pref.edit { putString(KEY_VENDOR_ELEMENTS, VendorElements.serialize(value)) }
        @get:RequiresApi(36)
        @set:RequiresApi(36)
        var pccModeConnectionType: Int
            get() = app.pref.getInt(KEY_PCC_MODE_CONNECTION_TYPE, WifiP2pConfig.PCC_MODE_CONNECTION_TYPE_LEGACY_ONLY)
            set(value) = app.pref.edit { putInt(KEY_PCC_MODE_CONNECTION_TYPE, value) }
        var securityType: Int
            get() = if (Build.VERSION.SDK_INT >= 36) {
                pccModeConnectionType + SoftApConfiguration.SECURITY_TYPE_WPA2_PSK
            } else SoftApConfiguration.SECURITY_TYPE_WPA2_PSK
            set(value) {
                if (Build.VERSION.SDK_INT >= 36) {
                    pccModeConnectionType = value - SoftApConfiguration.SECURITY_TYPE_WPA2_PSK
                } else if (value != SoftApConfiguration.SECURITY_TYPE_WPA2_PSK) throw UnsupportedOperationException()
            }

        var dismissHandle: TileServiceDismissHandle? = null
        private fun dismissIfApplicable() = dismissHandle?.run {
            get()?.dismiss()
            dismissHandle = null
        }
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
                value?.clientList?.let { timeoutMonitor?.onClientsChanged(it.isEmpty()) }
            }
        val groupChanged = StickyEvent1 { group }

        suspend fun obtainDeviceAddress(): MacAddress? {
            return if (Build.VERSION.SDK_INT >= 29) p2pManager.requestDeviceAddress(channel ?: return null) ?: try {
                RootManager.use { it.execute(RepeaterCommands.RequestDeviceAddress()) }
            } catch (e: Exception) {
                Timber.d(e)
                null
            } else lastMac?.let { MacAddress.fromString(it) }
        }

        @SuppressLint("NewApi") // networkId is available since Android 4.2
        suspend fun fetchPersistentGroup() {
            val ownerAddress = obtainDeviceAddress() ?: return
            val channel = channel ?: return
            fun Collection<WifiP2pGroup>.filterUselessGroups(): List<WifiP2pGroup> {
                if (isNotEmpty()) persistentSupported = true
                val ownedGroups = filter {
                    if (!it.isGroupOwner) return@filter false
                    val address = try {
                        MacAddress.fromString(it.owner.deviceAddress)
                    } catch (e: IllegalArgumentException) {
                        Timber.w(e)
                        return@filter true  // assuming it was changed due to privacy
                    }
                    // WifiP2pServiceImpl only removes self address
                    Build.VERSION.SDK_INT >= 29 && address == MacAddressCompat.ANY_ADDRESS || address == ownerAddress
                }
                val main = ownedGroups.minByOrNull { it.networkId }
                // do not replace current group if it's better
                if (binder.group?.passphrase == null) binder.group = main
                return if (main != null) ownedGroups.filter { it.networkId != main.networkId } else emptyList()
            }
            fun Int?.print(group: WifiP2pGroup) {
                if (this == null) Timber.i("Removed redundant owned group: $group")
                else SmartSnackbar.make(formatReason(R.string.repeater_clean_pog_failure, this)).show()
            }
            // we only get empty list on permission denial. Is there a better permission check?
            if (Build.VERSION.SDK_INT < 30 || checkSelfPermission("android.permission.READ_WIFI_CREDENTIAL") ==
                    PackageManager.PERMISSION_GRANTED) try {
                for (group in p2pManager.requestPersistentGroupInfo(channel).filterUselessGroups()) {
                    p2pManager.deletePersistentGroup(channel, group.networkId).print(group)
                }
                return
            } catch (e: ReflectiveOperationException) {
                Timber.w(e)
            }
            try {
                RootManager.use { server ->
                    if (deinitPending.getAndSet(false)) server.execute(RepeaterCommands.Deinit())
                    @Suppress("UNCHECKED_CAST")
                    val groups = server.execute(RepeaterCommands.RequestPersistentGroupInfo())
                            .value as List<WifiP2pGroup>
                    for (group in groups.filterUselessGroups()) {
                        server.execute(RepeaterCommands.DeletePersistentGroup(group.networkId))?.value.print(group)
                    }
                }
            } catch (e: Exception) {
                Timber.w(e)
                SmartSnackbar.make(e).show()
            }
        }

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

    @Parcelize
    class Starter : BootReceiver.Startable {
        override fun start(context: Context) {
            context.startForegroundService(Intent(context, RepeaterService::class.java))
        }
    }

    private val p2pManager get() = Services.p2p!!
    private var channel: WifiP2pManager.Channel? = null
    private val binder = Binder()
    private var timeoutMonitor: TetherTimeoutMonitor? = null
    private var receiverRegistered = false
    private val receiver = broadcastReceiver { _, intent ->
        when (intent.action) {
            WifiP2pManager.WIFI_P2P_STATE_CHANGED_ACTION ->
                if (intent.getIntExtra(WifiP2pManager.EXTRA_WIFI_STATE, 0) ==
                        WifiP2pManager.WIFI_P2P_STATE_DISABLED) launch { cleanLocked() }    // ignore P2P enabled
            WifiP2pManager.WIFI_P2P_CONNECTION_CHANGED_ACTION -> onP2pConnectionChanged(
                    intent.getParcelableExtra(WifiP2pManager.EXTRA_WIFI_P2P_INFO),
                    intent.getParcelableExtra(WifiP2pManager.EXTRA_WIFI_P2P_GROUP))
            LocationManager.MODE_CHANGED_ACTION -> @TargetApi(30) {
                onLocationModeChanged(intent.getBooleanExtra(LocationManager.EXTRA_LOCATION_ENABLED, false))
            }
        }
    }
    private val deviceListener = broadcastReceiver { _, intent ->
        val addr = intent.getParcelableExtra<WifiP2pDevice>(WifiP2pManager.EXTRA_WIFI_P2P_DEVICE)?.deviceAddress
        if (!addr.isNullOrEmpty()) lastMac = addr
    }
    private var lastMac: String? = null
    /**
     * Writes and critical reads to routingManager should be protected with this context.
     */
    private val dispatcher = Dispatchers.Default.limitedParallelism(1, "RepeaterService")
    override val coroutineContext = dispatcher + Job()
    private var routingManager: RoutingManager? = null
    private var persistNextGroup = false
    private val deinitPending = AtomicBoolean(true)

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
        initializeChannel()
        if (Build.VERSION.SDK_INT < 29) {
            registerReceiver(deviceListener, intentFilter(WifiP2pManager.WIFI_P2P_THIS_DEVICE_CHANGED_ACTION))
        }
        app.pref.registerOnSharedPreferenceChangeListener(this)
    }

    override fun onBind(intent: Intent) = binder

    private suspend fun setOperatingChannel(oc: Int = operatingChannel) {
        val channel = channel
        if (channel != null) {
            val reason = try {
                // we don't care about listening channel
                p2pManager.setWifiP2pChannels(channel, 0, oc) ?: return
            } catch (e: InvocationTargetException) {
                if (oc != 0) {
                    val message = getString(R.string.repeater_set_oc_failure, e.message)
                    SmartSnackbar.make(message).show()
                    Timber.w(RuntimeException("Failed to set operating channel $oc", e))
                } else Timber.w(e)
                return
            }
            if (reason == WifiP2pManager.ERROR && Build.VERSION.SDK_INT >= 30) {
                val rootReason = try {
                    RootManager.use {
                        if (deinitPending.getAndSet(false)) it.execute(RepeaterCommands.Deinit())
                        it.execute(RepeaterCommands.SetChannel(oc))
                    }
                } catch (e: Exception) {
                    Timber.w(e)
                    SmartSnackbar.make(e).show()
                    null
                } ?: return
                SmartSnackbar.make(formatReason(R.string.repeater_set_oc_failure, rootReason.value)).show()
            } else SmartSnackbar.make(formatReason(R.string.repeater_set_oc_failure, reason)).show()
        } else SmartSnackbar.make(R.string.repeater_failure_disconnected).show()
    }
    @RequiresApi(33)
    private suspend fun setVendorElements(ve: List<ScanResult.InformationElement> = vendorElements) {
        val channel = channel
        if (channel != null) {
            val reason = try {
                p2pManager.setVendorElements(channel, ve) ?: return
            } catch (e: IllegalArgumentException) {
                SmartSnackbar.make(getString(R.string.repeater_set_vendor_elements_failure, e.message)).show()
                return
            } catch (e: UnsupportedOperationException) {
                if (ve.isNotEmpty()) {
                    SmartSnackbar.make(getString(R.string.repeater_set_vendor_elements_failure, e.message)).show()
                }
                return
            }
            if (reason == WifiP2pManager.ERROR) {
                val rootReason = try {
                    RootManager.use {
                        if (deinitPending.getAndSet(false)) it.execute(RepeaterCommands.Deinit())
                        it.execute(RepeaterCommands.SetVendorElements(ve))
                    } ?: return
                } catch (e: Exception) {
                    Timber.w(e)
                    SmartSnackbar.make(e).show()
                    return
                }
                SmartSnackbar.make(formatReason(R.string.repeater_set_vendor_elements_failure, rootReason.value)).show()
            } else SmartSnackbar.make(formatReason(R.string.repeater_set_vendor_elements_failure, reason)).show()
        } else SmartSnackbar.make(R.string.repeater_failure_disconnected).show()
    }

    private fun initializeChannel() {
        channel = null
        deinitPending.set(true)
        if (status != Status.DESTROYED) try {
            // WifiP2pManager.Channel uses AsyncChannel which is leaky, prevent holding onto the Context
            val ref = WeakReference(this)
            channel = p2pManager.initialize(app, Looper.getMainLooper()) { ref.get()?.initializeChannel() }
        } catch (e: RuntimeException) {
            Timber.w(e)
            launch(Dispatchers.Main) {
                delay(1000)
                initializeChannel()
            }
        }
    }

    override fun onSharedPreferenceChanged(sharedPreferences: SharedPreferences?, key: String?) {
        when (key) {
            KEY_OPERATING_CHANNEL -> if (!safeMode) launch { setOperatingChannel() }
            KEY_VENDOR_ELEMENTS -> if (Build.VERSION.SDK_INT >= 33) launch { setVendorElements() }
            KEY_SAFE_MODE -> if (!safeMode) deinitPending.set(true)
        }
    }

    private var p2pPoller: Job? = null
    @RequiresApi(30)
    private fun onLocationModeChanged(enabled: Boolean) = if (enabled) p2pPoller?.cancel() else {
        SmartSnackbar.make(R.string.repeater_location_off).apply {
            action(R.string.repeater_location_off_configure) {
                it.context.startActivity(Intent(Settings.ACTION_LOCATION_SOURCE_SETTINGS))
            }
        }.show()
        p2pPoller = launch(start = CoroutineStart.UNDISPATCHED) {
            while (true) {
                delay(1000)
                val channel = channel ?: return@launch
                coroutineScope {
                    launch(start = CoroutineStart.UNDISPATCHED) {
                        if (p2pManager.requestP2pState(channel) == WifiP2pManager.WIFI_P2P_STATE_DISABLED) cleanLocked()
                    }
                    val info = async(start = CoroutineStart.UNDISPATCHED) { p2pManager.requestConnectionInfo(channel) }
                    val group = p2pManager.requestGroupInfo(channel)
                    onP2pConnectionChanged(info.await(), group)
                }
            }
        }
    }

    /**
     * startService Step 1
     */
    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        BootReceiver.startIfEnabled()
        if (status != Status.IDLE) return START_NOT_STICKY
        val channel = channel ?: return START_NOT_STICKY.also { stopSelf() }
        status = Status.STARTING
        // bump self to foreground location service (API 29+) to use location later, also to avoid getting killed
        showNotification()
        launch {
            val filter = intentFilter(WifiP2pManager.WIFI_P2P_STATE_CHANGED_ACTION,
                WifiP2pManager.WIFI_P2P_CONNECTION_CHANGED_ACTION)
            if (Build.VERSION.SDK_INT in 30 until 33) filter.addAction(LocationManager.MODE_CHANGED_ACTION)
            registerReceiver(receiver, filter)
            receiverRegistered = true
            val group = p2pManager.requestGroupInfo(channel)
            when {
                group == null -> doStart()
                group.isGroupOwner -> if (routingManager == null) doStartLocked(group)
                else -> {
                    Timber.i("Removing old group ($group)")
                    p2pManager.removeGroup(channel, object : WifiP2pManager.ActionListener {
                        override fun onSuccess() {
                            launch { doStart() }
                        }
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
    @SuppressLint("MissingPermission")  // missing permission will simply leading to returning ERROR
    private suspend fun doStart() {
        val listener = object : WifiP2pManager.ActionListener {
            override fun onFailure(reason: Int) {
                startFailure(formatReason(R.string.repeater_create_group_failure, reason),
                        showWifiEnable = reason == WifiP2pManager.BUSY)
            }
            override fun onSuccess() {
                // wait for WIFI_P2P_CONNECTION_CHANGED_ACTION to fire to go to step 3
                // in order for this to happen, we need to make sure that the callbacks are firing
                if (Build.VERSION.SDK_INT in 30 until 33) onLocationModeChanged(app.location?.isLocationEnabled == true)
            }
        }
        val channel = channel ?: return listener.onFailure(WifiP2pManager.BUSY)
        if (!safeMode) {
            binder.fetchPersistentGroup()
            setOperatingChannel()
        }
        if (Build.VERSION.SDK_INT >= 33) setVendorElements()
        val networkName = networkName?.toString()
        val passphrase = passphrase
        if (!safeMode || networkName == null || passphrase.isNullOrEmpty()) {
            persistNextGroup = true
            p2pManager.createGroup(channel, listener)
        } else @TargetApi(29) {
            p2pManager.createGroup(channel, WifiP2pConfig.Builder().apply {
                try {   // bypass networkName check
                    UnblockCentral.WifiP2pConfig_Builder_mNetworkName.set(this, networkName)
                } catch (e: ReflectiveOperationException) {
                    Timber.w(e)
                    try {
                        setNetworkName(networkName)
                    } catch (e: IllegalArgumentException) {
                        Timber.w(e)
                        return startFailure(e.readableMessage)
                    }
                }
                setPassphrase(passphrase)
                when (val oc = operatingChannel) {
                    0 -> setGroupOperatingBand(when (val band = operatingBand) {
                        SoftApConfiguration.BAND_2GHZ -> WifiP2pConfig.GROUP_OWNER_BAND_2GHZ
                        SoftApConfiguration.BAND_5GHZ -> WifiP2pConfig.GROUP_OWNER_BAND_5GHZ
                        SoftApConfiguration.BAND_6GHZ -> WifiP2pConfig.GROUP_OWNER_BAND_6GHZ
                        else -> {
                            require(SoftApConfigurationCompat.isLegacyEitherBand(band)) { "Unknown band $band" }
                            WifiP2pConfig.GROUP_OWNER_BAND_AUTO
                        }
                    })
                    else -> {
                        setGroupOperatingFrequency(SoftApConfigurationCompat.channelToFrequency(operatingBand, oc))
                    }
                }
                setDeviceAddress(deviceAddress)
                if (Build.VERSION.SDK_INT >= 36) setPccModeConnectionType(pccModeConnectionType)
            }.build(), listener)
        }
    }
    /**
     * Used during step 2, also called when connection changed
     */
    private fun onP2pConnectionChanged(info: WifiP2pInfo?, group: WifiP2pGroup?) = launch {
        Timber.d("P2P connection changed: $info\n$group")
        when {
            info?.groupFormed != true || !info.isGroupOwner || group?.isGroupOwner != true -> {
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
        if (isAutoShutdownEnabled) timeoutMonitor = TetherTimeoutMonitor(shutdownTimeoutMillis, coroutineContext) {
            binder.shutdown()
        }
        binder.group = group
        if (persistNextGroup) {
            networkName = WifiSsidCompat.fromUtf8Text(group.networkName)
            passphrase = group.passphrase
            persistNextGroup = false
        }
        check(routingManager == null)
        routingManager = RoutingManager.LocalOnly(this@RepeaterService, group.`interface`!!).apply { start() }
        status = Status.ACTIVE
        showNotification(group)
        BootReceiver.add<RepeaterService>(Starter())
    }
    private fun startFailure(msg: CharSequence, group: WifiP2pGroup? = null, showWifiEnable: Boolean = false) {
        dismissIfApplicable()
        SmartSnackbar.make(msg).apply {
            if (showWifiEnable) action(R.string.repeater_p2p_unavailable_enable) {
                if (Build.VERSION.SDK_INT < 29) @Suppress("DEPRECATION") {
                    Services.wifi.isWifiEnabled = true
                } else it.context.startActivity(Intent(Settings.Panel.ACTION_WIFI))
            }
        }.show()
        if (group != null) removeGroup() else launch { cleanLocked() }
    }

    private fun showNotification(group: WifiP2pGroup? = null) = ServiceNotification.startForeground(this,
            if (group == null) emptyMap() else mapOf(group.`interface` to (group.clientList?.size ?: 0)))

    private fun removeGroup() {
        p2pManager.removeGroup(channel ?: return, object : WifiP2pManager.ActionListener {
            override fun onSuccess() {
                launch { cleanLocked() }
            }
            override fun onFailure(reason: Int) {
                if (reason != WifiP2pManager.BUSY) {
                    dismissIfApplicable()
                    SmartSnackbar.make(formatReason(R.string.repeater_remove_group_failure, reason)).show()
                }   // else assuming it's already gone
                onSuccess()
            }
        })
    }
    private fun cleanLocked(shouldDisable: Boolean = true) {
        if (shouldDisable) BootReceiver.delete<RepeaterService>()
        if (receiverRegistered) {
            ensureReceiverUnregistered(receiver)
            p2pPoller?.cancel()
            receiverRegistered = false
        }
        timeoutMonitor?.close()
        timeoutMonitor = null
        routingManager?.stop()
        routingManager = null
        status = Status.IDLE
        ServiceNotification.stopForeground(this)
        stopSelf()
    }

    override fun onDestroy() {
        if (status != Status.IDLE) binder.shutdown()
        launch {    // force clean to prevent leakage
            cleanLocked(false)
            cancel()
        }
        app.pref.unregisterOnSharedPreferenceChangeListener(this)
        if (Build.VERSION.SDK_INT < 29) unregisterReceiver(deviceListener)
        status = Status.DESTROYED
        channel?.close()
        super.onDestroy()
    }
}
