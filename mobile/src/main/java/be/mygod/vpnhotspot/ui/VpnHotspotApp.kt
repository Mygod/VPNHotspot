package be.mygod.vpnhotspot.ui

import android.app.Activity
import android.content.Intent
import android.os.Parcelable
import androidx.activity.compose.BackHandler
import androidx.annotation.DrawableRes
import androidx.annotation.StringRes
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.WindowInsetsSides
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.only
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawing
import androidx.compose.material3.Badge
import androidx.compose.material3.BadgedBox
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.ExperimentalMaterial3ExpressiveApi
import androidx.compose.material3.Icon
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.NavigationBar
import androidx.compose.material3.NavigationBarItem
import androidx.compose.material3.NavigationRail
import androidx.compose.material3.NavigationRailItem
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Snackbar
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.SwipeToDismissBox
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.adaptive.ExperimentalMaterial3AdaptiveApi
import androidx.compose.material3.adaptive.currentWindowAdaptiveInfo
import androidx.compose.material3.rememberSwipeToDismissBoxState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.lifecycle.ViewModel
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.viewmodel.compose.viewModel
import androidx.navigation.NavDestination.Companion.hierarchy
import androidx.navigation.NavGraph.Companion.findStartDestination
import androidx.navigation.NavHostController
import androidx.navigation.compose.NavHost
import androidx.navigation.compose.composable
import androidx.navigation.compose.currentBackStackEntryAsState
import androidx.navigation.compose.rememberNavController
import androidx.window.core.layout.WindowSizeClass.Companion.WIDTH_DP_MEDIUM_LOWER_BOUND
import be.mygod.vpnhotspot.LocalOnlyHotspotService
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.RepeaterService
import be.mygod.vpnhotspot.TetheringService
import be.mygod.vpnhotspot.client.ClientViewModel
import be.mygod.vpnhotspot.net.wifi.P2pSupplicantConfiguration
import be.mygod.vpnhotspot.net.wifi.SoftApConfigurationCompat
import be.mygod.vpnhotspot.ui.apconfiguration.ApConfigurationScreen
import be.mygod.vpnhotspot.ui.apconfiguration.ApConfigurationSession
import be.mygod.vpnhotspot.ui.apconfiguration.ApConfigurationState
import be.mygod.vpnhotspot.ui.apconfiguration.ApConfigurationTarget
import be.mygod.vpnhotspot.ui.apconfiguration.ApConfigurationTopBarActions
import be.mygod.vpnhotspot.ui.apconfiguration.applyRepeaterApConfiguration
import be.mygod.vpnhotspot.ui.apconfiguration.applySystemApConfiguration
import be.mygod.vpnhotspot.ui.apconfiguration.loadRepeaterApConfiguration
import be.mygod.vpnhotspot.ui.apconfiguration.loadSystemApConfiguration
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.launch
import kotlinx.parcelize.Parcelize

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

@Parcelize
private data class SavedApConfigurationSession(
    val initial: SoftApConfigurationCompat,
    val readOnly: Boolean,
    val p2pMode: Boolean,
    val target: ApConfigurationTarget,
) : Parcelable {
    fun toSession(
        repeaterBinder: RepeaterService.Binder?,
        repeaterMaster: P2pSupplicantConfiguration?,
        snackbarHostState: SnackbarHostState,
    ) = ApConfigurationSession(initial, readOnly, p2pMode, target) { config ->
        when (target) {
            ApConfigurationTarget.System -> applySystemApConfiguration(config, snackbarHostState)
            ApConfigurationTarget.Repeater ->
                applyRepeaterApConfiguration(repeaterBinder, config, snackbarHostState, repeaterMaster)
            ApConfigurationTarget.Temporary -> false
        }
    }
}

internal class ApConfigurationSessionHolder : ViewModel() {
    var repeaterMaster: P2pSupplicantConfiguration? = null
}

