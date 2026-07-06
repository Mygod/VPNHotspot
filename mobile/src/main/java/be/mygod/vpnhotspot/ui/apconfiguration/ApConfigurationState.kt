package be.mygod.vpnhotspot.ui.apconfiguration

import android.content.ClipData
import android.content.ClipDescription
import android.content.Context
import android.net.MacAddress
import android.net.wifi.SoftApConfiguration
import android.os.Build
import android.os.Parcelable
import android.os.PersistableBundle
import android.util.Base64
import android.util.SparseIntArray
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.saveable.Saver
import androidx.compose.runtime.setValue
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.RepeaterService
import be.mygod.vpnhotspot.net.wifi.SoftApConfigurationCompat
import be.mygod.vpnhotspot.net.wifi.VendorData
import be.mygod.vpnhotspot.net.wifi.VendorElements
import be.mygod.vpnhotspot.net.wifi.WifiSsidCompat
import be.mygod.vpnhotspot.net.wifi.softApChannelWidthLookup
import be.mygod.vpnhotspot.root.RepeaterCommands.SupplicantCapability
import be.mygod.vpnhotspot.util.RangeInput
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.readableMessage
import be.mygod.vpnhotspot.util.toByteArray
import be.mygod.vpnhotspot.util.toParcelable
import kotlinx.parcelize.Parcelize
import timber.log.Timber

