package be.mygod.vpnhotspot

import android.annotation.SuppressLint
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.hardware.wifi.supplicant.KeyMgmtMask
import android.net.MacAddress
import android.net.wifi.OuiKeyedData
import android.net.wifi.ScanResult
import android.net.wifi.SoftApConfiguration
import android.net.wifi.WpsInfo
import android.net.wifi.p2p.WifiP2pConfig
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
import be.mygod.vpnhotspot.net.TetherStates
import be.mygod.vpnhotspot.net.monitor.TetherTimeoutMonitor
import be.mygod.vpnhotspot.net.wifi.SoftApConfigurationCompat
import be.mygod.vpnhotspot.net.wifi.VendorData
import be.mygod.vpnhotspot.net.wifi.VendorElements
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.createGroup
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.removeGroup
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.requestConnectionInfo
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.requestDeviceAddress
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.requestGroupInfo
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.requestPersistentGroupInfo
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.requestP2pState
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.setVendorElements
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper.startWps
import be.mygod.vpnhotspot.net.wifi.WifiSsidCompat
import be.mygod.vpnhotspot.root.RepeaterCommands
import be.mygod.vpnhotspot.root.RootManager
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.TileServiceDismissHandle
import be.mygod.vpnhotspot.util.UnblockCentral
import be.mygod.vpnhotspot.util.broadcastReceiver
import be.mygod.vpnhotspot.util.ensureReceiverUnregistered
import be.mygod.vpnhotspot.util.intentFilter
import be.mygod.vpnhotspot.util.readableMessage
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.NonCancellable
import kotlinx.coroutines.async
import kotlinx.coroutines.cancel
import kotlinx.coroutines.channels.awaitClose
import kotlinx.coroutines.currentCoroutineContext
import kotlinx.coroutines.delay
import kotlinx.coroutines.flow.callbackFlow
import kotlinx.coroutines.flow.conflate
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.flow.flow
import kotlinx.coroutines.flow.mapNotNull
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.merge
import kotlinx.coroutines.flow.takeWhile
import kotlinx.coroutines.job
import kotlinx.coroutines.launch
import kotlinx.coroutines.supervisorScope
import kotlinx.coroutines.withContext
import kotlinx.parcelize.Parcelize
import timber.log.Timber
import java.lang.ref.WeakReference
import kotlin.time.Duration.Companion.seconds

/**
 * Service for handling Wi-Fi P2P. `supported` must be checked before this service is started otherwise it would crash.
 */
