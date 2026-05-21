package be.mygod.vpnhotspot.ui

import android.content.ClipData
import android.content.ClipDescription
import android.graphics.Bitmap
import android.graphics.Color
import android.icu.text.MeasureFormat
import android.icu.util.Measure
import android.icu.util.MeasureUnit
import android.net.MacAddress
import android.net.wifi.SoftApConfiguration
import android.net.wifi.p2p.WifiP2pGroup
import android.os.Build
import android.os.Parcelable
import android.os.PersistableBundle
import android.util.Base64
import android.util.SparseIntArray
import android.widget.Toast
import androidx.annotation.DrawableRes
import androidx.annotation.StringRes
import androidx.compose.foundation.Image
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.asPaddingValues
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.navigationBars
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.Icon
import androidx.compose.material3.LocalTextStyle
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.runtime.saveable.Saver
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Modifier
import androidx.compose.ui.autofill.ContentType
import androidx.compose.ui.autofill.contentType
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.res.dimensionResource
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.TextRange
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.text.input.TextFieldValue
import androidx.compose.ui.text.input.VisualTransformation
import androidx.compose.ui.unit.dp
import androidx.core.graphics.set
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.RepeaterService
import be.mygod.vpnhotspot.net.monitor.TetherTimeoutMonitor
import be.mygod.vpnhotspot.net.wifi.SoftApConfigurationCompat
import be.mygod.vpnhotspot.net.wifi.SoftApConfigurationCompat.Companion.toCompat
import be.mygod.vpnhotspot.net.wifi.SoftApInfo
import be.mygod.vpnhotspot.net.wifi.P2pSupplicantConfiguration
import be.mygod.vpnhotspot.net.wifi.VendorElements
import be.mygod.vpnhotspot.net.wifi.WifiApManager
import be.mygod.vpnhotspot.net.wifi.WifiSsidCompat
import be.mygod.vpnhotspot.root.RootManager
import be.mygod.vpnhotspot.root.WifiApCommands
import be.mygod.vpnhotspot.util.RangeInput
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.getRootCause
import be.mygod.vpnhotspot.util.readableMessage
import be.mygod.vpnhotspot.util.toByteArray
import be.mygod.vpnhotspot.util.toParcelable
import com.google.zxing.BarcodeFormat
import com.google.zxing.EncodeHintType
import com.google.zxing.MultiFormatWriter
import com.google.zxing.WriterException
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.DelicateCoroutinesApi
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.parcelize.Parcelize
import timber.log.Timber
import java.lang.reflect.InvocationTargetException
import java.nio.charset.StandardCharsets
import java.text.DecimalFormat
import java.text.DecimalFormatSymbols