@OptIn(
    ExperimentalMaterial3AdaptiveApi::class,
    ExperimentalMaterial3Api::class,
    ExperimentalMaterial3ExpressiveApi::class,
)
@Composable
fun VpnHotspotApp(clientViewModel: ClientViewModel, validClientCount: Int) {
    val appContext = LocalContext.current
    val navController = rememberNavController()
    val snackbarHostState = remember { SnackbarHostState() }
    SmartSnackbarBridge(snackbarHostState)
    val scope = rememberCoroutineScope()
    val apSessionHolder = viewModel<ApConfigurationSessionHolder>()
    var savedApSession by rememberSaveable { mutableStateOf<SavedApConfigurationSession?>(null) }
    var apConfigurationLoading by remember { mutableStateOf(false) }
    val backStackEntry by navController.currentBackStackEntryAsState()
    val route = backStackEntry?.destination?.route
    val rootDestination = RootDestination.entries.firstOrNull { it.route == route }
    val visibleEntries by navController.visibleEntries.collectAsStateWithLifecycle()
    val appDestinationVisible = visibleEntries.any { entry ->
        entry.destination.hierarchy.any { destination ->
            AppDestination.entries.any { it.route == destination.route }
        }
    }
    val tetheringDestinationVisible = rootDestination == RootDestination.Tethering || visibleEntries.any { entry ->
        entry.destination.hierarchy.any { it.route == RootDestination.Tethering.route }
    }
    val bindRepeaterService = tetheringDestinationVisible ||
            savedApSession?.target == ApConfigurationTarget.Repeater
    val repeaterBinder by if (bindRepeaterService) {
        rememberServiceBinder<RepeaterService.Binder>(RepeaterService::class.java)
    } else rememberNullState()
    val localOnlyBinder by if (tetheringDestinationVisible) {
        rememberServiceBinder<LocalOnlyHotspotService.Binder>(LocalOnlyHotspotService::class.java)
    } else rememberNullState()
    val tetheringBinder by if (tetheringDestinationVisible) {
        rememberServiceBinder<TetheringService.Binder>(TetheringService::class.java)
    } else rememberNullState()
    val tetherStates by clientViewModel.tetherStates.collectAsStateWithLifecycle()
    val tetheringServiceState by produceState(TetheringServiceState(), tetheringBinder) {
        val binder = tetheringBinder
        if (binder == null) {
            value = TetheringServiceState()
            return@produceState
        }
        value = TetheringServiceState(
            managedIfaces = binder.managedIfaces.value.toSet(),
            inactiveIfaces = binder.inactiveIfaces.value.toSet(),
            monitoredIfaces = binder.monitoredIfaces.value.toSet(),
        )
        coroutineScope {
            launch {
                binder.managedIfaces.collect {
                    value = value.copy(managedIfaces = it.toSet())
                }
            }
            launch {
                binder.inactiveIfaces.collect {
                    value = value.copy(inactiveIfaces = it.toSet())
                }
            }
            launch {
                binder.monitoredIfaces.collect {
                    value = value.copy(monitoredIfaces = it.toSet())
                }
            }
        }
    }
    val temporaryHotspotConfiguration by (localOnlyBinder?.configuration)?.collectAsStateWithLifecycle(null)
        ?: rememberNullState()
    val monitorableIfaces = remember(tetherStates, tetheringServiceState.monitoredIfaces) {
        (tetherStates.tethered - tetheringServiceState.monitoredIfaces).sorted()
    }
    val apSession = remember(savedApSession, repeaterBinder, snackbarHostState) {
        savedApSession?.toSession(repeaterBinder, apSessionHolder.repeaterMaster, snackbarHostState)
    }
    val apState = savedApSession?.let { session ->
        rememberSaveable(session, saver = ApConfigurationState.Saver) {
            ApConfigurationState(session.initial, session.readOnly, session.p2pMode)
        }
    }
    LaunchedEffect(rootDestination, appDestinationVisible) {
        if (rootDestination != null && !appDestinationVisible) {
            savedApSession = null
            apSessionHolder.repeaterMaster = null
        }
    }
    BackHandler(rootDestination != null) {
        (appContext as? Activity)?.finish()
    }
    val navFadeSpec = MaterialTheme.motionScheme.defaultEffectsSpec<Float>()
    NavHost(
        navController = navController,
        startDestination = RootDestination.Tethering.route,
        modifier = Modifier.fillMaxSize(),
        enterTransition = { fadeIn(navFadeSpec) },
        exitTransition = { fadeOut(navFadeSpec) },
        popEnterTransition = { fadeIn(navFadeSpec) },
        popExitTransition = { fadeOut(navFadeSpec) },
    ) {
        composable(RootDestination.Tethering.route) {
            RootDestinationScaffold(
                title = R.string.app_name,
                selectedDestination = RootDestination.Tethering,
                navController = navController,
                validClientCount = validClientCount,
                snackbarHostState = snackbarHostState,
                showSnackbarHost = route == RootDestination.Tethering.route,
                actions = {
                    TetheringActions(
                        monitorableIfaces = monitorableIfaces,
                        onMonitorInterface = { iface ->
                            appContext.startForegroundService(Intent(appContext, TetheringService::class.java)
                                .putExtra(TetheringService.EXTRA_ADD_INTERFACE_MONITOR, iface))
                        },
                    )
                },
            ) {
                TetheringScreen(
                    snackbarHostState,
                    repeaterBinder,
                    localOnlyBinder,
                    tetherStates,
                    tetheringServiceState,
                    onConfigureRepeater = {
                        if (!apConfigurationLoading) {
                            apConfigurationLoading = true
                            scope.launch {
                                try {
                                    loadRepeaterApConfiguration(repeaterBinder, snackbarHostState)?.let { session ->
                                        apSessionHolder.repeaterMaster = session.repeaterMaster
                                        savedApSession = session.toSaved()
                                        navController.navigate(AppDestination.RepeaterConfiguration.route)
                                    }
                                } finally {
                                    apConfigurationLoading = false
                                }
                            }
                        }
                    },
                    onConfigureTemporaryHotspot = temporaryHotspotConfiguration?.let { configuration ->
                        {
                            val session = ApConfigurationSession(
                                initial = configuration,
                                readOnly = true,
                                target = ApConfigurationTarget.Temporary,
                                onApply = { false },
                            )
                            apSessionHolder.repeaterMaster = null
                            savedApSession = session.toSaved()
                            navController.navigate(AppDestination.TemporaryHotspotConfiguration.route)
                        }
                    },
                    onConfigureAp = {
                        if (!apConfigurationLoading) {
                            apConfigurationLoading = true
                            scope.launch {
                                try {
                                    loadSystemApConfiguration(snackbarHostState)?.let { configuration ->
                                        val session = ApConfigurationSession(
                                            configuration,
                                            target = ApConfigurationTarget.System,
                                        ) { config -> applySystemApConfiguration(config, snackbarHostState) }
                                        apSessionHolder.repeaterMaster = null
                                        savedApSession = session.toSaved()
                                        navController.navigate(AppDestination.ApConfiguration.route)
                                    }
                                } finally {
                                    apConfigurationLoading = false
                                }
                            }
                        }
                    },
                )
            }
        }
        composable(RootDestination.Clients.route) {
            RootDestinationScaffold(
                title = R.string.app_name,
                selectedDestination = RootDestination.Clients,
                navController = navController,
                validClientCount = validClientCount,
                snackbarHostState = snackbarHostState,
                showSnackbarHost = route == RootDestination.Clients.route,
            ) {
                ClientsScreen(clientViewModel, snackbarHostState)
            }
        }
        composable(RootDestination.Settings.route) {
            RootDestinationScaffold(
                title = R.string.app_name,
                selectedDestination = RootDestination.Settings,
                navController = navController,
                validClientCount = validClientCount,
                snackbarHostState = snackbarHostState,
                showSnackbarHost = route == RootDestination.Settings.route,
            ) {
                SettingsScreen(snackbarHostState)
            }
        }
        composable(AppDestination.ApConfiguration.route) {
            ApConfigurationRoute(
                AppDestination.ApConfiguration,
                route == AppDestination.ApConfiguration.route,
                apState,
                apSession,
                navController,
                snackbarHostState,
            )
        }
        composable(AppDestination.RepeaterConfiguration.route) {
            ApConfigurationRoute(
                AppDestination.RepeaterConfiguration,
                route == AppDestination.RepeaterConfiguration.route,
                apState,
                apSession,
                navController,
                snackbarHostState,
            )
        }
        composable(AppDestination.TemporaryHotspotConfiguration.route) {
            ApConfigurationRoute(
                AppDestination.TemporaryHotspotConfiguration,
                route == AppDestination.TemporaryHotspotConfiguration.route,
                apState,
                apSession,
                navController,
                snackbarHostState,
            )
        }
    }
}