class ApConfigurationState(
    initial: SoftApConfigurationCompat,
    val readOnly: Boolean,
    val target: ApConfigurationTarget,
) {
    companion object {
        val Saver = Saver<ApConfigurationState, Parcelable>(
            save = { it.save() },
            restore = { ApConfigurationState(it as SavedApConfigurationState) },
        )
    }

    private constructor(saved: SavedApConfigurationState) : this(
        saved.base,
        saved.readOnly,
        saved.target,
    ) {
        originalUnderlying = saved.originalUnderlying
        _useFramework = saved.useFramework
        disablePowerSave = saved.disablePowerSave
        hexSsid = saved.hexSsid
        ssid = saved.ssid
        securityType = saved.securityType
        password = saved.password
        autoShutdown = saved.autoShutdown
        timeout = saved.timeout
        bridgedModeOpportunisticShutdown = saved.bridgedModeOpportunisticShutdown
        bridgedTimeout = saved.bridgedTimeout
        primaryChannel = ChannelOption(saved.primaryBand, saved.primaryChannel)
        secondaryChannel = if (saved.secondaryBand < 0) {
            ChannelOption.Disabled
        } else ChannelOption(saved.secondaryBand, saved.secondaryChannel)
        bandOptimization = saved.bandOptimization
        maxClients = saved.maxClients
        blockedList = saved.blockedList
        clientUserControl = saved.clientUserControl
        allowedList = saved.allowedList
        macRandomization = saved.macRandomization
        bssid = saved.bssid
        persistentRandomizedMac = saved.persistentRandomizedMac
        hiddenSsid = saved.hiddenSsid
        ieee80211ax = saved.ieee80211ax
        ieee80211be = saved.ieee80211be
        vendorElements = saved.vendorElements
        vendorData = saved.vendorData
        clientIsolation = saved.clientIsolation
        userConfig = saved.userConfig
        acs2g = saved.acs2g
        acs5g = saved.acs5g
        acs6g = saved.acs6g
        maxChannelBandwidth = normalizeMaxChannelBandwidth(saved.maxChannelBandwidth)
    }

    private var base = initial
    private var originalUnderlying = initial.underlying
    val p2pMode get() = target == ApConfigurationTarget.Repeater
    private var _useFramework by mutableStateOf(if (p2pMode) RepeaterService.useFramework else true)
    /**
     * Repeater configuration method, screen-local so dependent option lists update immediately. Like every
     * other field on this screen it is only persisted from the apply path, so backing out without saving discards it.
     */
    var useFramework: Boolean
        get() = _useFramework
        set(value) {
            if (_useFramework == value) return
            _useFramework = value
        }
    var disablePowerSave by mutableStateOf(if (p2pMode) RepeaterService.disablePowerSave else false)
    private var _supplicantCapability by mutableStateOf<SupplicantCapability?>(null)
    /** Best-effort live Supplicant backend capability, shown to explain which backend will receive the config. */
    var supplicantCapability: SupplicantCapability?
        get() = _supplicantCapability
        set(value) {
            _supplicantCapability = value
        }
    private val ssidHexToggleable get() = if (p2pMode) !useFramework else Build.VERSION.SDK_INT >= 33
    private var hexSsid = false
    private val p2pWpa3Supported get() =
        if (useFramework) Build.VERSION.SDK_INT >= 36 else supplicantCapability?.aidlV3 == true
    val securityEntries get() = when {
        p2pMode && p2pWpa3Supported -> buildList {
            add(SecurityOption(R.string.wifi_security_wpa2_personal, SoftApConfiguration.SECURITY_TYPE_WPA2_PSK))
            add(SecurityOption(R.string.wifi_security_wpa3_personal_transition,
                SoftApConfiguration.SECURITY_TYPE_WPA3_SAE_TRANSITION))
            add(SecurityOption(R.string.wifi_security_wpa3_personal, SoftApConfiguration.SECURITY_TYPE_WPA3_SAE))
        }
        p2pMode -> listOf(SecurityOption(R.string.wifi_security_wpa2_personal,
            SoftApConfiguration.SECURITY_TYPE_WPA2_PSK))
        else -> listOf(
            R.string.wifi_security_open,
            R.string.wifi_security_wpa2_psk,
            R.string.wifi_security_wpa3_sae_transition,
            R.string.wifi_security_wpa3_sae,
            R.string.wifi_security_wpa3_owe_transition,
            R.string.wifi_security_wpa3_owe,
        ).mapIndexed { index, label -> SecurityOption(label, index) }
    }
    private val channelOptions get() = currentChannelOptions(p2pMode)
    val bandwidthEntries = if (Build.VERSION.SDK_INT >= 33) {
        softApChannelWidthLookup.lookup.let { lookup ->
            List(lookup.size()) {
                val width = lookup.keyAt(it)
                BandWidth(width, lookup.valueAt(it).substring(14))
            }.sortedBy { it.width }
        }
    } else emptyList()

    private fun normalizeMaxChannelBandwidth(
        width: Int,
        fallback: Int = bandwidthEntries.firstOrNull()?.width ?: width,
    ): Int {
        if (Build.VERSION.SDK_INT < 33 || bandwidthEntries.any { it.width == width }) return width
        Timber.w(Exception("Cannot locate bandwidth $width"))
        return fallback
    }

    var ssid by mutableStateOf(displaySsid(initial.ssid))
    var securityType by mutableIntStateOf(initial.securityType)
    var password by mutableStateOf(initial.passphrase.orEmpty())
    var autoShutdown by mutableStateOf(initial.isAutoShutdownEnabled)
    var timeout by mutableStateOf(initial.shutdownTimeoutMillis.let { if (it <= 0) "" else it.toString() })
    var bridgedModeOpportunisticShutdown by mutableStateOf(initial.isBridgedModeOpportunisticShutdownEnabled)
    var bridgedTimeout by mutableStateOf(initial.bridgedModeOpportunisticShutdownTimeoutMillis.let {
        if (it == -1L) "" else it.toString()
    })
    var primaryChannel by mutableStateOf(locate(initial.channels, 0, channelOptions, p2pMode))
    var secondaryChannel by mutableStateOf(if (initial.channels.size() > 1) {
        locate(initial.channels, 1, channelOptions, p2pMode)
    } else ChannelOption.Disabled)
    var bandOptimization by mutableStateOf(if (!p2pMode && Build.VERSION.SDK_INT >= 30 &&
            SoftApConfigurationCompat.isBandOptimizationSupported) {
        initial.isBandOptimizationEnabled ?: true
    } else false)
    var maxClients by mutableStateOf(initial.maxNumberOfClients.let { if (it == 0) "" else it.toString() })
    var blockedList by mutableStateOf(initial.blockedClientList.joinToString("\n"))
    var clientUserControl by mutableStateOf(initial.isClientControlByUserEnabled)
    var allowedList by mutableStateOf(initial.allowedClientList.joinToString("\n"))
    var macRandomization by mutableIntStateOf(initial.macRandomizationSetting)
    var bssid by mutableStateOf(initial.bssid?.toString().orEmpty())
    var persistentRandomizedMac by mutableStateOf(initial.persistentRandomizedMacAddress?.toString().orEmpty())
    var hiddenSsid by mutableStateOf(initial.isHiddenSsid)
    var ieee80211ax by mutableStateOf(initial.isIeee80211axEnabled)
    var ieee80211be by mutableStateOf(initial.isIeee80211beEnabled)
    var vendorElements by mutableStateOf(VendorElements.serialize(initial.vendorElements))
    var vendorData by mutableStateOf(VendorData.serialize(initial.vendorData))
    var clientIsolation by mutableStateOf(if (Build.VERSION.SDK_INT >= 36) initial.isClientIsolationEnabled else false)
    var userConfig by mutableStateOf(initial.isUserConfiguration)
    var acs2g by mutableStateOf(RangeInput.toString(initial.allowedAcsChannels[SoftApConfiguration.BAND_2GHZ]).orEmpty())
    var acs5g by mutableStateOf(RangeInput.toString(initial.allowedAcsChannels[SoftApConfiguration.BAND_5GHZ]).orEmpty())
    var acs6g by mutableStateOf(RangeInput.toString(initial.allowedAcsChannels[SoftApConfiguration.BAND_6GHZ]).orEmpty())
    var maxChannelBandwidth by mutableIntStateOf(normalizeMaxChannelBandwidth(initial.maxChannelBandwidth))

    val canShare get() = try {
        generateConfig(requirePassword = false, full = false)
        true
    } catch (_: RuntimeException) {
        false
    }
    val ssidHex get() = hexSsid
    val canToggleSsidHex get() = ssidHexToggleable
    val vendorDataEditable get() = if (p2pMode) {
        !useFramework && supplicantCapability?.aidlV3 == true
    } else Build.VERSION.SDK_INT >= 35
    val passwordEnabled get() = when (selectedSecurityType) {
        SoftApConfiguration.SECURITY_TYPE_OPEN,
        SoftApConfiguration.SECURITY_TYPE_WPA3_OWE_TRANSITION,
        SoftApConfiguration.SECURITY_TYPE_WPA3_OWE -> false
        else -> true
    }
    val passwordMaxLength get() = selectedSecurityType != SoftApConfiguration.SECURITY_TYPE_WPA3_SAE
    val channelError get() = if (!p2pMode && Build.VERSION.SDK_INT >= 30) try {
        SoftApConfigurationCompat.testPlatformValidity(generateChannels())
        null
    } catch (e: Exception) {
        e.readableMessage
    } else null
    val maxChannelBandwidthError get() = if (!p2pMode && Build.VERSION.SDK_INT >= 33) try {
        SoftApConfigurationCompat.testPlatformValidity(maxChannelBandwidth)
        null
    } catch (e: Exception) {
        e.readableMessage
    } else null
    val selectedSecurityType get() =
        if (p2pMode && !p2pWpa3Supported) SoftApConfiguration.SECURITY_TYPE_WPA2_PSK else securityType

    fun copyError(context: Context) = generateConfigError(context, requirePassword = false, checkChannels = false)
    fun saveError(context: Context) = if (readOnly) null else generateConfigError(
        context,
        requirePassword = true,
        checkChannels = true,
    )
    fun canSave(context: Context) = !readOnly && saveError(context) == null
    fun possiblyInvalid(context: Context) = canSave(context) && Build.VERSION.SDK_INT >= 34 && !p2pMode && try {
        !Services.wifi.validateSoftApConfiguration(generateConfig().toPlatform())
    } catch (e: Exception) {
        Timber.d(e)
        false
    }

    private fun generateConfigError(context: Context, requirePassword: Boolean, checkChannels: Boolean): String? {
        ssidError(ssid, hexSsid, context)?.let { return it }
        if (requirePassword) passwordError(password, context)?.let { return it }
        validateOptionalLong(timeout) {
            if (!p2pMode && Build.VERSION.SDK_INT >= 30) {
                SoftApConfigurationCompat.testPlatformTimeoutValidity(it)
            }
        }?.let { return it }
        if (checkChannels && !p2pMode && Build.VERSION.SDK_INT >= 30) try {
            SoftApConfigurationCompat.testPlatformValidity(generateChannels())
        } catch (e: Exception) {
            return e.readableMessage
        }
        if (bssidEditable(macRandomization)) validateOptionalMac(bssid) {
            if (Build.VERSION.SDK_INT >= 30 && !p2pMode) SoftApConfigurationCompat.testPlatformValidity(it)
        }?.let { return it }
        if (maxClients.isNotEmpty()) try {
            maxClients.toInt()
        } catch (e: NumberFormatException) {
            return e.readableMessage
        }
        if (Build.VERSION.SDK_INT >= 30) {
            val blocked = try {
                parseMacList(blockedList).toSet()
            } catch (e: IllegalArgumentException) {
                return e.readableMessage
            }
            try {
                for (mac in parseMacList(allowedList)) {
                    require(mac !in blocked) { context.getString(R.string.wifi_client_lists_overlap_error) }
                }
            } catch (e: IllegalArgumentException) {
                return e.readableMessage
            }
        }
        validateOptionalLong(
            bridgedTimeout,
            SoftApConfigurationCompat::testPlatformBridgedTimeoutValidity,
        )?.let { return it }
        if (Build.VERSION.SDK_INT >= 33) try {
            val elements = VendorElements.deserialize(vendorElements)
            if (!p2pMode) SoftApConfigurationCompat.testPlatformValidity(elements)
        } catch (e: Exception) {
            return e.readableMessage
        }
        if (vendorDataEditable) try {
            VendorData.deserialize(vendorData, context)
        } catch (e: Exception) {
            return e.readableMessage
        }
        validateOptionalMac(persistentRandomizedMac)?.let { return it }
        if (!p2pMode && Build.VERSION.SDK_INT >= 33) {
            validateAcsChannels(SoftApConfiguration.BAND_2GHZ, acs2g)?.let { return it }
            validateAcsChannels(SoftApConfiguration.BAND_5GHZ, acs5g)?.let { return it }
            validateAcsChannels(SoftApConfiguration.BAND_6GHZ, acs6g)?.let { return it }
            maxChannelBandwidthError?.let { return it }
        }
        return null
    }

    fun generateConfig(requirePassword: Boolean = true, full: Boolean = true): SoftApConfigurationCompat {
        if (requirePassword) passwordError(password)?.let { throw IllegalArgumentException(it) }
        val parsedSsid = if (hexSsid) WifiSsidCompat.fromHex(ssid) else WifiSsidCompat.fromUtf8Text(ssid)
        require((parsedSsid?.bytes?.size ?: 0) in 1..32) { app.getString(R.string.wifi_ssid) }
        return base.copy(
            ssid = parsedSsid,
            passphrase = password.ifEmpty { null },
        ).apply {
            if (!p2pMode || Build.VERSION.SDK_INT >= 36) securityType = selectedSecurityType
            if (!p2pMode) isHiddenSsid = hiddenSsid
            if (!full) return@apply
            isAutoShutdownEnabled = autoShutdown
            shutdownTimeoutMillis = timeout.ifEmpty { "0" }.toLong()
            channels = generateChannels()
            maxNumberOfClients = maxClients.ifEmpty { "0" }.toInt()
            isClientControlByUserEnabled = clientUserControl
            allowedClientList = parseMacList(allowedList)
            blockedClientList = parseMacList(blockedList)
            macRandomizationSetting = macRandomization
            bssid = if (bssidEditable(macRandomization) && this@ApConfigurationState.bssid.isNotEmpty()) {
                MacAddress.fromString(this@ApConfigurationState.bssid)
            } else null
            isBridgedModeOpportunisticShutdownEnabled = bridgedModeOpportunisticShutdown
            isIeee80211axEnabled = ieee80211ax
            isIeee80211beEnabled = ieee80211be
            isUserConfiguration = userConfig
            bridgedModeOpportunisticShutdownTimeoutMillis = bridgedTimeout.ifEmpty { "-1" }.toLong()
            vendorElements = VendorElements.deserialize(this@ApConfigurationState.vendorElements)
            if (vendorDataEditable) vendorData = VendorData.deserialize(this@ApConfigurationState.vendorData)
            persistentRandomizedMacAddress = persistentRandomizedMac.ifEmpty { null }?.let(MacAddress::fromString)
            allowedAcsChannels = mapOf(
                SoftApConfiguration.BAND_2GHZ to RangeInput.fromString(acs2g),
                SoftApConfiguration.BAND_5GHZ to RangeInput.fromString(acs5g),
                SoftApConfiguration.BAND_6GHZ to RangeInput.fromString(acs6g),
            )
            isBandOptimizationEnabled = bandOptimization
            if (p2pMode || Build.VERSION.SDK_INT < 33) return@apply
            maxChannelBandwidth = this@ApConfigurationState.maxChannelBandwidth
            if (Build.VERSION.SDK_INT >= 36) isClientIsolationEnabled = clientIsolation
        }
    }

    fun copyToClipboard() {
        app.clipboard.setPrimaryClip(ClipData.newPlainText(null,
            Base64.encodeToString(generateConfig(requirePassword = false).toByteArray(), BASE64_FLAGS)).apply {
            description.extras = PersistableBundle().apply {
                putBoolean(ClipDescription.EXTRA_IS_SENSITIVE, true)
            }
        })
    }

    fun pasteFromClipboard() {
        app.clipboard.primaryClip?.getItemAt(0)?.text?.let { text ->
            Base64.decode(text.toString(), BASE64_FLAGS).toParcelable<SoftApConfigurationCompat>(
                SoftApConfigurationCompat::class.java.classLoader)?.let { config ->
                val newUnderlying = config.underlying
                if (newUnderlying != null) {
                    originalUnderlying?.let { check(it.javaClass == newUnderlying.javaClass) }
                } else config.underlying = base.underlying
                load(config)
            }
        }
    }

    fun setSsid(value: String, hex: Boolean) {
        hexSsid = hex
        ssid = value
    }

    fun convertSsidDisplay(value: String, hex: Boolean, context: Context = app): String {
        val parsedSsid = if (hex) WifiSsidCompat.fromHex(value) else WifiSsidCompat.fromUtf8Text(value)
        return if (hex) parsedSsid?.decode()
            ?: throw IllegalArgumentException(context.getString(R.string.wifi_ssid_invalid_utf8))
        else parsedSsid?.hex.orEmpty()
    }

    fun ssidSupplicantModeWarning(value: String, hex: Boolean, context: Context = app): String? {
        if (!p2pMode || !useFramework) return null
        val size = ssidByteCount(value, hex)
        return if (size in 1..8) context.getString(R.string.settings_service_repeater_supplicant_mode_warning) else null
    }

    fun ssidByteCount(value: String, hex: Boolean) = try {
        (if (hex) WifiSsidCompat.fromHex(value) else WifiSsidCompat.fromUtf8Text(value))?.bytes?.size ?: 0
    } catch (_: IllegalArgumentException) {
        0
    }

    fun ssidError(value: String, hex: Boolean, context: Context = app): String? = try {
        val size = (if (hex) WifiSsidCompat.fromHex(value) else WifiSsidCompat.fromUtf8Text(value))?.bytes?.size ?: 0
        if (size in 1..32) null else context.getString(R.string.wifi_ssid)
    } catch (e: IllegalArgumentException) {
        e.readableMessage
    }

    fun passwordError(value: String, context: Context = app): String? {
        return when (selectedSecurityType) {
            SoftApConfiguration.SECURITY_TYPE_OPEN,
            SoftApConfiguration.SECURITY_TYPE_WPA3_OWE_TRANSITION,
            SoftApConfiguration.SECURITY_TYPE_WPA3_OWE -> null
            SoftApConfiguration.SECURITY_TYPE_WPA2_PSK,
            SoftApConfiguration.SECURITY_TYPE_WPA3_SAE_TRANSITION -> {
                if (value.length in 8..63) null else context.getString(R.string.wifi_password_error_length)
            }
            else -> if (value.isNotEmpty()) null else context.getString(R.string.wifi_password_error_required)
        }
    }

    private fun load(config: SoftApConfigurationCompat) {
        base = config
        ssid = displaySsid(config.ssid)
        securityType = config.securityType
        password = config.passphrase.orEmpty()
        autoShutdown = config.isAutoShutdownEnabled
        timeout = config.shutdownTimeoutMillis.let { if (it <= 0) "" else it.toString() }
        bridgedModeOpportunisticShutdown = config.isBridgedModeOpportunisticShutdownEnabled
        bridgedTimeout = config.bridgedModeOpportunisticShutdownTimeoutMillis.let { if (it == -1L) "" else it.toString() }
        primaryChannel = locate(config.channels, 0, channelOptions, p2pMode, true)
        secondaryChannel = if (config.channels.size() > 1) locate(config.channels, 1, channelOptions, p2pMode, true)
        else ChannelOption.Disabled
        bandOptimization = if (!p2pMode && Build.VERSION.SDK_INT >= 30 &&
                SoftApConfigurationCompat.isBandOptimizationSupported) {
            config.isBandOptimizationEnabled ?: true
        } else false
        maxClients = config.maxNumberOfClients.let { if (it == 0) "" else it.toString() }
        blockedList = config.blockedClientList.joinToString("\n")
        clientUserControl = config.isClientControlByUserEnabled
        allowedList = config.allowedClientList.joinToString("\n")
        macRandomization = config.macRandomizationSetting
        bssid = config.bssid?.toString().orEmpty()
        persistentRandomizedMac = config.persistentRandomizedMacAddress?.toString().orEmpty()
        hiddenSsid = config.isHiddenSsid
        ieee80211ax = config.isIeee80211axEnabled
        ieee80211be = config.isIeee80211beEnabled
        vendorElements = VendorElements.serialize(config.vendorElements)
        vendorData = VendorData.serialize(config.vendorData)
        clientIsolation = if (Build.VERSION.SDK_INT >= 36) config.isClientIsolationEnabled else false
        userConfig = config.isUserConfiguration
        acs2g = RangeInput.toString(config.allowedAcsChannels[SoftApConfiguration.BAND_2GHZ]).orEmpty()
        acs5g = RangeInput.toString(config.allowedAcsChannels[SoftApConfiguration.BAND_5GHZ]).orEmpty()
        acs6g = RangeInput.toString(config.allowedAcsChannels[SoftApConfiguration.BAND_6GHZ]).orEmpty()
        maxChannelBandwidth = normalizeMaxChannelBandwidth(config.maxChannelBandwidth, maxChannelBandwidth)
    }

    private fun displaySsid(value: WifiSsidCompat?): String {
        value ?: return ""
        if (hexSsid) return value.hex
        if (!ssidHexToggleable) return value.toString()
        val decoded = value.decode()
        return if (decoded == null) {
            hexSsid = true
            value.hex
        } else decoded
    }

    fun channelEntries(allowDisabled: Boolean = false) = if (allowDisabled) listOf(ChannelOption.Disabled) +
            channelOptions else channelOptions
    fun bssidEditable(macRandomization: Int) = !p2pMode && (Build.VERSION.SDK_INT < 31 ||
            macRandomization == SoftApConfigurationCompat.RANDOMIZATION_NONE)
    fun macAddressSummary(context: Context) = if (p2pMode) {
        if (macRandomization == SoftApConfigurationCompat.RANDOMIZATION_NON_PERSISTENT) {
            context.getString(R.string.wifi_mac_address_non_persistent_randomization)
        } else ""
    } else {
        when {
            macRandomization == SoftApConfigurationCompat.RANDOMIZATION_NON_PERSISTENT ->
                context.getString(R.string.wifi_mac_address_non_persistent_randomization)
            macRandomization == SoftApConfigurationCompat.RANDOMIZATION_PERSISTENT -> {
                if (persistentRandomizedMac.isEmpty()) ""
                else context.getString(R.string.wifi_mac_address_persistent_randomization, persistentRandomizedMac)
            }
            else -> bssid
        }
    }

    private fun generateChannels() = SparseIntArray(2).also { channels ->
        if (!p2pMode && Build.VERSION.SDK_INT >= 31 && secondaryChannel.band >= 0) {
            channels.put(secondaryChannel.band, secondaryChannel.channel)
        }
        channels.put(primaryChannel.band, primaryChannel.channel)
    }

    private fun save() = SavedApConfigurationState(
        base = base,
        originalUnderlying = originalUnderlying,
        readOnly = readOnly,
        target = target,
        useFramework = useFramework,
        disablePowerSave = disablePowerSave,
        hexSsid = hexSsid,
        ssid = ssid,
        securityType = securityType,
        password = password,
        autoShutdown = autoShutdown,
        timeout = timeout,
        bridgedModeOpportunisticShutdown = bridgedModeOpportunisticShutdown,
        bridgedTimeout = bridgedTimeout,
        primaryBand = primaryChannel.band,
        primaryChannel = primaryChannel.channel,
        secondaryBand = secondaryChannel.band,
        secondaryChannel = secondaryChannel.channel,
        bandOptimization = bandOptimization,
        maxClients = maxClients,
        blockedList = blockedList,
        clientUserControl = clientUserControl,
        allowedList = allowedList,
        macRandomization = macRandomization,
        bssid = bssid,
        persistentRandomizedMac = persistentRandomizedMac,
        hiddenSsid = hiddenSsid,
        ieee80211ax = ieee80211ax,
        ieee80211be = ieee80211be,
        vendorElements = vendorElements,
        vendorData = vendorData,
        clientIsolation = clientIsolation,
        userConfig = userConfig,
        acs2g = acs2g,
        acs5g = acs5g,
        acs6g = acs6g,
        maxChannelBandwidth = maxChannelBandwidth,
    )
}

