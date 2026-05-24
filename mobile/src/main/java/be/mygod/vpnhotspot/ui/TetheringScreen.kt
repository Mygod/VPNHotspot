package be.mygod.vpnhotspot.ui

import android.Manifest
import android.bluetooth.BluetoothManager
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.content.res.Configuration
import android.net.MacAddress
import android.net.TetheringManager
import android.net.wifi.OuiKeyedData
import android.net.wifi.SoftApCapability
import android.net.wifi.SoftApInfo
import android.net.wifi.WifiClient
import android.net.wifi.`WifiManager$SoftApCallback`
import android.net.wifi.p2p.WifiP2pGroup
import android.os.Build
import android.provider.Settings
import android.widget.Toast
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.annotation.DrawableRes
import androidx.annotation.StringRes
import androidx.compose.foundation.gestures.Orientation
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.text.input.TextFieldLineLimits
import androidx.compose.foundation.text.input.TextFieldState
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.material3.scrollbar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.State
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalInspectionMode
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.TextLinkStyles
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.tooling.preview.Preview
import androidx.compose.ui.unit.dp
import androidx.core.content.ContextCompat
import androidx.core.content.getSystemService
import androidx.core.net.toUri
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.repeatOnLifecycle
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.LocalOnlyHotspotService
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.RepeaterService
import be.mygod.vpnhotspot.StaticIpSetter
import be.mygod.vpnhotspot.TetheringService
import be.mygod.vpnhotspot.manage.BluetoothTethering
import be.mygod.vpnhotspot.manage.ManageBar
import be.mygod.vpnhotspot.net.MacAddressCompat
import be.mygod.vpnhotspot.net.TetherStates
import be.mygod.vpnhotspot.net.TetherType
import be.mygod.vpnhotspot.net.TetheringManagerCompat
import be.mygod.vpnhotspot.net.wifi.SoftApConfigurationCompat
import be.mygod.vpnhotspot.net.wifi.WifiApManager
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper
import be.mygod.vpnhotspot.net.wifi.apInstanceIdentifierOrNull
import be.mygod.vpnhotspot.root.WifiApCommands
import be.mygod.vpnhotspot.ui.apconfiguration.VendorData
import be.mygod.vpnhotspot.ui.apconfiguration.formatTimeoutMillis
import be.mygod.vpnhotspot.ui.theme.VpnHotspotPreviewSurface
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.readableMessage
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.launch
import timber.log.Timber
import java.lang.reflect.InvocationTargetException
import java.net.NetworkInterface
import java.net.SocketException
import java.text.NumberFormat
import java.util.Locale

data class TetheringServiceState(
    val managedIfaces: Set<String> = emptySet(),
    val inactiveIfaces: Set<String> = emptySet(),
    val monitoredIfaces: Set<String> = emptySet(),
)

