package be.mygod.vpnhotspot.ui

import android.content.ClipData
import android.content.ClipDescription
import android.graphics.Bitmap
import android.graphics.Color
import android.net.MacAddress
import android.net.wifi.SoftApConfiguration
import android.net.wifi.p2p.WifiP2pGroup
import android.os.Build
import android.os.PersistableBundle
import android.util.Base64
import android.util.SparseIntArray
import androidx.annotation.StringRes
import androidx.compose.foundation.Image
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.itemsIndexed
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.ListItem
import androidx.compose.material3.LocalTextStyle
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.res.dimensionResource
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
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
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
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
    private val base = initial
    private var hexSsid = initial.ssid?.decode() == null
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

    var revision by mutableIntStateOf(0)
        private set
    var ssid by mutableStateOf(initial.ssid?.let { if (hexSsid) it.hex else it.decode() ?: it.hex }.orEmpty())
    var securityType by mutableIntStateOf(initial.securityType)
    var password by mutableStateOf(initial.passphrase.orEmpty())
    var autoShutdown by mutableStateOf(initial.isAutoShutdownEnabled)
    var timeout by mutableStateOf(initial.shutdownTimeoutMillis.let { if (it <= 0) "" else it.toString() })
    var bridgedModeOpportunisticShutdown by mutableStateOf(initial.isBridgedModeOpportunisticShutdownEnabled)
    var bridgedTimeout by mutableStateOf(initial.bridgedModeOpportunisticShutdownTimeoutMillis.let {
        if (it == -1L) "" else it.toString()
    })
    var primaryChannel by mutableStateOf(locate(initial.channels, 0, channelOptions))
    var secondaryChannel by mutableStateOf(if (initial.channels.size() > 1) {
        locate(initial.channels, 1, channelOptions)
    } else ChannelOption.Disabled)
    var bandOptimization by mutableStateOf(initial.isBandOptimizationEnabled ?: true)
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
    var vendorElements by mutableStateOf(if (Build.VERSION.SDK_INT >= 33) {
        VendorElements.serialize(initial.vendorElements)
    } else "")
    var clientIsolation by mutableStateOf(if (Build.VERSION.SDK_INT >= 36) initial.isClientIsolationEnabled else false)
    var userConfig by mutableStateOf(initial.isUserConfiguration)
    var acs2g by mutableStateOf(RangeInput.toString(initial.allowedAcsChannels[SoftApConfiguration.BAND_2GHZ]).orEmpty())
    var acs5g by mutableStateOf(RangeInput.toString(initial.allowedAcsChannels[SoftApConfiguration.BAND_5GHZ]).orEmpty())
    var acs6g by mutableStateOf(RangeInput.toString(initial.allowedAcsChannels[SoftApConfiguration.BAND_6GHZ]).orEmpty())
    var maxChannelBandwidth by mutableIntStateOf(initial.maxChannelBandwidth)

    val canSave get() = !readOnly && generateConfigOrNull() != null
    val canCopy get() = generateConfigOrNull(requirePassword = false) != null
    val possiblyInvalid get() = Build.VERSION.SDK_INT >= 34 && !p2pMode && !readOnly && try {
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

    fun generateConfigOrNull(requirePassword: Boolean = true) = try {
        generateConfig(requirePassword)
    } catch (e: Exception) {
        null
    }

    fun generateConfig(requirePassword: Boolean = true): SoftApConfigurationCompat {
        val selectedSecurityType = if (p2pMode && Build.VERSION.SDK_INT < 36) {
            SoftApConfiguration.SECURITY_TYPE_WPA2_PSK
        } else securityType
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
            securityType = selectedSecurityType
            if (!p2pMode) isHiddenSsid = hiddenSsid
            isAutoShutdownEnabled = autoShutdown
            shutdownTimeoutMillis = timeout.ifEmpty { "0" }.toLong()
            channels = SparseIntArray(2).also { channels ->
                if (!p2pMode && Build.VERSION.SDK_INT >= 31 && secondaryChannel.band >= 0) {
                    channels.put(secondaryChannel.band, secondaryChannel.channel)
                }
                channels.put(primaryChannel.band, primaryChannel.channel)
            }
            if (!p2pMode && Build.VERSION.SDK_INT >= 30 && SoftApConfigurationCompat.isBandOptimizationSupported) {
                isBandOptimizationEnabled = bandOptimization
            }
            if (!p2pMode && Build.VERSION.SDK_INT >= 30) {
                maxNumberOfClients = maxClients.ifEmpty { "0" }.toInt()
                isClientControlByUserEnabled = clientUserControl
                blockedClientList = parseMacList(blockedList)
                allowedClientList = parseMacList(allowedList).also { allowed ->
                    val blocked = blockedClientList.toSet()
                    require(allowed.none { it in blocked }) { "A MAC address exists in both client lists" }
                }
            }
            macRandomizationSetting = macRandomization
            bssid = if ((p2pMode || Build.VERSION.SDK_INT < 31 &&
                    macRandomizationSetting == SoftApConfigurationCompat.RANDOMIZATION_NONE) &&
                    this@ApConfigurationState.bssid.isNotEmpty()) {
                MacAddress.fromString(this@ApConfigurationState.bssid)
            } else null
            if (!p2pMode && Build.VERSION.SDK_INT >= 31) {
                isBridgedModeOpportunisticShutdownEnabled = bridgedModeOpportunisticShutdown
                isIeee80211axEnabled = ieee80211ax
                isUserConfiguration = userConfig
            }
            if (!p2pMode && Build.VERSION.SDK_INT >= 33) {
                bridgedModeOpportunisticShutdownTimeoutMillis = bridgedTimeout.ifEmpty { "-1" }.toLong()
                isIeee80211beEnabled = ieee80211be
                vendorElements = VendorElements.deserialize(this@ApConfigurationState.vendorElements)
                persistentRandomizedMacAddress = persistentRandomizedMac.ifEmpty { null }?.let(MacAddress::fromString)
                allowedAcsChannels = mapOf(
                    SoftApConfiguration.BAND_2GHZ to RangeInput.fromString(acs2g),
                    SoftApConfiguration.BAND_5GHZ to RangeInput.fromString(acs5g),
                    SoftApConfiguration.BAND_6GHZ to RangeInput.fromString(acs6g),
                )
                maxChannelBandwidth = this@ApConfigurationState.maxChannelBandwidth
                if (Build.VERSION.SDK_INT >= 36) isClientIsolationEnabled = clientIsolation
            }
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
                if (newUnderlying != null) base.underlying?.let { check(it.javaClass == newUnderlying.javaClass) }
                else config.underlying = base.underlying
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

    private fun load(config: SoftApConfigurationCompat) {
        hexSsid = config.ssid?.decode() == null
        ssid = config.ssid?.let { if (hexSsid) it.hex else it.decode() ?: it.hex }.orEmpty()
        securityType = config.securityType
        password = config.passphrase.orEmpty()
        autoShutdown = config.isAutoShutdownEnabled
        timeout = config.shutdownTimeoutMillis.let { if (it <= 0) "" else it.toString() }
        bridgedModeOpportunisticShutdown = config.isBridgedModeOpportunisticShutdownEnabled
        bridgedTimeout = config.bridgedModeOpportunisticShutdownTimeoutMillis.let { if (it == -1L) "" else it.toString() }
        primaryChannel = locate(config.channels, 0, channelOptions)
        secondaryChannel = if (config.channels.size() > 1) locate(config.channels, 1, channelOptions)
        else ChannelOption.Disabled
        bandOptimization = config.isBandOptimizationEnabled ?: true
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
        vendorElements = if (Build.VERSION.SDK_INT >= 33) VendorElements.serialize(config.vendorElements) else ""
        clientIsolation = if (Build.VERSION.SDK_INT >= 36) config.isClientIsolationEnabled else false
        userConfig = config.isUserConfiguration
        acs2g = RangeInput.toString(config.allowedAcsChannels[SoftApConfiguration.BAND_2GHZ]).orEmpty()
        acs5g = RangeInput.toString(config.allowedAcsChannels[SoftApConfiguration.BAND_5GHZ]).orEmpty()
        acs6g = RangeInput.toString(config.allowedAcsChannels[SoftApConfiguration.BAND_6GHZ]).orEmpty()
        maxChannelBandwidth = config.maxChannelBandwidth
        revision++
    }

    fun securityEntries() = securityOptions
    fun channelEntries(allowDisabled: Boolean = false) = if (allowDisabled) listOf(ChannelOption.Disabled) +
            channelOptions else channelOptions
    fun bandwidthEntries() = bandwidthOptions
}

internal class ApConfigurationSession(
    val initial: SoftApConfigurationCompat,
    val readOnly: Boolean = false,
    val p2pMode: Boolean = false,
    val onApply: suspend (SoftApConfigurationCompat) -> Boolean,
)

@Composable
internal fun ApConfigurationScreen(state: ApConfigurationState) {
    SettingsList {
        item { SsidApRow(state) }
        item {
            ListApRow(
                title = R.string.wifi_security,
                selected = state.securityLabel,
                enabled = !state.readOnly && (!state.p2pMode || Build.VERSION.SDK_INT >= 36),
                entries = state.securityEntries(),
                entryLabel = { it.label },
                onSelect = { state.securityType = it.value },
            )
        }
        if (state.securityType !in SECURITY_TYPES_WITHOUT_PASSWORD) {
            item { PasswordApRow(state) }
        }
        item { SwitchApRow(R.string.wifi_hotspot_auto_off, state.autoShutdown, state.readOnly) { state.autoShutdown = it } }
        item { TextApRow(R.string.wifi_hotspot_timeout, state.timeout, state.readOnly) { state.timeout = it } }
        if (!state.p2pMode && Build.VERSION.SDK_INT >= 31) {
            item {
                SwitchApRow(
                    R.string.wifi_bridged_mode_opportunistic_shutdown,
                    state.bridgedModeOpportunisticShutdown,
                    state.readOnly,
                ) { state.bridgedModeOpportunisticShutdown = it }
            }
            item {
                TextApRow(
                    R.string.wifi_hotspot_timeout_bridged,
                    state.bridgedTimeout,
                    state.readOnly,
                ) { state.bridgedTimeout = it }
            }
        }
        item { SectionHeader(stringResource(R.string.wifi_hotspot_ap_band_title)) }
        if (!state.p2pMode && Build.VERSION.SDK_INT >= 30 && SoftApConfigurationCompat.isBandOptimizationSupported) {
            item { SwitchApRow(R.string.wifi_band_optimization, state.bandOptimization, state.readOnly) {
                state.bandOptimization = it
            } }
        }
        item {
            ListApRow(
                title = R.string.wifi_hotspot_ap_band_title,
                selected = state.primaryChannelLabel,
                enabled = !state.readOnly,
                entries = state.channelEntries(),
                entryLabel = { it.toString() },
                onSelect = { state.primaryChannel = it },
            )
        }
        if (!state.p2pMode && Build.VERSION.SDK_INT >= 31) item {
            ListApRow(
                title = R.string.wifi_hotspot_ap_band_title,
                selected = state.secondaryChannelLabel,
                enabled = !state.readOnly,
                entries = state.channelEntries(allowDisabled = true),
                entryLabel = { it.toString() },
                onSelect = { state.secondaryChannel = it },
            )
        }
        if (!state.p2pMode && Build.VERSION.SDK_INT >= 33) {
            item { TextApRow(R.string.wifi_hotspot_acs_channel_2g, state.acs2g, state.readOnly) { state.acs2g = it } }
            item { TextApRow(R.string.wifi_hotspot_acs_channel_5g, state.acs5g, state.readOnly) { state.acs5g = it } }
            item { TextApRow(R.string.wifi_hotspot_acs_channel_6g, state.acs6g, state.readOnly) { state.acs6g = it } }
            item {
                ListApRow(
                    title = R.string.wifi_hotspot_max_channel_bandwidth,
                    selected = state.maxChannelBandwidthLabel,
                    enabled = !state.readOnly,
                    entries = state.bandwidthEntries(),
                    entryLabel = { it.name },
                    onSelect = { state.maxChannelBandwidth = it.width },
                )
            }
        }
        if (!state.p2pMode && Build.VERSION.SDK_INT >= 30) {
            item { SectionHeader(stringResource(R.string.wifi_hotspot_access_control_title)) }
            item { TextApRow(R.string.wifi_max_clients, state.maxClients, state.readOnly) { state.maxClients = it } }
            item { TextApRow(R.string.wifi_blocked_list, state.blockedList, state.readOnly) { state.blockedList = it } }
            item { SwitchApRow(R.string.wifi_client_user_control, state.clientUserControl, state.readOnly) {
                state.clientUserControl = it
            } }
            item {
                TextApRow(
                    R.string.wifi_allowed_list,
                    state.allowedList,
                    state.readOnly || !state.clientUserControl,
                ) { state.allowedList = it }
            }
        }
        item { SectionHeader(stringResource(R.string.wifi_hotspot_ap_advanced_title)) }
        if (!state.p2pMode && Build.VERSION.SDK_INT >= 31) item {
            ListApRow(
                title = R.string.wifi_mac_randomization,
                selected = state.macRandomizationLabel,
                enabled = !state.readOnly,
                entries = app.resources.getStringArray(R.array.wifi_mac_randomization).mapIndexed { index, label ->
                    index to label
                },
                entryLabel = { it.second },
                onSelect = { state.macRandomization = it.first },
            )
        }
        if (state.p2pMode || Build.VERSION.SDK_INT < 31 ||
            state.macRandomization == SoftApConfigurationCompat.RANDOMIZATION_NONE) {
            item { TextApRow(R.string.wifi_advanced_mac_address_title, state.bssid, state.readOnly) { state.bssid = it } }
        }
        if (!state.p2pMode && Build.VERSION.SDK_INT >= 33) {
            item {
                TextApRow(
                    R.string.wifi_advanced_mac_address_persistent_randomized,
                    state.persistentRandomizedMac,
                    state.readOnly,
                ) { state.persistentRandomizedMac = it }
            }
        }
        if (!state.p2pMode) item { SwitchApRow(R.string.wifi_hidden_network, state.hiddenSsid, state.readOnly) {
            state.hiddenSsid = it
        } }
        if (!state.p2pMode && Build.VERSION.SDK_INT >= 31) {
            item { SwitchApRow(R.string.wifi_ieee_80211ax, state.ieee80211ax, state.readOnly) {
                state.ieee80211ax = it
            } }
            if (Build.VERSION.SDK_INT >= 33) item { SwitchApRow(R.string.wifi_ieee_80211be, state.ieee80211be,
                state.readOnly) { state.ieee80211be = it } }
        }
        if (Build.VERSION.SDK_INT >= 33) {
            item { TextApRow(R.string.wifi_vendor_elements, state.vendorElements, state.readOnly) {
                state.vendorElements = it
            } }
        }
        if (!state.p2pMode && Build.VERSION.SDK_INT >= 36) {
            item { SwitchApRow(R.string.wifi_client_isolation, state.clientIsolation, state.readOnly) {
                state.clientIsolation = it
            } }
        }
        if (!state.p2pMode && Build.VERSION.SDK_INT >= 31) {
            item { SwitchApRow(R.string.wifi_user_config, state.userConfig, true) { state.userConfig = it } }
        }
    }
}

@Composable
internal fun ApConfigurationTopBarActions(
    state: ApConfigurationState,
    session: ApConfigurationSession,
    snackbarHostState: SnackbarHostState,
    onApplied: () -> Unit,
) {
    androidx.compose.runtime.rememberCoroutineScope().let { scope ->
        var qrCode by androidx.compose.runtime.remember { mutableStateOf<String?>(null) }
        var overflowExpanded by androidx.compose.runtime.remember { mutableStateOf(false) }
        if (state.possiblyInvalid) Icon(
            painter = painterResource(R.drawable.ic_alert_warning),
            contentDescription = stringResource(R.string.configuration_invalid),
            tint = MaterialTheme.colorScheme.error,
            modifier = Modifier.size(24.dp),
        )
        if (!state.readOnly) IconButton(
            enabled = state.canSave,
            onClick = {
                scope.launch {
                    try {
                        if (session.onApply(state.generateConfig())) onApplied()
                    } catch (e: CancellationException) {
                        throw e
                    } catch (e: Exception) {
                        Timber.w(e)
                        snackbarHostState.showSnackbar(e.readableMessage)
                    }
                }
            },
        ) {
            Icon(
                painter = painterResource(R.drawable.ic_content_save),
                contentDescription = stringResource(R.string.wifi_save),
            )
        }
        IconButton(
            enabled = state.canCopy,
            onClick = {
                try {
                    state.copyToClipboard()
                } catch (e: RuntimeException) {
                    scope.launch { snackbarHostState.showSnackbar(e.readableMessage) }
                }
            },
        ) {
            Icon(
                painter = painterResource(R.drawable.ic_content_file_copy),
                contentDescription = stringResource(android.R.string.copy),
            )
        }
        IconButton(onClick = { overflowExpanded = true }) {
            Icon(
                painter = painterResource(R.drawable.ic_more_vert),
                contentDescription = stringResource(R.string.configuration_view),
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
                        scope.launch { snackbarHostState.showSnackbar(e.readableMessage) }
                    }
                },
            )
            DropdownMenuItem(
                text = { Text(stringResource(R.string.configuration_share)) },
                enabled = state.canCopy,
                leadingIcon = {
                    Icon(
                        painter = painterResource(R.drawable.ic_settings_qrcode),
                        contentDescription = null,
                    )
                },
                onClick = {
                    overflowExpanded = false
                    try {
                        qrCode = state.generateConfig(requirePassword = false).toQrCode()
                    } catch (e: RuntimeException) {
                        scope.launch { snackbarHostState.showSnackbar(e.readableMessage) }
                    }
                },
            )
        }
        qrCode?.let { value ->
            QrCodeDialog(value) { qrCode = null }
        }
    }
}

