package be.mygod.vpnhotspot.ui

import android.content.Context
import android.content.Intent
import android.content.SharedPreferences
import android.content.res.Configuration
import android.net.ConnectivityManager
import android.net.LinkProperties
import android.net.Network
import android.net.NetworkCapabilities
import android.os.Build
import android.os.ext.SdkExtensions
import android.text.Html
import androidx.annotation.DrawableRes
import androidx.annotation.StringRes
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ExposedDropdownMenuAnchorType
import androidx.compose.material3.ExposedDropdownMenuBox
import androidx.compose.material3.ExposedDropdownMenuDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.State
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalInspectionMode
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.res.stringArrayResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.TextRange
import androidx.compose.ui.text.fromHtml
import androidx.compose.ui.text.input.TextFieldValue
import androidx.compose.ui.tooling.preview.Preview
import androidx.compose.ui.unit.dp
import androidx.core.content.FileProvider
import androidx.core.content.edit
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.BootReceiver
import be.mygod.vpnhotspot.BuildConfig
import be.mygod.vpnhotspot.LocalOnlyHotspotService
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.RepeaterService
import be.mygod.vpnhotspot.RoutingManager
import be.mygod.vpnhotspot.net.Routing.Ipv6Mode
import be.mygod.vpnhotspot.net.TetherOffloadManager
import be.mygod.vpnhotspot.net.monitor.Upstream
import be.mygod.vpnhotspot.net.monitor.Upstreams
import be.mygod.vpnhotspot.net.wifi.WifiDoubleLock
import be.mygod.vpnhotspot.root.Dump
import be.mygod.vpnhotspot.root.RootManager
import be.mygod.vpnhotspot.root.daemon.MasqueradeMode
import be.mygod.vpnhotspot.ui.theme.VpnHotspotTheme
import be.mygod.vpnhotspot.util.allInterfaceNames
import be.mygod.vpnhotspot.util.allRoutes
import be.mygod.vpnhotspot.util.globalNetworkRequestBuilder
import be.mygod.vpnhotspot.util.launchUrl
import be.mygod.vpnhotspot.util.readableMessage
import be.mygod.vpnhotspot.util.Services
import com.google.android.gms.oss.licenses.R as OssLicensesR
import com.google.android.gms.oss.licenses.v2.OssLicensesMenuActivity
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.DelicateCoroutinesApi
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import timber.log.Timber
import java.io.File
import java.io.FileOutputStream
import java.io.IOException
import java.io.PrintWriter
import java.net.InetAddress

