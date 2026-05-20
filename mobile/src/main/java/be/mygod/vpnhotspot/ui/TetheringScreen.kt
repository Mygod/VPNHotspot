package be.mygod.vpnhotspot.ui

import android.Manifest
import android.app.Service
import android.bluetooth.BluetoothManager
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.content.pm.PackageManager
import android.net.TetheringManager
import android.net.wifi.SoftApConfiguration
import android.os.Build
import android.os.IBinder
import android.os.Parcelable
import android.provider.Settings
import android.text.TextUtils
import android.text.format.DateUtils
import android.widget.Toast
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.contract.ActivityResultContracts
import androidx.annotation.DrawableRes
import androidx.annotation.StringRes
import androidx.collection.ScatterSet
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.ListItem
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.State
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.unit.dp
import androidx.core.net.toUri
import androidx.core.content.ContextCompat
import androidx.core.content.getSystemService
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.compose.collectAsStateWithLifecycle
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
import be.mygod.vpnhotspot.util.makeMacSpan
import be.mygod.vpnhotspot.util.readableMessage
import be.mygod.vpnhotspot.util.stopAndUnbind
import kotlinx.coroutines.launch
import timber.log.Timber
import java.lang.reflect.InvocationTargetException
import java.net.NetworkInterface
import java.net.SocketException
import java.text.NumberFormat
import java.util.Locale

