package be.mygod.vpnhotspot.ui.apconfiguration

import android.net.wifi.SoftApConfiguration
import android.os.Build
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.asPaddingValues
import androidx.compose.foundation.layout.navigationBars
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalInspectionMode
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.net.monitor.TetherTimeoutMonitor
import be.mygod.vpnhotspot.net.wifi.SoftApConfigurationCompat
import be.mygod.vpnhotspot.net.wifi.VendorElements
import be.mygod.vpnhotspot.ui.PreferenceGroup
import be.mygod.vpnhotspot.ui.SettingsList
import be.mygod.vpnhotspot.ui.annotatedStringResource
import be.mygod.vpnhotspot.util.readableMessage

@Composable
internal fun ApConfigurationScreen(state: ApConfigurationState) {
    val context = LocalContext.current
    val inspectionMode = LocalInspectionMode.current
    val defaultTimeout = if (inspectionMode) 600_000L else TetherTimeoutMonitor.defaultTimeout.toLong()
    val defaultBridgedTimeout = if (inspectionMode || Build.VERSION.SDK_INT < 31) {
        600_000L
    } else TetherTimeoutMonitor.defaultTimeoutBridged.toLong()
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
                state.actionError(context)?.let {
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
                if (state.p2pMode || Build.VERSION.SDK_INT >= 30) {
                    row {
                        TextSwitchApRow(
                            icon = R.drawable.ic_action_timer,
                            title = R.string.wifi_hotspot_auto_off,
                            valueTitle = R.string.wifi_hotspot_timeout,
                            checked = state.autoShutdown,
                            value = state.timeout,
                            valueSummary = "",
                            switchReadOnly = false,
                            valueReadOnly = false,
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
                            suffix = "ms",
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
                    row {
                        SwitchApRow(
                            icon = R.drawable.ic_action_timer,
                            title = R.string.wifi_hotspot_auto_off,
                            checked = state.autoShutdown,
                            readOnly = false,
                            summary = annotatedStringResource(
                                R.string.wifi_hotspot_auto_off_help,
                                timeoutSummary(context, state.timeout, defaultTimeout),
                            ),
                        ) {
                            state.autoShutdown = it
                        }
                    }
                }
                if (!state.p2pMode && Build.VERSION.SDK_INT >= 31) {
                    row {
                        TextSwitchApRow(
                            icon = R.drawable.ic_action_timer,
                            title = R.string.wifi_bridged_mode_opportunistic_shutdown,
                            valueTitle = R.string.wifi_hotspot_timeout_bridged,
                            checked = state.bridgedModeOpportunisticShutdown,
                            value = state.bridgedTimeout,
                            valueSummary = "",
                            switchReadOnly = false,
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
                            suffix = "ms",
                            validator = { value ->
                                validateOptionalLong(value, SoftApConfigurationCompat::testPlatformBridgedTimeoutValidity)
                            },
                            onCheckedChange = { state.bridgedModeOpportunisticShutdown = it },
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
                    ChannelApRow(
                        icon = R.drawable.ic_action_settings_input_antenna,
                        title = R.string.wifi_hotspot_ap_channel_band_title,
                        selected = state.primaryChannel,
                        entries = state.channelEntries(),
                        description = annotatedStringResource(R.string.wifi_hotspot_ap_channel_band_help),
                        onSelect = { state.primaryChannel = it },
                    )
                }
                if (!state.p2pMode && Build.VERSION.SDK_INT >= 31) row {
                    ChannelApRow(
                        icon = R.drawable.ic_action_settings_input_antenna,
                        title = R.string.wifi_hotspot_concurrent_ap_channel_band_title,
                        selected = state.secondaryChannel,
                        entries = state.channelEntries(allowDisabled = true),
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
                            selected = state.maxChannelBandwidthLabel(context),
                            enabled = true,
                            entries = state.bandwidthEntries(),
                            entryLabel = { it.label(context) },
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
                        TextSwitchApRow(
                            icon = R.drawable.ic_social_people,
                            title = R.string.wifi_client_user_control,
                            valueTitle = R.string.wifi_allowed_list,
                            checked = state.clientUserControl,
                            value = state.allowedList,
                            valueSummary = state.allowedList,
                            switchReadOnly = false,
                            valueReadOnly = false,
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
                                        "A MAC address exists in both client lists"
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
        }
        item {
            PreferenceGroup(title = stringResource(R.string.wifi_hotspot_ap_advanced_title)) {
                row { MacAddressApRow(state) }
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
