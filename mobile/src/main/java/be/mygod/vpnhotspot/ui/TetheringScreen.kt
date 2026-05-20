package be.mygod.vpnhotspot.ui

import android.Manifest
import android.app.Service
import android.bluetooth.BluetoothManager
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.content.pm.PackageManager
import android.net.MacAddress
import android.net.TetheringManager
import android.net.wifi.SoftApConfiguration
import android.net.wifi.p2p.WifiP2pGroup
import android.net.wifi.p2p.WifiP2pManager
import android.os.Build
import android.os.IBinder
import android.os.Parcelable
import android.provider.Settings
import android.text.SpannableStringBuilder
import android.text.TextUtils
import android.text.format.DateUtils
import android.widget.Toast
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.annotation.DrawableRes
import androidx.annotation.StringRes
import androidx.collection.ScatterSet
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Switch
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
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.input.KeyboardType
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
import be.mygod.vpnhotspot.net.TetherStates
import be.mygod.vpnhotspot.net.TetherType
import be.mygod.vpnhotspot.net.TetheringManagerCompat
import be.mygod.vpnhotspot.net.wifi.SoftApCapability
import be.mygod.vpnhotspot.net.wifi.SoftApConfigurationCompat
import be.mygod.vpnhotspot.net.wifi.SoftApInfo
import be.mygod.vpnhotspot.net.wifi.WifiApManager
import be.mygod.vpnhotspot.net.wifi.WifiP2pManagerHelper
import be.mygod.vpnhotspot.root.WifiApCommands
import be.mygod.vpnhotspot.util.RangeInput
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.formatAddresses
import be.mygod.vpnhotspot.util.joinToSpanned
import be.mygod.vpnhotspot.util.makeMacSpan
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
) {
    val context = LocalContext.current
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
    var wpsDialog by rememberSaveable { mutableStateOf(false) }
    var wpsPin by rememberSaveable(wpsDialog) { mutableStateOf("") }
    val tetherTypeVersion by rememberTetherTypeVersion()
    var manageBarVersion by remember { mutableIntStateOf(0) }
    val manageOffloadEnabled = remember(manageBarVersion) { ManageBar.offloadEnabled }
    val ifaceLookup = remember(tetherStates, managedIfaces, inactiveIfaces, monitoredIfaces, localOnlyIface, repeaterGroup) {
        networkInterfaceLookup()
    }
    val monitored = monitoredIfaces.toSet()
    val managed = managedIfaces.toSet()
    val inactive = inactiveIfaces.toSet()
    val tetheredTypes = remember(tetherStates, tetherTypeVersion) {
        tetherStates.tethered.map { TetherType.ofInterface(it) }.toSet()
    }
    val wifiSummary by rememberWifiSummary(tetherError(tetherStates, TetherType.WIFI))
    var bluetoothVersion by remember { mutableIntStateOf(0) }
    val bluetoothAdapter = remember {
        context.getSystemService<BluetoothManager>()?.adapter
    }
    val bluetoothTethering = remember(bluetoothAdapter) {
        bluetoothAdapter?.let { BluetoothTethering(context, it) { bluetoothVersion++ } }
    }
    val requestBluetooth = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
        if (granted) {
            bluetoothTethering?.ensureInit(context)
            bluetoothVersion++
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
            } else requestBluetooth.launch(Manifest.permission.BLUETOOTH_CONNECT)
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
    val startRepeater = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
        if (granted) app.startServiceWithLocation<RepeaterService>(context) else GlobalScope.launch(Dispatchers.Main.immediate) {
            snackbarHostState.showLongSnackbar(repeaterMissingLocationPermissions)
        }
    }
    val startLocalOnly = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) {
        app.startServiceWithLocation<LocalOnlyHotspotService>(context)
    }

    SettingsList {
        item {
            PreferenceGroup {
                if (Services.p2p != null) {
                    val active = repeaterStatus == RepeaterService.Status.ACTIVE
                    val switchEnabled = repeaterStatus == RepeaterService.Status.IDLE || active
                    row {
                        TetheringRow(
                            icon = R.drawable.ic_action_settings_input_antenna,
                            title = repeaterTitle(repeaterGroup?.frequency),
                            summary = repeaterSummary(repeaterGroup, ifaceLookup),
                            checked = repeaterStatus == RepeaterService.Status.STARTING || active,
                            enabled = true,
                            switchEnabled = switchEnabled,
                            onClick = {
                                when (repeaterStatus) {
                                    RepeaterService.Status.IDLE -> startRepeater.launch(if (Build.VERSION.SDK_INT >= 33) {
                                        Manifest.permission.NEARBY_WIFI_DEVICES
                                    } else Manifest.permission.ACCESS_FINE_LOCATION)
                                    RepeaterService.Status.ACTIVE -> repeaterBinder?.shutdown()
                                    else -> { }
                                }
                            },
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
                    TetheringRow(
                        icon = R.drawable.ic_action_perm_scan_wifi,
                        title = stringResource(R.string.tethering_temp_hotspot),
                        summary = ifaceLookup[localOnlyIface]?.formatAddresses() ?: "",
                        checked = localOnlyIface != null,
                        enabled = true,
                        onClick = {
                            if (localOnlyIface == null) startLocalOnly.launch(localOnlyHotspotPermission)
                            else localOnlyBinder?.stop()
                        },
                    )
                }
                row {
                    TetheringRow(
                        icon = R.drawable.ic_content_push_pin,
                        title = stringResource(R.string.tethering_static_ip),
                        summary = staticIpAddresses,
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
                    androidx.compose.runtime.key(iface) {
                        row {
                            val active = managed.contains(iface)
                            val title = if (monitored.contains(iface)) {
                                stringResource(R.string.tethering_state_monitored, iface)
                            } else iface
                            TetheringRow(
                                icon = TetherType.ofInterface(iface).icon,
                                title = title,
                                summary = ifaceLookup[iface]?.formatAddresses(inactive.contains(iface)) ?: "",
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
                if (context.packageManager.hasSystemFeature(PackageManager.FEATURE_BLUETOOTH) &&
                    bluetoothTethering != null) {
                    bluetoothVersion
                    val active = bluetoothTethering.active
                    row {
                        BluetoothTetheringRow(
                            active = active,
                            summary = listOfNotNull(
                                if (active == null) bluetoothTethering.activeFailureCause?.readableMessage else null,
                                tetherError(tetherStates, TetherType.BLUETOOTH),
                            ).joinToString("\n"),
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

    staticIpDraft?.let { draft ->
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
                OutlinedTextField(
                    value = draft,
                    onValueChange = { staticIpDraft = it },
                    modifier = Modifier
                        .fillMaxWidth()
                        .focusRequester(focusRequester),
                    keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri),
                    minLines = 2,
                )
            },
            confirmButton = {
                TextButton(onClick = {
                    StaticIpSetter.ips = draft.trim()
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
                    repeaterBinder?.startWps(wpsPin)
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
    summary: CharSequence?,
    tetheringType: Int,
    snackbarHostState: SnackbarHostState,
) {
    val context = LocalContext.current
    TetheringRow(
        icon = icon,
        title = stringResource(title),
        summary = summary ?: "",
        checked = checked,
        enabled = true,
        onClick = {
            if (!Settings.System.canWrite(context)) try {
                context.startActivity(Intent(Settings.ACTION_MANAGE_WRITE_SETTINGS, "package:${context.packageName}".toUri()))
                return@TetheringRow
            } catch (e: RuntimeException) {
                app.logEvent("manage_write_settings") { param("message", e.toString()) }
            }
            val callback = tetheringCallback(context, snackbarHostState, TetherType.fromTetheringType(tetheringType))
            if (checked) TetheringManagerCompat.stopTethering(tetheringType, callback)
            else TetheringManagerCompat.startTethering(tetheringType, true, callback)
        },
    )
}

@Composable
private fun BluetoothTetheringRow(
    active: Boolean?,
    summary: String,
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
    summary: CharSequence = "",
    checked: Boolean,
    enabled: Boolean,
    switchEnabled: Boolean = enabled,
    onClick: () -> Unit,
    onCheckedChange: (() -> Unit)? = null,
) {
    PreferenceRow(
        icon = icon,
        title = title,
        summaryContent = if (summary.isEmpty()) null else {
            {
                RowSelectionContainer {
                    LinkedText(summary)
                }
            }
        },
        enabled = enabled,
        trailing = {
            Switch(
                checked = checked,
                enabled = switchEnabled,
                onCheckedChange = if (onCheckedChange == null) null else { _: Boolean -> onCheckedChange() },
            )
        },
        onClick = onClick,
    )
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
private fun rememberWifiSummary(baseError: CharSequence?): State<CharSequence?> {
    val context = LocalContext.current
    val lifecycleOwner = LocalLifecycleOwner.current
    val locale = LocalConfiguration.current.locales[0]
    return produceState(baseError, context, lifecycleOwner, locale, baseError) {
        var wifiFailureReason: Int? = null
        var wifiNumClients: Int? = null
        var wifiInfo = emptyList<Parcelable>()
        var wifiCapability: Parcelable? = null
        fun update() {
            value = wifiSummary(context, locale, wifiFailureReason, wifiNumClients, wifiInfo, wifiCapability, baseError)
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

            override fun onCapabilityChanged(capability: Parcelable) {
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
    info: List<Parcelable>,
    capability: Parcelable?,
    baseError: CharSequence?,
): CharSequence? {
    val integerFormat = NumberFormat.getIntegerInstance(locale)
    val summary = listOfNotNull<CharSequence>(
        failureReason?.let { WifiApManager.failureReasonLookup(it) },
        baseError,
        if (info.isEmpty()) null else info.joinToSpanned("\n") { parcel ->
            val softApInfo = SoftApInfo(parcel)
            val frequency = softApInfo.frequency
            val channel = SoftApConfigurationCompat.frequencyToChannel(frequency)
            val bandwidth = SoftApInfo.channelWidthLookup(softApInfo.bandwidth, true)
            SpannableStringBuilder().apply {
                append(if (Build.VERSION.SDK_INT >= 31) {
                    val bssid = softApInfo.bssid?.let { makeMacSpan(it.toString()) }
                    val bssidAp = softApInfo.apInstanceIdentifier?.let {
                        when (bssid) {
                            null -> it
                            is String -> "$bssid%$it"
                            else -> SpannableStringBuilder(bssid).append("%$it")
                        }
                    } ?: bssid ?: "?"
                    val timeout = softApInfo.autoShutdownTimeoutMillis
                    TextUtils.expandTemplate(context.getText(if (timeout == 0L) {
                        R.string.tethering_manage_wifi_info_timeout_disabled
                    } else R.string.tethering_manage_wifi_info_timeout_enabled),
                        integerFormat.format(frequency.toLong()),
                        integerFormat.format(channel.toLong()),
                        bandwidth,
                        bssidAp,
                        integerFormat.format(softApInfo.wifiStandard.toLong()),
                        DateUtils.formatElapsedTime(timeout / 1000),
                    )
                } else TextUtils.expandTemplate(
                    context.getText(R.string.tethering_manage_wifi_info),
                    integerFormat.format(frequency.toLong()),
                    integerFormat.format(channel.toLong()),
                    bandwidth,
                ))
                softApInfo.mldAddress?.let { append(", MLD MAC ").append(makeMacSpan(it.toString())) }
            }
        },
        capability?.let { parcel ->
            val capability = SoftApCapability(parcel)
            val maxClients = capability.maxSupportedClients
            var features = capability.supportedFeatures
            if (Build.VERSION.SDK_INT >= 31) for ((flag, band) in arrayOf(
                SoftApCapability.SOFTAP_FEATURE_BAND_24G_SUPPORTED to SoftApConfiguration.BAND_2GHZ,
                SoftApCapability.SOFTAP_FEATURE_BAND_5G_SUPPORTED to SoftApConfiguration.BAND_5GHZ,
                SoftApCapability.SOFTAP_FEATURE_BAND_6G_SUPPORTED to SoftApConfiguration.BAND_6GHZ,
                SoftApCapability.SOFTAP_FEATURE_BAND_60G_SUPPORTED to SoftApConfiguration.BAND_60GHZ,
            )) {
                if (capability.getSupportedChannelList(band).isEmpty()) continue
                features = features and flag.inv()
            }
            SpannableStringBuilder().apply {
                append(TextUtils.expandTemplate(
                    context.resources.getQuantityText(R.plurals.tethering_manage_wifi_capabilities, numClients ?: 0),
                    numClients?.let { integerFormat.format(it.toLong()) } ?: "?",
                    integerFormat.format(maxClients.toLong()),
                    sequence {
                        if (WifiApManager.isApMacRandomizationSupported) yield(context.getText(
                            R.string.tethering_manage_wifi_feature_ap_mac_randomization))
                        if (Services.wifi.isStaApConcurrencySupported) yield(context.getText(
                            R.string.tethering_manage_wifi_feature_sta_ap_concurrency))
                        if (Build.VERSION.SDK_INT >= 31) {
                            if (Services.wifi.isBridgedApConcurrencySupported) yield(context.getText(
                                R.string.tethering_manage_wifi_feature_bridged_ap_concurrency))
                            if (Services.wifi.isStaBridgedApConcurrencySupported) yield(context.getText(
                                R.string.tethering_manage_wifi_feature_sta_bridged_ap_concurrency))
                        }
                        if (features != 0L) while (features != 0L) {
                            val bit = features.takeLowestOneBit()
                            yield(SoftApCapability.featureLookup(bit, true).replace('_', ' '))
                            features = features and bit.inv()
                        }
                    }.joinToSpanned().ifEmpty { context.getText(R.string.tethering_manage_wifi_no_features) },
                ))
                if (Build.VERSION.SDK_INT >= 31) {
                    val channels = buildList {
                        for (band in SoftApConfigurationCompat.BAND_TYPES) {
                            val list = capability.getSupportedChannelList(band)
                            if (list.isNotEmpty()) {
                                add("${SoftApConfigurationCompat.bandLookup(band, true)} (${RangeInput.toString(list)})")
                            }
                        }
                    }
                    if (channels.isNotEmpty()) append(TextUtils.expandTemplate(
                        context.getText(R.string.tethering_manage_wifi_supported_channels),
                        channels.joinToString("; "),
                    ))
                    capability.countryCode?.let {
                        append(TextUtils.expandTemplate(context.getText(R.string.tethering_manage_wifi_country_code), it))
                    }
                }
            }
        } ?: numClients?.let {
            TextUtils.expandTemplate(
                context.resources.getQuantityText(R.plurals.tethering_manage_wifi_clients, it),
                integerFormat.format(it.toLong()),
            )
        },
    ).joinToSpanned("\n")
    return if (summary.isEmpty()) null else summary
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

private fun tetherError(states: TetherStates, tetherType: TetherType): CharSequence? {
    val interested = states.errored.keys.filter { TetherType.ofInterface(it).isA(tetherType) }
    return if (interested.isEmpty()) null else interested.joinToString("\n") { iface ->
        "$iface: " + try {
            TetheringManagerCompat.tetherErrorLookup(if (Build.VERSION.SDK_INT < 30) @Suppress("DEPRECATION") {
                TetheringManagerCompat.getLastTetherError(iface)
            } else states.errored[iface] ?: 0)
        } catch (e: InvocationTargetException) {
            if (e.cause !is SecurityException) Timber.w(e) else Timber.d(e)
            e.readableMessage
        }
    }
}

@Composable
private fun repeaterTitle(frequency: Int?): String {
    return if (frequency != null && frequency != 0) {
        stringResource(R.string.repeater_channel, frequency, SoftApConfigurationCompat.frequencyToChannel(frequency))
    } else stringResource(R.string.title_repeater)
}

@Composable
private fun repeaterSummary(group: WifiP2pGroup?, ifaceLookup: Map<String, NetworkInterface>): CharSequence {
    val result = SpannableStringBuilder()
    val features = stringResource(R.string.repeater_features)
    fun WifiP2pManager.test(label: String, sdk: Int, action: (WifiP2pManager) -> Boolean) {
        try {
            if (!action(this)) return
            if (result.isEmpty()) result.append(features) else result.append(", ")
            result.append(label)
        } catch (e: NoSuchMethodError) {
            if (Build.VERSION.SDK_INT >= sdk) Timber.w(e)
        }
    }
    if (Build.VERSION.SDK_INT >= 30) Services.p2p?.apply {
        test(stringResource(R.string.repeater_feature_set_vendor_elements), 33) { isSetVendorElementsSupported }
        test(stringResource(R.string.repeater_feature_group_client_removal), 33) { isGroupClientRemovalSupported }
        test(stringResource(R.string.repeater_feature_pcc_mode), 36) { isPccModeSupported }
        test(stringResource(R.string.repeater_feature_wifi_direct_r2), 36) { isWiFiDirectR2Supported }
    }
    val addresses = group?.let { p2pGroup ->
        ifaceLookup[p2pGroup.`interface`]?.formatAddresses(macOverride = if (Build.VERSION.SDK_INT >= 30) try {
            (wifiP2pGroupInterfaceAddress[p2pGroup] as ByteArray?)?.let(MacAddress::fromBytes)
        } catch (e: NoSuchFieldException) {
            if (Build.VERSION.SDK_INT >= 34) Timber.w(e)
            null
        } else null)
    } ?: ""
    if (addresses.isNotEmpty()) {
        if (result.isNotEmpty()) result.appendLine()
        result.append(addresses)
    }
    return result
}

private val wifiP2pGroupInterfaceAddress by lazy { WifiP2pGroup::class.java.getDeclaredField("interfaceAddress") }