@Composable
fun TetheringScreen(
    snackbarHostState: SnackbarHostState,
    repeaterBinder: RepeaterService.Binder?,
    localOnlyBinder: LocalOnlyHotspotService.Binder?,
    tetherStates: TetherStates,
    tetheringServiceState: TetheringServiceState,
    interfaceRefreshVersion: Int = 0,
    onConfigureRepeater: () -> Unit,
    onConfigureTemporaryHotspot: (() -> Unit)?,
    onConfigureAp: () -> Unit,
) {
    val context = LocalContext.current
    val inspectionMode = LocalInspectionMode.current
    val scope = rememberCoroutineScope()
    val linkStyles = rememberNetworkAddressLinkStyles()
    val repeaterMissingLocationPermissions = stringResource(R.string.repeater_missing_location_permissions)
    val localOnlyIface by (localOnlyBinder?.iface)?.collectAsStateWithLifecycle(null)
        ?: remember { mutableStateOf(null) }
    val repeaterStatus by (repeaterBinder?.status)?.collectAsStateWithLifecycle(null)
        ?: remember { mutableStateOf(null) }
    val repeaterGroup by (repeaterBinder?.group)?.collectAsStateWithLifecycle(null)
        ?: remember { mutableStateOf(null) }
    val staticIpActive by StaticIpSetter.active.collectAsStateWithLifecycle()
    val staticIpAddresses by StaticIpSetter.addresses.collectAsStateWithLifecycle()
    val staticIpApplying by StaticIpSetter.applying.collectAsStateWithLifecycle()
    var staticIpDraft by rememberSaveable { mutableStateOf<String?>(null) }
    val staticIpDraftText = rememberSaveable(
        staticIpDraft.orEmpty(),
        staticIpDraft != null,
        saver = TextFieldState.Saver,
    ) {
        TextFieldState(staticIpDraft.orEmpty())
    }
    var wpsDialog by rememberSaveable { mutableStateOf(false) }
    var wpsPin by rememberTextFieldValueAtEnd("", wpsDialog)
    val tetherTypeVersion by if (inspectionMode) remember { mutableIntStateOf(0) } else rememberTetherTypeVersion()
    var manageBarVersion by remember { mutableIntStateOf(0) }
    val manageOffloadEnabled = if (inspectionMode) false else remember(manageBarVersion) { ManageBar.offloadEnabled }
    val ifaceLookup = remember(
        tetherStates,
        tetheringServiceState,
        localOnlyIface,
        repeaterGroup,
        interfaceRefreshVersion,
    ) {
        networkInterfaceLookup()
    }
    val monitored = tetheringServiceState.monitoredIfaces
    val managed = tetheringServiceState.managedIfaces
    val inactive = tetheringServiceState.inactiveIfaces
    val tetheredTypes = remember(tetherStates, tetherTypeVersion) {
        tetherStates.tethered.map { TetherType.ofInterface(it) }.toSet()
    }
    val interfaceIfaces = remember(tetherStates, monitored) {
        (tetherStates.tethered + monitored).toSortedSet().toList()
    }
    val wifiBaseError = tetherError(context, tetherStates, TetherType.WIFI)
    val wifiSummary by if (inspectionMode) {
        remember(wifiBaseError) { mutableStateOf(wifiBaseError) }
    } else rememberWifiSummary(wifiBaseError, linkStyles)
    var bluetoothVersion by remember { mutableIntStateOf(0) }
    val bluetoothAdapter = if (inspectionMode) null else remember {
        context.getSystemService<BluetoothManager>()?.adapter
    }
    val bluetoothTethering = remember(bluetoothAdapter) {
        bluetoothAdapter?.let { BluetoothTethering(context, it) { bluetoothVersion++ } }
    }
    val requestBluetooth = if (inspectionMode) null else {
        rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
            if (granted) {
                bluetoothTethering?.ensureInit(context)
                bluetoothVersion++
            }
        }
    }
    val lifecycleOwner = LocalLifecycleOwner.current
    DisposableEffect(lifecycleOwner, bluetoothTethering) {
        var resumed = false
        fun refreshBluetooth() {
            if (resumed) return
            resumed = true
            manageBarVersion++
            if (bluetoothTethering == null || Build.VERSION.SDK_INT < 31) return
            if (ContextCompat.checkSelfPermission(context, Manifest.permission.BLUETOOTH_CONNECT) ==
                PackageManager.PERMISSION_GRANTED) {
                bluetoothTethering.ensureInit(context)
                bluetoothVersion++
            } else requestBluetooth?.launch(Manifest.permission.BLUETOOTH_CONNECT)
        }
        val observer = object : DefaultLifecycleObserver {
            override fun onResume(owner: LifecycleOwner) = refreshBluetooth()
            override fun onPause(owner: LifecycleOwner) {
                resumed = false
            }
        }
        lifecycleOwner.lifecycle.addObserver(observer)
        if (lifecycleOwner.lifecycle.currentState.isAtLeast(Lifecycle.State.RESUMED)) refreshBluetooth()
        onDispose {
            lifecycleOwner.lifecycle.removeObserver(observer)
            bluetoothTethering?.close()
        }
    }
    val startRepeater: (String) -> Unit = if (inspectionMode) {
        { _: String -> }
    } else {
        val launcher = rememberLauncherForActivityResult(
            ActivityResultContracts.RequestPermission(),
            onResult = { granted ->
                if (granted) {
                    app.startServiceWithLocation<RepeaterService>(context)
                } else scope.launch {
                    snackbarHostState.showLongSnackbar(repeaterMissingLocationPermissions)
                }
            },
        )
        launcher::launch
    }
    val startLocalOnly: (String) -> Unit = if (inspectionMode) {
        { _: String -> }
    } else {
        val launcher = rememberLauncherForActivityResult(
            ActivityResultContracts.RequestPermission(),
            onResult = { app.startServiceWithLocation<LocalOnlyHotspotService>(context) },
        )
        launcher::launch
    }
    val showRepeater = inspectionMode || Services.p2p != null
    val showRepeaterWps = (repeaterStatus == RepeaterService.Status.STARTING ||
            repeaterStatus == RepeaterService.Status.ACTIVE) && WifiP2pManagerHelper.startWps != null
    val showBluetooth = inspectionMode || (context.packageManager.hasSystemFeature(PackageManager.FEATURE_BLUETOOTH) &&
            bluetoothTethering != null)
    val bluetoothActive = remember(showBluetooth, bluetoothTethering, bluetoothVersion, tetherStates.tethered) {
        if (showBluetooth) bluetoothTethering?.active else null
    }

    SettingsList {
        preferenceGroup(key = "active_tethering") {
            if (showRepeater) {
                val active = repeaterStatus == RepeaterService.Status.ACTIVE
                val switchEnabled = repeaterStatus == RepeaterService.Status.IDLE || active
                val toggleRepeater: () -> Unit = {
                    when (repeaterStatus) {
                        RepeaterService.Status.IDLE -> startRepeater(if (Build.VERSION.SDK_INT >= 33) {
                            Manifest.permission.NEARBY_WIFI_DEVICES
                        } else Manifest.permission.ACCESS_FINE_LOCATION)
                        RepeaterService.Status.ACTIVE -> repeaterBinder?.shutdown()
                        else -> { }
                    }
                }
                row(R.string.title_repeater) {
                    TetheringRow(
                        icon = R.drawable.ic_router,
                        title = stringResource(R.string.title_repeater),
                        summary = repeaterSummary(context, repeaterGroup, ifaceLookup, linkStyles),
                        checked = repeaterStatus == RepeaterService.Status.STARTING || active,
                        switchEnabled = switchEnabled,
                        onClick = onConfigureRepeater,
                        onCheckedChange = toggleRepeater,
                    )
                }
                if (showRepeaterWps) row("repeater_wps") {
                    PreferenceRow(
                        modifier = Modifier.padding(start = 48.dp),
                        icon = R.drawable.ic_wifi_protected_setup,
                        title = stringResource(R.string.repeater_wps),
                        onClick = { if (repeaterBinder?.active == true) wpsDialog = true },
                    )
                }
            }
            row(R.string.tethering_temp_hotspot) {
                val toggleLocalOnly: () -> Unit = {
                    if (localOnlyIface == null) {
                        startLocalOnly(if (Build.VERSION.SDK_INT >= 33) {
                            Manifest.permission.NEARBY_WIFI_DEVICES
                        } else Manifest.permission.ACCESS_FINE_LOCATION)
                    } else localOnlyBinder?.stop()
                }
                TetheringRow(
                    icon = R.drawable.ic_android_wifi_3_bar_plus,
                    title = stringResource(R.string.tethering_temp_hotspot),
                    summary = networkInterfaceAddressesText(ifaceLookup[localOnlyIface], linkStyles),
                    checked = localOnlyIface != null,
                    onClick = onConfigureTemporaryHotspot ?: toggleLocalOnly,
                    onCheckedChange = if (onConfigureTemporaryHotspot == null) null else toggleLocalOnly,
                )
            }
            row(R.string.tethering_static_ip) {
                TetheringRow(
                    icon = R.drawable.ic_push_pin,
                    title = stringResource(R.string.tethering_static_ip),
                    summary = buildAnnotatedString {
                        for ((address, prefixLength) in staticIpAddresses) {
                            if (length > 0) append('\n')
                            appendIpAddress(address, linkStyles)
                            if (prefixLength.toInt() != address.address.size * 8) append("/$prefixLength")
                        }
                    },
                    checked = staticIpActive,
                    switchEnabled = !staticIpApplying,
                    onClick = {
                        staticIpDraft = StaticIpSetter.ips
                    },
                    onCheckedChange = { StaticIpSetter.enable(!staticIpActive) },
                )
            }
        }
        for (iface in interfaceIfaces) {
            item(key = "interface_$iface") {
                val active = managed.contains(iface)
                val watch = monitored.contains(iface)
                val ifaceInactive = inactive.contains(iface)
                val vpnTethering = active && !ifaceInactive
                PreferenceGroup {
                    row("vpn_tethering") {
                        TetheringRow(
                            icon = TetherType.ofInterface(iface).icon,
                            title = iface,
                            summary = networkInterfaceAddressesText(
                                ifaceLookup[iface],
                                linkStyles,
                                macOnly = ifaceInactive,
                            ),
                            checked = vpnTethering,
                            enabled = !ifaceInactive,
                            onClick = {
                                if (active) context.startService(Intent(context, TetheringService::class.java)
                                    .putExtra(TetheringService.EXTRA_REMOVE_INTERFACE, iface))
                                else context.startForegroundService(Intent(context, TetheringService::class.java)
                                    .putExtra(TetheringService.EXTRA_ADD_INTERFACES, arrayOf(iface)))
                            },
                        )
                    }
                    if (vpnTethering || watch) row("watch_reconnect") {
                        PreferenceRow(
                            title = stringResource(R.string.tethering_watch_reconnect),
                            trailing = {
                                PreferenceSwitch(
                                    checked = watch,
                                    onCheckedChange = null,
                                )
                            },
                            onClick = {
                                if (watch) context.startService(Intent(context, TetheringService::class.java)
                                    .putExtra(TetheringService.EXTRA_REMOVE_INTERFACE_MONITOR, iface))
                                else context.startForegroundService(Intent(context, TetheringService::class.java)
                                    .putExtra(TetheringService.EXTRA_ADD_INTERFACE_MONITOR, iface))
                            },
                        )
                    }
                }
            }
        }
        preferenceGroup(key = "manage_tethering") {
            row(R.string.tethering_manage) {
                PreferenceRow(
                    icon = R.drawable.ic_add,
                    iconTint = MaterialTheme.colorScheme.secondary,
                    title = stringResource(R.string.tethering_manage),
                    summary = if (manageOffloadEnabled) {
                        stringResource(R.string.tethering_manage_offload_enabled)
                    } else null,
                    onClick = { ManageBar.start(context::startActivity) },
                )
            }
            row(R.string.tethering_manage_wifi) {
                TetheringTypeRow(
                    icon = R.drawable.ic_network_wifi,
                    title = R.string.tethering_manage_wifi,
                    checked = tetheredTypes.contains(TetherType.WIFI),
                    summary = wifiSummary,
                    tetheringType = TetheringManager.TETHERING_WIFI,
                    snackbarHostState = snackbarHostState,
                    onConfigure = onConfigureAp,
                )
            }
            row(R.string.tethering_manage_usb) {
                TetheringTypeRow(
                    icon = R.drawable.ic_usb,
                    title = R.string.tethering_manage_usb,
                    checked = tetheredTypes.contains(TetherType.USB) || tetheredTypes.contains(TetherType.NCM),
                    summary = tetherError(context, tetherStates, TetherType.USB),
                    tetheringType = TetheringManagerCompat.TETHERING_USB,
                    snackbarHostState = snackbarHostState,
                )
            }
            if (showBluetooth) {
                row(R.string.tethering_manage_bluetooth) {
                    BluetoothTetheringRow(
                        active = bluetoothActive,
                        summary = buildAnnotatedString {
                            if (bluetoothActive == null) bluetoothTethering?.activeFailureCause?.readableMessage?.let {
                                append(it)
                            }
                            tetherError(context, tetherStates, TetherType.BLUETOOTH)?.let {
                                if (length > 0) append('\n')
                                append(it)
                            }
                        },
                        bluetoothTethering = bluetoothTethering,
                        snackbarHostState = snackbarHostState,
                        onRefresh = { bluetoothVersion++ },
                    )
                }
            }
            if (Build.VERSION.SDK_INT >= 30) row(R.string.tethering_manage_ethernet) {
                TetheringTypeRow(
                    icon = TetherType.ETHERNET.icon,
                    title = R.string.tethering_manage_ethernet,
                    checked = tetheredTypes.contains(TetherType.ETHERNET),
                    summary = tetherError(context, tetherStates, TetherType.ETHERNET),
                    tetheringType = TetheringManagerCompat.TETHERING_ETHERNET,
                    snackbarHostState = snackbarHostState,
                )
            }
        }
    }

    if (staticIpDraft != null) {
        val focusRequester = rememberDialogFocusRequester()
        AlertDialog(
            onDismissRequest = {
                staticIpDraft = null
            },
            title = { Text(stringResource(R.string.tethering_static_ip)) },
            text = {
                val scrollState = rememberScrollState()
                Column(verticalArrangement = Arrangement.spacedBy(16.dp)) {
                    Text(
                        text = stringResource(R.string.tethering_static_ip_help),
                        style = MaterialTheme.typography.bodyMedium,
                    )
                    OutlinedTextField(
                        state = staticIpDraftText,
                        modifier = Modifier
                            .fillMaxWidth()
                            .focusRequester(focusRequester)
                            .scrollbar(
                                state = scrollState.scrollIndicatorState,
                                orientation = Orientation.Vertical,
                                isFadeEnabled = false,
                            ),
                        keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri),
                        lineLimits = TextFieldLineLimits.MultiLine(
                            minHeightInLines = 2,
                            maxHeightInLines = 2,
                        ),
                        scrollState = scrollState,
                    )
                }
            },
            confirmButton = {
                DialogConfirmButton(onClick = {
                    StaticIpSetter.ips = staticIpDraftText.text.toString().trim()
                    staticIpDraft = null
                }) {
                    Text(stringResource(android.R.string.ok))
                }
            },
            dismissButton = {
                DialogDismissButton(onClick = {
                    staticIpDraft = null
                }) {
                    Text(stringResource(android.R.string.cancel))
                }
            },
        )
    }
    if (wpsDialog) {
        val focusRequester = rememberDialogFocusRequester()
        AlertDialog(
            onDismissRequest = { wpsDialog = false },
            title = { Text(stringResource(R.string.repeater_wps_dialog_title)) },
            text = {
                OutlinedTextField(
                    value = wpsPin,
                    onValueChange = { wpsPin = it },
                    modifier = Modifier
                        .fillMaxWidth()
                        .focusRequester(focusRequester),
                    keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Number),
                    singleLine = true,
                )
            },
            confirmButton = {
                DialogConfirmButton(onClick = {
                    repeaterBinder?.startWps(wpsPin.text)
                    wpsDialog = false
                }) {
                    Text(stringResource(android.R.string.ok))
                }
            },
            dismissButton = {
                DialogNeutralButton(onClick = {
                    repeaterBinder?.startWps(null)
                    wpsDialog = false
                }) {
                    Text(stringResource(R.string.repeater_wps_dialog_pbc))
                }
                DialogDismissButton(onClick = { wpsDialog = false }) {
                    Text(stringResource(android.R.string.cancel))
                }
            },
        )
    }
}