private fun ApConfigurationSession.toSaved() = SavedApConfigurationSession(initial, readOnly, p2pMode, target)

@Composable
@OptIn(ExperimentalMaterial3AdaptiveApi::class)
private fun RootDestinationScaffold(
    @StringRes title: Int,
    selectedDestination: RootDestination,
    navController: NavHostController,
    validClientCount: Int,
    snackbarHostState: SnackbarHostState,
    showSnackbarHost: Boolean,
    actions: @Composable RowScope.() -> Unit = {},
    content: @Composable () -> Unit,
) {
    val adaptiveInfo = currentWindowAdaptiveInfo()
    val useNavigationRail = adaptiveInfo.windowSizeClass.isWidthAtLeastBreakpoint(WIDTH_DP_MEDIUM_LOWER_BOUND)
    val navigateToRoot: (RootDestination) -> Unit = { destination ->
        navController.navigate(destination.route) {
            popUpTo(navController.graph.findStartDestination().id) {
                saveState = true
            }
            launchSingleTop = true
            restoreState = true
        }
    }
    if (useNavigationRail) {
        Row(Modifier.fillMaxSize()) {
            NavigationRail {
                for (destination in RootDestination.entries) {
                    NavigationRailItem(
                        selected = destination == selectedDestination,
                        onClick = { navigateToRoot(destination) },
                        icon = { RootNavigationIcon(destination, validClientCount) },
                        label = { Text(stringResource(destination.title)) },
                    )
                }
            }
            DestinationScaffold(
                title = title,
                modifier = Modifier.weight(1f),
                snackbarHostState = snackbarHostState,
                showSnackbarHost = showSnackbarHost,
                actions = actions,
                contentWindowInsets = WindowInsets.safeDrawing.only(WindowInsetsSides.Bottom),
                content = content,
            )
        }
    } else {
        DestinationScaffold(
            title = title,
            snackbarHostState = snackbarHostState,
            showSnackbarHost = showSnackbarHost,
            actions = actions,
            bottomBar = {
                NavigationBar {
                    for (destination in RootDestination.entries) {
                        NavigationBarItem(
                            selected = destination == selectedDestination,
                            onClick = { navigateToRoot(destination) },
                            icon = { RootNavigationIcon(destination, validClientCount) },
                            label = { Text(stringResource(destination.title)) },
                        )
                    }
                }
            },
            content = content,
        )
    }
}

