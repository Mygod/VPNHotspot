package be.mygod.vpnhotspot.ui

import android.Manifest
import android.app.Service
import android.bluetooth.BluetoothManager
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.content.pm.PackageManager
import android.content.res.Configuration
import android.net.MacAddress
import android.net.TetheringManager
import android.net.wifi.p2p.WifiP2pGroup
import android.os.Build
import android.os.IBinder
import android.os.Parcelable
import android.provider.Settings
import android.text.format.DateUtils
import android.widget.Toast
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.annotation.DrawableRes
import androidx.annotation.StringRes
import androidx.collection.ScatterSet
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.State
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalInspectionMode
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.TextLinkStyles
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.tooling.preview.Preview
import androidx.compose.ui.unit.dp
import androidx.core.net.toUri
import androidx.core.content.ContextCompat
import androidx.core.content.getSystemService
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
import be.mygod.vpnhotspot.net.wifi.SoftApInfo
import be.mygod.vpnhotspot.net.wifi.WifiApManager
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper
import be.mygod.vpnhotspot.root.WifiApCommands
import be.mygod.vpnhotspot.ui.theme.VpnHotspotPreviewSurface
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.readableMessage
import be.mygod.vpnhotspot.util.stopAndUnbind
import kotlinx.coroutines.DelicateCoroutinesApi
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.launch
import timber.log.Timber
import java.lang.reflect.InvocationTargetException
import java.net.NetworkInterface
import java.net.SocketException
import java.text.NumberFormat
import java.util.Locale