@Composable
private fun TetheringTypeRow(
    @DrawableRes icon: Int,
    @StringRes title: Int,
    checked: Boolean,
    summary: AnnotatedString?,
    tetheringType: Int,
    snackbarHostState: SnackbarHostState,
    onConfigure: (() -> Unit)? = null,
) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    val toggle = toggle@{
        if (!Settings.System.canWrite(context)) try {
            context.startActivity(Intent(Settings.ACTION_MANAGE_WRITE_SETTINGS, "package:${context.packageName}".toUri()))
            return@toggle
        } catch (e: RuntimeException) {
            app.logEvent("manage_write_settings") { param("message", e.toString()) }
        }
        val callback = tetheringCallback(
            context,
            snackbarHostState,
            TetherType.fromTetheringType(tetheringType),
            scope,
        )
        if (checked) TetheringManagerCompat.stopTethering(tetheringType, callback)
        else TetheringManagerCompat.startTethering(tetheringType, true, callback)
    }
    TetheringRow(
        icon = icon,
        title = stringResource(title),
        summary = summary,
        checked = checked,
        onClick = onConfigure ?: toggle,
        onCheckedChange = if (onConfigure == null) null else toggle,
    )
}

@Composable
private fun BluetoothTetheringRow(
    active: Boolean?,
    summary: AnnotatedString,
    bluetoothTethering: BluetoothTethering?,
    snackbarHostState: SnackbarHostState,
    onRefresh: () -> Unit,
) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    TetheringRow(
        icon = R.drawable.ic_bluetooth,
        title = stringResource(R.string.tethering_manage_bluetooth),
        summary = summary,
        checked = active == true,
        enabled = bluetoothTethering != null,
        onClick = {
            if (!Settings.System.canWrite(context)) try {
                context.startActivity(Intent(Settings.ACTION_MANAGE_WRITE_SETTINGS, "package:${context.packageName}".toUri()))
                return@TetheringRow
            } catch (e: RuntimeException) {
                app.logEvent("manage_write_settings") { param("message", e.toString()) }
            }
            val callback = tetheringCallback(context, snackbarHostState, TetherType.BLUETOOTH, scope, onRefresh)
            when (active) {
                true -> {
                    bluetoothTethering?.stop(callback)
                    onRefresh()
                }
                false -> bluetoothTethering?.start(callback, context)
                null -> ManageBar.start(context::startActivity)
            }
        },
    )
}

