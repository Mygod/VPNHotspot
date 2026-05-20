package be.mygod.vpnhotspot.ui

import android.content.Intent
import androidx.annotation.DrawableRes
import androidx.annotation.StringRes
import androidx.collection.ScatterSet
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Badge
import androidx.compose.material3.BadgedBox
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.navigation.NavDestination.Companion.hierarchy
import androidx.navigation.NavGraph.Companion.findStartDestination
import androidx.navigation.compose.NavHost
import androidx.navigation.compose.composable
import androidx.navigation.compose.currentBackStackEntryAsState
import androidx.navigation.compose.rememberNavController
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import be.mygod.vpnhotspot.LocalOnlyHotspotService
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.RepeaterService
import be.mygod.vpnhotspot.TetheringService
import be.mygod.vpnhotspot.client.ClientViewModel
import be.mygod.vpnhotspot.net.TetherStates
import be.mygod.vpnhotspot.net.wifi.SoftApConfigurationCompat
import be.mygod.vpnhotspot.util.Services
import kotlinx.coroutines.launch

private enum class RootDestination(
    val route: String,
    @param:StringRes val title: Int,
    @param:DrawableRes val icon: Int,
) {
    Tethering("tethering", R.string.title_tethering, R.drawable.ic_device_wifi_tethering),
    Clients("clients", R.string.title_clients, R.drawable.ic_device_devices),
    Settings("settings", R.string.title_settings, R.drawable.ic_action_settings),
}