@OptIn(DelicateCoroutinesApi::class)
@Composable
internal fun TetheringScreen(
    snackbarHostState: SnackbarHostState,
    repeaterBinder: RepeaterService.Binder?,
    localOnlyBinder: LocalOnlyHotspotService.Binder?,
    tetheringBinder: TetheringService.Binder?,
    tetherStates: TetherStates,
    onConfigureRepeater: () -> Unit,
    onConfigureTemporaryHotspot: (() -> Unit)?,
    onConfigureAp: () -> Unit,
) {
    val context = LocalContext.current
    val inspectionMode = LocalInspectionMode.current
    val linkStyles = rememberNetworkAddressLinkStyles()
    val repeaterMissingLocationPermissions = stringResource(R.string.repeater_missing_location_permissions)
    val managedIfaces by (tetheringBinder?.managedIfaces)?.collectAsStateWithLifecycle(null) ?: rememberNullState()
    val inactiveIfaces by (tetheringBinder?.inactiveIfaces)?.collectAsStateWithLifecycle(null) ?: rememberNullState()
    val monitoredIfaces by (tetheringBinder?.monitoredIfaces)?.collectAsStateWithLifecycle(null) ?: rememberNullState()
    val localOnlyIface by (localOnlyBinder?.iface)?.collectAsStateWithLifecycle(null) ?: rememberNullState()
    val repeaterStatus by (repeaterBinder?.status)?.collectAsStateWithLifecycle(null) ?: rememberNullState()
    val repeaterGroup by (repeaterBinder?.group)?.collectAsStateWithLifecycle(null) ?: rememberNullState()
    val staticIpActive by StaticIpSetter.active.collectAsStateWithLifecycle()
    val staticIpAddresses by StaticIpSetter.addresses.collectAsStateWithLifecycle()
    val staticIpApplying by StaticIpSetter.applying.collectAsStateWithLifecycle()
    var staticIpDraft by rememberSaveable { mutableStateOf<String?>(null) }
    var staticIpDraftText by rememberTextFieldValueAtEnd(staticIpDraft.orEmpty(), staticIpDraft != null)
    var wpsDialog by rememberSaveable { mutableStateOf(false) }
    var wpsPin by rememberTextFieldValueAtEnd("", wpsDialog)
    val tetherTypeVersion by if (inspectionMode) remember { mutableIntStateOf(0) } else rememberTetherTypeVersion()
    var manageBarVersion by remember { mutableIntStateOf(0) }
    val manageOffloadEnabled = if (inspectionMode) false else remember(manageBarVersion) { ManageBar.offloadEnabled }
    val ifaceLookup = remember(tetherStates, managedIfaces, inactiveIfaces, monitoredIfaces, localOnlyIface, repeaterGroup) {
        networkInterfaceLookup()
    }
    val monitored = monitoredIfaces.toSet()
    val managed = managedIfaces.toSet()
    val inactive = inactiveIfaces.toSet()
    val tetheredTypes = remember(tetherStates, tetherTypeVersion) {
        tetherStates.tethered.map { TetherType.ofInterface(it) }.toSet()
    }
    val wifiBaseError = tetherError(tetherStates, TetherType.WIFI)
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
    } else run {
        val startRepeaterLauncher = rememberLauncherForActivityResult(
            ActivityResultContracts.RequestPermission(),
            onResult = { granted ->
                if (granted) {
                    app.startServiceWithLocation<RepeaterService>(context)
                } else GlobalScope.launch(Dispatchers.Main.immediate) {
                    snackbarHostState.showLongSnackbar(repeaterMissingLocationPermissions)
                }
            },
        )
        val startRepeaterAction: (String) -> Unit = { permission -> startRepeaterLauncher.launch(permission) }
        startRepeaterAction
    }
    val startLocalOnly: (String) -> Unit = if (inspectionMode) {
        { _: String -> }
    } else run {
        val startLocalOnlyLauncher = rememberLauncherForActivityResult(
            ActivityResultContracts.RequestPermission(),
            onResult = { app.startServiceWithLocation<LocalOnlyHotspotService>(context) },
        )
        val startLocalOnlyAction: (String) -> Unit = { permission -> startLocalOnlyLauncher.launch(permission) }
        startLocalOnlyAction
    }
    val p2p = if (inspectionMode) null else Services.p2p
    val showRepeater = inspectionMode || p2p != null
    val showBluetooth = inspectionMode || (context.packageManager.hasSystemFeature(PackageManager.FEATURE_BLUETOOTH) &&
            bluetoothTethering != null)

    SettingsList {
        item {
            PreferenceGroup {
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
                    row {
                        TetheringRow(
                            icon = R.drawable.ic_action_settings_input_antenna,
                            title = repeaterTitle(repeaterGroup?.frequency),
                            summary = repeaterSummary(repeaterGroup, ifaceLookup, linkStyles),
                            checked = repeaterStatus == RepeaterService.Status.STARTING || active,
                            enabled = true,
                            switchEnabled = switchEnabled,
                            onClick = onConfigureRepeater,
                            onCheckedChange = toggleRepeater,
                        )
                    }
                    if ((repeaterStatus == RepeaterService.Status.STARTING ||
                            repeaterStatus == RepeaterService.Status.ACTIVE) &&
                        WifiP2pManagerHelper.startWps != null) {
                        row {
                            PreferenceRow(
                                modifier = Modifier.padding(start = 40.dp),
                                icon = R.drawable.ic_action_wifi_protected_setup,
                                title = stringResource(R.string.repeater_wps),
                                onClick = { if (repeaterBinder?.active == true) wpsDialog = true },
                            )
                        }
                    }
                }
                row {
                    val toggleLocalOnly: () -> Unit = {
                        if (localOnlyIface == null) startLocalOnly(localOnlyHotspotPermission) else localOnlyBinder?.stop()
                    }
                    TetheringRow(
                        icon = R.drawable.ic_action_perm_scan_wifi,
                        title = stringResource(R.string.tethering_temp_hotspot),
                        summary = networkInterfaceAddressesText(ifaceLookup[localOnlyIface], linkStyles),
                        checked = localOnlyIface != null,
                        enabled = true,
                        onClick = onConfigureTemporaryHotspot ?: toggleLocalOnly,
                        onCheckedChange = onConfigureTemporaryHotspot?.let { { toggleLocalOnly() } },
                    )
                }
                row {
                    TetheringRow(
                        icon = R.drawable.ic_content_push_pin,
                        title = stringResource(R.string.tethering_static_ip),
                        summary = buildAnnotatedString {
                            for ((address, prefixLength) in staticIpAddresses) {
                                if (length > 0) append('\n')
                                appendIpAddress(address, linkStyles)
                                if (prefixLength.toInt() != address.address.size * 8) append("/$prefixLength")
                            }
                        },
                        checked = staticIpActive,
                        enabled = true,
                        switchEnabled = !staticIpApplying,
                        onClick = {
                            staticIpDraft = StaticIpSetter.ips
                        },
                        onCheckedChange = { StaticIpSetter.enable(!staticIpActive) },
                    )
                }
                for (iface in (tetherStates.tethered + monitored).toSortedSet()) {
                    row(key = iface) {
                        val active = managed.contains(iface)
                        val title = if (monitored.contains(iface)) {
                            stringResource(R.string.tethering_state_monitored, iface)
                        } else iface
                        TetheringRow(
                            icon = TetherType.ofInterface(iface).icon,
                            title = title,
                            summary = networkInterfaceAddressesText(
                                ifaceLookup[iface],
                                linkStyles,
                                macOnly = inactive.contains(iface),
                            ),
                            checked = active,
                            enabled = true,
                            onClick = {
                                if (active) context.startService(Intent(context, TetheringService::class.java)
                                    .putExtra(TetheringService.EXTRA_REMOVE_INTERFACE, iface))
                                else context.startForegroundService(Intent(context, TetheringService::class.java)
                                    .putExtra(TetheringService.EXTRA_ADD_INTERFACES, arrayOf(iface)))
                            },
                        )
                    }
                }
            }
        }
        item {
            PreferenceGroup {
                row {
                    PreferenceRow(
                        icon = R.drawable.ic_content_add,
                        iconTint = MaterialTheme.colorScheme.secondary,
                        title = stringResource(R.string.tethering_manage),
                        summary = if (manageOffloadEnabled) {
                            stringResource(R.string.tethering_manage_offload_enabled)
                        } else null,
                        onClick = { ManageBar.start(context::startActivity) },
                    )
                }
                row {
                    TetheringTypeRow(
                        icon = R.drawable.ic_device_network_wifi,
                        title = R.string.tethering_manage_wifi,
                        checked = tetheredTypes.contains(TetherType.WIFI),
                        summary = wifiSummary,
                        tetheringType = TetheringManager.TETHERING_WIFI,
                        snackbarHostState = snackbarHostState,
                        onConfigure = onConfigureAp,
                    )
                }
                row {
                    TetheringTypeRow(
                        icon = R.drawable.ic_device_usb,
                        title = R.string.tethering_manage_usb,
                        checked = tetheredTypes.contains(TetherType.USB) || tetheredTypes.contains(TetherType.NCM),
                        summary = tetherError(tetherStates, TetherType.USB),
                        tetheringType = TetheringManagerCompat.TETHERING_USB,
                        snackbarHostState = snackbarHostState,
                    )
                }
                if (showBluetooth) {
                    bluetoothVersion
                    val active = bluetoothTethering?.active
                    row {
                        BluetoothTetheringRow(
                            active = active,
                            summary = buildAnnotatedString {
                                if (active == null) bluetoothTethering?.activeFailureCause?.readableMessage?.let {
                                    append(it)
                                }
                                tetherError(tetherStates, TetherType.BLUETOOTH)?.let {
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
                if (Build.VERSION.SDK_INT >= 30) {
                    row {
                        TetheringTypeRow(
                            icon = TetherType.ETHERNET.icon,
                            title = R.string.tethering_manage_ethernet,
                            checked = tetheredTypes.contains(TetherType.ETHERNET),
                            summary = tetherError(tetherStates, TetherType.ETHERNET),
                            tetheringType = TetheringManagerCompat.TETHERING_ETHERNET,
                            snackbarHostState = snackbarHostState,
                        )
                    }
                }
            }
        }
    }

    if (staticIpDraft != null) {
        val focusRequester = remember { FocusRequester() }
        val keyboard = LocalSoftwareKeyboardController.current
        LaunchedEffect(Unit) {
            focusRequester.requestFocus()
            keyboard?.show()
        }
        AlertDialog(
            onDismissRequest = {
                staticIpDraft = null
            },
            title = { Text(stringResource(R.string.tethering_static_ip)) },
            text = {
                Column(verticalArrangement = Arrangement.spacedBy(16.dp)) {
                    Text(
                        text = stringResource(R.string.tethering_static_ip_help),
                        style = MaterialTheme.typography.bodyMedium,
                    )
                    OutlinedTextField(
                        value = staticIpDraftText,
                        onValueChange = { staticIpDraftText = it },
                        modifier = Modifier
                            .fillMaxWidth()
                            .focusRequester(focusRequester),
                        keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri),
                        minLines = 2,
                    )
                }
            },
            confirmButton = {
                TextButton(onClick = {
                    StaticIpSetter.ips = staticIpDraftText.text.trim()
                    staticIpDraft = null
                }) {
                    Text(stringResource(android.R.string.ok))
                }
            },
            dismissButton = {
                TextButton(onClick = {
                    staticIpDraft = null
                }) {
                    Text(stringResource(android.R.string.cancel))
                }
            },
        )
    }
    if (wpsDialog) {
        val focusRequester = remember { FocusRequester() }
        val keyboard = LocalSoftwareKeyboardController.current
        LaunchedEffect(Unit) {
            focusRequester.requestFocus()
            keyboard?.show()
        }
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
                TextButton(onClick = {
                    repeaterBinder?.startWps(wpsPin.text)
                    wpsDialog = false
                }) {
                    Text(stringResource(android.R.string.ok))
                }
            },
            dismissButton = {
                TextButton(onClick = {
                    repeaterBinder?.startWps(null)
                    wpsDialog = false
                }) {
                    Text(stringResource(R.string.repeater_wps_dialog_pbc))
                }
                TextButton(onClick = { wpsDialog = false }) {
                    Text(stringResource(android.R.string.cancel))
                }
            },
        )
    }
}