@Composable
private fun TetheringRow(
    @DrawableRes icon: Int,
    title: String,
    summary: AnnotatedString? = null,
    checked: Boolean,
    enabled: Boolean = true,
    switchEnabled: Boolean = enabled,
    onClick: () -> Unit,
    onCheckedChange: (() -> Unit)? = null,
) {
    PreferenceRow(
        icon = icon,
        title = title,
        summaryContent = summary?.takeIf { it.text.isNotEmpty() }?.let {
            {
                RowSelectionContainer {
                    Text(it)
                }
            }
        },
        enabled = enabled,
        trailing = {
            if (onCheckedChange == null) {
                PreferenceSwitch(
                    checked = checked,
                    enabled = switchEnabled,
                    onCheckedChange = null,
                )
            } else {
                PreferenceSplitSwitch(
                    checked = checked,
                    enabled = switchEnabled,
                    onCheckedChange = { onCheckedChange() },
                )
            }
        },
        onClick = onClick,
    )
}

@Preview(name = "Tethering", showBackground = true, widthDp = 420, heightDp = 720)
@Preview(
    name = "Tethering - dark",
    showBackground = true,
    widthDp = 420,
    heightDp = 720,
    uiMode = Configuration.UI_MODE_NIGHT_YES,
)
@Composable
private fun TetheringPreview() {
    VpnHotspotPreviewSurface {
        TetheringScreen(
            snackbarHostState = remember { SnackbarHostState() },
            repeaterBinder = null,
            localOnlyBinder = null,
            tetherStates = TetherStates(),
            tetheringServiceState = TetheringServiceState(),
            onConfigureRepeater = {},
            onConfigureTemporaryHotspot = null,
            onConfigureAp = {},
        )
    }
}