@Parcelize
private data class SavedApConfigurationState(
    val base: SoftApConfigurationCompat,
    val originalUnderlying: Parcelable?,
    val readOnly: Boolean,
    val target: ApConfigurationTarget,
    val useFramework: Boolean,
    val disablePowerSave: Boolean,
    val hexSsid: Boolean,
    val ssid: String,
    val securityType: Int,
    val password: String,
    val autoShutdown: Boolean,
    val timeout: String,
    val bridgedModeOpportunisticShutdown: Boolean,
    val bridgedTimeout: String,
    val primaryBand: Int,
    val primaryChannel: Int,
    val secondaryBand: Int,
    val secondaryChannel: Int,
    val bandOptimization: Boolean,
    val maxClients: String,
    val blockedList: String,
    val clientUserControl: Boolean,
    val allowedList: String,
    val macRandomization: Int,
    val bssid: String,
    val persistentRandomizedMac: String,
    val hiddenSsid: Boolean,
    val ieee80211ax: Boolean,
    val ieee80211be: Boolean,
    val vendorElements: String,
    val vendorData: String,
    val clientIsolation: Boolean,
    val userConfig: Boolean,
    val acs2g: String,
    val acs5g: String,
    val acs6g: String,
    val maxChannelBandwidth: Int,
) : Parcelable
