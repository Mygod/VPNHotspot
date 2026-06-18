package be.mygod.vpnhotspot.ui.apconfiguration

import android.content.Context
import android.net.wifi.SoftApConfiguration
import android.net.wifi.p2p.WifiP2pManager
import android.os.Build
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.asPaddingValues
import androidx.compose.foundation.layout.navigationBars
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalInspectionMode
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.withStyle
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.net.monitor.TetherTimeoutMonitor
import be.mygod.vpnhotspot.net.wifi.SoftApConfigurationCompat
import be.mygod.vpnhotspot.net.wifi.VendorData
import be.mygod.vpnhotspot.net.wifi.VendorElements
import be.mygod.vpnhotspot.net.wifi.WifiApManager
import be.mygod.vpnhotspot.root.RepeaterCommands
import be.mygod.vpnhotspot.root.RootManager
import be.mygod.vpnhotspot.ui.SettingsList
import be.mygod.vpnhotspot.ui.annotatedStringResource
import be.mygod.vpnhotspot.ui.channelBandwidthLabel
import be.mygod.vpnhotspot.ui.preferenceGroup
import be.mygod.vpnhotspot.ui.showLongSnackbar
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.readableMessage
import kotlinx.coroutines.launch
import timber.log.Timber

@Composable
fun ApConfigurationScreen(
    state: ApConfigurationState,
    snackbarHostState: SnackbarHostState,
    floatingActionButtonPadding: Dp,
) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    var qrCode by rememberSaveable { mutableStateOf<String?>(null) }
    val inspectionMode = LocalInspectionMode.current
    val defaultTimeout = if (inspectionMode) 600_000L else TetherTimeoutMonitor.defaultTimeout.toLong()
    val defaultBridgedTimeout = if (inspectionMode || Build.VERSION.SDK_INT < 31) {
        600_000L
    } else TetherTimeoutMonitor.defaultTimeoutBridged.toLong()
    val softApRuntimeInfo by if (!state.p2pMode && Build.VERSION.SDK_INT >= 30 && !inspectionMode) {
        rememberSoftApRuntimeInfo(state.target)
    } else {
        remember { mutableStateOf(null) }
    }
    val navigationBarPadding = WindowInsets.navigationBars.asPaddingValues()
    if (state.p2pMode && !inspectionMode) LaunchedEffect(Unit) {
        state.supplicantCapability = try {
            RootManager.use { it.execute(RepeaterCommands.QuerySupplicantCapability()) }
        } catch (e: Exception) {
            Timber.d(e)
            null
        }
    }
    SettingsList(
        contentPadding = PaddingValues(
            top = 8.dp,
            bottom = 8.dp + floatingActionButtonPadding + navigationBarPadding.calculateBottomPadding(),
        ),
    ) {
        preferenceGroup(key = "ap_basic") {
            if (state.p2pMode) row(R.string.repeater_configuration_method) {
                val frameworkLabel = stringResource(R.string.repeater_configuration_method_framework)
                val supplicantLabel = stringResource(R.string.repeater_configuration_method_supplicant)
                val frameworkSummary = annotatedStringResource(R.string.repeater_configuration_method_framework_summary)
                val supplicantSummary = annotatedStringResource(
                    R.string.repeater_configuration_method_supplicant_summary,
                    state.supplicantCapability?.label.orEmpty(),
                )
                ListApRow(
                    icon = R.drawable.ic_health_and_safety,
                    title = R.string.repeater_configuration_method,
                    selected = state.useFramework,
                    entries = listOf(true, false),
                    entryLabel = { if (it) frameworkLabel else supplicantLabel },
                    selectedLabel = if (state.useFramework) frameworkLabel else supplicantLabel,
                    entrySummary = { if (it) frameworkSummary else supplicantSummary },
                    onSelect = { state.useFramework = it },
                )
            }
            row(R.string.wifi_ssid) {
                SsidApRow(
                    state = state,
                    onShowQrCode = {
                        try {
                            qrCode = state.generateConfig(requirePassword = false, full = false).toQrCode()
                        } catch (e: RuntimeException) {
                            scope.launch { snackbarHostState.showLongSnackbar(e.readableMessage) }
                        }
                    },
                )
            }
            (state.copyError(context) ?: state.saveError(context))?.let {
                contentItem("action_error") { ErrorApText(it, Modifier.padding(horizontal = 24.dp, vertical = 4.dp)) }
            }
            if (!state.p2pMode || Build.VERSION.SDK_INT >= 36) row(R.string.wifi_security) {
                val entries = state.securityEntries
                val selected = entries.firstOrNull { it.value == state.securityType }
                ListApRow(
                    icon = R.drawable.ic_encrypted,
                    title = R.string.wifi_security,
                    selected = selected,
                    selectedLabel = selected?.label(context) ?: state.securityType.toString(),
                    entries = entries,
                    entryLabel = { it.label(context) },
                    onSelect = { state.securityType = it.value },
                )
            }
            if (state.passwordEnabled) row("password") { PasswordApRow(state) }
            if (state.target != ApConfigurationTarget.Temporary || Build.VERSION.SDK_INT >= 30) {
                if (state.p2pMode || Build.VERSION.SDK_INT >= 30) {
                    row(R.string.wifi_hotspot_auto_off) {
                        TextSwitchApRow(
                            icon = R.drawable.ic_timer,
                            title = R.string.wifi_hotspot_auto_off,
                            valueTitle = R.string.wifi_hotspot_timeout,
                            checked = state.autoShutdown,
                            value = state.timeout,
                            summary = annotatedStringResource(
                                R.string.wifi_hotspot_auto_off_help,
                                timeoutSummary(context, state.timeout, defaultTimeout),
                            ),
                            description = annotatedStringResource(
                                R.string.wifi_hotspot_timeout_help,
                                formatTimeoutMillis(context, defaultTimeout),
                            ),
                            keyboardType = KeyboardType.Number,
                            maxLength = 19,
                            placeholder = defaultTimeout.toString(),
                            suffix = stringResource(R.string.wifi_hotspot_timeout_milliseconds),
                            validator = { value ->
                                validateOptionalLong(value) { timeout ->
                                    if (!state.p2pMode && Build.VERSION.SDK_INT >= 30) {
                                        SoftApConfigurationCompat.testPlatformTimeoutValidity(timeout)
                                    }
                                }
                            },
                            onCheckedChange = { state.autoShutdown = it },
                        ) { state.timeout = it }
                    }
                } else {
                    row(R.string.wifi_hotspot_auto_off) {
                        SwitchApRow(
                            icon = R.drawable.ic_timer,
                            title = R.string.wifi_hotspot_auto_off,
                            checked = state.autoShutdown,
                            summary = annotatedStringResource(
                                R.string.wifi_hotspot_auto_off_help,
                                timeoutSummary(context, state.timeout, defaultTimeout),
                            ),
                        ) {
                            state.autoShutdown = it
                        }
                    }
                }
            }
        }
        preferenceGroup(title = R.string.wifi_hotspot_ap_band_title) {
            if (!state.p2pMode && Build.VERSION.SDK_INT >= 30 &&
                SoftApConfigurationCompat.isBandOptimizationSupported) {
                row(R.string.wifi_band_optimization) {
                    SwitchApRow(
                        icon = R.drawable.ic_tune,
                        title = R.string.wifi_band_optimization,
                        checked = state.bandOptimization,
                        summary = annotatedStringResource(R.string.wifi_band_optimization_help),
                    ) {
                        state.bandOptimization = it
                    }
                }
            }
            row(R.string.wifi_hotspot_ap_channel_band_title) {
                ChannelApRow(
                    icon = R.drawable.ic_settings_input_antenna,
                    title = R.string.wifi_hotspot_ap_channel_band_title,
                    selected = state.primaryChannel,
                    entries = state.channelEntries(),
                    description = annotatedStringResource(R.string.wifi_hotspot_ap_channel_band_help),
                    onSelect = { state.primaryChannel = it },
                )
            }
            if (!state.p2pMode && Build.VERSION.SDK_INT >= 31) row(
                R.string.wifi_hotspot_concurrent_ap_channel_band_title,
            ) {
                ChannelApRow(
                    icon = R.drawable.ic_wifi_channel,
                    title = R.string.wifi_hotspot_concurrent_ap_channel_band_title,
                    selected = state.secondaryChannel,
                    entries = state.channelEntries(allowDisabled = true),
                    description = annotatedStringResource(R.string.wifi_hotspot_concurrent_ap_channel_band_help),
                    onSelect = { state.secondaryChannel = it },
                )
            }
            state.channelError?.let {
                contentItem("channel_error") { ErrorApText(it, Modifier.padding(horizontal = 24.dp, vertical = 4.dp)) }
            }
            if (!state.p2pMode && Build.VERSION.SDK_INT >= 31) {
                row(R.string.wifi_bridged_mode_opportunistic_shutdown) {
                    TextSwitchApRow(
                        icon = R.drawable.ic_auto_timer,
                        title = R.string.wifi_bridged_mode_opportunistic_shutdown,
                        valueTitle = R.string.wifi_hotspot_timeout_bridged,
                        checked = state.bridgedModeOpportunisticShutdown,
                        value = state.bridgedTimeout,
                        valueReadOnly = Build.VERSION.SDK_INT < 33,
                        summary = annotatedStringResource(
                            R.string.wifi_bridged_mode_opportunistic_shutdown_help,
                            timeoutSummary(context, state.bridgedTimeout, defaultBridgedTimeout),
                        ),
                        description = annotatedStringResource(
                            R.string.wifi_hotspot_timeout_bridged_help,
                            formatTimeoutMillis(context, defaultBridgedTimeout),
                        ),
                        keyboardType = KeyboardType.Number,
                        maxLength = 19,
                        placeholder = defaultBridgedTimeout.toString(),
                        suffix = stringResource(R.string.wifi_hotspot_timeout_milliseconds),
                        validator = { value ->
                            validateOptionalLong(value, SoftApConfigurationCompat::testPlatformBridgedTimeoutValidity)
                        },
                        onCheckedChange = { state.bridgedModeOpportunisticShutdown = it },
                    ) { state.bridgedTimeout = it }
                }
            }
            if (!state.p2pMode && Build.VERSION.SDK_INT >= 33) {
                row(R.string.wifi_hotspot_acs_channel_2g) {
                    TextApRow(
                        icon = R.drawable.ic_filter_2,
                        title = R.string.wifi_hotspot_acs_channel_2g,
                        value = state.acs2g,
                        description = annotatedStringResource(R.string.wifi_hotspot_acs_channel_help),
                        keyboardOptions = MACHINE_TEXT_KEYBOARD_OPTIONS,
                        validator = { validateAcsChannels(SoftApConfiguration.BAND_2GHZ, it) },
                    ) { state.acs2g = it }
                }
                row(R.string.wifi_hotspot_acs_channel_5g) {
                    TextApRow(
                        icon = R.drawable.ic_filter_5,
                        title = R.string.wifi_hotspot_acs_channel_5g,
                        value = state.acs5g,
                        description = annotatedStringResource(R.string.wifi_hotspot_acs_channel_help),
                        keyboardOptions = MACHINE_TEXT_KEYBOARD_OPTIONS,
                        validator = { validateAcsChannels(SoftApConfiguration.BAND_5GHZ, it) },
                    ) { state.acs5g = it }
                }
                row(R.string.wifi_hotspot_acs_channel_6g) {
                    TextApRow(
                        icon = R.drawable.ic_filter_6,
                        title = R.string.wifi_hotspot_acs_channel_6g,
                        value = state.acs6g,
                        description = annotatedStringResource(R.string.wifi_hotspot_acs_channel_help),
                        keyboardOptions = MACHINE_TEXT_KEYBOARD_OPTIONS,
                        validator = { validateAcsChannels(SoftApConfiguration.BAND_6GHZ, it) },
                    ) { state.acs6g = it }
                }
                row(R.string.wifi_hotspot_max_channel_bandwidth) {
                    val entries = state.bandwidthEntries
                    val selected = entries.firstOrNull { it.width == state.maxChannelBandwidth }
                    ListApRow(
                        icon = R.drawable.ic_speed,
                        title = R.string.wifi_hotspot_max_channel_bandwidth,
                        selected = selected,
                        selectedLabel = selected?.label(context)
                            ?: channelBandwidthLabel(context, state.maxChannelBandwidth),
                        entries = entries,
                        entryLabel = { it.label(context) },
                        description = annotatedStringResource(R.string.wifi_hotspot_max_channel_bandwidth_help),
                        onSelect = { state.maxChannelBandwidth = it.width },
                    )
                }
                state.maxChannelBandwidthError?.let {
                    contentItem("max_channel_bandwidth_error") {
                        ErrorApText(it, Modifier.padding(horizontal = 24.dp, vertical = 4.dp))
                    }
                }
            }
            if (!state.p2pMode && Build.VERSION.SDK_INT >= 31) {
                softApRuntimeInfo?.supportedChannels?.let { text ->
                    contentItem("supported_channels") { StaticApInfoText(text) }
                }
            }
        }
        if (!state.p2pMode && Build.VERSION.SDK_INT >= 30) {
            preferenceGroup(title = R.string.wifi_hotspot_access_control_title) {
                row(R.string.wifi_max_clients) {
                    TextApRow(
                        icon = R.drawable.ic_groups,
                        title = R.string.wifi_max_clients,
                        value = state.maxClients,
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
                row(R.string.wifi_blocked_list) {
                    TextApRow(
                        icon = R.drawable.ic_person_cancel,
                        title = R.string.wifi_blocked_list,
                        value = state.blockedList,
                        description = annotatedStringResource(R.string.wifi_blocked_list_help),
                        keyboardOptions = MACHINE_TEXT_KEYBOARD_OPTIONS,
                        minLines = 3,
                        validator = { validateMacList(it) },
                    ) { state.blockedList = it }
                }
                row(R.string.wifi_client_user_control) {
                    val clientListsOverlapError = stringResource(R.string.wifi_client_lists_overlap_error)
                    TextSwitchApRow(
                        icon = R.drawable.ic_supervisor_account,
                        title = R.string.wifi_client_user_control,
                        valueTitle = R.string.wifi_allowed_list,
                        checked = state.clientUserControl,
                        value = state.allowedList,
                        valueSummary = state.allowedList,
                        description = annotatedStringResource(R.string.wifi_client_user_control_help),
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
                                    clientListsOverlapError
                                }
                                null
                            } catch (e: IllegalArgumentException) {
                                e.readableMessage
                            }
                        },
                        onCheckedChange = { state.clientUserControl = it },
                    ) { state.allowedList = it }
                }
            }
        }
        preferenceGroup(title = R.string.wifi_hotspot_ap_advanced_title) {
            if (!state.p2pMode || (!state.useFramework && WifiApManager.p2pMacRandomizationSupported)) {
                row(R.string.wifi_advanced_mac_address_title) { MacAddressApRow(state) }
            }
            if (!state.p2pMode) row(R.string.wifi_hidden_network) {
                SwitchApRow(
                    icon = R.drawable.ic_visibility_off,
                    title = R.string.wifi_hidden_network,
                    checked = state.hiddenSsid,
                    summary = annotatedStringResource(R.string.wifi_hidden_network_help),
                ) {
                    state.hiddenSsid = it
                }
            }
            if (!state.p2pMode && Build.VERSION.SDK_INT >= 31) {
                row(R.string.wifi_ieee_80211ax) {
                    SwitchApRow(
                        icon = R.drawable.ic_counter_6,
                        title = R.string.wifi_ieee_80211ax,
                        checked = state.ieee80211ax,
                        summary = annotatedStringResource(R.string.wifi_ieee_80211ax_help),
                    ) {
                        state.ieee80211ax = it
                    }
                }
                if (Build.VERSION.SDK_INT >= 33) row(R.string.wifi_ieee_80211be) {
                    SwitchApRow(
                        icon = R.drawable.ic_counter_7,
                        title = R.string.wifi_ieee_80211be,
                        checked = state.ieee80211be,
                        summary = annotatedStringResource(R.string.wifi_ieee_80211be_help),
                    ) { state.ieee80211be = it }
                }
            }
            if (Build.VERSION.SDK_INT >= 33) {
                row(R.string.wifi_vendor_elements) {
                    TextApRow(
                        icon = R.drawable.ic_data_object,
                        title = R.string.wifi_vendor_elements,
                        value = state.vendorElements,
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
            if (state.vendorDataEditable) {
                row(R.string.wifi_vendor_data) {
                    val description = annotatedStringResource(R.string.wifi_vendor_data_help).let { help ->
                        buildAnnotatedString {
                            append(help)
                            withStyle(SpanStyle(fontFamily = FontFamily.Monospace)) {
                                append("""
                                    00aabb:<bundle>
                                    <string name="profile">lab</string>
                                    <int name="boost" value="1" />
                                    <boolean name="enabled" value="true" />
                                    </bundle>
                                """.trimIndent())
                            }
                        }
                    }
                    TextApRow(
                        icon = R.drawable.ic_code,
                        title = R.string.wifi_vendor_data,
                        value = state.vendorData,
                        description = description,
                        keyboardOptions = MACHINE_TEXT_KEYBOARD_OPTIONS,
                        minLines = 3,
                        validator = { value ->
                            try {
                                VendorData.deserialize(value, context)
                                null
                            } catch (e: Exception) {
                                e.readableMessage
                            }
                        },
                    ) { state.vendorData = it }
                }
            }
            if (!state.p2pMode && Build.VERSION.SDK_INT >= 36) {
                row(R.string.wifi_client_isolation) {
                    SwitchApRow(
                        icon = R.drawable.ic_safety_divider,
                        title = R.string.wifi_client_isolation,
                        checked = state.clientIsolation,
                        summary = annotatedStringResource(R.string.wifi_client_isolation_help),
                    ) {
                        state.clientIsolation = it
                    }
                }
            }
            if (!state.p2pMode && Build.VERSION.SDK_INT >= 31) {
                row(R.string.wifi_user_config) {
                    SwitchApRow(
                        icon = R.drawable.ic_settings,
                        title = R.string.wifi_user_config,
                        checked = state.userConfig,
                    ) {
                        state.userConfig = it
                    }
                }
            }
            val staticInfo = if (state.p2pMode) {
                repeaterSupportedFeatures(context)
            } else {
                softApRuntimeInfo?.advancedInfo
            }
            staticInfo?.let { contentItem("advanced_info") { StaticApInfoText(it) } }
        }
    }
    qrCode?.let { value ->
        QrCodeDialog(value) { qrCode = null }
    }
}

@Composable
private fun StaticApInfoText(text: String) {
    Text(
        text = text,
        modifier = Modifier.padding(horizontal = 16.dp, vertical = 4.dp),
        color = MaterialTheme.colorScheme.onSurfaceVariant,
        style = MaterialTheme.typography.bodyMedium,
    )
}

private fun repeaterSupportedFeatures(context: Context): String? {
    if (Build.VERSION.SDK_INT < 30) return null
    val p2p = Services.p2p ?: return null
    val labels = mutableListOf<String>()
    fun WifiP2pManager.test(label: String, sdk: Int, action: (WifiP2pManager) -> Boolean) {
        try {
            if (action(this)) labels += label
        } catch (e: NoSuchMethodError) {
            if (Build.VERSION.SDK_INT >= sdk) Timber.w(e)
        }
    }
    p2p.test(context.getString(R.string.repeater_feature_set_vendor_elements), 33) {
        it.isSetVendorElementsSupported
    }
    p2p.test(context.getString(R.string.repeater_feature_group_client_removal), 33) {
        it.isGroupClientRemovalSupported
    }
    p2p.test(context.getString(R.string.repeater_feature_pcc_mode), 36) { it.isPccModeSupported }
    p2p.test(context.getString(R.string.repeater_feature_wifi_direct_r2), 36) { it.isWiFiDirectR2Supported }
    return if (labels.isEmpty()) null else context.getString(R.string.repeater_features) + labels.joinAsBullets()
}

internal fun Iterable<String>.joinAsBullets() = joinToString(separator = "\n• ", prefix = "\n• ")