private enum class AppDestination(val route: String, @param:StringRes val title: Int) {
    ApConfiguration("ap_configuration", R.string.configuration_view),
    RepeaterConfiguration("repeater_configuration", R.string.configuration_view),
    TemporaryHotspotConfiguration("temporary_hotspot_configuration", R.string.configuration_view),
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun VpnHotspotApp(clientViewModel: ClientViewModel, validClientCount: Int) {
    val appContext = LocalContext.current
    val navController = rememberNavController()
    val snackbarHostState = remember { SnackbarHostState() }
    SmartSnackbarBridge(snackbarHostState)
    val scope = rememberCoroutineScope()
    var apSession by remember { mutableStateOf<ApConfigurationSession?>(null) }
    val apState = remember(apSession) {
        apSession?.let { ApConfigurationState(it.initial, it.readOnly, it.p2pMode) }
    }
    val backStackEntry by navController.currentBackStackEntryAsState()
    val route = backStackEntry?.destination?.route
    val rootDestination = RootDestination.entries.firstOrNull { it.route == route }
    val appDestination = AppDestination.entries.firstOrNull { it.route == route }
    val repeaterBinder by if (rootDestination == RootDestination.Tethering) {
        rememberServiceBinder<RepeaterService.Binder>(RepeaterService::class.java)
    } else rememberNullState()
    val localOnlyBinder by if (rootDestination == RootDestination.Tethering) {
        rememberServiceBinder<LocalOnlyHotspotService.Binder>(LocalOnlyHotspotService::class.java)
    } else rememberNullState()
    val tetheringBinder by if (rootDestination == RootDestination.Tethering) {
        rememberServiceBinder<TetheringService.Binder>(TetheringService::class.java)
    } else rememberNullState()
    val tetherStates by if (rootDestination == RootDestination.Tethering) {
        rememberTetherStates()
    } else remember { mutableStateOf(TetherStates()) }
    val monitoredIfaces by (tetheringBinder?.monitoredIfaces)?.collectAsStateWithLifecycle(null) ?: rememberNullState()
    val temporaryHotspotConfiguration by (localOnlyBinder?.configuration)?.collectAsStateWithLifecycle(null)
        ?: rememberNullState()
    val monitorableIfaces = remember(tetherStates, monitoredIfaces) {
        (tetherStates.tethered - monitoredIfaces.toStringSet()).sorted()
    }
    val title = appDestination?.title ?: R.string.app_name
    Scaffold(
        topBar = {
            TopAppBar(
                title = { Text(stringResource(title)) },
                navigationIcon = {
                    if (appDestination != null) IconButton(onClick = { navController.popBackStack() }) {
                        NavIcon(R.drawable.ic_navigation_arrow_back, androidx.appcompat.R.string.abc_action_bar_up_description)
                    }
                },
                actions = {
                    if (rootDestination == RootDestination.Tethering) TetheringActions(
                        monitorableIfaces = monitorableIfaces,
                        onMonitorInterface = { iface ->
                            appContext.startForegroundService(Intent(appContext, TetheringService::class.java)
                                .putExtra(TetheringService.EXTRA_ADD_INTERFACE_MONITOR, iface))
                        },
                        onConfigureRepeater = {
                            scope.launch {
                                loadRepeaterApConfiguration(repeaterBinder, snackbarHostState)?.let { session ->
                                    apSession = session
                                    navController.navigate(AppDestination.RepeaterConfiguration.route)
                                }
                            }
                        },
                        onConfigureTemporaryHotspot = temporaryHotspotConfiguration?.let { configuration ->
                            {
                                apSession = ApConfigurationSession(
                                    initial = configuration,
                                    readOnly = true,
                                    onApply = { false },
                                )
                                navController.navigate(AppDestination.TemporaryHotspotConfiguration.route)
                            }
                        },
                        onConfigureAp = {
                            scope.launch {
                                loadSystemApConfiguration(snackbarHostState)?.let { configuration ->
                                    apSession = ApConfigurationSession(configuration) { config ->
                                        applySystemApConfiguration(config, snackbarHostState)
                                    }
                                    navController.navigate(AppDestination.ApConfiguration.route)
                                }
                            }
                        },
                    )
                    if (appDestination != null && apState != null && apSession != null) {
                        ApConfigurationTopBarActions(
                            state = apState,
                            session = apSession!!,
                            snackbarHostState = snackbarHostState,
                            onApplied = { navController.popBackStack() },
                        )
                    }
                },
            )
        },
        bottomBar = {
            if (rootDestination != null) NavigationBar {
                for (destination in RootDestination.entries) {
                    val selected = backStackEntry?.destination?.hierarchy?.any { it.route == destination.route } == true
                    NavigationBarItem(
                        selected = selected,
                        onClick = {
                            navController.navigate(destination.route) {
                                popUpTo(navController.graph.findStartDestination().id) {
                                    saveState = true
                                }
                                launchSingleTop = true
                                restoreState = true
                            }
                        },
                        icon = {
                            if (destination == RootDestination.Clients && validClientCount > 0) {
                                BadgedBox(badge = { Badge { Text(validClientCount.toString()) } }) {
                                    NavIcon(destination.icon, destination.title)
                                }
                            } else NavIcon(destination.icon, destination.title)
                        },
                        label = { Text(stringResource(destination.title)) },
                    )
                }
            }
        },
        snackbarHost = { SnackbarHost(snackbarHostState) },
    ) { contentPadding ->
        NavHost(
            navController = navController,
            startDestination = RootDestination.Tethering.route,
            modifier = Modifier.padding(contentPadding),
        ) {
            composable(RootDestination.Tethering.route) { TetheringScreen(snackbarHostState) }
            composable(RootDestination.Clients.route) { ClientsScreen(clientViewModel, snackbarHostState) }
            composable(RootDestination.Settings.route) { SettingsScreen(snackbarHostState) }
            composable(AppDestination.ApConfiguration.route) {
                ApConfigurationScreen(apState ?: ApConfigurationState(SoftApConfigurationCompat(), false, false))
            }
            composable(AppDestination.RepeaterConfiguration.route) {
                ApConfigurationScreen(apState ?: ApConfigurationState(SoftApConfigurationCompat(), false, true))
            }
            composable(AppDestination.TemporaryHotspotConfiguration.route) {
                ApConfigurationScreen(apState ?: ApConfigurationState(SoftApConfigurationCompat(), true, false))
            }
        }
    }
}

@Composable
private fun NavIcon(@DrawableRes icon: Int, @StringRes description: Int) {
    Icon(
        painter = painterResource(icon),
        contentDescription = stringResource(description),
    )
}

@Composable
private fun TetheringActions(
    monitorableIfaces: List<String>,
    onMonitorInterface: (String) -> Unit,
    onConfigureRepeater: () -> Unit,
    onConfigureTemporaryHotspot: (() -> Unit)?,
    onConfigureAp: () -> Unit,
) {
    var monitorExpanded by remember { mutableStateOf(false) }
    var expanded by remember { mutableStateOf(false) }
    if (monitorableIfaces.isNotEmpty()) {
        IconButton(onClick = { monitorExpanded = true }) {
            NavIcon(R.drawable.ic_image_remove_red_eye, R.string.tethering_monitor)
        }
        DropdownMenu(expanded = monitorExpanded, onDismissRequest = { monitorExpanded = false }) {
            for (iface in monitorableIfaces) DropdownMenuItem(
                text = { Text(iface) },
                onClick = {
                    monitorExpanded = false
                    onMonitorInterface(iface)
                },
            )
        }
    }
    IconButton(onClick = { expanded = true }) {
        NavIcon(R.drawable.ic_device_wifi_lock, R.string.configuration_view)
    }
    DropdownMenu(expanded = expanded, onDismissRequest = { expanded = false }) {
        if (Services.p2p != null) DropdownMenuItem(
            text = { Text(stringResource(R.string.title_repeater)) },
            onClick = {
                expanded = false
                onConfigureRepeater()
            },
        )
        if (onConfigureTemporaryHotspot != null) DropdownMenuItem(
            text = { Text(stringResource(R.string.tethering_temp_hotspot)) },
            onClick = {
                expanded = false
                onConfigureTemporaryHotspot()
            },
        )
        DropdownMenuItem(
            text = { Text(stringResource(R.string.tethering_manage_wifi)) },
            onClick = {
                expanded = false
                onConfigureAp()
            },
        )
    }
}

private fun ScatterSet<String>?.toStringSet(): Set<String> = buildSet {
    this@toStringSet?.forEach { add(it) }
}