@Composable
private fun ApConfigurationRoute(
    destination: AppDestination,
    showSnackbarHost: Boolean,
    state: ApConfigurationState?,
    session: ApConfigurationSession?,
    navController: NavHostController,
    snackbarHostState: SnackbarHostState,
) {
    if (state == null) LaunchedEffect(Unit) {
        navController.popBackStack()
    } else DestinationScaffold(
        title = destination.title,
        snackbarHostState = snackbarHostState,
        showSnackbarHost = showSnackbarHost,
        onNavigateUp = { navController.popBackStack() },
        actions = {
            if (session != null) ApConfigurationTopBarActions(
                state = state,
                session = session,
                snackbarHostState = snackbarHostState,
                onApplied = { navController.popBackStack() },
            )
        },
    ) {
        ApConfigurationScreen(state, snackbarHostState)
    }
}

@Composable
private fun DestinationScaffold(
    @StringRes title: Int,
    modifier: Modifier = Modifier,
    snackbarHostState: SnackbarHostState,
    showSnackbarHost: Boolean,
    onNavigateUp: (() -> Unit)? = null,
    actions: @Composable RowScope.() -> Unit = {},
    bottomBar: @Composable () -> Unit = {},
    contentWindowInsets: WindowInsets = WindowInsets(),
    content: @Composable () -> Unit,
) {
    Scaffold(
        modifier = modifier,
        topBar = { DestinationTopAppBar(title, onNavigateUp, actions) },
        bottomBar = bottomBar,
        snackbarHost = {
            if (showSnackbarHost) SnackbarHost(snackbarHostState) { data ->
                SwipeToDismissBox(
                    state = rememberSwipeToDismissBoxState(),
                    backgroundContent = {},
                    onDismiss = { data.dismiss() },
                ) {
                    Snackbar(data)
                }
            }
        },
        contentWindowInsets = contentWindowInsets,
    ) { contentPadding ->
        Box(Modifier.padding(contentPadding)) {
            content()
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
private fun DestinationTopAppBar(
    @StringRes title: Int,
    onNavigateUp: (() -> Unit)? = null,
    actions: @Composable RowScope.() -> Unit = {},
) {
    TopAppBar(
        title = { Text(stringResource(title)) },
        navigationIcon = {
            if (onNavigateUp != null) {
                val tooltip = stringResource(R.string.action_bar_up_description)
                TooltipIconButton(
                    tooltip = tooltip,
                    onClick = onNavigateUp,
                ) {
                    NavIcon(R.drawable.ic_navigation_arrow_back, tooltip)
                }
            }
        },
        actions = actions,
    )
}

@Composable
private fun NavIcon(@DrawableRes icon: Int, @StringRes description: Int) {
    NavIcon(icon, stringResource(description))
}

@Composable
private fun RootNavigationIcon(destination: RootDestination, validClientCount: Int) {
    if (destination == RootDestination.Clients && validClientCount > 0) {
        BadgedBox(badge = {
            Badge(
                containerColor = MaterialTheme.colorScheme.secondary,
                contentColor = MaterialTheme.colorScheme.onSecondary,
            ) {
                Text(validClientCount.toString())
            }
        }) {
            NavIcon(destination.icon, destination.title)
        }
    } else NavIcon(destination.icon, destination.title)
}

@Composable
private fun NavIcon(@DrawableRes icon: Int, description: String) {
    Icon(
        painter = painterResource(icon),
        contentDescription = description,
    )
}

@Composable
private fun TetheringActions(
    monitorableIfaces: List<String>,
    onMonitorInterface: (String) -> Unit,
) {
    var monitorExpanded by remember { mutableStateOf(false) }
    if (monitorableIfaces.isNotEmpty()) {
        val tooltip = stringResource(R.string.tethering_monitor)
        TooltipIconButton(
            tooltip = tooltip,
            onClick = { monitorExpanded = true },
        ) {
            NavIcon(R.drawable.ic_image_remove_red_eye, tooltip)
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
}