class RepeaterService : Service(), CoroutineScope {
    companion object {
        const val KEY_SAFE_MODE = "service.repeater.safeMode"

        private const val KEY_NETWORK_NAME = "service.repeater.networkName"
        private const val KEY_NETWORK_NAME_HEX = "service.repeater.networkNameHex"
        private const val KEY_PASSPHRASE = "service.repeater.passphrase"
        private const val KEY_OPERATING_BAND = "service.repeater.band.v4"
        private const val KEY_OPERATING_CHANNEL = "service.repeater.oc.v3"
        private const val KEY_AUTO_SHUTDOWN = "service.repeater.autoShutdown"
        private const val KEY_SHUTDOWN_TIMEOUT = "service.repeater.shutdownTimeout"
        private const val KEY_VENDOR_ELEMENTS = "service.repeater.vendorElements"
        private const val KEY_PCC_MODE_CONNECTION_TYPE = "service.repeater.pccModeConnectionType"
        private const val KEY_RANDOMIZE_MAC = "service.repeater.randomizeMac"
        private const val KEY_VENDOR_DATA = "service.repeater.vendorData"

        /**
         * The persisted repeater configuration method. `true` selects the Framework backend; `false` selects
         * the Supplicant backend. Backed by the legacy `KEY_SAFE_MODE` value (true == old safe mode), kept as-is
         * so existing installs upgrade without migration.
         */
        var useFramework: Boolean
            get() = app.pref.getBoolean(KEY_SAFE_MODE, true)
            set(value) = app.pref.edit { putBoolean(KEY_SAFE_MODE, value) }

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
        /** Whether the repeater asks the Supplicant backend to randomize the group owner's MAC address. */
        var randomizeMac: Boolean
            get() = app.pref.getBoolean(KEY_RANDOMIZE_MAC, true)
            set(value) = app.pref.edit { putBoolean(KEY_RANDOMIZE_MAC, value) }
        @get:RequiresApi(33)
        @set:RequiresApi(33)
        var vendorElements: List<ScanResult.InformationElement>
            get() = VendorElements.deserialize(app.pref.getString(KEY_VENDOR_ELEMENTS, null))
            set(value) = app.pref.edit { putString(KEY_VENDOR_ELEMENTS, VendorElements.serialize(value)) }
        /** Vendor-specific data carried by the Supplicant backend on AIDL v3+. */
        @get:RequiresApi(35)
        @set:RequiresApi(35)
        var vendorData: List<OuiKeyedData>
            get() = VendorData.deserialize(app.pref.getString(KEY_VENDOR_DATA, null))
            set(value) = app.pref.edit { putString(KEY_VENDOR_DATA, VendorData.serialize(value)) }
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

    class Binder(owner: RepeaterService) : android.os.Binder() {
        @Volatile
        private var service: RepeaterService? = owner
        val active = owner.active.asStateFlow()
        val group = owner.group.asStateFlow()

        fun detach() {
            service = null
        }

        suspend fun obtainDeviceAddress(): MacAddress? {
            val service = service ?: return null
            return service.p2pManager.requestDeviceAddress(service.channel ?: return null) ?: try {
                RootManager.use { it.execute(RepeaterCommands.RequestDeviceAddress()) }
            } catch (e: Exception) {
                Timber.d(e)
                null
            }
        }

        /**
         * The group to adopt when nothing is persisted: the currently active group if the repeater is up,
         * otherwise the persistent group the framework would reuse on a direct start
         * (`WifiP2pGroupList.getNetworkId`). Read-only — we never delete persistent groups.
         */
        suspend fun obtainGroup(): WifiP2pGroup? {
            group.value?.let { return it }
            val service = service ?: return null
            val deviceAddress = obtainDeviceAddress()?.toString()
            fun List<WifiP2pGroup>.pick() = firstOrNull {
                if (!it.isGroupOwner) return@firstOrNull false
                val ownerAddress = it.owner?.deviceAddress
                if (ownerAddress.isNullOrEmpty()) true
                else if (deviceAddress != null && ownerAddress.equals(deviceAddress, ignoreCase = true)) true
                else try {
                    MacAddress.fromString(ownerAddress) == MacAddressCompat.ANY_ADDRESS
                } catch (e: IllegalArgumentException) {
                    Timber.w(e)
                    true  // assume the owner was redacted or otherwise hidden for privacy
                }
            }
            // the direct read silently returns an empty list without READ_WIFI_CREDENTIAL (API 30+), so fall to root
            if (Build.VERSION.SDK_INT < 30 || service.checkSelfPermission(
                    "android.permission.READ_WIFI_CREDENTIAL") == PackageManager.PERMISSION_GRANTED) try {
                return service.p2pManager.requestPersistentGroupInfo(service.channel ?: return null).pick()
            } catch (e: ReflectiveOperationException) {
                Timber.w(e)
            }
            return try {
                RootManager.use { server ->
                    @Suppress("UNCHECKED_CAST")
                    server.execute(RepeaterCommands.RequestPersistentGroupInfo()).value as List<WifiP2pGroup>
                }.pick()
            } catch (e: Exception) {
                Timber.w(e)
                null
            }
        }

        fun startWps(pin: String? = null) {
            val service = service ?: return
            if (active.value) service.launch {
                val reason = service.p2pManager.startWps(service.channel ?: return@launch SmartSnackbar.make(
                    R.string.repeater_failure_disconnected).show(), WpsInfo().apply {
                    setup = if (pin == null) WpsInfo.PBC else {
                        this.pin = pin
                        WpsInfo.KEYPAD
                    }
                })
                if (reason == null) SmartSnackbar.make(
                        if (pin == null) R.string.repeater_wps_success_pbc else R.string.repeater_wps_success_keypad)
                        .shortToast().show()
                else SmartSnackbar.make(service.formatReason(R.string.repeater_wps_failure, reason)).show()
            }
        }

        fun shutdown() = service?.session?.cancel()
    }

    @Parcelize
    class Starter : BootReceiver.Startable {
        override fun start(context: Context) {
            context.startForegroundService(Intent(context, RepeaterService::class.java))
        }
    }

    /** Carries a user-facing start failure out of [runLifespan]. */
    private class StartFailure(message: String, val showWifiEnable: Boolean = false) : Exception(message)

    private val p2pManager get() = Services.p2p!!
    private var channel: WifiP2pManager.Channel? = null
    private val active = MutableStateFlow(false)
    private val group = MutableStateFlow<WifiP2pGroup?>(null)
    private val binder = Binder(this)
    private var timeoutMonitor: TetherTimeoutMonitor? = null
    override val coroutineContext = Dispatchers.Main.immediate + Job()
    /**
     * The job owning the current repeater lifespan, or `null` when idle: the single source of truth for the service's
     * lifecycle. It is created, replaced and cleared only on the main thread; a lifespan winds the
     * service down only while it is still the job [session] points at, so whether [session] still refers
     * to it doubles as the "is a restart pending" signal — no separate flag is needed.
     */
    private var session: Job? = null
    private var destroyed = false

    private fun updateGroup(value: WifiP2pGroup?) {
        group.value = value
        value?.clientList?.let { timeoutMonitor?.onClientsChanged(it.isEmpty()) }
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
    }
    private fun initializeChannel() {
        if (destroyed) return
        channel = null
        try {
            // WifiP2pManager.Channel uses AsyncChannel which is leaky, prevent holding onto the Context
            val ref = WeakReference(this)
            channel = p2pManager.initialize(app, Looper.getMainLooper()) {
                ref.get()?.apply {
                    session?.cancel()
                    initializeChannel()
                }
            }
        } catch (e: RuntimeException) {
            Timber.w(e)
        }
    }

    override fun onBind(intent: Intent) = binder

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        launch { BootReceiver.startIfEnabled() }
        if (channel == null) return START_NOT_STICKY.also { stopSelf() }
        if (destroyed || session?.isActive == true) return START_NOT_STICKY
        // claim the foreground slot synchronously: startForegroundService requires startForeground() promptly, and a
        // previous lifespan may still be tearing down. Re-show the current group so a duplicate start — which
        // startSession() coalesces — doesn't blank the active interface/client count from the shared notification
        // until the next P2P broadcast. startSession() then launches (or coalesces into) the lifespan.
        showNotification(group.value)
        val predecessor = session
        val job = launch(start = CoroutineStart.LAZY) {
            val self = coroutineContext.job
            try {
                predecessor?.join()
                active.value = true
                channel?.let { runLifespan(it) }
            } catch (_: CancellationException) {
                // a stop, possibly while still waiting behind the barrier: unwind into settle below
            } catch (e: StartFailure) {
                dismissIfApplicable()
                SmartSnackbar.make(e.message!!).apply {
                    if (e.showWifiEnable) action(R.string.repeater_p2p_unavailable_enable) {
                        startActivity(Intent(Settings.Panel.ACTION_WIFI).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
                    }
                }.show()
            } catch (e: Exception) {
                dismissIfApplicable()
                Timber.w(e)
                SmartSnackbar.make(e.readableMessage).show()
            }
            withContext(NonCancellable) {
                // wind down only after the predecessor has fully torn down: the cancellable barrier above may have been
                // aborted mid-wait, so re-join here (a no-op once already joined) to keep the Wi-Fi P2P interface
                // single-owner across a cancelled restart, and so a successor that joins us inherits that ordering
                predecessor?.join()
                if (session !== self) return@withContext
                if (!destroyed) {
                    BootReceiver.delete<RepeaterService>()
                    if (session !== self) return@withContext
                }
                session = null
                updateGroup(null)
                ServiceNotification.stopForeground(this@RepeaterService)
                active.value = false
                if (destroyed) this@RepeaterService.cancel() else stopSelf()
            }
        }
        session = job
        job.start()
        return START_NOT_STICKY
    }

    /**
     * One repeater lifespan: bring a group up through the configured backend, serve it, and always tear it back down.
     * Cancellation at any suspension point (a stop cancelling this job) unwinds into the same teardown, which is why no
     * separate starting state or startup timeout is needed. Exactly one runs at a time — the barrier
     * serializes lifespans — so it can own the Wi-Fi P2P interface outright without any locking of its own.
     */
    private suspend fun runLifespan(channel: WifiP2pManager.Channel) = try {
        val existing = p2pManager.requestGroupInfo(channel)
        val initialGroup = when {
            existing == null -> createGroup(channel)
            existing.isGroupOwner -> existing
            else -> {   // a stale non-owner group holds the interface; drop it before creating ours
                Timber.i("Removing old group ($existing)")
                p2pManager.removeGroup(channel)?.let {
                    throw StartFailure(formatReason(R.string.repeater_remove_old_group_failure, it))
                }
                createGroup(channel)
            }
        }
        val routing = RoutingManager.LocalOnly(this, initialGroup.`interface`!!)
        check(routing.start())
        try {
            timeoutMonitor = if (isAutoShutdownEnabled) {
                TetherTimeoutMonitor(shutdownTimeoutMillis, currentCoroutineContext()) { binder.shutdown() }
            } else null
            updateGroup(initialGroup)
            showNotification(initialGroup)
            BootReceiver.add<RepeaterService>(Starter())
            (if (Build.VERSION.SDK_INT in 30 until 33) merge(flow {
                while (true) {
                    if (p2pManager.requestP2pState(channel) == WifiP2pManager.WIFI_P2P_STATE_DISABLED) return@flow emit(
                        null to null)
                    if (app.location?.isLocationEnabled != true) emit(
                        p2pManager.requestConnectionInfo(channel) to p2pManager.requestGroupInfo(channel))
                    delay(1.seconds)
                }
            }, p2pConnections) else p2pConnections).takeWhile { (info, group) ->
                info?.groupFormed == true && info.isGroupOwner && group?.isGroupOwner == true
            }.collect { (_, group) ->
                updateGroup(group!!)
                showNotification(group)
            }
        } finally {
            timeoutMonitor?.close()
            timeoutMonitor = null
            withContext(NonCancellable) { routing.stop() }
        }
    } finally {
        withContext(NonCancellable) {
            val group = try {
                p2pManager.requestGroupInfo(channel)
            } catch (e: Exception) {
                Timber.w(e)
                return@withContext
            }
            if (group?.isGroupOwner != true) return@withContext // nothing of ours left to remove
            val reason = p2pManager.removeGroup(channel)
            if (reason != null && reason != WifiP2pManager.BUSY) {   // BUSY means it is already going away
                dismissIfApplicable()
                SmartSnackbar.make(formatReason(R.string.repeater_remove_group_failure, reason)).show()
            }
        }
    }

    @RequiresApi(33)
    private suspend fun setVendorElements(channel: WifiP2pManager.Channel) {
        val ve = vendorElements
        val reason = try {
            p2pManager.setVendorElements(channel, ve) ?: return
        } catch (e: IllegalArgumentException) {
            return SmartSnackbar.make(getString(R.string.repeater_set_vendor_elements_failure, e.message)).show()
        } catch (e: UnsupportedOperationException) {
            if (ve.isNotEmpty()) SmartSnackbar.make(
                getString(R.string.repeater_set_vendor_elements_failure, e.message)).show()
            return
        }
        if (reason == WifiP2pManager.ERROR) {
            val rootReason = try {
                RootManager.use { it.execute(RepeaterCommands.SetVendorElements(ve)) } ?: return
            } catch (e: Exception) {
                Timber.w(e)
                SmartSnackbar.make(e).show()
                return
            }
            SmartSnackbar.make(formatReason(R.string.repeater_set_vendor_elements_failure, rootReason.value)).show()
        } else SmartSnackbar.make(formatReason(R.string.repeater_set_vendor_elements_failure, reason)).show()
    }
    /**
     * Create a group owner through the configured backend and suspend until it is ready for routing.
     *
     * Framework mode submits a [WifiP2pConfig]; Supplicant mode (`!useFramework`) adds a persistent group through
     * `wpa_supplicant`. With no credentials configured we let the framework generate and then capture them.
     */
    private suspend fun createGroup(channel: WifiP2pManager.Channel) = supervisorScope {
        if (Build.VERSION.SDK_INT >= 33) setVendorElements(channel)
        val ssid = networkName
        val psk = passphrase
        val ready = async(start = CoroutineStart.UNDISPATCHED) {
            TetherStates.flow.mapNotNull { states ->
                val info = p2pManager.requestConnectionInfo(channel)
                val group = p2pManager.requestGroupInfo(channel)
                group?.takeIf {
                    info?.groupFormed == true && info.isGroupOwner && it.isGroupOwner &&
                            states.localOnly.contains(it.`interface`)
                }
            }.first()
        }
        try {
            when {
                !useFramework -> {
                    if (ssid == null || psk.isNullOrEmpty()) {
                        throw StartFailure(getString(R.string.repeater_configure_failure))
                    }
                    RootManager.use {
                        it.execute(RepeaterCommands.AddPersistentGroupWithConfig(ssid.bytes, psk,
                            when (val oc = operatingChannel) {
                                0 -> when (operatingBand) {
                                    SoftApConfiguration.BAND_2GHZ -> 2
                                    SoftApConfiguration.BAND_5GHZ -> 5
                                    SoftApConfiguration.BAND_6GHZ -> 6
                                    else -> 0
                                }
                                else -> SoftApConfigurationCompat.channelToFrequency(operatingBand, oc)
                            },
                            when (securityType) {
                                SoftApConfiguration.SECURITY_TYPE_WPA3_SAE -> KeyMgmtMask.SAE
                                SoftApConfiguration.SECURITY_TYPE_WPA3_SAE_TRANSITION ->
                                    KeyMgmtMask.WPA_PSK or KeyMgmtMask.SAE
                                else -> KeyMgmtMask.WPA_PSK
                            }, randomizeMac, if (Build.VERSION.SDK_INT >= 35) vendorData.map { data ->
                                android.hardware.wifi.common.OuiKeyedData().apply {
                                    oui = data.oui
                                    vendorData = data.data
                                }
                            }.toTypedArray() else emptyArray()))
                    }?.let {
                        val macRandomizationError = it.unwrap()
                        Timber.w(macRandomizationError)
                        SmartSnackbar.make(macRandomizationError).show()
                    }
                    ready.await()
                }
                ssid != null && !psk.isNullOrEmpty() -> {
                    createFrameworkGroup(channel, WifiP2pConfig.Builder().apply {
                        val networkName = ssid.toString()
                        try {   // bypass networkName check
                            UnblockCentral.WifiP2pConfig_Builder_mNetworkName.set(this, networkName)
                        } catch (e: ReflectiveOperationException) {
                            Timber.w(e)
                            setNetworkName(networkName)   // IllegalArgumentException propagates to runRepeater's handler
                        }
                        setPassphrase(psk)
                        val oc = operatingChannel
                        if (oc == 0) setGroupOperatingBand(when (val band = operatingBand) {
                            SoftApConfiguration.BAND_2GHZ -> WifiP2pConfig.GROUP_OWNER_BAND_2GHZ
                            SoftApConfiguration.BAND_5GHZ -> WifiP2pConfig.GROUP_OWNER_BAND_5GHZ
                            SoftApConfiguration.BAND_6GHZ -> WifiP2pConfig.GROUP_OWNER_BAND_6GHZ
                            else -> {
                                require(SoftApConfigurationCompat.isLegacyEitherBand(band)) { "Unknown band $band" }
                                WifiP2pConfig.GROUP_OWNER_BAND_AUTO
                            }
                        }) else setGroupOperatingFrequency(SoftApConfigurationCompat.channelToFrequency(operatingBand, oc))
                        if (Build.VERSION.SDK_INT >= 36) setPccModeConnectionType(pccModeConnectionType)
                    }.build())
                    ready.await()
                }
                else -> {   // no credentials configured: let the framework pick them, then adopt them as ours
                    createFrameworkGroup(channel, null)
                    ready.await().also { group ->
                        networkName = WifiSsidCompat.fromUtf8Text(group.networkName)
                        passphrase = group.passphrase
                        if (Build.VERSION.SDK_INT >= 36) pccModeConnectionType = group.securityType
                    }
                }
            }
        } finally {
            ready.cancel()
        }
    }

    @SuppressLint("MissingPermission")  // a missing permission merely yields ERROR, surfaced below
    private suspend fun createFrameworkGroup(channel: WifiP2pManager.Channel, config: WifiP2pConfig?) {
        val reason = if (config == null) p2pManager.createGroup(channel) else p2pManager.createGroup(channel, config)
        if (reason != null) throw StartFailure(formatReason(R.string.repeater_create_group_failure, reason),
                showWifiEnable = reason == WifiP2pManager.BUSY)
    }

    /**
     * The latest P2P connection snapshot on every connection-changed broadcast; completes when Wi-Fi P2P is disabled.
     */
    private val p2pConnections = callbackFlow {
        val receiver = broadcastReceiver { _, intent ->
            when (intent.action) {
                WifiP2pManager.WIFI_P2P_CONNECTION_CHANGED_ACTION -> trySend(
                        intent.getParcelableExtra<WifiP2pInfo>(WifiP2pManager.EXTRA_WIFI_P2P_INFO) to
                                intent.getParcelableExtra<WifiP2pGroup>(WifiP2pManager.EXTRA_WIFI_P2P_GROUP))
                WifiP2pManager.WIFI_P2P_STATE_CHANGED_ACTION -> if (intent.getIntExtra(
                                WifiP2pManager.EXTRA_WIFI_STATE, 0) == WifiP2pManager.WIFI_P2P_STATE_DISABLED) close()
            }
        }
        registerReceiver(receiver, intentFilter(WifiP2pManager.WIFI_P2P_CONNECTION_CHANGED_ACTION,
                WifiP2pManager.WIFI_P2P_STATE_CHANGED_ACTION))
        awaitClose { ensureReceiverUnregistered(receiver) }
    }.conflate()


    private fun showNotification(group: WifiP2pGroup? = null) = ServiceNotification.startForeground(this,
        if (group == null) emptyMap() else mapOf(group.`interface` to (group.clientList?.size ?: 0)))

    override fun onDestroy() {
        binder.detach()
        destroyed = true
        // Cancel the live lifespan; its settle() drops the shared notification, keeps the boot entry and ends the scope
        // once its own teardown finishes. With nothing running, do that wind-down here instead.
        val current = session
        if (current == null) {
            ServiceNotification.stopForeground(this)
            cancel()
        } else current.cancel()
        channel?.close()
        channel = null
        super.onDestroy()
    }
}