@Composable
internal fun TetheringScreen(snackbarHostState: SnackbarHostState) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    val repeaterBinder by rememberServiceBinder<RepeaterService.Binder>(RepeaterService::class.java)
    val localOnlyBinder by rememberServiceBinder<LocalOnlyHotspotService.Binder>(LocalOnlyHotspotService::class.java)
    val tetheringBinder by rememberServiceBinder<TetheringService.Binder>(TetheringService::class.java)
    val tetherStates by rememberTetherStates()
    val managedIfaces by (tetheringBinder?.managedIfaces)?.collectAsStateWithLifecycle(null) ?: rememberNullState()
    val inactiveIfaces by (tetheringBinder?.inactiveIfaces)?.collectAsStateWithLifecycle(null) ?: rememberNullState()
    val monitoredIfaces by (tetheringBinder?.monitoredIfaces)?.collectAsStateWithLifecycle(null) ?: rememberNullState()
    val localOnlyIface by (localOnlyBinder?.iface)?.collectAsStateWithLifecycle(null) ?: rememberNullState()
    val repeaterStatus by (repeaterBinder?.status)?.collectAsStateWithLifecycle(null) ?: rememberNullState()
    val repeaterGroup by (repeaterBinder?.group)?.collectAsStateWithLifecycle(null) ?: rememberNullState()
    val staticIpActive by StaticIpSetter.active.collectAsStateWithLifecycle()
    val staticIpAddresses by StaticIpSetter.addresses.collectAsStateWithLifecycle()
    val staticIpApplying by StaticIpSetter.applying.collectAsStateWithLifecycle()
    var staticIpDialog by remember { mutableStateOf(false) }
    var staticIpDraft by remember(staticIpDialog) { mutableStateOf(StaticIpSetter.ips) }
    var wpsDialog by remember { mutableStateOf(false) }
    var wpsPin by remember(wpsDialog) { mutableStateOf("") }
    val tetherTypeVersion by rememberTetherTypeVersion()
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
        if (granted) bluetoothTethering?.ensureInit(context)
    }
    val lifecycleOwner = LocalLifecycleOwner.current
    DisposableEffect(lifecycleOwner, bluetoothTethering) {
        val observer = object : DefaultLifecycleObserver {
            override fun onResume(owner: LifecycleOwner) {
                if (bluetoothTethering == null || Build.VERSION.SDK_INT < 31) return
                if (ContextCompat.checkSelfPermission(context, Manifest.permission.BLUETOOTH_CONNECT) ==
                    PackageManager.PERMISSION_GRANTED) {
                    bluetoothTethering.ensureInit(context)
                } else requestBluetooth.launch(Manifest.permission.BLUETOOTH_CONNECT)
            }
        }
        lifecycleOwner.lifecycle.addObserver(observer)
        onDispose {
            lifecycleOwner.lifecycle.removeObserver(observer)
            bluetoothTethering?.close()
        }
    }
    val startRepeater = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
        if (granted) app.startServiceWithLocation<RepeaterService>(context) else scope.launch {
            snackbarHostState.showSnackbar(context.getString(R.string.repeater_missing_location_permissions))
        }
    }
    val startLocalOnly = rememberLauncherForActivityResult(ActivityResultContracts.RequestPermission()) {
        app.startServiceWithLocation<LocalOnlyHotspotService>(context)
    }

    SettingsList {
        if (Services.p2p != null) {
            item {
                val active = repeaterStatus == RepeaterService.Status.ACTIVE
                val switchEnabled = repeaterStatus == RepeaterService.Status.IDLE || active
                TetheringRow(
                    icon = R.drawable.ic_action_settings_input_antenna,
                    title = repeaterTitle(repeaterGroup?.frequency),
                    summary = repeaterSummary(repeaterGroup?.`interface`, ifaceLookup),
                    checked = repeaterStatus == RepeaterService.Status.STARTING || active,
                    enabled = switchEnabled,
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
            if (repeaterBinder?.active == true && WifiP2pManagerHelper.startWps != null) item {
                PreferenceRow(
                    icon = R.drawable.ic_action_wifi_protected_setup,
                    title = stringResource(R.string.repeater_wps),
                    onClick = { wpsDialog = true },
                )
            }
        }
        item {
            TetheringRow(
                icon = R.drawable.ic_action_perm_scan_wifi,
                title = stringResource(R.string.tethering_temp_hotspot),
                summary = ifaceLookup[localOnlyIface]?.formatAddresses()?.toString().orEmpty(),
                checked = localOnlyIface != null,
                enabled = true,
                onClick = {
                    if (localOnlyIface == null) startLocalOnly.launch(localOnlyHotspotPermission)
                    else localOnlyBinder?.stop()
                },
            )
        }
        item {
            TetheringRow(
                icon = R.drawable.ic_content_push_pin,
                title = stringResource(R.string.tethering_static_ip),
                summary = staticIpAddresses.toString(),
                checked = staticIpActive,
                enabled = !staticIpApplying,
                onClick = { staticIpDialog = true },
                onCheckedChange = { StaticIpSetter.enable(!staticIpActive) },
            )
        }
        for (iface in (tetherStates.tethered + monitored).toSortedSet()) item(key = "iface:$iface") {
            val active = managed.contains(iface)
            val title = if (monitored.contains(iface)) {
                stringResource(R.string.tethering_state_monitored, iface)
            } else iface
            TetheringRow(
                icon = TetherType.ofInterface(iface).icon,
                title = title,
                summary = ifaceLookup[iface]?.formatAddresses(inactive.contains(iface))?.toString().orEmpty(),
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
        item {
            PreferenceRow(
                icon = R.drawable.ic_content_add,
                title = stringResource(R.string.tethering_manage),
                summary = if (ManageBar.offloadEnabled) {
                    stringResource(R.string.tethering_manage_offload_enabled)
                } else null,
                onClick = { ManageBar.start(context::startActivity) },
            )
        }
        item {
            TetheringTypeRow(
                icon = R.drawable.ic_device_network_wifi,
                title = R.string.tethering_manage_wifi,
                checked = tetheredTypes.contains(TetherType.WIFI),
                summary = wifiSummary,
                tetheringType = TetheringManager.TETHERING_WIFI,
                snackbarHostState = snackbarHostState,
            )
        }
        item {
            TetheringTypeRow(
                icon = R.drawable.ic_device_usb,
                title = R.string.tethering_manage_usb,
                checked = tetheredTypes.contains(TetherType.USB) || tetheredTypes.contains(TetherType.NCM),
                summary = tetherError(tetherStates, TetherType.USB),
                tetheringType = TetheringManagerCompat.TETHERING_USB,
                snackbarHostState = snackbarHostState,
            )
        }
        if (context.packageManager.hasSystemFeature(PackageManager.FEATURE_BLUETOOTH) && bluetoothTethering != null) item {
            bluetoothVersion
            val active = bluetoothTethering.active
            BluetoothTetheringRow(
                active = active,
                summary = listOfNotNull(
                    if (active == null) bluetoothTethering.activeFailureCause?.readableMessage else null,
                    tetherError(tetherStates, TetherType.BLUETOOTH),
                ).joinToString("\n"),
                bluetoothTethering = bluetoothTethering,
                snackbarHostState = snackbarHostState,
            )
        }
        if (Build.VERSION.SDK_INT >= 30) item {
            TetheringTypeRow(
                icon = R.drawable.ic_action_settings_ethernet,
                title = R.string.tethering_manage_ethernet,
                checked = tetheredTypes.contains(TetherType.ETHERNET),
                summary = tetherError(tetherStates, TetherType.ETHERNET),
                tetheringType = TetheringManagerCompat.TETHERING_ETHERNET,
                snackbarHostState = snackbarHostState,
            )
        }
    }

    if (staticIpDialog) AlertDialog(
        onDismissRequest = { staticIpDialog = false },
        title = { Text(stringResource(R.string.tethering_static_ip)) },
        text = {
            OutlinedTextField(
                value = staticIpDraft,
                onValueChange = { staticIpDraft = it },
                modifier = Modifier.fillMaxWidth(),
                keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri),
                minLines = 2,
            )
        },
        confirmButton = {
            TextButton(onClick = {
                StaticIpSetter.ips = staticIpDraft.trim()
                staticIpDialog = false
            }) {
                Text(stringResource(android.R.string.ok))
            }
        },
        dismissButton = {
            TextButton(onClick = { staticIpDialog = false }) {
                Text(stringResource(android.R.string.cancel))
            }
        },
    )
    if (wpsDialog) AlertDialog(
        onDismissRequest = { wpsDialog = false },
        title = { Text(stringResource(R.string.repeater_wps_dialog_title)) },
        text = {
            OutlinedTextField(
                value = wpsPin,
                onValueChange = { wpsPin = it },
                modifier = Modifier.fillMaxWidth(),
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

private val localOnlyHotspotPermission = if (Build.VERSION.SDK_INT >= 33) {
    Manifest.permission.NEARBY_WIFI_DEVICES
} else Manifest.permission.ACCESS_FINE_LOCATION

@Composable
private fun TetheringTypeRow(
    @DrawableRes icon: Int,
    @StringRes title: Int,
    checked: Boolean,
    summary: String?,
    tetheringType: Int,
    snackbarHostState: SnackbarHostState,
) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    TetheringRow(
        icon = icon,
        title = stringResource(title),
        summary = summary.orEmpty(),
        checked = checked,
        enabled = true,
        indent = true,
        onClick = {
            if (!Settings.System.canWrite(context)) try {
                context.startActivity(Intent(Settings.ACTION_MANAGE_WRITE_SETTINGS, "package:${context.packageName}".toUri()))
                return@TetheringRow
            } catch (e: RuntimeException) {
                app.logEvent("manage_write_settings") { param("message", e.toString()) }
            }
            val callback = tetheringCallback(context, snackbarHostState, scope)
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
) {
    val context = LocalContext.current
    val scope = rememberCoroutineScope()
    TetheringRow(
        icon = R.drawable.ic_device_bluetooth,
        title = stringResource(R.string.tethering_manage_bluetooth),
        summary = summary,
        checked = active == true,
        enabled = bluetoothTethering != null,
        indent = true,
        onClick = {
            if (!Settings.System.canWrite(context)) try {
                context.startActivity(Intent(Settings.ACTION_MANAGE_WRITE_SETTINGS, "package:${context.packageName}".toUri()))
                return@TetheringRow
            } catch (e: RuntimeException) {
                app.logEvent("manage_write_settings") { param("message", e.toString()) }
            }
            val callback = tetheringCallback(context, snackbarHostState, scope)
            when (active) {
                true -> bluetoothTethering?.stop(callback)
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
    summary: String = "",
    checked: Boolean,
    enabled: Boolean,
    onClick: () -> Unit,
    onCheckedChange: (() -> Unit)? = null,
    indent: Boolean = false,
) {
    ListItem(
        headlineContent = { Text(title) },
        supportingContent = if (summary.isEmpty()) null else {
            {
                RowSelectionContainer {
                    Text(summary)
                }
            }
        },
        leadingContent = {
            Icon(
                painter = painterResource(icon),
                contentDescription = null,
                modifier = Modifier.size(24.dp),
            )
        },
        trailingContent = {
            Switch(
                checked = checked,
                enabled = enabled,
                onCheckedChange = if (onCheckedChange == null) null else { _: Boolean -> onCheckedChange() },
            )
        },
        modifier = Modifier
            .padding(start = if (indent) 40.dp else 0.dp)
            .fillMaxWidth()
            .clickable(enabled = enabled, onClick = onClick),
    )
    HorizontalDivider()
}

private fun tetheringCallback(
    context: Context,
    snackbarHostState: SnackbarHostState,
    scope: kotlinx.coroutines.CoroutineScope,
) = object : TetheringManagerCompat.StartTetheringCallback, TetheringManagerCompat.StopTetheringCallback {
    override fun onTetheringFailed(error: Int?) {
        error?.let {
            if (Build.VERSION.SDK_INT >= 30 && it == TetheringManager.TETHER_ERROR_NO_CHANGE_TETHERING_PERMISSION) {
                scope.launch {
                    Toast.makeText(context, R.string.permission_missing, Toast.LENGTH_LONG).show()
                    ManageBar.start(context::startActivity)
                }
            } else scope.launch { snackbarHostState.showSnackbar(TetheringManagerCompat.tetherErrorLookup(it)) }
        }
    }

    override fun onStopTetheringFailed(error: Int) {
        if (Build.VERSION.SDK_INT >= 30 && error == TetheringManager.TETHER_ERROR_NO_CHANGE_TETHERING_PERMISSION) {
            scope.launch {
                Toast.makeText(context, R.string.permission_missing, Toast.LENGTH_LONG).show()
                ManageBar.start(context::startActivity)
            }
        } else scope.launch { snackbarHostState.showSnackbar(TetheringManagerCompat.tetherErrorLookup(error)) }
    }

    override fun onException(e: Exception) {
        super<TetheringManagerCompat.StartTetheringCallback>.onException(e)
        scope.launch {
            Toast.makeText(context, e.readableMessage, Toast.LENGTH_LONG).show()
            ManageBar.start(context::startActivity)
        }
    }
}

@Composable
internal fun rememberTetherStates(): State<TetherStates> {
    return produceState(TetherStates()) {
        val callback = object : TetherStates.Callback {
            override fun onTetherStatesChanged(states: TetherStates) {
                value = states
            }
        }
        TetherStates.registerCallback(callback)
        awaitDispose { TetherStates.unregisterCallback(callback) }
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
        val observer = object : DefaultLifecycleObserver {
            override fun onStart(owner: LifecycleOwner) {
                bound = context.bindService(Intent(context, clazz), connection, Context.BIND_AUTO_CREATE)
            }

            override fun onStop(owner: LifecycleOwner) {
                if (bound) {
                    context.stopAndUnbind(connection)
                    bound = false
                }
            }
        }
        lifecycleOwner.lifecycle.addObserver(observer)
        awaitDispose {
            lifecycleOwner.lifecycle.removeObserver(observer)
            if (bound) context.stopAndUnbind(connection)
        }
    }
}

@Composable
internal fun <T> rememberNullState(): State<T?> = remember { mutableStateOf(null) }

@Composable
private fun rememberTetherTypeVersion(): State<Int> {
    return if (Build.VERSION.SDK_INT < 30) remember { mutableIntStateOf(0) } else produceState(0) {
        TetherType.changes.collect { value++ }
    }
}

@Composable
private fun rememberWifiSummary(baseError: String?): State<String?> {
    val context = LocalContext.current
    val locale = context.resources.configuration.locales[0]
    return produceState(baseError, context, locale, baseError) {
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
        WifiApCommands.registerSoftApCallback(callback)
        awaitDispose { WifiApCommands.unregisterSoftApCallback(callback) }
    }
}

private fun wifiSummary(
    context: Context,
    locale: Locale,
    failureReason: Int?,
    numClients: Int?,
    info: List<Parcelable>,
    capability: Parcelable?,
    baseError: String?,
): String? {
    val integerFormat = NumberFormat.getIntegerInstance(locale)
    return listOfNotNull(
        failureReason?.let { WifiApManager.failureReasonLookup(it) },
        baseError,
        if (info.isEmpty()) null else info.joinToString("\n") { parcel ->
            val softApInfo = SoftApInfo(parcel)
            val frequency = softApInfo.frequency
            val channel = SoftApConfigurationCompat.frequencyToChannel(frequency)
            val bandwidth = SoftApInfo.channelWidthLookup(softApInfo.bandwidth, true)
            buildString {
                append(if (Build.VERSION.SDK_INT >= 31) {
                    val bssid = softApInfo.bssid?.let { makeMacSpan(it.toString()).toString() }
                    val bssidAp = softApInfo.apInstanceIdentifier?.let {
                        if (bssid == null) it else "$bssid%$it"
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
                softApInfo.mldAddress?.let { append(", MLD MAC ").append(makeMacSpan(it.toString()).toString()) }
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
            buildString {
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
                    }.joinToString().ifEmpty { context.getText(R.string.tethering_manage_wifi_no_features) },
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
            ).toString()
        },
    ).joinToString("\n").ifEmpty { null }
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

private fun tetherError(states: TetherStates, tetherType: TetherType): String? {
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

private fun repeaterSummary(iface: String?, ifaceLookup: Map<String, NetworkInterface>): String {
    return ifaceLookup[iface]?.formatAddresses()?.toString().orEmpty()
}