internal class ApConfigurationState(
    initial: SoftApConfigurationCompat,
    val readOnly: Boolean,
    val p2pMode: Boolean,
) {
    companion object {
        val Saver = Saver<ApConfigurationState, Parcelable>(
            save = { it.save() },
            restore = { ApConfigurationState(it as SavedApConfigurationState) },
        )
    }

    private constructor(saved: SavedApConfigurationState) : this(saved.base, saved.readOnly, saved.p2pMode) {
        originalUnderlying = saved.originalUnderlying
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
        clientIsolation = saved.clientIsolation
        userConfig = saved.userConfig
        acs2g = saved.acs2g
        acs5g = saved.acs5g
        acs6g = saved.acs6g
        maxChannelBandwidth = normalizeMaxChannelBandwidth(saved.maxChannelBandwidth)
    }

    private var base = initial
    private var originalUnderlying = initial.underlying
    private val ssidHexToggleable = if (p2pMode) !RepeaterService.safeMode else Build.VERSION.SDK_INT >= 33
    private var hexSsid = false
    private val securityOptions = when {
        p2pMode && Build.VERSION.SDK_INT >= 36 -> P2P_SECURITY_TYPES.mapIndexed { index, label ->
            SecurityOption(label, index + SoftApConfiguration.SECURITY_TYPE_WPA2_PSK)
        }
        p2pMode -> listOf(SecurityOption("WPA2-Personal", SoftApConfiguration.SECURITY_TYPE_WPA2_PSK))
        else -> SoftApConfigurationCompat.securityTypes.mapIndexed { index, label -> SecurityOption(label, index) }
    }
    private val channelOptions = currentChannelOptions(p2pMode)
    private val bandwidthOptions = if (Build.VERSION.SDK_INT >= 33) {
        SoftApInfo.channelWidthLookup.lookup.let { lookup ->
            List(lookup.size()) { BandWidth(lookup.keyAt(it), lookup.valueAt(it).substring(14)) }.sorted()
        }
    } else emptyList()

    private fun normalizeMaxChannelBandwidth(
        width: Int,
        fallback: Int = bandwidthOptions.firstOrNull()?.width ?: width,
    ): Int {
        if (Build.VERSION.SDK_INT < 33 || bandwidthOptions.any { it.width == width }) return width
        Timber.w(Exception("Cannot locate bandwidth $width"))
        return fallback
    }

    var revision by mutableIntStateOf(0)
        private set
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
    var clientIsolation by mutableStateOf(if (Build.VERSION.SDK_INT >= 36) initial.isClientIsolationEnabled else false)
    var userConfig by mutableStateOf(initial.isUserConfiguration)
    var acs2g by mutableStateOf(RangeInput.toString(initial.allowedAcsChannels[SoftApConfiguration.BAND_2GHZ]).orEmpty())
    var acs5g by mutableStateOf(RangeInput.toString(initial.allowedAcsChannels[SoftApConfiguration.BAND_5GHZ]).orEmpty())
    var acs6g by mutableStateOf(RangeInput.toString(initial.allowedAcsChannels[SoftApConfiguration.BAND_6GHZ]).orEmpty())
    var maxChannelBandwidth by mutableIntStateOf(normalizeMaxChannelBandwidth(initial.maxChannelBandwidth))

    val copyError get() = generateConfigError(requirePassword = false, checkChannels = false)
    val saveError get() = if (readOnly) null else generateConfigError(requirePassword = true, checkChannels = true)
    val actionError get() = copyError ?: saveError
    val canSave get() = !readOnly && saveError == null
    val canCopy get() = copyError == null
    val canShare get() = generateConfigOrNull(requirePassword = false, full = false) != null
    val possiblyInvalid get() = canSave && Build.VERSION.SDK_INT >= 34 && !p2pMode && try {
        generateConfigOrNull()?.let { !Services.wifi.validateSoftApConfiguration(it.toPlatform()) } == true
    } catch (e: Exception) {
        Timber.d(e)
        false
    }
    val securityLabel get() = securityOptions.firstOrNull { it.value == securityType }?.label ?: securityType.toString()
    val primaryChannelLabel get() = primaryChannel.toString()
    val secondaryChannelLabel get() = secondaryChannel.toString()
    val macRandomizationLabel get() = app.resources.getStringArray(R.array.wifi_mac_randomization)
        .getOrElse(macRandomization) { macRandomization.toString() }
    val maxChannelBandwidthLabel get() = bandwidthOptions.firstOrNull { it.width == maxChannelBandwidth }?.name
        ?: maxChannelBandwidth.toString()
    val ssidHex get() = hexSsid
    val ssidWarning get() = ssidSafeModeWarning(ssid, hexSsid)
    val canToggleSsidHex get() = ssidHexToggleable
    val passwordEnabled get() = selectedSecurityType !in SECURITY_TYPES_WITHOUT_PASSWORD
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
    private val selectedSecurityType get() = if (p2pMode && Build.VERSION.SDK_INT < 36) {
        SoftApConfiguration.SECURITY_TYPE_WPA2_PSK
    } else securityType

    fun generateConfigOrNull(requirePassword: Boolean = true, full: Boolean = true) = try {
        generateConfig(requirePassword, full)
    } catch (e: Exception) {
        null
    }

    private fun generateConfigError(requirePassword: Boolean, checkChannels: Boolean): String? {
        ssidError(ssid, hexSsid)?.let { return it }
        if (requirePassword) passwordError(password)?.let { return it }
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
        val hideBssid = !p2pMode && Build.VERSION.SDK_INT >= 31 &&
                macRandomization != SoftApConfigurationCompat.RANDOMIZATION_NONE
        if (!hideBssid) validateOptionalMac(bssid) {
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
                    require(mac !in blocked) { "A MAC address exists in both client lists" }
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
        val passwordValid = when (selectedSecurityType) {
            SoftApConfiguration.SECURITY_TYPE_OPEN,
            SoftApConfiguration.SECURITY_TYPE_WPA3_OWE_TRANSITION,
            SoftApConfiguration.SECURITY_TYPE_WPA3_OWE -> true
            SoftApConfiguration.SECURITY_TYPE_WPA2_PSK,
            SoftApConfiguration.SECURITY_TYPE_WPA3_SAE_TRANSITION -> password.length in 8..63
            else -> password.isNotEmpty()
        }
        if (requirePassword) require(passwordValid) { app.getString(R.string.wifi_password) }
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
            bssid = if ((p2pMode || Build.VERSION.SDK_INT < 31 &&
                    macRandomizationSetting == SoftApConfigurationCompat.RANDOMIZATION_NONE) &&
                    this@ApConfigurationState.bssid.isNotEmpty()) {
                MacAddress.fromString(this@ApConfigurationState.bssid)
            } else null
            isBridgedModeOpportunisticShutdownEnabled = bridgedModeOpportunisticShutdown
            isIeee80211axEnabled = ieee80211ax
            isIeee80211beEnabled = ieee80211be
            isUserConfiguration = userConfig
            bridgedModeOpportunisticShutdownTimeoutMillis = bridgedTimeout.ifEmpty { "-1" }.toLong()
            vendorElements = VendorElements.deserialize(this@ApConfigurationState.vendorElements)
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
        revision++
    }

    fun convertSsidDisplay(value: String, hex: Boolean): String {
        val parsedSsid = if (hex) WifiSsidCompat.fromHex(value) else WifiSsidCompat.fromUtf8Text(value)
        return if (hex) parsedSsid?.decode() ?: throw IllegalArgumentException("Invalid UTF-8")
        else parsedSsid?.hex.orEmpty()
    }

    fun ssidSafeModeWarning(value: String, hex: Boolean): String? {
        if (!p2pMode || !RepeaterService.safeMode) return null
        val size = ssidByteCount(value, hex)
        return if (size in 1..8) app.getString(R.string.settings_service_repeater_safe_mode_warning) else null
    }

    fun ssidByteCount(value: String, hex: Boolean) = try {
        (if (hex) WifiSsidCompat.fromHex(value) else WifiSsidCompat.fromUtf8Text(value))?.bytes?.size ?: 0
    } catch (_: IllegalArgumentException) {
        0
    }

    fun ssidError(value: String, hex: Boolean): String? = try {
        val size = (if (hex) WifiSsidCompat.fromHex(value) else WifiSsidCompat.fromUtf8Text(value))?.bytes?.size ?: 0
        if (size in 1..32) null else app.getString(R.string.wifi_ssid)
    } catch (e: IllegalArgumentException) {
        e.readableMessage
    }

    fun passwordError(value: String): String? {
        val valid = when (selectedSecurityType) {
            SoftApConfiguration.SECURITY_TYPE_OPEN,
            SoftApConfiguration.SECURITY_TYPE_WPA3_OWE_TRANSITION,
            SoftApConfiguration.SECURITY_TYPE_WPA3_OWE -> true
            SoftApConfiguration.SECURITY_TYPE_WPA2_PSK,
            SoftApConfiguration.SECURITY_TYPE_WPA3_SAE_TRANSITION -> value.length in 8..63
            else -> value.isNotEmpty()
        }
        return if (valid) null else app.getString(R.string.wifi_password)
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
        clientIsolation = if (Build.VERSION.SDK_INT >= 36) config.isClientIsolationEnabled else false
        userConfig = config.isUserConfiguration
        acs2g = RangeInput.toString(config.allowedAcsChannels[SoftApConfiguration.BAND_2GHZ]).orEmpty()
        acs5g = RangeInput.toString(config.allowedAcsChannels[SoftApConfiguration.BAND_5GHZ]).orEmpty()
        acs6g = RangeInput.toString(config.allowedAcsChannels[SoftApConfiguration.BAND_6GHZ]).orEmpty()
        maxChannelBandwidth = normalizeMaxChannelBandwidth(config.maxChannelBandwidth, maxChannelBandwidth)
        revision++
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

    fun securityEntries() = securityOptions
    fun channelEntries(allowDisabled: Boolean = false) = if (allowDisabled) listOf(ChannelOption.Disabled) +
            channelOptions else channelOptions
    fun bandwidthEntries() = bandwidthOptions

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
        p2pMode = p2pMode,
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
    val p2pMode: Boolean,
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
    val clientIsolation: Boolean,
    val userConfig: Boolean,
    val acs2g: String,
    val acs5g: String,
    val acs6g: String,
    val maxChannelBandwidth: Int,
) : Parcelable

internal class ApConfigurationSession(
    val initial: SoftApConfigurationCompat,
    val readOnly: Boolean = false,
    val p2pMode: Boolean = false,
    val target: ApConfigurationTarget,
    val repeaterMaster: P2pSupplicantConfiguration? = null,
    val onApply: suspend (SoftApConfigurationCompat) -> Boolean,
)

internal enum class ApConfigurationTarget {
    System,
    Repeater,
    Temporary,
}

@Composable
internal fun ApConfigurationScreen(state: ApConfigurationState) {
    val navigationBarPadding = WindowInsets.navigationBars.asPaddingValues()
    SettingsList(
        contentPadding = PaddingValues(
            top = 8.dp,
            bottom = 8.dp + navigationBarPadding.calculateBottomPadding(),
        ),
    ) {
        item {
            PreferenceGroup {
                row { SsidApRow(state) }
                state.actionError?.let {
                    contentItem { ErrorApText(it, Modifier.padding(horizontal = 24.dp, vertical = 4.dp)) }
                }
                if (!state.p2pMode || Build.VERSION.SDK_INT >= 36) row {
                    ListApRow(
                        icon = R.drawable.ic_action_wifi_protected_setup,
                        title = R.string.wifi_security,
                        selected = state.securityLabel,
                        enabled = true,
                        entries = state.securityEntries(),
                        entryLabel = { it.label },
                        onSelect = { state.securityType = it.value },
                    )
                }
                row { PasswordApRow(state) }
                row {
                    SwitchApRow(
                        icon = R.drawable.ic_action_timer,
                        title = R.string.wifi_hotspot_auto_off,
                        checked = state.autoShutdown,
                        readOnly = false,
                        summary = annotatedStringResource(R.string.wifi_hotspot_auto_off_help),
                    ) {
                        state.autoShutdown = it
                    }
                }
                if (state.p2pMode || Build.VERSION.SDK_INT >= 30) {
                    row {
                        TextApRow(
                            icon = R.drawable.ic_action_timer,
                            title = R.string.wifi_hotspot_timeout,
                            value = state.timeout,
                            summary = timeoutSummary(
                                state.timeout,
                                TetherTimeoutMonitor.defaultTimeout.toLong(),
                            ),
                            readOnly = false,
                            description = annotatedStringResource(
                                R.string.wifi_hotspot_timeout_help,
                                formatTimeoutMillis(TetherTimeoutMonitor.defaultTimeout.toLong()),
                            ),
                            keyboardType = KeyboardType.Number,
                            maxLength = 19,
                            placeholder = TetherTimeoutMonitor.defaultTimeout.toString(),
                            suffix = "ms",
                            validator = { value ->
                                validateOptionalLong(value) { timeout ->
                                    if (!state.p2pMode && Build.VERSION.SDK_INT >= 30) {
                                        SoftApConfigurationCompat.testPlatformTimeoutValidity(timeout)
                                    }
                                }
                            },
                        ) { state.timeout = it }
                    }
                }
                if (!state.p2pMode && Build.VERSION.SDK_INT >= 31) {
                    row {
                        SwitchApRow(
                            icon = R.drawable.ic_action_timer,
                            title = R.string.wifi_bridged_mode_opportunistic_shutdown,
                            checked = state.bridgedModeOpportunisticShutdown,
                            readOnly = false,
                            summary = annotatedStringResource(R.string.wifi_bridged_mode_opportunistic_shutdown_help),
                        ) { state.bridgedModeOpportunisticShutdown = it }
                    }
                    row {
                        TextApRow(
                            icon = R.drawable.ic_action_timer,
                            title = R.string.wifi_hotspot_timeout_bridged,
                            value = state.bridgedTimeout,
                            summary = timeoutSummary(
                                state.bridgedTimeout,
                                TetherTimeoutMonitor.defaultTimeoutBridged.toLong(),
                            ),
                            readOnly = Build.VERSION.SDK_INT < 33,
                            description = annotatedStringResource(
                                R.string.wifi_hotspot_timeout_bridged_help,
                                formatTimeoutMillis(TetherTimeoutMonitor.defaultTimeoutBridged.toLong()),
                            ),
                            keyboardType = KeyboardType.Number,
                            maxLength = 19,
                            placeholder = TetherTimeoutMonitor.defaultTimeoutBridged.toString(),
                            suffix = "ms",
                            validator = { value ->
                                validateOptionalLong(value, SoftApConfigurationCompat::testPlatformBridgedTimeoutValidity)
                            },
                        ) { state.bridgedTimeout = it }
                    }
                }
            }
        }
        item {
            PreferenceGroup(title = stringResource(R.string.wifi_hotspot_ap_band_title)) {
                if (!state.p2pMode && Build.VERSION.SDK_INT >= 30 &&
                    SoftApConfigurationCompat.isBandOptimizationSupported) {
                    row {
                        SwitchApRow(
                            icon = R.drawable.ic_action_tune,
                            title = R.string.wifi_band_optimization,
                            checked = state.bandOptimization,
                            readOnly = false,
                            summary = annotatedStringResource(R.string.wifi_band_optimization_help),
                        ) {
                            state.bandOptimization = it
                        }
                    }
                }
                row {
                    ListApRow(
                        icon = R.drawable.ic_action_settings_input_antenna,
                        title = R.string.wifi_hotspot_ap_channel_band_title,
                        selected = state.primaryChannelLabel,
                        enabled = true,
                        entries = state.channelEntries(),
                        entryLabel = { it.toString() },
                        description = annotatedStringResource(R.string.wifi_hotspot_ap_channel_band_help),
                        onSelect = { state.primaryChannel = it },
                    )
                }
                if (!state.p2pMode && Build.VERSION.SDK_INT >= 31) row {
                    ListApRow(
                        icon = R.drawable.ic_action_settings_input_antenna,
                        title = R.string.wifi_hotspot_concurrent_ap_channel_band_title,
                        selected = state.secondaryChannelLabel,
                        enabled = true,
                        entries = state.channelEntries(allowDisabled = true),
                        entryLabel = { it.toString() },
                        description = annotatedStringResource(R.string.wifi_hotspot_concurrent_ap_channel_band_help),
                        onSelect = { state.secondaryChannel = it },
                    )
                }
                state.channelError?.let {
                    contentItem { ErrorApText(it, Modifier.padding(horizontal = 24.dp, vertical = 4.dp)) }
                }
                if (!state.p2pMode && Build.VERSION.SDK_INT >= 33) {
                    row {
                        TextApRow(
                            icon = R.drawable.ic_action_tune,
                            title = R.string.wifi_hotspot_acs_channel_2g,
                            value = state.acs2g,
                            readOnly = false,
                            description = annotatedStringResource(R.string.wifi_hotspot_acs_channel_help),
                            keyboardOptions = MACHINE_TEXT_KEYBOARD_OPTIONS,
                            validator = { validateAcsChannels(SoftApConfiguration.BAND_2GHZ, it) },
                        ) { state.acs2g = it }
                    }
                    row {
                        TextApRow(
                            icon = R.drawable.ic_action_tune,
                            title = R.string.wifi_hotspot_acs_channel_5g,
                            value = state.acs5g,
                            readOnly = false,
                            description = annotatedStringResource(R.string.wifi_hotspot_acs_channel_help),
                            keyboardOptions = MACHINE_TEXT_KEYBOARD_OPTIONS,
                            validator = { validateAcsChannels(SoftApConfiguration.BAND_5GHZ, it) },
                        ) { state.acs5g = it }
                    }
                    row {
                        TextApRow(
                            icon = R.drawable.ic_action_tune,
                            title = R.string.wifi_hotspot_acs_channel_6g,
                            value = state.acs6g,
                            readOnly = false,
                            description = annotatedStringResource(R.string.wifi_hotspot_acs_channel_help),
                            keyboardOptions = MACHINE_TEXT_KEYBOARD_OPTIONS,
                            validator = { validateAcsChannels(SoftApConfiguration.BAND_6GHZ, it) },
                        ) { state.acs6g = it }
                    }
                    row {
                        ListApRow(
                            icon = R.drawable.ic_action_settings_input_antenna,
                            title = R.string.wifi_hotspot_max_channel_bandwidth,
                            selected = state.maxChannelBandwidthLabel,
                            enabled = true,
                            entries = state.bandwidthEntries(),
                            entryLabel = { it.name },
                            description = annotatedStringResource(R.string.wifi_hotspot_max_channel_bandwidth_help),
                            onSelect = { state.maxChannelBandwidth = it.width },
                        )
                    }
                    state.maxChannelBandwidthError?.let {
                        contentItem { ErrorApText(it, Modifier.padding(horizontal = 24.dp, vertical = 4.dp)) }
                    }
                }
            }
        }
        if (!state.p2pMode && Build.VERSION.SDK_INT >= 30) {
            item {
                PreferenceGroup(title = stringResource(R.string.wifi_hotspot_access_control_title)) {
                    row {
                        TextApRow(
                            icon = R.drawable.ic_social_people,
                            title = R.string.wifi_max_clients,
                            value = state.maxClients,
                            readOnly = false,
                            description = annotatedStringResource(R.string.wifi_max_clients_help),
                            keyboardType = KeyboardType.Number,
                            maxLength = 10,
                            validator = { value ->
                                if (value.isEmpty()) null else try {
                                    value.toInt()
                                    null
                                } catch (e: NumberFormatException) {
                                    e.readableMessage
                                }
                            },
                        ) { state.maxClients = it }
                    }
                    row {
                        TextApRow(
                            icon = R.drawable.ic_action_block,
                            title = R.string.wifi_blocked_list,
                            value = state.blockedList,
                            readOnly = false,
                            description = annotatedStringResource(R.string.wifi_blocked_list_help),
                            keyboardOptions = MACHINE_TEXT_KEYBOARD_OPTIONS,
                            minLines = 3,
                            validator = { validateMacList(it) },
                        ) { state.blockedList = it }
                    }
                    row {
                        SwitchApRow(
                            icon = R.drawable.ic_social_people,
                            title = R.string.wifi_client_user_control,
                            checked = state.clientUserControl,
                            readOnly = false,
                            summary = annotatedStringResource(R.string.wifi_client_user_control_help),
                        ) {
                            state.clientUserControl = it
                        }
                    }
                    row {
                        TextApRow(
                            icon = R.drawable.ic_action_check_circle,
                            title = R.string.wifi_allowed_list,
                            value = state.allowedList,
                            readOnly = !state.clientUserControl,
                            description = annotatedStringResource(R.string.wifi_allowed_list_help),
                            keyboardOptions = MACHINE_TEXT_KEYBOARD_OPTIONS,
                            minLines = 3,
                            validator = { value ->
                                validateMacList(value) ?: try {
                                    val blocked = try {
                                        parseMacList(state.blockedList).toSet()
                                    } catch (_: IllegalArgumentException) {
                                        emptySet()
                                    }
                                    require(parseMacList(value).none { it in blocked }) {
                                        "A MAC address exists in both client lists"
                                    }
                                    null
                                } catch (e: IllegalArgumentException) {
                                    e.readableMessage
                                }
                            },
                        ) { state.allowedList = it }
                    }
                }
            }
        }
        item {
            PreferenceGroup(title = stringResource(R.string.wifi_hotspot_ap_advanced_title)) {
                if (state.p2pMode || Build.VERSION.SDK_INT >= 31) row {
                    ListApRow(
                        icon = R.drawable.ic_action_autorenew,
                        title = R.string.wifi_mac_randomization,
                        selected = state.macRandomizationLabel,
                        enabled = !state.p2pMode,
                        entries = app.resources.getStringArray(R.array.wifi_mac_randomization).mapIndexed { index, label ->
                            index to label
                        },
                        entryLabel = { it.second },
                        entrySummary = {
                            annotatedStringResource(when (it.first) {
                                SoftApConfigurationCompat.RANDOMIZATION_NONE ->
                                    R.string.wifi_mac_randomization_none_help
                                SoftApConfigurationCompat.RANDOMIZATION_PERSISTENT ->
                                    R.string.wifi_mac_randomization_persistent_help
                                else -> R.string.wifi_mac_randomization_non_persistent_help
                            })
                        },
                        description = annotatedStringResource(R.string.wifi_mac_randomization_help),
                        onSelect = { state.macRandomization = it.first },
                    )
                }
                if (state.p2pMode || Build.VERSION.SDK_INT < 31 ||
                    state.macRandomization == SoftApConfigurationCompat.RANDOMIZATION_NONE) {
                    row {
                        TextApRow(
                            icon = R.drawable.ic_content_push_pin,
                            title = R.string.wifi_advanced_mac_address_title,
                            value = state.bssid,
                            readOnly = false,
                            description = annotatedStringResource(R.string.wifi_advanced_mac_address_help),
                            keyboardOptions = MACHINE_TEXT_KEYBOARD_OPTIONS,
                            maxLength = 17,
                            validator = { value ->
                                validateOptionalMac(value) { mac ->
                                    if (Build.VERSION.SDK_INT >= 30 && !state.p2pMode) {
                                        SoftApConfigurationCompat.testPlatformValidity(mac)
                                    }
                                }
                            },
                        ) { state.bssid = it }
                    }
                }
                if (!state.p2pMode && Build.VERSION.SDK_INT >= 33) {
                    row {
                        TextApRow(
                            icon = R.drawable.ic_action_autorenew,
                            title = R.string.wifi_advanced_mac_address_persistent_randomized,
                            value = state.persistentRandomizedMac,
                            readOnly = false,
                            description = annotatedStringResource(
                                R.string.wifi_advanced_mac_address_persistent_randomized_help,
                            ),
                            keyboardOptions = MACHINE_TEXT_KEYBOARD_OPTIONS,
                            maxLength = 17,
                            validator = { validateOptionalMac(it) },
                        ) { state.persistentRandomizedMac = it }
                    }
                }
                if (!state.p2pMode) row {
                    SwitchApRow(
                        icon = R.drawable.ic_action_visibility_off,
                        title = R.string.wifi_hidden_network,
                        checked = state.hiddenSsid,
                        readOnly = false,
                        summary = annotatedStringResource(R.string.wifi_hidden_network_help),
                    ) {
                        state.hiddenSsid = it
                    }
                }
                if (!state.p2pMode && Build.VERSION.SDK_INT >= 31) {
                    row {
                        SwitchApRow(
                            icon = R.drawable.ic_image_looks_6,
                            title = R.string.wifi_ieee_80211ax,
                            checked = state.ieee80211ax,
                            readOnly = false,
                            summary = annotatedStringResource(R.string.wifi_ieee_80211ax_help),
                        ) {
                            state.ieee80211ax = it
                        }
                    }
                    if (Build.VERSION.SDK_INT >= 33) row {
                        SwitchApRow(
                            icon = R.drawable.ic_device_network_wifi,
                            title = R.string.wifi_ieee_80211be,
                            checked = state.ieee80211be,
                            readOnly = false,
                            summary = annotatedStringResource(R.string.wifi_ieee_80211be_help),
                        ) { state.ieee80211be = it }
                    }
                }
                if (Build.VERSION.SDK_INT >= 33) {
                    row {
                        TextApRow(
                            icon = R.drawable.ic_action_code,
                            title = R.string.wifi_vendor_elements,
                            value = state.vendorElements,
                            readOnly = false,
                            description = annotatedStringResource(R.string.wifi_vendor_elements_help),
                            keyboardOptions = MACHINE_TEXT_KEYBOARD_OPTIONS,
                            minLines = 3,
                            validator = { value ->
                                try {
                                    VendorElements.deserialize(value).also {
                                        if (!state.p2pMode) SoftApConfigurationCompat.testPlatformValidity(it)
                                    }
                                    null
                                } catch (e: Exception) {
                                    e.readableMessage
                                }
                            },
                        ) { state.vendorElements = it }
                    }
                }
                if (!state.p2pMode && Build.VERSION.SDK_INT >= 36) {
                    row {
                        SwitchApRow(
                            icon = R.drawable.ic_action_block,
                            title = R.string.wifi_client_isolation,
                            checked = state.clientIsolation,
                            readOnly = false,
                            summary = annotatedStringResource(R.string.wifi_client_isolation_help),
                        ) {
                            state.clientIsolation = it
                        }
                    }
                }
                if (!state.p2pMode && Build.VERSION.SDK_INT >= 31) {
                    row {
                        SwitchApRow(
                            icon = R.drawable.ic_action_settings,
                            title = R.string.wifi_user_config,
                            checked = state.userConfig,
                            readOnly = true,
                            summary = annotatedStringResource(R.string.wifi_user_config_help),
                        ) {
                            state.userConfig = it
                        }
                    }
                }
            }
        }
    }
}

@Composable
@OptIn(DelicateCoroutinesApi::class)
internal fun ApConfigurationTopBarActions(
    state: ApConfigurationState,
    session: ApConfigurationSession,
    snackbarHostState: SnackbarHostState,
    onApplied: () -> Unit,
) {
    androidx.compose.runtime.rememberCoroutineScope().let { scope ->
        var qrCode by rememberSaveable { mutableStateOf<String?>(null) }
        var overflowExpanded by androidx.compose.runtime.remember { mutableStateOf(false) }
        if (state.possiblyInvalid) Icon(
            painter = painterResource(R.drawable.ic_alert_warning),
            contentDescription = stringResource(R.string.configuration_invalid),
            tint = MaterialTheme.colorScheme.error,
            modifier = Modifier.size(24.dp),
        )
        TooltipIconButton(
            tooltip = stringResource(android.R.string.copy),
            enabled = state.canCopy,
            onClick = {
                try {
                    state.copyToClipboard()
                } catch (e: RuntimeException) {
                    scope.launch { snackbarHostState.showLongSnackbar(e.readableMessage) }
                }
            },
        ) {
            Icon(
                painter = painterResource(R.drawable.ic_content_file_copy),
                contentDescription = stringResource(android.R.string.copy),
            )
        }
        TooltipIconButton(
            tooltip = stringResource(androidx.appcompat.R.string.abc_action_menu_overflow_description),
            onClick = { overflowExpanded = true },
        ) {
            Icon(
                painter = painterResource(R.drawable.ic_more_vert),
                contentDescription = stringResource(androidx.appcompat.R.string.abc_action_menu_overflow_description),
            )
        }
        DropdownMenu(expanded = overflowExpanded, onDismissRequest = { overflowExpanded = false }) {
            DropdownMenuItem(
                text = { Text(stringResource(android.R.string.paste)) },
                leadingIcon = {
                    Icon(
                        painter = painterResource(R.drawable.ic_content_paste),
                        contentDescription = null,
                    )
                },
                onClick = {
                    overflowExpanded = false
                    try {
                        state.pasteFromClipboard()
                    } catch (e: RuntimeException) {
                        scope.launch { snackbarHostState.showLongSnackbar(e.readableMessage) }
                    }
                },
            )
            DropdownMenuItem(
                text = { Text(stringResource(R.string.configuration_share)) },
                enabled = state.canShare,
                leadingIcon = {
                    Icon(
                        painter = painterResource(R.drawable.ic_settings_qrcode),
                        contentDescription = null,
                    )
                },
                onClick = {
                    overflowExpanded = false
                    try {
                        qrCode = state.generateConfig(requirePassword = false, full = false).toQrCode()
                    } catch (e: RuntimeException) {
                        scope.launch { snackbarHostState.showLongSnackbar(e.readableMessage) }
                    }
                },
            )
        }
        if (!state.readOnly) TooltipIconButton(
            tooltip = stringResource(R.string.wifi_save),
            enabled = state.canSave,
            onClick = {
                val config = state.generateConfig()
                GlobalScope.launch(Dispatchers.Main.immediate) {
                    try {
                        if (session.onApply(config)) onApplied()
                    } catch (e: CancellationException) {
                        throw e
                    } catch (e: Exception) {
                        Timber.w(e)
                        snackbarHostState.showLongSnackbar(e.readableMessage)
                    }
                }
            },
        ) {
            Icon(
                painter = painterResource(R.drawable.ic_content_save),
                contentDescription = stringResource(R.string.wifi_save),
            )
        }
        qrCode?.let { value ->
            QrCodeDialog(value) { qrCode = null }
        }
    }
}

@Composable
private fun QrCodeDialog(value: String, onDismiss: () -> Unit) {
    val context = androidx.compose.ui.platform.LocalContext.current
    val size = dimensionResource(R.dimen.qrcode_size)
    val density = LocalDensity.current
    val (bitmap, error) = androidx.compose.runtime.remember(value, size, density) {
        try {
            val sizePx = with(density) { size.roundToPx() }.coerceAtLeast(1)
            val hints = mutableMapOf<EncodeHintType, Any>()
            if (!StandardCharsets.ISO_8859_1.newEncoder().canEncode(value)) {
                hints[EncodeHintType.CHARACTER_SET] = StandardCharsets.UTF_8.name()
            }
            val qrBits = MultiFormatWriter().encode(value, BarcodeFormat.QR_CODE, sizePx, sizePx, hints)
            Bitmap.createBitmap(sizePx, sizePx, Bitmap.Config.RGB_565).also {
                for (x in 0 until sizePx) for (y in 0 until sizePx) {
                    it[x, y] = if (qrBits.get(x, y)) Color.BLACK else Color.WHITE
                }
            } to null
        } catch (e: WriterException) {
            Timber.w(e)
            null to e.readableMessage
        }
    }
    LaunchedEffect(error) {
        error?.let {
            Toast.makeText(context, it, Toast.LENGTH_LONG).show()
            onDismiss()
        }
    }
    AlertDialog(
        onDismissRequest = onDismiss,
        text = {
            if (bitmap == null) Text(stringResource(R.string.configuration_share))
            else Image(
                    bitmap = bitmap.asImageBitmap(),
                    contentDescription = stringResource(R.string.configuration_share),
                    modifier = Modifier.size(size),
                )
        },
        confirmButton = {
            TextButton(onClick = onDismiss) {
                Text(stringResource(android.R.string.ok))
            }
        },
    )
}

internal suspend fun loadSystemApConfiguration(snackbarHostState: SnackbarHostState): SoftApConfigurationCompat? {
    return try {
        if (Build.VERSION.SDK_INT < 30) @Suppress("DEPRECATION") {
            WifiApManager.configurationLegacy?.toCompat() ?: SoftApConfigurationCompat()
        } else WifiApManager.configuration.toCompat()
    } catch (e: InvocationTargetException) {
        if (e.targetException !is SecurityException) Timber.w(e)
        try {
            if (Build.VERSION.SDK_INT < 30) @Suppress("DEPRECATION") {
                RootManager.use { it.execute(WifiApCommands.GetConfigurationLegacy()) }?.toCompat()
                    ?: SoftApConfigurationCompat()
            } else RootManager.use { it.execute(WifiApCommands.GetConfiguration()) }.toCompat()
        } catch (_: CancellationException) {
            null
        } catch (eRoot: Exception) {
            eRoot.addSuppressed(e)
            if (Build.VERSION.SDK_INT >= 30 || eRoot.getRootCause() !is SecurityException) Timber.w(eRoot)
            snackbarHostState.showLongSnackbar(eRoot.readableMessage)
            null
        }
    } catch (e: IllegalArgumentException) {
        Timber.w(e)
        snackbarHostState.showLongSnackbar(e.readableMessage)
        null
    }
}

internal suspend fun applySystemApConfiguration(
    configuration: SoftApConfigurationCompat,
    snackbarHostState: SnackbarHostState,
): Boolean {
    if (Build.VERSION.SDK_INT < 30) @Suppress("DEPRECATION") {
        if (configuration.isAutoShutdownEnabled != TetherTimeoutMonitor.enabled) try {
            TetherTimeoutMonitor.setEnabled(configuration.isAutoShutdownEnabled)
        } catch (e: Exception) {
            Timber.w(e)
            snackbarHostState.showLongSnackbar(e.readableMessage)
        }
        val wc = configuration.toWifiConfiguration()
        try {
            if (WifiApManager.setConfiguration(wc)) return true
        } catch (e: InvocationTargetException) {
            try {
                if (RootManager.use { it.execute(WifiApCommands.SetConfigurationLegacy(wc)) }.value) return true
            } catch (eCancel: CancellationException) {
                snackbarHostState.showLongSnackbar(eCancel.readableMessage)
                return false
            } catch (eRoot: Exception) {
                eRoot.addSuppressed(e)
                Timber.w(eRoot)
                snackbarHostState.showLongSnackbar(eRoot.readableMessage)
                return false
            }
        }
    } else {
        val platform = try {
            configuration.toPlatform()
        } catch (e: InvocationTargetException) {
            Timber.w(e)
            snackbarHostState.showLongSnackbar(e.readableMessage)
            return false
        }
        try {
            if (WifiApManager.setConfiguration(platform)) return true
        } catch (e: InvocationTargetException) {
            try {
                if (RootManager.use { it.execute(WifiApCommands.SetConfiguration(platform)) }.value) return true
            } catch (eCancel: CancellationException) {
                snackbarHostState.showLongSnackbar(eCancel.readableMessage)
                return false
            } catch (eRoot: Exception) {
                eRoot.addSuppressed(e)
                Timber.w(eRoot)
                snackbarHostState.showLongSnackbar(eRoot.readableMessage)
                return false
            }
        }
    }
    snackbarHostState.showLongSnackbar(app.getString(R.string.configuration_rejected))
    return false
}

internal suspend fun loadRepeaterApConfiguration(
    binder: RepeaterService.Binder?,
    snackbarHostState: SnackbarHostState,
): ApConfigurationSession? {
    if (RepeaterService.safeMode) {
        val networkName = RepeaterService.networkName
        val passphrase = RepeaterService.passphrase
        if (networkName != null && passphrase != null) {
            val config = SoftApConfigurationCompat(
                ssid = networkName,
                passphrase = passphrase,
                securityType = RepeaterService.securityType,
                isAutoShutdownEnabled = RepeaterService.isAutoShutdownEnabled,
                shutdownTimeoutMillis = RepeaterService.shutdownTimeoutMillis,
                macRandomizationSetting = if (WifiApManager.p2pMacRandomizationSupported) {
                    SoftApConfigurationCompat.RANDOMIZATION_NON_PERSISTENT
                } else SoftApConfigurationCompat.RANDOMIZATION_NONE,
                vendorElements = RepeaterService.vendorElements,
            ).apply {
                bssid = RepeaterService.deviceAddress
                setChannel(RepeaterService.operatingChannel, RepeaterService.operatingBand)
            }
            return ApConfigurationSession(config, p2pMode = true, target = ApConfigurationTarget.Repeater) {
                applyRepeaterApConfiguration(null, it, snackbarHostState)
            }
        }
    } else if (binder != null) {
        val group = binder.group.value ?: binder.fetchPersistentGroup().let { binder.group.value }
        if (group != null) {
            var readOnly = false
            var master: P2pSupplicantConfiguration? = null
            val config = SoftApConfigurationCompat(
                ssid = WifiSsidCompat.fromUtf8Text(group.networkName),
                securityType = if (Build.VERSION.SDK_INT >= 36) when (group.securityType) {
                    WifiP2pGroup.SECURITY_TYPE_WPA3_COMPATIBILITY -> SoftApConfiguration.SECURITY_TYPE_WPA3_SAE_TRANSITION
                    WifiP2pGroup.SECURITY_TYPE_WPA3_SAE -> SoftApConfiguration.SECURITY_TYPE_WPA3_SAE
                    else -> SoftApConfiguration.SECURITY_TYPE_WPA2_PSK
                } else SoftApConfiguration.SECURITY_TYPE_WPA2_PSK,
                isAutoShutdownEnabled = RepeaterService.isAutoShutdownEnabled,
                shutdownTimeoutMillis = RepeaterService.shutdownTimeoutMillis,
                macRandomizationSetting = if (WifiApManager.p2pMacRandomizationSupported) {
                    SoftApConfigurationCompat.RANDOMIZATION_NON_PERSISTENT
                } else SoftApConfigurationCompat.RANDOMIZATION_NONE,
                vendorElements = RepeaterService.vendorElements,
            ).apply {
                setChannel(RepeaterService.operatingChannel)
                try {
                    val parsed = P2pSupplicantConfiguration(group)
                    parsed.init(binder.obtainDeviceAddress()?.toString())
                    master = parsed
                    passphrase = parsed.psk
                    bssid = parsed.bssid
                } catch (e: Exception) {
                    if (e is P2pSupplicantConfiguration.LoggedException) Timber.d(e)
                    else if (e !is CancellationException) Timber.w(e)
                    readOnly = true
                    passphrase = group.passphrase
                    try {
                        bssid = group.owner?.deviceAddress?.let(MacAddress::fromString)
                    } catch (_: IllegalArgumentException) { }
                }
            }
            return ApConfigurationSession(
                config,
                readOnly = readOnly,
                p2pMode = true,
                target = ApConfigurationTarget.Repeater,
                repeaterMaster = master,
            ) {
                applyRepeaterApConfiguration(binder, it, snackbarHostState, master)
            }
        }
    }
    snackbarHostState.showLongSnackbar(app.getString(R.string.repeater_configure_failure))
    return null
}

internal suspend fun applyRepeaterApConfiguration(
    binder: RepeaterService.Binder?,
    config: SoftApConfigurationCompat,
    snackbarHostState: SnackbarHostState,
    master: P2pSupplicantConfiguration? = null,
): Boolean {
    if (RepeaterService.safeMode) {
        RepeaterService.networkName = config.ssid
        RepeaterService.deviceAddress = config.bssid
        RepeaterService.passphrase = config.passphrase
        RepeaterService.securityType = config.securityType
        applyRepeaterCommonConfiguration(config)
        return true
    }
    if (binder == null && master == null) {
        snackbarHostState.showLongSnackbar(app.getString(R.string.repeater_configure_failure))
        return false
    }
    val group = binder?.group?.value ?: binder?.fetchPersistentGroup()?.let { binder.group.value }
    if (group == null && master == null) {
        snackbarHostState.showLongSnackbar(app.getString(R.string.repeater_configure_failure))
        return false
    }
    val parsed = master ?: try {
        P2pSupplicantConfiguration(group).also { it.init(binder?.obtainDeviceAddress()?.toString()) }
    } catch (e: CancellationException) {
        throw e
    } catch (e: Exception) {
        if (e is P2pSupplicantConfiguration.LoggedException) Timber.d(e) else Timber.w(e)
        snackbarHostState.showLongSnackbar(e.readableMessage)
        return false
    }
    applySupplicantRepeaterConfiguration(parsed, binder, config, snackbarHostState)
    applyRepeaterCommonConfiguration(config)
    return true
}

private suspend fun applySupplicantRepeaterConfiguration(
    master: P2pSupplicantConfiguration,
    binder: RepeaterService.Binder?,
    config: SoftApConfigurationCompat,
    snackbarHostState: SnackbarHostState,
) {
    val group = binder?.group?.value
    val mayBeModified = master.psk != config.passphrase || master.bssid != config.bssid || config.ssid.run {
        if (this != null) decode().let { it == null || group?.networkName != it }
        else group?.networkName != null
    }
    if (!mayBeModified) return
    try {
        withContext(Dispatchers.Default) { master.update(config.ssid!!, config.passphrase!!, config.bssid) }
        binder?.clearGroup()
    } catch (e: Exception) {
        Timber.w(e)
        snackbarHostState.showLongSnackbar(e.readableMessage)
    }
}

private fun applyRepeaterCommonConfiguration(config: SoftApConfigurationCompat) {
    val (band, channel) = SoftApConfigurationCompat.requireSingleBand(config.channels)
    RepeaterService.operatingBand = band
    RepeaterService.operatingChannel = channel
    RepeaterService.isAutoShutdownEnabled = config.isAutoShutdownEnabled
    RepeaterService.shutdownTimeoutMillis = config.shutdownTimeoutMillis
    RepeaterService.vendorElements = config.vendorElements
}

@Composable
private fun SsidApRow(state: ApConfigurationState) {
    var editing by rememberSaveable(state.ssid) { mutableStateOf(false) }
    var draft by rememberTextFieldValueAtEnd(state.ssid, editing)
    var draftHex by rememberSaveable(state.ssid, editing) { mutableStateOf(state.ssidHex) }
    var error by androidx.compose.runtime.remember(editing) { mutableStateOf<String?>(null) }
    val draftError = error ?: state.ssidError(draft.text, draftHex)
    val draftByteCount = state.ssidByteCount(draft.text, draftHex)
    PreferenceRow(
        icon = R.drawable.ic_device_network_wifi,
        title = stringResource(R.string.wifi_ssid),
        summaryContent = {
            Column {
                Text(state.ssid)
                state.ssidWarning?.let { ErrorApText(it) }
            }
        },
        enabled = true,
        onClick = { editing = true },
    )
    if (editing) AlertDialog(
        onDismissRequest = { editing = false },
        title = { Text(stringResource(R.string.wifi_ssid)) },
        text = {
            val focusRequester = rememberDialogFocusRequester()
            Column(verticalArrangement = Arrangement.spacedBy(16.dp)) {
                Text(
                    text = annotatedStringResource(R.string.wifi_ssid_help),
                    style = MaterialTheme.typography.bodyMedium,
                )
                OutlinedTextField(
                    value = draft,
                    onValueChange = {
                        draft = it
                        error = null
                    },
                    modifier = Modifier
                        .fillMaxWidth()
                        .focusRequester(focusRequester)
                        .contentType(WIFI_SSID_CONTENT_TYPE),
                    singleLine = true,
                    isError = draftError != null,
                    supportingText = {
                        Column {
                            draftError?.let { ErrorApText(it) } ?: state.ssidSafeModeWarning(draft.text, draftHex)?.let {
                                ErrorApText(it)
                            }
                            Text("$draftByteCount/32")
                        }
                    },
                    trailingIcon = if (state.canToggleSsidHex) {
                        {
                            val tooltip = stringResource(R.string.wifi_ssid_toggle_hex)
                            TooltipIconButton(
                                tooltip = tooltip,
                                onClick = {
                                    try {
                                        val converted = state.convertSsidDisplay(draft.text, draftHex)
                                        draft = TextFieldValue(converted, TextRange(converted.length))
                                        draftHex = !draftHex
                                        error = null
                                    } catch (e: RuntimeException) {
                                        error = e.readableMessage
                                    }
                                },
                            ) {
                                Icon(
                                    painter = painterResource(if (draftHex) {
                                        R.drawable.ic_av_closed_caption
                                    } else R.drawable.ic_av_closed_caption_off),
                                    contentDescription = tooltip,
                                )
                            }
                        }
                    } else null,
                )
            }
        },
        confirmButton = {
            TextButton(
                enabled = draftError == null,
                onClick = {
                    state.setSsid(draft.text, draftHex)
                    editing = false
                },
            ) {
                Text(stringResource(android.R.string.ok))
            }
        },
        dismissButton = {
            TextButton(onClick = { editing = false }) {
                Text(stringResource(android.R.string.cancel))
            }
        },
    )
}

@Composable
private fun TextApRow(
    @DrawableRes icon: Int,
    @StringRes title: Int,
    value: String,
    readOnly: Boolean,
    summary: String = value,
    description: AnnotatedString? = null,
    keyboardType: KeyboardType = KeyboardType.Text,
    keyboardOptions: KeyboardOptions = KeyboardOptions(keyboardType = keyboardType),
    maxLength: Int? = null,
    minLines: Int = if (value.contains('\n')) 3 else 1,
    placeholder: String? = null,
    suffix: String? = null,
    supportingText: String? = null,
    validator: (String) -> String? = { null },
    onValueChange: (String) -> Unit,
) {
    var editing by rememberSaveable(value) { mutableStateOf(false) }
    var draft by rememberTextFieldValueAtEnd(value, editing)
    val error = validator(draft.text)
    PreferenceRow(
        icon = icon,
        title = stringResource(title),
        summary = summary,
        enabled = !readOnly,
        onClick = { editing = true },
    )
    if (editing) AlertDialog(
        onDismissRequest = { editing = false },
        title = { Text(stringResource(title)) },
        text = {
            val focusRequester = rememberDialogFocusRequester()
            Column(verticalArrangement = Arrangement.spacedBy(16.dp)) {
                description?.let {
                    Text(
                        text = it,
                        style = MaterialTheme.typography.bodyMedium,
                    )
                }
                OutlinedTextField(
                    value = draft,
                    onValueChange = { draft = maxLength?.let(it::takeText) ?: it },
                    modifier = Modifier
                        .fillMaxWidth()
                        .focusRequester(focusRequester),
                    keyboardOptions = keyboardOptions,
                    placeholder = placeholder?.let { { Text(it) } },
                    singleLine = minLines == 1,
                    minLines = minLines,
                    isError = error != null,
                    suffix = suffix?.let { { Text(it) } },
                    supportingText = if (error != null || supportingText != null || maxLength != null) {
                        {
                            Column {
                                error?.let { ErrorApText(it) }
                                if (error == null) supportingText?.let { Text(it) }
                                maxLength?.let { Text("${draft.text.length}/$it") }
                            }
                        }
                    } else null,
                )
            }
        },
        confirmButton = {
            TextButton(
                enabled = error == null,
                onClick = {
                    onValueChange(draft.text)
                    editing = false
                },
            ) {
                Text(stringResource(android.R.string.ok))
            }
        },
        dismissButton = {
            TextButton(onClick = { editing = false }) {
                Text(stringResource(android.R.string.cancel))
            }
        },
    )
}

@Composable
private fun PasswordApRow(state: ApConfigurationState) {
    val enabled = state.passwordEnabled
    val maxLength = state.passwordMaxLength
    var editing by rememberSaveable(state.password) { mutableStateOf(false) }
    var draft by rememberTextFieldValueAtEnd(state.password, editing)
    var visible by rememberSaveable(editing) { mutableStateOf(false) }
    val error = state.passwordError(draft.text)
    PreferenceRow(
        icon = R.drawable.ic_device_wifi_lock,
        title = stringResource(R.string.wifi_password),
        summary = if (!enabled || state.password.isEmpty()) "" else "\u2022".repeat(8),
        enabled = enabled,
        onClick = if (enabled) ({ editing = true }) else null,
    )
    if (editing) AlertDialog(
        onDismissRequest = { editing = false },
        title = { Text(stringResource(R.string.wifi_password)) },
        text = {
            val focusRequester = rememberDialogFocusRequester()
            Column(verticalArrangement = Arrangement.spacedBy(16.dp)) {
                Text(
                    text = annotatedStringResource(R.string.wifi_password_help),
                    style = MaterialTheme.typography.bodyMedium,
                )
                OutlinedTextField(
                    value = draft,
                    onValueChange = { draft = if (maxLength) it.takeText(63) else it },
                    modifier = Modifier
                        .fillMaxWidth()
                        .focusRequester(focusRequester)
                        .contentType(WIFI_PASSWORD_CONTENT_TYPE),
                    keyboardOptions = KeyboardOptions(
                        autoCorrectEnabled = false,
                        keyboardType = KeyboardType.Password,
                    ),
                    singleLine = true,
                    isError = error != null,
                    supportingText = if (error != null || maxLength) {
                        {
                            Column {
                                error?.let { ErrorApText(it) }
                                if (maxLength) Text("${draft.text.length}/63")
                            }
                        }
                    } else null,
                    trailingIcon = {
                        TooltipIconButton(
                            tooltip = stringResource(R.string.wifi_password),
                            onClick = { visible = !visible },
                        ) {
                            Icon(
                                painter = painterResource(R.drawable.ic_image_remove_red_eye),
                                contentDescription = stringResource(R.string.wifi_password),
                            )
                        }
                    },
                    textStyle = LocalTextStyle.current.copy(fontFamily = FontFamily.Monospace),
                    visualTransformation = if (visible) VisualTransformation.None else PasswordVisualTransformation(),
                )
            }
        },
        confirmButton = {
            TextButton(
                enabled = error == null,
                onClick = {
                    state.password = draft.text
                    editing = false
                },
            ) {
                Text(stringResource(android.R.string.ok))
            }
        },
        dismissButton = {
            TextButton(onClick = { editing = false }) {
                Text(stringResource(android.R.string.cancel))
            }
        },
    )
}

@Composable
private fun rememberDialogFocusRequester(): FocusRequester {
    val focusRequester = remember { FocusRequester() }
    val keyboard = LocalSoftwareKeyboardController.current
    LaunchedEffect(Unit) {
        focusRequester.requestFocus()
        keyboard?.show()
    }
    return focusRequester
}

private fun TextFieldValue.takeText(maxLength: Int): TextFieldValue {
    if (text.length <= maxLength) return this
    val text = text.take(maxLength)
    return TextFieldValue(
        text,
        TextRange(selection.start.coerceIn(0, text.length), selection.end.coerceIn(0, text.length)),
    )
}

@Composable
private fun ErrorApText(text: String, modifier: Modifier = Modifier) {
    Text(
        text = text,
        color = MaterialTheme.colorScheme.error,
        modifier = modifier,
    )
}

@Composable
private fun SwitchApRow(
    @DrawableRes icon: Int,
    @StringRes title: Int,
    checked: Boolean,
    readOnly: Boolean,
    summary: AnnotatedString? = null,
    onCheckedChange: (Boolean) -> Unit,
) {
    PreferenceRow(
        icon = icon,
        title = stringResource(title),
        summaryContent = summary?.let { { Text(it) } },
        enabled = !readOnly,
        trailing = {
            PreferenceSwitch(
                checked = checked,
                enabled = !readOnly,
                onCheckedChange = if (readOnly) null else onCheckedChange,
            )
        },
        onClick = { onCheckedChange(!checked) },
    )
}

@Composable
private fun <T> ListApRow(
    @DrawableRes icon: Int,
    @StringRes title: Int,
    selected: String,
    enabled: Boolean,
    entries: List<T>,
    entryLabel: (T) -> String,
    entrySummary: @Composable (T) -> AnnotatedString? = { null },
    description: AnnotatedString? = null,
    onSelect: (T) -> Unit,
) {
    var selecting by rememberSaveable { mutableStateOf(false) }
    PreferenceRow(
        icon = icon,
        title = stringResource(title),
        summary = selected,
        enabled = enabled,
        onClick = { selecting = true },
    )
    if (selecting) PreferenceSelectionDialog(
        title = stringResource(title),
        entryCount = entries.size,
        selectedIndex = entries.indexOfFirst { entryLabel(it) == selected },
        entryLabel = { entryLabel(entries[it]) },
        entrySummary = { entrySummary(entries[it]) },
        description = description,
        onDismissRequest = { selecting = false },
        onSelect = { onSelect(entries[it]) },
    )
}

private fun parseMacList(value: String) = value.split(NON_MAC_CHARS).filter { it.isNotEmpty() }.map(MacAddress::fromString)

private fun validateOptionalLong(value: String, validate: (Long) -> Unit): String? {
    if (value.isEmpty()) return null
    return try {
        validate(value.toLong())
        null
    } catch (e: Exception) {
        e.readableMessage
    }
}

private fun validateMacList(value: String): String? = try {
    parseMacList(value)
    null
} catch (e: IllegalArgumentException) {
    e.readableMessage
}

private fun validateOptionalMac(value: String, validate: (MacAddress) -> Unit = {}): String? {
    if (value.isEmpty()) return null
    return try {
        validate(MacAddress.fromString(value))
        null
    } catch (e: Exception) {
        e.readableMessage
    }
}

private fun validateAcsChannels(band: Int, value: String): String? = try {
    SoftApConfigurationCompat.testPlatformValidity(band, RangeInput.fromString(value).toIntArray())
    null
} catch (e: Exception) {
    e.readableMessage
}

private fun timeoutSummary(value: String, defaultMillis: Long): String {
    val millis = value.toLongOrNull()
    return if (millis == null || millis <= 0) {
        app.getString(R.string.wifi_hotspot_timeout_default, formatTimeoutMillis(defaultMillis))
    } else formatTimeoutMillis(millis)
}

private fun formatTimeoutMillis(millis: Long): String {
    val formatter = MeasureFormat.getInstance(
        app.resources.configuration.locales[0],
        MeasureFormat.FormatWidth.WIDE,
    )
    var remaining = millis.coerceAtLeast(0)
    val measures = ArrayList<Measure>(5)
    for ((unitMillis, unit) in arrayOf(
        86_400_000L to MeasureUnit.DAY,
        3_600_000L to MeasureUnit.HOUR,
        60_000L to MeasureUnit.MINUTE,
        1_000L to MeasureUnit.SECOND,
        1L to MeasureUnit.MILLISECOND,
    )) {
        val count = remaining / unitMillis
        if (count <= 0) continue
        measures += Measure(count, unit)
        remaining %= unitMillis
    }
    if (measures.isEmpty()) measures += Measure(0, MeasureUnit.MILLISECOND)
    return formatter.formatMeasures(*measures.toTypedArray())
}

private fun currentChannelOptions(p2pMode: Boolean): List<ChannelOption> = when {
    !p2pMode -> SOFT_AP_OPTIONS
    RepeaterService.safeMode -> P2P_SAFE_OPTIONS
    else -> P2P_UNSAFE_OPTIONS
}

private fun locate(
    channels: SparseIntArray,
    index: Int,
    options: List<ChannelOption>,
    p2pMode: Boolean,
    pasted: Boolean = false,
): ChannelOption {
    val band = channels.keyAt(index)
    val channel = channels.valueAt(index)
    return options.firstOrNull { it.band == band && it.channel == channel } ?: run {
        val msg = "Unable to locate $band, $channel, ${p2pMode && !RepeaterService.safeMode}"
        if (pasted || p2pMode) Timber.w(msg) else Timber.w(Exception(msg))
        options.first()
    }
}

private fun genAutoOptions(band: Int) = (1..band).filter { it and band == it }.map { ChannelOption(it) }

internal data class SecurityOption(val label: String, val value: Int)

internal open class ChannelOption(val band: Int = 0, val channel: Int = 0) {
    object Disabled : ChannelOption(-1) {
        override fun toString() = app.getString(R.string.wifi_ap_choose_disabled)
    }

    override fun toString() = if (channel == 0) {
        val format = DecimalFormat("#.#", DecimalFormatSymbols.getInstance(app.resources.configuration.locales[0]))
        app.getString(R.string.wifi_ap_choose_G, arrayOf(
            SoftApConfiguration.BAND_2GHZ to 2.4,
            SoftApConfiguration.BAND_5GHZ to 5,
            SoftApConfiguration.BAND_6GHZ to 6,
            SoftApConfiguration.BAND_60GHZ to 60,
        ).filter { (mask, _) -> band and mask == mask }.joinToString("/") { (_, name) -> format.format(name) })
    } else "${SoftApConfigurationCompat.channelToFrequency(band, channel)} MHz ($channel)"
}

internal class BandWidth(val width: Int, val name: String = "") : Comparable<BandWidth> {
    override fun compareTo(other: BandWidth) = width - other.width
}

private const val BASE64_FLAGS = Base64.NO_PADDING or Base64.NO_WRAP
private val MACHINE_TEXT_KEYBOARD_OPTIONS = KeyboardOptions(
    autoCorrectEnabled = false,
    keyboardType = KeyboardType.Ascii,
)
private val WIFI_SSID_CONTENT_TYPE = ContentType.NewUsername + ContentType.Username
private val WIFI_PASSWORD_CONTENT_TYPE = ContentType("wifiPassword") + ContentType.Password
private val NON_MAC_CHARS = "[^0-9a-fA-F:]+".toRegex()
private val CHANNELS_2G = (1..14).map { ChannelOption(SoftApConfiguration.BAND_2GHZ, it) }
private val CHANNELS_6G by lazy {
    val c5g = CHANNELS_2G + (1..196).map { ChannelOption(SoftApConfiguration.BAND_5GHZ, it) }
    if (Build.VERSION.SDK_INT >= 30) c5g + (1..253).map { ChannelOption(SoftApConfiguration.BAND_6GHZ, it) }
    else c5g
}
private val P2P_UNSAFE_OPTIONS by lazy {
    listOf(ChannelOption(SoftApConfigurationCompat.BAND_LEGACY)) +
            CHANNELS_2G + (15..165).map { ChannelOption(SoftApConfiguration.BAND_5GHZ, it) }
}
private val P2P_SAFE_OPTIONS by lazy {
    (if (Build.VERSION.SDK_INT >= 36) listOf(
        ChannelOption(SoftApConfigurationCompat.BAND_ANY_30),
        ChannelOption(SoftApConfiguration.BAND_2GHZ),
        ChannelOption(SoftApConfiguration.BAND_5GHZ),
        ChannelOption(SoftApConfiguration.BAND_6GHZ),
    ) else genAutoOptions(SoftApConfigurationCompat.BAND_LEGACY)) + CHANNELS_6G
}
private val SOFT_AP_OPTIONS by lazy {
    if (Build.VERSION.SDK_INT >= 30) {
        genAutoOptions(SoftApConfigurationCompat.BAND_ANY_31) + CHANNELS_6G +
                (1..6).map { ChannelOption(SoftApConfiguration.BAND_60GHZ, it) }
    } else P2P_SAFE_OPTIONS
}
private val SECURITY_TYPES_WITHOUT_PASSWORD = setOf(
    SoftApConfiguration.SECURITY_TYPE_OPEN,
    SoftApConfiguration.SECURITY_TYPE_WPA3_OWE_TRANSITION,
    SoftApConfiguration.SECURITY_TYPE_WPA3_OWE,
)
private val P2P_SECURITY_TYPES = arrayOf("WPA2-Personal", "WPA3-Personal Compatibility Mode", "WPA3-Personal")