private fun tetheringCallback(
    context: Context,
    snackbarHostState: SnackbarHostState,
    tetherType: TetherType,
    scope: CoroutineScope,
    onChanged: () -> Unit = {},
) = object : TetheringManagerCompat.StartTetheringCallback, TetheringManagerCompat.StopTetheringCallback {
    override fun onTetheringStarted() = onChanged()

    override fun onStopTetheringSucceeded() = onChanged()

    override fun onTetheringFailed(error: Int?) {
        error?.let {
            if (Build.VERSION.SDK_INT >= 30 && it == TetheringManager.TETHER_ERROR_NO_CHANGE_TETHERING_PERMISSION) {
                scope.launch {
                    Toast.makeText(context, R.string.permission_missing, Toast.LENGTH_LONG).show()
                    ManageBar.start(context::startActivity)
                }
            } else scope.launch {
                snackbarHostState.showLongSnackbar(tetherErrorMessage(context, tetherType, it))
            }
        }
        onChanged()
    }

    override fun onStopTetheringFailed(error: Int) {
        if (error == TetheringManager.TETHER_ERROR_NO_CHANGE_TETHERING_PERMISSION) {
            scope.launch {
                Toast.makeText(context, R.string.permission_missing, Toast.LENGTH_LONG).show()
                ManageBar.start(context::startActivity)
            }
        } else scope.launch {
            snackbarHostState.showLongSnackbar(tetherErrorMessage(context, tetherType, error))
        }
        onChanged()
    }

    override fun onException(e: Exception) {
        super<TetheringManagerCompat.StartTetheringCallback>.onException(e)
        scope.launch {
            Toast.makeText(context, e.readableMessage, Toast.LENGTH_LONG).show()
            ManageBar.start(context::startActivity)
        }
    }
}