@Composable
private fun QrCodeDialog(value: String, onDismiss: () -> Unit) {
    val size = dimensionResource(R.dimen.qrcode_size)
    val density = LocalDensity.current
    val bitmap = androidx.compose.runtime.remember(value, size, density) {
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
            }
        } catch (e: Exception) {
            Timber.w(e)
            null
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
        } catch (eCancel: CancellationException) {
            throw eCancel
        } catch (eRoot: Exception) {
            eRoot.addSuppressed(e)
            if (Build.VERSION.SDK_INT >= 30 || eRoot.getRootCause() !is SecurityException) Timber.w(eRoot)
            snackbarHostState.showSnackbar(eRoot.readableMessage)
            null
        }
    } catch (e: IllegalArgumentException) {
        Timber.w(e)
        snackbarHostState.showSnackbar(e.readableMessage)
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
            snackbarHostState.showSnackbar(e.readableMessage)
        }
        val wc = configuration.toWifiConfiguration()
        try {
            if (WifiApManager.setConfiguration(wc)) return true
        } catch (e: InvocationTargetException) {
            try {
                if (RootManager.use { it.execute(WifiApCommands.SetConfigurationLegacy(wc)) }.value) return true
            } catch (eCancel: CancellationException) {
                throw eCancel
            } catch (eRoot: Exception) {
                eRoot.addSuppressed(e)
                Timber.w(eRoot)
                snackbarHostState.showSnackbar(eRoot.readableMessage)
                return false
            }
        }
    } else {
        val platform = try {
            configuration.toPlatform()
        } catch (e: InvocationTargetException) {
            Timber.w(e)
            snackbarHostState.showSnackbar(e.readableMessage)
            return false
        }
        try {
            if (WifiApManager.setConfiguration(platform)) return true
        } catch (e: InvocationTargetException) {
            try {
                if (RootManager.use { it.execute(WifiApCommands.SetConfiguration(platform)) }.value) return true
            } catch (eCancel: CancellationException) {
                throw eCancel
            } catch (eRoot: Exception) {
                eRoot.addSuppressed(e)
                Timber.w(eRoot)
                snackbarHostState.showSnackbar(eRoot.readableMessage)
                return false
            }
        }
    }
    snackbarHostState.showSnackbar(app.getString(R.string.configuration_rejected))
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
            return ApConfigurationSession(config, p2pMode = true) {
                applySafeModeRepeaterConfiguration(it)
                true
            }
        }
    } else if (binder != null) {
        val group = binder.group.value ?: binder.fetchPersistentGroup().let { binder.group.value }
        if (group != null) {
            var supplicantConfig: P2pSupplicantConfiguration? = null
            var readOnly = false
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
                    val master = P2pSupplicantConfiguration(group)
                    master.init(binder.obtainDeviceAddress()?.toString())
                    supplicantConfig = master
                    passphrase = master.psk
                    bssid = master.bssid
                } catch (e: Exception) {
                    if (e is P2pSupplicantConfiguration.LoggedException) Timber.d(e)
                    else if (e !is CancellationException) Timber.w(e)
                    else throw e
                    readOnly = true
                    passphrase = group.passphrase
                    try {
                        bssid = group.owner?.deviceAddress?.let(MacAddress::fromString)
                    } catch (_: IllegalArgumentException) { }
                }
            }
            return ApConfigurationSession(config, readOnly = readOnly, p2pMode = true) {
                supplicantConfig?.let { master ->
                    applySupplicantRepeaterConfiguration(master, binder, it, snackbarHostState)
                }
                applyRepeaterCommonConfiguration(it)
                true
            }
        }
    }
    snackbarHostState.showSnackbar(app.getString(R.string.repeater_configure_failure))
    return null
}