private val localOnlyHotspotPermission = if (Build.VERSION.SDK_INT >= 33) {
    Manifest.permission.NEARBY_WIFI_DEVICES
} else Manifest.permission.ACCESS_FINE_LOCATION

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
    val toggle = toggle@{
        if (!Settings.System.canWrite(context)) try {
            context.startActivity(Intent(Settings.ACTION_MANAGE_WRITE_SETTINGS, "package:${context.packageName}".toUri()))
            return@toggle
        } catch (e: RuntimeException) {
            app.logEvent("manage_write_settings") { param("message", e.toString()) }
        }
        val callback = tetheringCallback(context, snackbarHostState, TetherType.fromTetheringType(tetheringType))
        if (checked) TetheringManagerCompat.stopTethering(tetheringType, callback)
        else TetheringManagerCompat.startTethering(tetheringType, true, callback)
    }
    TetheringRow(
        icon = icon,
        title = stringResource(title),
        summary = summary ?: AnnotatedString(""),
        checked = checked,
        enabled = true,
        onClick = onConfigure ?: toggle,
        onCheckedChange = onConfigure?.let { { toggle() } },
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
    TetheringRow(
        icon = R.drawable.ic_device_bluetooth,
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
            val callback = tetheringCallback(context, snackbarHostState, TetherType.BLUETOOTH, onRefresh)
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
    summary: AnnotatedString = AnnotatedString(""),
    checked: Boolean,
    enabled: Boolean,
    switchEnabled: Boolean = enabled,
    onClick: () -> Unit,
    onCheckedChange: (() -> Unit)? = null,
) {
    PreferenceRow(
        icon = icon,
        title = title,
        summaryContent = if (summary.text.isEmpty()) null else {
            {
                RowSelectionContainer {
                    Text(summary)
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
@Composable
private fun TetheringPreview() = TetheringPreviewContent()

@Preview(
    name = "Tethering - dark",
    showBackground = true,
    widthDp = 420,
    heightDp = 720,
    uiMode = Configuration.UI_MODE_NIGHT_YES,
)
@Composable
private fun TetheringDarkPreview() = TetheringPreviewContent()

@Composable
private fun TetheringPreviewContent() {
    VpnHotspotPreviewSurface {
        TetheringScreen(
            snackbarHostState = remember { SnackbarHostState() },
            repeaterBinder = null,
            localOnlyBinder = null,
            tetheringBinder = null,
            tetherStates = TetherStates(),
            onConfigureRepeater = {},
            onConfigureTemporaryHotspot = null,
            onConfigureAp = {},
        )
    }
}

@OptIn(DelicateCoroutinesApi::class)
private fun tetheringCallback(
    context: Context,
    snackbarHostState: SnackbarHostState,
    tetherType: TetherType,
    onChanged: () -> Unit = {},
) = object : TetheringManagerCompat.StartTetheringCallback, TetheringManagerCompat.StopTetheringCallback {
    override fun onTetheringStarted() = onChanged()

    override fun onStopTetheringSucceeded() = onChanged()

    override fun onTetheringFailed(error: Int?) {
        error?.let {
            if (Build.VERSION.SDK_INT >= 30 && it == TetheringManager.TETHER_ERROR_NO_CHANGE_TETHERING_PERMISSION) {
                GlobalScope.launch(Dispatchers.Main.immediate) {
                    Toast.makeText(context, R.string.permission_missing, Toast.LENGTH_LONG).show()
                    ManageBar.start(context::startActivity)
                }
            } else GlobalScope.launch(Dispatchers.Main.immediate) {
                snackbarHostState.showLongSnackbar("$tetherType: ${TetheringManagerCompat.tetherErrorLookup(it)}")
            }
        }
        onChanged()
    }

    override fun onStopTetheringFailed(error: Int) {
        if (error == TetheringManager.TETHER_ERROR_NO_CHANGE_TETHERING_PERMISSION) {
            GlobalScope.launch(Dispatchers.Main.immediate) {
                Toast.makeText(context, R.string.permission_missing, Toast.LENGTH_LONG).show()
                ManageBar.start(context::startActivity)
            }
        } else GlobalScope.launch(Dispatchers.Main.immediate) {
            snackbarHostState.showLongSnackbar("$tetherType: ${TetheringManagerCompat.tetherErrorLookup(error)}")
        }
        onChanged()
    }

    override fun onException(e: Exception) {
        super<TetheringManagerCompat.StartTetheringCallback>.onException(e)
        GlobalScope.launch(Dispatchers.Main.immediate) {
            Toast.makeText(context, e.readableMessage, Toast.LENGTH_LONG).show()
            ManageBar.start(context::startActivity)
        }
    }
}

@Composable
internal fun rememberTetherStates(): State<TetherStates> {
    val lifecycleOwner = LocalLifecycleOwner.current
    return produceState(TetherStates(), lifecycleOwner) {
        val callback = object : TetherStates.Callback {
            override fun onTetherStatesChanged(states: TetherStates) {
                value = states
            }
        }
        var registered = false
        fun register() {
            if (!registered) {
                TetherStates.registerCallback(callback)
                registered = true
            }
        }
        fun unregister() {
            if (registered) {
                TetherStates.unregisterCallback(callback)
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

@Composable
internal fun <T : IBinder> rememberServiceBinder(clazz: Class<out Service>): State<T?> {
    val context = LocalContext.current
    val lifecycleOwner = LocalLifecycleOwner.current
    return produceState<T?>(null, context, lifecycleOwner, clazz) {
        var bound = false
        val connection = object : ServiceConnection {
            override fun onServiceConnected(name: ComponentName?, service: IBinder?) {
                @Suppress("UNCHECKED_CAST")
                value = service as? T
            }

            override fun onServiceDisconnected(name: ComponentName?) {
                value = null
            }
        }
        fun bind() {
            if (!bound) bound = context.bindService(Intent(context, clazz), connection, Context.BIND_AUTO_CREATE)
        }
        fun unbind() {
            if (bound) {
                context.stopAndUnbind(connection)
                bound = false
            }
        }
        val observer = object : DefaultLifecycleObserver {
            override fun onStart(owner: LifecycleOwner) = bind()
            override fun onStop(owner: LifecycleOwner) = unbind()
        }
        lifecycleOwner.lifecycle.addObserver(observer)
        if (lifecycleOwner.lifecycle.currentState.isAtLeast(Lifecycle.State.STARTED)) bind()
        awaitDispose {
            lifecycleOwner.lifecycle.removeObserver(observer)
            unbind()
        }
    }
}

@Composable
internal fun <T> rememberNullState(): State<T?> = remember { mutableStateOf(null) }

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
        var wifiInfo = emptyList<Parcelable>()
        fun update() {
            value = wifiSummary(
                context,
                locale,
                wifiFailureReason,
                wifiNumClients,
                wifiInfo,
                baseError,
                linkStyles,
            )
        }
        val callback = object : WifiApManager.SoftApCallbackCompat {
            override fun onStateChanged(state: Int, failureReason: Int) {
                if (!WifiApManager.checkWifiApState(state)) return
                wifiFailureReason = if (state == WifiApManager.WIFI_AP_STATE_FAILED) failureReason else null
                update()
            }

            override fun onNumClientsChanged(numClients: Int) {
                wifiNumClients = numClients
                update()
            }

            override fun onInfoChanged(info: List<Parcelable>) {
                wifiInfo = info
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
    info: List<Parcelable>,
    baseError: AnnotatedString?,
    linkStyles: TextLinkStyles,
): AnnotatedString? {
    val integerFormat = NumberFormat.getIntegerInstance(locale)
    val summary = buildAnnotatedString {
        fun line(content: AnnotatedString.Builder.() -> Unit) {
            if (length > 0) append('\n')
            content()
        }
        failureReason?.let { line { append(WifiApManager.failureReasonLookup(it)) } }
        baseError?.takeIf { it.text.isNotEmpty() }?.let { line { append(it) } }
        for (parcel in info) line {
            val softApInfo = SoftApInfo(parcel)
            val frequency = softApInfo.frequency
            val channel = SoftApConfigurationCompat.frequencyToChannel(frequency)
            val bandwidth = SoftApInfo.channelWidthLookup(softApInfo.bandwidth, true)
            if (Build.VERSION.SDK_INT >= 31) {
                val bssid = softApInfo.bssid?.toString()
                val bssidAp = bssid?.let { softApInfo.apInstanceIdentifier?.let { id -> "$it%$id" } ?: it }
                    ?: softApInfo.apInstanceIdentifier ?: "?"
                val timeout = softApInfo.autoShutdownTimeoutMillis
                val line = context.getString(if (timeout == 0L) {
                    R.string.tethering_manage_wifi_info_timeout_disabled
                } else R.string.tethering_manage_wifi_info_timeout_enabled,
                    integerFormat.format(frequency.toLong()),
                    integerFormat.format(channel.toLong()),
                    bandwidth,
                    bssidAp,
                    integerFormat.format(softApInfo.wifiStandard.toLong()),
                    DateUtils.formatElapsedTime(timeout / 1000),
                )
                val bssidText = bssid
                if (bssidText == null) {
                    append(line)
                } else {
                    val bssidStart = line.indexOf(bssidText)
                    if (bssidStart < 0) append(line) else {
                        append(line.substring(0, bssidStart))
                        appendMacAddress(bssidText, linkStyles)
                        append(line.substring(bssidStart + bssidText.length))
                    }
                }
            } else append(context.getString(
                    R.string.tethering_manage_wifi_info,
                    integerFormat.format(frequency.toLong()),
                    integerFormat.format(channel.toLong()),
                    bandwidth,
                ))
            softApInfo.mldAddress?.let {
                append(", MLD MAC ")
                appendMacAddress(it.toString(), linkStyles)
            }
        }
        numClients?.let {
            line { append(context.resources.getQuantityString(
                R.plurals.tethering_manage_wifi_clients,
                it,
                integerFormat.format(it.toLong()),
            )) }
        }
    }
    return if (summary.text.isEmpty()) null else summary
}

private fun ScatterSet<String>?.toSet(): Set<String> = buildSet {
    this@toSet?.forEach { add(it) }
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

private fun tetherError(states: TetherStates, tetherType: TetherType): AnnotatedString? {
    val interested = states.errored.keys.filter { TetherType.ofInterface(it).isA(tetherType) }
    return if (interested.isEmpty()) null else AnnotatedString(interested.joinToString("\n") { iface ->
        "$iface: " + try {
            TetheringManagerCompat.tetherErrorLookup(if (Build.VERSION.SDK_INT < 30) @Suppress("DEPRECATION") {
                TetheringManagerCompat.getLastTetherError(iface)
            } else states.errored[iface] ?: 0)
        } catch (e: InvocationTargetException) {
            if (e.cause !is SecurityException) Timber.w(e) else Timber.d(e)
            e.readableMessage
        }
    })
}

@Composable
private fun repeaterTitle(frequency: Int?): String {
    return if (frequency != null && frequency != 0) {
        stringResource(R.string.repeater_channel, frequency, SoftApConfigurationCompat.frequencyToChannel(frequency))
    } else stringResource(R.string.title_repeater)
}

@Composable
private fun repeaterSummary(
    group: WifiP2pGroup?,
    ifaceLookup: Map<String, NetworkInterface>,
    linkStyles: TextLinkStyles,
): AnnotatedString {
    val addresses = group?.let { p2pGroup ->
        networkInterfaceAddressesText(
            ifaceLookup[p2pGroup.`interface`],
            linkStyles,
            macOverride = if (Build.VERSION.SDK_INT >= 30) try {
                (wifiP2pGroupInterfaceAddress[p2pGroup] as ByteArray?)?.let(MacAddress::fromBytes)
            } catch (e: NoSuchFieldException) {
                if (Build.VERSION.SDK_INT >= 34) Timber.w(e)
                null
            } else null,
        )
    } ?: AnnotatedString("")
    return addresses
}

private val wifiP2pGroupInterfaceAddress by lazy { WifiP2pGroup::class.java.getDeclaredField("interfaceAddress") }