private fun tetherErrorMessage(context: Context, tetherType: TetherType, error: Int) = context.getString(
    R.string.tether_error_message,
    context.getString(tetherType.label),
    tetherErrorLabel(context, error),
)

@Composable
private fun rememberTetherTypeVersion(): State<Int> {
    val lifecycleOwner = LocalLifecycleOwner.current
    return if (Build.VERSION.SDK_INT < 30) remember { mutableIntStateOf(0) } else produceState(0, lifecycleOwner) {
        lifecycleOwner.lifecycle.repeatOnLifecycle(Lifecycle.State.STARTED) {
            TetherType.changes.collect { value++ }
        }
    }
}

@Composable
private fun rememberWifiSummary(
    baseError: AnnotatedString?,
    linkStyles: TextLinkStyles,
): State<AnnotatedString?> {
    val context = LocalContext.current
    val lifecycleOwner = LocalLifecycleOwner.current
    val locale = LocalConfiguration.current.locales[0]
    return produceState(baseError, context, lifecycleOwner, locale, baseError, linkStyles) {
        var wifiFailureReason: Int? = null
        var wifiNumClients: Int? = null
        var wifiInfo = emptyList<SoftApInfo>()
        var wifiCapability: SoftApCapability? = null
        fun update() {
            value = wifiSummary(
                context,
                locale,
                wifiFailureReason,
                wifiNumClients,
                wifiInfo,
                wifiCapability,
                baseError,
                linkStyles,
            )
        }
        val callback = object : `WifiManager$SoftApCallback` {
            override fun onStateChanged(state: Int, failureReason: Int) {
                if (!WifiApManager.checkWifiApState(state)) return
                wifiFailureReason = if (state == WifiApManager.WIFI_AP_STATE_FAILED) failureReason else null
                update()
            }

            override fun onNumClientsChanged(numClients: Int) {
                wifiNumClients = numClients
                update()
            }

            override fun onConnectedClientsChanged(clients: List<WifiClient>) {
                wifiNumClients = clients.size
                update()
            }

            override fun onInfoChanged(info: SoftApInfo) {
                wifiInfo = if (info.frequency == 0 && info.bandwidth ==
                    SoftApInfo.CHANNEL_WIDTH_INVALID) emptyList() else listOf(info)
                update()
            }

            override fun onInfoChanged(info: List<SoftApInfo>) {
                wifiInfo = info
                update()
            }

            override fun onCapabilityChanged(capability: SoftApCapability) {
                wifiCapability = capability
                update()
            }
        }
        var registered = false
        fun register() {
            if (!registered) {
                WifiApCommands.registerSoftApCallback(callback)
                registered = true
            }
        }
        fun unregister() {
            if (registered) {
                WifiApCommands.unregisterSoftApCallback(callback)
                registered = false
            }
        }
        val observer = object : DefaultLifecycleObserver {
            override fun onStart(owner: LifecycleOwner) = register()
            override fun onStop(owner: LifecycleOwner) = unregister()
        }
        lifecycleOwner.lifecycle.addObserver(observer)
        if (lifecycleOwner.lifecycle.currentState.isAtLeast(Lifecycle.State.STARTED)) register()
        awaitDispose {
            lifecycleOwner.lifecycle.removeObserver(observer)
            unregister()
        }
    }
}