private fun applySafeModeRepeaterConfiguration(config: SoftApConfigurationCompat) {
    RepeaterService.networkName = config.ssid
    RepeaterService.deviceAddress = config.bssid
    RepeaterService.passphrase = config.passphrase
    RepeaterService.securityType = config.securityType
    applyRepeaterCommonConfiguration(config)
}

private suspend fun applySupplicantRepeaterConfiguration(
    master: P2pSupplicantConfiguration,
    binder: RepeaterService.Binder,
    config: SoftApConfigurationCompat,
    snackbarHostState: SnackbarHostState,
) {
    val mayBeModified = master.psk != config.passphrase || master.bssid != config.bssid || config.ssid.run {
        if (this != null) decode().let { it == null || binder.group.value?.networkName != it }
        else binder.group.value?.networkName != null
    }
    if (!mayBeModified) return
    try {
        withContext(Dispatchers.Default) { master.update(config.ssid!!, config.passphrase!!, config.bssid) }
        binder.clearGroup()
    } catch (e: CancellationException) {
        throw e
    } catch (e: Exception) {
        Timber.w(e)
        snackbarHostState.showSnackbar(e.readableMessage)
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
    var editing by androidx.compose.runtime.remember(state.ssid) { mutableStateOf(false) }
    var draft by androidx.compose.runtime.remember(state.ssid, editing) { mutableStateOf(state.ssid) }
    var draftHex by androidx.compose.runtime.remember(state.ssid, editing) { mutableStateOf(state.ssidHex) }
    var error by androidx.compose.runtime.remember(editing) { mutableStateOf<String?>(null) }
    PreferenceRow(
        title = stringResource(R.string.wifi_ssid),
        summary = state.ssid,
        enabled = !state.readOnly,
        onClick = { editing = true },
    )
    if (editing) AlertDialog(
        onDismissRequest = { editing = false },
        title = { Text(stringResource(R.string.wifi_ssid)) },
        text = {
            androidx.compose.foundation.layout.Column {
                OutlinedTextField(
                    value = draft,
                    onValueChange = {
                        draft = it
                        error = null
                    },
                    modifier = Modifier.fillMaxWidth(),
                    singleLine = true,
                    isError = error != null,
                )
                error?.let { Text(it, color = MaterialTheme.colorScheme.error) }
            }
        },
        confirmButton = {
            TextButton(onClick = {
                state.setSsid(draft, draftHex)
                editing = false
            }) {
                Text(stringResource(android.R.string.ok))
            }
        },
        dismissButton = {
            TextButton(onClick = {
                try {
                    draft = state.convertSsidDisplay(draft, draftHex)
                    draftHex = !draftHex
                    error = null
                } catch (e: RuntimeException) {
                    error = e.readableMessage
                }
            }) {
                Text(stringResource(R.string.wifi_ssid_toggle_hex))
            }
            TextButton(onClick = { editing = false }) {
                Text(stringResource(android.R.string.cancel))
            }
        },
    )
}

@Composable
private fun TextApRow(
    @StringRes title: Int,
    value: String,
    readOnly: Boolean,
    onValueChange: (String) -> Unit,
) {
    var editing by androidx.compose.runtime.remember(value) { mutableStateOf(false) }
    var draft by androidx.compose.runtime.remember(value, editing) { mutableStateOf(value) }
    PreferenceRow(
        title = stringResource(title),
        summary = value,
        enabled = !readOnly,
        onClick = { editing = true },
    )
    if (editing) AlertDialog(
        onDismissRequest = { editing = false },
        title = { Text(stringResource(title)) },
        text = {
            OutlinedTextField(
                value = draft,
                onValueChange = { draft = it },
                modifier = Modifier.fillMaxWidth(),
                minLines = if (value.contains('\n')) 3 else 1,
            )
        },
        confirmButton = {
            TextButton(onClick = {
                onValueChange(draft)
                editing = false
            }) {
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
    val maxLength = state.securityType != SoftApConfiguration.SECURITY_TYPE_WPA3_SAE
    var editing by androidx.compose.runtime.remember(state.password) { mutableStateOf(false) }
    var draft by androidx.compose.runtime.remember(state.password, editing) { mutableStateOf(state.password) }
    PreferenceRow(
        title = stringResource(R.string.wifi_password),
        summary = if (state.password.isEmpty()) "" else "\u2022".repeat(8),
        enabled = !state.readOnly,
        onClick = { editing = true },
    )
    if (editing) AlertDialog(
        onDismissRequest = { editing = false },
        title = { Text(stringResource(R.string.wifi_password)) },
        text = {
            OutlinedTextField(
                value = draft,
                onValueChange = { draft = if (maxLength) it.take(63) else it },
                modifier = Modifier.fillMaxWidth(),
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Password),
                singleLine = true,
                supportingText = if (maxLength) {
                    { Text("${draft.length}/63") }
                } else null,
                textStyle = LocalTextStyle.current.copy(fontFamily = FontFamily.Monospace),
                visualTransformation = PasswordVisualTransformation(),
            )
        },
        confirmButton = {
            TextButton(onClick = {
                state.password = draft
                editing = false
            }) {
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
private fun SwitchApRow(
    @StringRes title: Int,
    checked: Boolean,
    readOnly: Boolean,
    onCheckedChange: (Boolean) -> Unit,
) {
    PreferenceRow(
        title = stringResource(title),
        enabled = !readOnly,
        trailing = {
            Switch(
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
    @StringRes title: Int,
    selected: String,
    enabled: Boolean,
    entries: List<T>,
    entryLabel: (T) -> String,
    onSelect: (T) -> Unit,
) {
    var selecting by androidx.compose.runtime.remember { mutableStateOf(false) }
    PreferenceRow(
        title = stringResource(title),
        summary = selected,
        enabled = enabled,
        onClick = { selecting = true },
    )
    if (selecting) AlertDialog(
        onDismissRequest = { selecting = false },
        title = { Text(stringResource(title)) },
        text = {
            LazyColumn {
                itemsIndexed(entries) { _, entry ->
                    ListItem(
                        headlineContent = { Text(entryLabel(entry)) },
                        modifier = Modifier
                            .fillMaxWidth()
                            .clickable {
                                onSelect(entry)
                                selecting = false
                            },
                    )
                }
            }
        },
        confirmButton = {
            TextButton(onClick = { selecting = false }) {
                Text(stringResource(android.R.string.cancel))
            }
        },
    )
}

private fun parseMacList(value: String) = value.split(NON_MAC_CHARS).filter { it.isNotEmpty() }.map(MacAddress::fromString)

private fun currentChannelOptions(p2pMode: Boolean): List<ChannelOption> = when {
    !p2pMode -> SOFT_AP_OPTIONS
    RepeaterService.safeMode -> P2P_SAFE_OPTIONS
    else -> P2P_UNSAFE_OPTIONS
}

private fun locate(channels: SparseIntArray, index: Int, options: List<ChannelOption>): ChannelOption {
    val band = channels.keyAt(index)
    val channel = channels.valueAt(index)
    return options.firstOrNull { it.band == band && it.channel == channel } ?: options.first()
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