@Composable
@OptIn(DelicateCoroutinesApi::class)
internal fun SettingsScreen(snackbarHostState: SnackbarHostState) {
    val context = LocalContext.current
    val inspectionMode = LocalInspectionMode.current
    val lifecycleOwner = LocalLifecycleOwner.current
    val scope = rememberCoroutineScope()
    var offloadEnabled by remember { mutableStateOf(!inspectionMode && TetherOffloadManager.enabled) }
    var offloadChanging by remember { mutableStateOf(false) }
    val primaryUpstream by if (inspectionMode) {
        remember { mutableStateOf<Upstream?>(null) }
    } else Upstreams.primary.collectAsStateWithLifecycle()
    val fallbackUpstream by if (inspectionMode) {
        remember { mutableStateOf<Upstream?>(null) }
    } else Upstreams.fallback.collectAsStateWithLifecycle()
    val primaryPreference by if (inspectionMode) {
        remember { mutableStateOf<String?>(null) }
    } else rememberPreferenceString(Upstreams.KEY_PRIMARY)
    val fallbackPreference by if (inspectionMode) {
        remember { mutableStateOf<String?>(null) }
    } else rememberPreferenceString(Upstreams.KEY_FALLBACK)
    var masqueradeMode by remember {
        mutableStateOf(if (inspectionMode) "Simple" else masqueradePreferenceValue())
    }
    var ipv6Mode by remember {
        mutableStateOf(if (inspectionMode) Ipv6Mode.Block.name else RoutingManager.ipv6Mode.name)
    }
    var wifiLockMode by remember {
        mutableStateOf(if (inspectionMode) WifiDoubleLock.Mode.None.name else WifiDoubleLock.mode.name)
    }
    val autoStart by if (inspectionMode) {
        remember { mutableStateOf(false) }
    } else rememberPreferenceBoolean(BootReceiver.KEY, false)
    val repeaterSafeMode by if (inspectionMode) {
        remember { mutableStateOf(true) }
    } else rememberPreferenceBoolean(RepeaterService.KEY_SAFE_MODE, true)
    val useSystemTempHotspot by if (inspectionMode) {
        remember { mutableStateOf(false) }
    } else rememberPreferenceBoolean(LocalOnlyHotspotService.KEY_USE_SYSTEM, false)

    if (!inspectionMode) {
        LaunchedEffect(Unit) {
            WifiDoubleLock.mode = WifiDoubleLock.mode
            wifiLockMode = WifiDoubleLock.mode.name
            RoutingManager.masqueradeMode = RoutingManager.masqueradeMode
            masqueradeMode = masqueradePreferenceValue()
            RoutingManager.ipv6Mode = RoutingManager.ipv6Mode
            ipv6Mode = RoutingManager.ipv6Mode.name
        }
        androidx.compose.runtime.DisposableEffect(lifecycleOwner, offloadChanging) {
            fun refresh() {
                masqueradeMode = masqueradePreferenceValue()
                ipv6Mode = RoutingManager.ipv6Mode.name
                wifiLockMode = WifiDoubleLock.mode.name
                if (!offloadChanging) offloadEnabled = TetherOffloadManager.enabled
            }
            val observer = object : DefaultLifecycleObserver {
                override fun onResume(owner: LifecycleOwner) = refresh()
            }
            lifecycleOwner.lifecycle.addObserver(observer)
            if (lifecycleOwner.lifecycle.currentState.isAtLeast(Lifecycle.State.RESUMED)) refresh()
            onDispose { lifecycleOwner.lifecycle.removeObserver(observer) }
        }
    }
    val showRepeaterSafeMode = inspectionMode || (Services.p2p != null && RepeaterService.safeModeConfigurable)

    SettingsList {
        item {
            PreferenceGroup {
                row {
                    PreferenceRow(
                        icon = R.drawable.ic_action_settings_backup_restore,
                        title = stringResource(R.string.settings_service_clean),
                        summary = stringResource(R.string.settings_service_clean_summary),
                        onClick = {
                            if (!inspectionMode) GlobalScope.launch(Dispatchers.Default) { RoutingManager.clean() }
                        },
                    )
                }
            }
        }
        item {
            PreferenceGroup(title = stringResource(R.string.settings_upstream)) {
                val fallback = stringResource(R.string.settings_service_upstream_auto)
                row {
                    TextPreferenceRow(
                        icon = R.drawable.ic_action_settings_ethernet,
                        title = R.string.settings_service_upstream,
                        value = primaryPreference.orEmpty(),
                        summary = upstreamSummary(
                            fallback = fallback,
                            preference = primaryPreference,
                            upstream = primaryUpstream,
                        ),
                        description = annotatedStringResource(R.string.settings_service_upstream_help),
                        placeholder = fallback,
                        suggestNetworkInterfaces = true,
                        onValueChange = {
                            if (!inspectionMode) app.pref.edit { putString(Upstreams.KEY_PRIMARY, it.ifBlank { null }) }
                        },
                    )
                }
                val fallbackLabel = stringResource(R.string.settings_upstream_fallback_auto)
                row {
                    TextPreferenceRow(
                        icon = R.drawable.ic_action_settings_input_component,
                        title = R.string.settings_upstream_fallback,
                        value = fallbackPreference.orEmpty(),
                        summary = upstreamSummary(
                            fallback = fallbackLabel,
                            preference = fallbackPreference,
                            upstream = fallbackUpstream,
                        ),
                        description = annotatedStringResource(R.string.settings_upstream_fallback_help),
                        placeholder = fallbackLabel,
                        suggestNetworkInterfaces = true,
                        onValueChange = {
                            if (!inspectionMode) {
                                app.pref.edit { putString(Upstreams.KEY_FALLBACK, it.ifBlank { null }) }
                            }
                        },
                    )
                }
            }
        }
        item {
            PreferenceGroup(title = stringResource(R.string.settings_downstream)) {
                row {
                    ListPreferenceRow(
                        icon = R.drawable.ic_social_people,
                        title = R.string.settings_service_masquerade,
                        entries = stringArrayResource(R.array.settings_service_masquerade),
                        entrySummaries = stringArrayResource(R.array.settings_service_masquerade_summaries),
                        values = stringArrayResource(R.array.settings_service_masquerade_values),
                        selectedValue = masqueradeMode,
                        onValueChange = {
                            masqueradeMode = it
                            if (!inspectionMode) RoutingManager.masqueradeMode = masqueradeModeFromPreferenceValue(it)
                        },
                    )
                }
                row {
                    ListPreferenceRow(
                        icon = R.drawable.ic_image_looks_6,
                        title = R.string.settings_service_ipv6_mode,
                        entries = stringArrayResource(R.array.settings_service_ipv6_mode),
                        entrySummaries = stringArrayResource(R.array.settings_service_ipv6_mode_summaries),
                        values = stringArrayResource(R.array.settings_service_ipv6_mode_values),
                        selectedValue = ipv6Mode,
                        onValueChange = {
                            ipv6Mode = it
                            if (!inspectionMode) RoutingManager.ipv6Mode = Ipv6Mode.valueOf(it)
                        },
                    )
                }
                row {
                    SwitchPreferenceRow(
                        icon = R.drawable.ic_device_battery_charging_full,
                        title = R.string.settings_system_tether_offload,
                        summary = stringResource(R.string.settings_system_tether_offload_summary),
                        checked = offloadEnabled,
                        enabled = !offloadChanging,
                        onCheckedChange = { enabled ->
                            if (inspectionMode) {
                                offloadEnabled = enabled
                                return@SwitchPreferenceRow
                            }
                            if (TetherOffloadManager.enabled == enabled) return@SwitchPreferenceRow
                            scope.launch {
                                offloadChanging = true
                                try {
                                    TetherOffloadManager.setEnabled(enabled)
                                } catch (e: CancellationException) {
                                    throw e
                                } catch (e: Exception) {
                                    Timber.w(e)
                                    snackbarHostState.showLongSnackbar(e.readableMessage)
                                } finally {
                                    offloadEnabled = TetherOffloadManager.enabled
                                    offloadChanging = false
                                }
                            }
                        },
                    )
                }
            }
        }
        item {
            PreferenceGroup(title = stringResource(R.string.settings_misc)) {
                row {
                    ListPreferenceRow(
                        icon = R.drawable.ic_device_wifi_lock,
                        title = R.string.settings_service_wifi_lock,
                        entries = stringArrayResource(R.array.settings_service_wifi_lock),
                        entrySummaries = stringArrayResource(R.array.settings_service_wifi_lock_summaries),
                        values = stringArrayResource(R.array.settings_service_wifi_lock_values),
                        selectedValue = wifiLockMode,
                        onValueChange = {
                            wifiLockMode = it
                            if (!inspectionMode) WifiDoubleLock.mode = WifiDoubleLock.Mode.valueOf(it)
                        },
                    )
                }
                row {
                    SwitchPreferenceRow(
                        icon = R.drawable.ic_action_autorenew,
                        title = R.string.settings_service_auto_start,
                        summary = stringResource(R.string.settings_service_auto_start_summary),
                        checked = autoStart,
                        onCheckedChange = { enabled ->
                            if (!inspectionMode) {
                                app.pref.edit { putBoolean(BootReceiver.KEY, enabled) }
                                scope.launch { BootReceiver.onUserSettingUpdated(enabled) }
                            }
                        },
                    )
                }
                if (showRepeaterSafeMode) row {
                    SwitchPreferenceRow(
                        icon = R.drawable.ic_alert_warning,
                        title = R.string.settings_service_repeater_safe_mode,
                        summary = stringResource(R.string.settings_service_repeater_safe_mode_summary),
                        checked = repeaterSafeMode,
                        onCheckedChange = {
                            if (!inspectionMode) app.pref.edit { putBoolean(RepeaterService.KEY_SAFE_MODE, it) }
                        },
                    )
                }
                if (Build.VERSION.SDK_INT >= 30) row {
                    SwitchPreferenceRow(
                        icon = R.drawable.ic_content_file_copy,
                        title = R.string.settings_service_temp_hotspot_use_system,
                        summary = stringResource(if (Build.VERSION.SDK_INT >= 31) {
                            R.string.settings_service_temp_hotspot_use_system_summary
                        } else R.string.settings_service_temp_hotspot_use_system_summary_api30),
                        checked = useSystemTempHotspot,
                        onCheckedChange = {
                            if (!inspectionMode) {
                                app.pref.edit { putBoolean(LocalOnlyHotspotService.KEY_USE_SYSTEM, it) }
                            }
                        },
                    )
                }
            }
        }
        item {
            PreferenceGroup(title = stringResource(R.string.settings_help)) {
                row {
                    PreferenceRow(
                        icon = R.drawable.ic_toggle_star,
                        title = stringResource(R.string.settings_misc_source),
                        summary = stringResource(R.string.settings_misc_source_summary),
                        onClick = {
                            if (!inspectionMode) {
                                context.launchUrl("https://github.com/Mygod/VPNHotspot/blob/master/README.md")
                            }
                        },
                    )
                }
                row {
                    PreferenceRow(
                        icon = R.drawable.ic_action_bug_report,
                        title = stringResource(R.string.settings_misc_logcat),
                        summary = stringResource(R.string.settings_misc_logcat_summary),
                        onClick = {
                            if (!inspectionMode) GlobalScope.launch(Dispatchers.Main.immediate) {
                                try {
                                    shareLogcat(context)
                                } catch (e: CancellationException) {
                                    throw e
                                } catch (e: Exception) {
                                    Timber.w(e)
                                    snackbarHostState.showLongSnackbar(e.readableMessage)
                                }
                            }
                        },
                    )
                }
                row {
                    PreferenceRow(
                        icon = R.drawable.ic_action_card_giftcard,
                        title = stringResource(R.string.settings_misc_donate),
                        summary = stringResource(R.string.settings_misc_donate_summary),
                        onClick = { if (!inspectionMode) context.launchUrl("https://mygod.be/donate/") },
                    )
                }
                row {
                    PreferenceRow(
                        icon = R.drawable.ic_action_code,
                        title = stringResource(OssLicensesR.string.oss_license_title),
                        summary = stringResource(OssLicensesR.string.preferences_license_summary),
                        onClick = {
                            if (!inspectionMode) {
                                context.startActivity(Intent(context, OssLicensesMenuActivity::class.java))
                            }
                        },
                    )
                }
            }
        }
    }
}