private fun wifiSummary(
    context: Context,
    locale: Locale,
    failureReason: Int?,
    numClients: Int?,
    infos: List<SoftApInfo>,
    capability: SoftApCapability?,
    baseError: AnnotatedString?,
    linkStyles: TextLinkStyles,
): AnnotatedString? {
    val integerFormat = NumberFormat.getIntegerInstance(locale)
    val summary = buildAnnotatedString {
        fun line(content: AnnotatedString.Builder.() -> Unit) {
            if (length > 0) append('\n')
            content()
        }
        failureReason?.let { line { append(softApStartFailureLabel(context, it)) } }
        baseError?.takeIf { it.text.isNotEmpty() }?.let { line { append(it) } }
        for (info in infos) line {
            val frequency = info.frequency
            val channel = SoftApConfigurationCompat.frequencyToChannel(frequency)
            val bandwidth = channelBandwidthLabel(context, info.bandwidth)
            if (Build.VERSION.SDK_INT >= 31) {
                val bssid = info.bssid?.toString()
                val apInstanceIdentifier = info.apInstanceIdentifierOrNull
                val bssidAp = bssid?.let { apInstanceIdentifier?.let { id -> "$it%$id" } ?: it }
                    ?: apInstanceIdentifier ?: "?"
                val timeout = info.autoShutdownTimeoutMillis
                val line = context.getString(if (timeout == 0L) {
                    R.string.tethering_manage_wifi_info_timeout_disabled
                } else R.string.tethering_manage_wifi_info_timeout_enabled,
                    integerFormat.format(frequency.toLong()),
                    integerFormat.format(channel.toLong()),
                    bandwidth,
                    bssidAp,
                    integerFormat.format(info.wifiStandard.toLong()),
                    formatTimeoutMillis(context, timeout),
                )
                if (bssid == null) {
                    append(line)
                } else {
                    val bssidStart = line.indexOf(bssid)
                    if (bssidStart < 0) append(line) else {
                        append(line.substring(0, bssidStart))
                        appendMacAddress(bssid, linkStyles)
                        append(line.substring(bssidStart + bssid.length))
                    }
                }
            } else append(context.getString(
                    R.string.tethering_manage_wifi_info,
                    integerFormat.format(frequency.toLong()),
                    integerFormat.format(channel.toLong()),
                    bandwidth,
                ))
            try {
                info.mldAddress
            } catch (e: NoSuchMethodError) {
                if (Build.VERSION.SDK_INT >= 36) Timber.w(e)
                null
            }?.let {
                append(", MLD MAC ")
                appendMacAddress(it.toString(), linkStyles)
            }
        }
        capability?.takeIf { Build.VERSION.SDK_INT >= 30 }?.let {
            line { append(context.resources.getQuantityString(
                R.plurals.tethering_manage_wifi_client_limit,
                numClients ?: 0,
                numClients?.let { integerFormat.format(it.toLong()) } ?: "?",
                integerFormat.format(capability.maxSupportedClients.toLong()),
            )) }
        } ?: numClients?.let {
            line { append(context.resources.getQuantityString(
                R.plurals.tethering_manage_wifi_clients,
                it,
                integerFormat.format(it.toLong()),
            )) }
        }
    }
    return if (summary.text.isEmpty()) null else summary
}

private fun networkInterfaceLookup(): Map<String, NetworkInterface> {
    return try {
        NetworkInterface.getNetworkInterfaces()?.asSequence()?.associateBy { it.name } ?: emptyMap()
    } catch (e: Exception) {
        if (e is SocketException) Timber.d(e) else Timber.w(e)
        emptyMap()
    }
}

private fun networkInterfaceAddressesText(
    iface: NetworkInterface?,
    linkStyles: TextLinkStyles,
    macOnly: Boolean = false,
    macOverride: MacAddress? = null,
): AnnotatedString = buildAnnotatedString {
    var macAddress = macOverride
    if (macAddress == null && iface != null) try {
        val hardwareAddress = iface.hardwareAddress
        macAddress = try {
            hardwareAddress?.let(MacAddress::fromBytes)
        } catch (e: IllegalArgumentException) {
            try {
                hardwareAddress?.let { MacAddress.fromString(String(it)) }.also { Timber.d(e) }
            } catch (e2: IllegalArgumentException) {
                e.addSuppressed(e2)
                Timber.w(e)
                null
            }
        }
    } catch (_: SocketException) { }
    if (macAddress != null && macAddress != MacAddressCompat.ANY_ADDRESS) appendMacAddress(macAddress.toString(), linkStyles)
    if (!macOnly && iface != null) for (address in iface.interfaceAddresses) {
        if (length > 0) append('\n')
        appendIpAddress(address.address, linkStyles)
        address.networkPrefixLength.also {
            if (it.toInt() != address.address.address.size * 8) append("/$it")
        }
    }
}

private fun tetherError(context: Context, states: TetherStates, tetherType: TetherType): AnnotatedString? {
    val interested = states.errored.keys.filter { TetherType.ofInterface(it).isA(tetherType) }
    return if (interested.isEmpty()) null else AnnotatedString(interested.joinToString("\n") { iface ->
        "$iface: " + try {
            tetherErrorLabel(context, if (Build.VERSION.SDK_INT < 30) @Suppress("DEPRECATION") {
                TetheringManagerCompat.getLastTetherError(iface)
            } else states.errored[iface] ?: 0)
        } catch (e: InvocationTargetException) {
            if (e.cause !is SecurityException) Timber.w(e) else Timber.d(e)
            e.readableMessage
        }
    })
}

private fun repeaterSummary(
    context: Context,
    group: WifiP2pGroup?,
    ifaceLookup: Map<String, NetworkInterface>,
    linkStyles: TextLinkStyles,
): AnnotatedString? {
    group ?: return null
    val summary = buildAnnotatedString {
        val frequency = group.frequency
        if (frequency != 0) append(context.getString(
            R.string.repeater_frequency,
            NumberFormat.getIntegerInstance(context.resources.configuration.locales[0]).format(frequency.toLong()),
        ))
        networkInterfaceAddressesText(
            ifaceLookup[group.`interface`],
            linkStyles,
            macOverride = if (Build.VERSION.SDK_INT >= 30) try {
                (wifiP2pGroupInterfaceAddress[group] as ByteArray?)?.let(MacAddress::fromBytes)
            } catch (e: NoSuchFieldException) {
                if (Build.VERSION.SDK_INT >= 34) Timber.w(e)
                null
            } else null,
        ).let { addresses ->
            if (addresses.text.isNotEmpty()) {
                if (length > 0) append('\n')
                append(addresses)
            }
        }
        if (Build.VERSION.SDK_INT >= 35) {
            @Suppress("UNCHECKED_CAST")
            val data = VendorData.serialize(getWifiP2pGroupVendorData(group) as List<OuiKeyedData>)
            if (data.isNotEmpty()) {
                if (length > 0) append('\n')
                append(context.getString(R.string.tethering_manage_wifi_vendor_data, data))
            }
        }
    }
    return if (summary.text.isEmpty()) null else summary
}

private val wifiP2pGroupInterfaceAddress by lazy { WifiP2pGroup::class.java.getDeclaredField("interfaceAddress") }
private val getWifiP2pGroupVendorData by lazy { WifiP2pGroup::class.java.getDeclaredMethod("getVendorData") }