@Composable
private fun ListPreferenceRow(
    @DrawableRes icon: Int,
    @StringRes title: Int,
    entries: Array<String>,
    entrySummaries: Array<String> = emptyArray(),
    values: Array<String>,
    selectedValue: String,
    description: AnnotatedString? = null,
    onValueChange: (String) -> Unit,
) {
    var selecting by rememberSaveable { mutableStateOf(false) }
    PreferenceRow(
        icon = icon,
        title = stringResource(title),
        summary = entries.getOrElse(values.indexOf(selectedValue)) { selectedValue },
        onClick = { selecting = true },
    )
    if (selecting) PreferenceSelectionDialog(
        title = stringResource(title),
        entryCount = entries.size,
        selectedIndex = values.indexOf(selectedValue),
        entryLabel = entries::get,
        entrySummary = { entrySummaries.getOrNull(it)?.let(AnnotatedString::fromHtml) },
        description = description,
        onDismissRequest = { selecting = false },
        onSelect = { values.getOrNull(it)?.let(onValueChange) },
    )
}

@Composable
@OptIn(ExperimentalMaterial3Api::class)
private fun TextPreferenceRow(
    @DrawableRes icon: Int,
    @StringRes title: Int,
    value: String,
    summary: AnnotatedString,
    description: AnnotatedString? = null,
    placeholder: String? = null,
    suggestNetworkInterfaces: Boolean = false,
    onValueChange: (String) -> Unit,
) {
    var editing by rememberSaveable { mutableStateOf(false) }
    var draft by rememberTextFieldValueAtEnd(value, editing)
    var suggestionsExpanded by remember(editing) { mutableStateOf(false) }
    val suggestions by rememberInterfaceNameSuggestions(
        !LocalInspectionMode.current && editing && suggestNetworkInterfaces,
    )
    val filteredSuggestions = remember(suggestions, draft.text) {
        suggestions.filter { draft.text.isBlank() || it.contains(draft.text, ignoreCase = true) }
    }
    LaunchedEffect(editing, suggestions) {
        if (editing && suggestions.isNotEmpty()) suggestionsExpanded = true
    }
    PreferenceRow(
        icon = icon,
        title = stringResource(title),
        summaryContent = { Text(summary) },
        onClick = { editing = true },
    )
    if (editing) {
        val focusRequester = remember { FocusRequester() }
        val keyboard = LocalSoftwareKeyboardController.current
        LaunchedEffect(Unit) {
            focusRequester.requestFocus()
            keyboard?.show()
        }
        AlertDialog(
            onDismissRequest = { editing = false },
            title = { Text(stringResource(title)) },
            text = {
                val menuExpanded = suggestionsExpanded && filteredSuggestions.isNotEmpty()
                Column(verticalArrangement = Arrangement.spacedBy(16.dp)) {
                    description?.let {
                        Text(
                            text = it,
                            style = MaterialTheme.typography.bodyMedium,
                        )
                    }
                    ExposedDropdownMenuBox(
                        expanded = menuExpanded,
                        onExpandedChange = { suggestionsExpanded = it },
                    ) {
                        OutlinedTextField(
                            value = draft,
                            onValueChange = {
                                draft = it
                                suggestionsExpanded = true
                            },
                            modifier = Modifier
                                .menuAnchor(ExposedDropdownMenuAnchorType.PrimaryEditable)
                                .fillMaxWidth()
                                .focusRequester(focusRequester),
                            placeholder = placeholder?.let { { Text(it) } },
                            trailingIcon = if (suggestNetworkInterfaces) {
                                {
                                    ExposedDropdownMenuDefaults.TrailingIcon(
                                        expanded = menuExpanded,
                                        modifier = Modifier.menuAnchor(ExposedDropdownMenuAnchorType.SecondaryEditable),
                                    )
                                }
                            } else null,
                            colors = ExposedDropdownMenuDefaults.outlinedTextFieldColors(),
                            singleLine = true,
                        )
                        ExposedDropdownMenu(
                            expanded = menuExpanded,
                            onDismissRequest = { suggestionsExpanded = false },
                        ) {
                            for (suggestion in filteredSuggestions) DropdownMenuItem(
                                text = { Text(suggestion) },
                                onClick = {
                                    draft = TextFieldValue(suggestion, TextRange(suggestion.length))
                                    suggestionsExpanded = false
                                },
                            )
                        }
                    }
                }
            },
            confirmButton = {
                TextButton(onClick = {
                    onValueChange(draft.text)
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
}

@Composable
private fun rememberInterfaceNameSuggestions(active: Boolean): State<List<String>> {
    val lifecycleOwner = LocalLifecycleOwner.current
    return produceState(initialValue = emptyList(), active, lifecycleOwner) {
        if (!active) return@produceState
        val interfaceNames = mutableMapOf<Network, List<String>>()
        fun update() {
            value = interfaceNames.values.flatten().distinct().sorted()
        }
        val callback = object : ConnectivityManager.NetworkCallback() {
            override fun onLinkPropertiesChanged(network: Network, properties: LinkProperties) {
                interfaceNames[network] = properties.allInterfaceNames
                update()
            }

            override fun onLost(network: Network) {
                interfaceNames.remove(network)
                update()
            }
        }
        var registered = false
        fun register() {
            if (!registered) {
                Services.registerNetworkCallback(globalNetworkRequestBuilder().apply {
                    removeCapability(NetworkCapabilities.NET_CAPABILITY_NOT_RESTRICTED)
                    removeCapability(NetworkCapabilities.NET_CAPABILITY_TRUSTED)
                    removeCapability(NetworkCapabilities.NET_CAPABILITY_NOT_VPN)
                }.build(), callback)
                registered = true
            }
        }
        fun unregister() {
            if (registered) {
                Services.connectivity.unregisterNetworkCallback(callback)
                registered = false
            }
        }
        val observer = object : DefaultLifecycleObserver {
            override fun onStart(owner: LifecycleOwner) = register()
            override fun onStop(owner: LifecycleOwner) {
                unregister()
                interfaceNames.clear()
                update()
            }
        }
        lifecycleOwner.lifecycle.addObserver(observer)
        if (lifecycleOwner.lifecycle.currentState.isAtLeast(Lifecycle.State.STARTED)) register()
        awaitDispose {
            lifecycleOwner.lifecycle.removeObserver(observer)
            unregister()
        }
    }
}

@Composable
private fun SwitchPreferenceRow(
    @DrawableRes icon: Int,
    @StringRes title: Int,
    summary: String,
    checked: Boolean,
    enabled: Boolean = true,
    onCheckedChange: (Boolean) -> Unit,
) {
    PreferenceRow(
        icon = icon,
        title = stringResource(title),
        summary = summary,
        enabled = enabled,
        trailing = {
            PreferenceSwitch(
                checked = checked,
                enabled = enabled,
                onCheckedChange = if (enabled) onCheckedChange else null,
            )
        },
        onClick = { onCheckedChange(!checked) },
    )
}

@Composable
private fun upstreamSummary(fallback: String, preference: String?, upstream: Upstream?) = AnnotatedString.fromHtml(
    stringResource(R.string.settings_upstream_current_summary,
        Html.escapeHtml(if (preference.isNullOrEmpty()) fallback else preference),
        mutableMapOf<String, Boolean>().let { interfaces ->
            for (route in upstream?.properties?.allRoutes ?: emptyList()) {
                interfaces.compute(route.`interface` ?: continue) { _, internet ->
                    internet == true || try {
                        route.matches(UPSTREAM_INTERNET_V4_ADDRESS) || route.matches(UPSTREAM_INTERNET_V6_ADDRESS)
                    } catch (e: RuntimeException) {
                        Timber.w(e)
                        false
                    }
                }
            }
            if (interfaces.isEmpty()) "\u2205" else buildString {
                interfaces.entries.forEachIndexed { index, (iface, internet) ->
                    if (index > 0) append(", ")
                    val escaped = Html.escapeHtml(iface)
                    if (internet) append("<b>").append(escaped).append("</b>") else append(escaped)
                }
            }
        }))

@Composable
private fun rememberPreferenceString(key: String): State<String?> {
    val pref = app.pref
    return produceState(initialValue = pref.getString(key, null), key) {
        val listener = SharedPreferences.OnSharedPreferenceChangeListener { sharedPreferences, changed ->
            if (changed == key) value = sharedPreferences.getString(key, null)
        }
        pref.registerOnSharedPreferenceChangeListener(listener)
        awaitDispose { pref.unregisterOnSharedPreferenceChangeListener(listener) }
    }
}

@Composable
private fun rememberPreferenceBoolean(key: String, defaultValue: Boolean): State<Boolean> {
    val pref = app.pref
    return produceState(initialValue = pref.getBoolean(key, defaultValue), key, defaultValue) {
        val listener = SharedPreferences.OnSharedPreferenceChangeListener { sharedPreferences, changed ->
            if (changed == key) value = sharedPreferences.getBoolean(key, defaultValue)
        }
        pref.registerOnSharedPreferenceChangeListener(listener)
        awaitDispose { pref.unregisterOnSharedPreferenceChangeListener(listener) }
    }
}

@Preview(name = "Settings", showBackground = true, widthDp = 420, heightDp = 720)
@Composable
private fun SettingsPreview() = SettingsPreviewContent()

@Preview(
    name = "Settings - dark",
    showBackground = true,
    widthDp = 420,
    heightDp = 720,
    uiMode = Configuration.UI_MODE_NIGHT_YES,
)
@Composable
private fun SettingsDarkPreview() = SettingsPreviewContent()

@Composable
private fun SettingsPreviewContent() {
    VpnHotspotTheme(dynamicColor = false) {
        Surface {
            SettingsScreen(snackbarHostState = remember { SnackbarHostState() })
        }
    }
}

private fun masqueradePreferenceValue() = when (RoutingManager.masqueradeMode) {
    MasqueradeMode.MASQUERADE_MODE_NONE -> "None"
    MasqueradeMode.MASQUERADE_MODE_SIMPLE -> "Simple"
    MasqueradeMode.MASQUERADE_MODE_NETD -> "Netd"
    is MasqueradeMode.Unrecognized -> throw IllegalArgumentException("Invalid masquerade mode")
}

private fun masqueradeModeFromPreferenceValue(value: String) = when (value) {
    "None" -> MasqueradeMode.MASQUERADE_MODE_NONE
    "Simple" -> MasqueradeMode.MASQUERADE_MODE_SIMPLE
    "Netd" -> MasqueradeMode.MASQUERADE_MODE_NETD
    else -> throw IllegalArgumentException("Invalid masquerade mode $value")
}

private suspend fun shareLogcat(context: Context) {
    val logFile = withContext(Dispatchers.IO) {
        val logDir = File(context.cacheDir, "log")
        logDir.mkdir()
        val logFile = File.createTempFile("vpnhotspot-", ".log", logDir)
        logFile.outputStream().use { out ->
            PrintWriter(out.bufferedWriter()).use { writer ->
                writer.println("${BuildConfig.VERSION_CODE} is running on API ${Build.VERSION.SDK_INT}")
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                    writer.println("S extension ${SdkExtensions.getExtensionVersion(Build.VERSION_CODES.S)}")
                }
                writer.println()
            }
        }
        try {
            ProcessBuilder(Dump.LOGCAT, "-d").apply {
                redirectErrorStream(true)
                redirectOutput(ProcessBuilder.Redirect.appendTo(logFile))
            }.start().waitFor()
        } catch (e: IOException) {
            Timber.w(e)
            logFile.appendText(e.stackTraceToString())
        }
        try {
            RootManager.use {
                it.execute(Dump(logFile.absolutePath))
            }
        } catch (e: Exception) {
            if (e !is CancellationException) Timber.w(e)
            PrintWriter(FileOutputStream(logFile, true)).use { e.printStackTrace(it) }
        }
        logFile
    }
    context.startActivity(Intent.createChooser(Intent(Intent.ACTION_SEND)
        .setType("text/x-log")
        .setFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
        .putExtra(Intent.EXTRA_STREAM, FileProvider.getUriForFile(context, "be.mygod.vpnhotspot.log", logFile)),
        "context.getString(androidx.appcompat.R.string.abc_shareactionprovider_share_with)"))
}

private val UPSTREAM_INTERNET_V4_ADDRESS = InetAddress.getByAddress(byteArrayOf(8, 8, 8, 8))
private val UPSTREAM_INTERNET_V6_ADDRESS = InetAddress.getByAddress(byteArrayOf(
    0x20, 0x01, 0x48, 0x60, 0x48, 0x60, 0, 0,
    0, 0, 0, 0, 0, 0, 0x88.toByte(), 0x88.toByte(),
))
