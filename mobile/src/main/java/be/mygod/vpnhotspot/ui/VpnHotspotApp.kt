package be.mygod.vpnhotspot.ui

import android.app.Activity
import android.app.Service
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.os.IBinder
import androidx.activity.compose.BackHandler
import androidx.annotation.DrawableRes
import androidx.annotation.StringRes
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.WindowInsetsSides
import androidx.compose.foundation.layout.asPaddingValues
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.only
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawing
import androidx.compose.material3.Badge
import androidx.compose.material3.BadgedBox
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
import androidx.compose.material3.adaptive.currentWindowAdaptiveInfoV2
import androidx.compose.material3.rememberSwipeToDismissBoxState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.State
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.produceState
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.shadow
import androidx.compose.ui.layout.onSizeChanged
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.withResumed
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
import be.mygod.vpnhotspot.net.wifi.SoftApConfigurationCompat
import be.mygod.vpnhotspot.ui.apconfiguration.ApConfigurationScreen
import be.mygod.vpnhotspot.ui.apconfiguration.ApConfigurationSaveFab
import be.mygod.vpnhotspot.ui.apconfiguration.ApConfigurationSession
import be.mygod.vpnhotspot.ui.apconfiguration.ApConfigurationState
import be.mygod.vpnhotspot.ui.apconfiguration.ApConfigurationTarget
import be.mygod.vpnhotspot.ui.apconfiguration.ApConfigurationTopBarActions
import be.mygod.vpnhotspot.ui.apconfiguration.applyRepeaterApConfiguration
import be.mygod.vpnhotspot.ui.apconfiguration.applySystemApConfiguration
import be.mygod.vpnhotspot.ui.apconfiguration.loadRepeaterApConfiguration
import be.mygod.vpnhotspot.ui.apconfiguration.loadSystemApConfiguration
import be.mygod.vpnhotspot.util.stopAndUnbind
import kotlinx.coroutines.Job
import kotlinx.coroutines.ensureActive
import kotlinx.coroutines.launch

private enum class RootDestination(
    val route: String,
    @param:StringRes val title: Int,
    @param:DrawableRes val icon: Int,
) {
    Tethering("tethering", R.string.title_tethering, R.drawable.ic_wifi_tethering),
    Clients("clients", R.string.title_clients, R.drawable.ic_devices),
    Settings("settings", R.string.title_settings, R.drawable.ic_settings),
}

private enum class AppDestination(val route: String) {
    ApConfiguration("ap_configuration"),
    RepeaterConfiguration("repeater_configuration"),
    TemporaryHotspotConfiguration("temporary_hotspot_configuration"),
}

@OptIn(
    ExperimentalMaterial3AdaptiveApi::class,
    ExperimentalMaterial3Api::class,
    ExperimentalMaterial3ExpressiveApi::class,
)
@Composable
fun VpnHotspotApp(clientViewModel: ClientViewModel) {
    val appContext = LocalContext.current
    val navController = rememberNavController()
    val snackbarHostState = remember { SnackbarHostState() }
    SmartSnackbarBridge(snackbarHostState)
    var snackbarStartPadding by remember { mutableStateOf(0.dp) }
    var snackbarBottomPadding by remember { mutableStateOf(0.dp) }
    var savedApSession by rememberSaveable { mutableStateOf<ApConfigurationSession?>(null) }
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
    val repeaterBinderState = rememberServiceBinder<RepeaterService.Binder>(
        bindRepeaterService,
        RepeaterService::class.java,
    )
    val repeaterBinder by repeaterBinderState
    val localOnlyBinderState = rememberServiceBinder<LocalOnlyHotspotService.Binder>(
        tetheringDestinationVisible,
        LocalOnlyHotspotService::class.java,
    )
    val localOnlyBinder by localOnlyBinderState
    val tetheringBinderState = rememberServiceBinder<TetheringService.Binder>(
        tetheringDestinationVisible,
        TetheringService::class.java,
    )
    val tetheringBinder by tetheringBinderState
    val validClientCount by clientViewModel.validClientCount.collectAsStateWithLifecycle()
    val tetherStates by clientViewModel.tetherStates.collectAsStateWithLifecycle()
    val tetheringServiceState = run {
        val binder = tetheringBinder
        if (binder == null) {
            TetheringServiceState()
        } else {
            val managedIfaces by binder.managedIfaces.collectAsStateWithLifecycle()
            val inactiveIfaces by binder.inactiveIfaces.collectAsStateWithLifecycle()
            val monitoredIfaces by binder.monitoredIfaces.collectAsStateWithLifecycle()
            TetheringServiceState(
                managedIfaces = managedIfaces.asSet(),
                inactiveIfaces = inactiveIfaces.asSet(),
                monitoredIfaces = monitoredIfaces.asSet(),
            )
        }
    }
    val temporaryHotspotConfiguration by (localOnlyBinder?.configuration)?.collectAsStateWithLifecycle(null)
        ?: remember { mutableStateOf<SoftApConfigurationCompat?>(null) }
    val apState = savedApSession?.let { session ->
        rememberSaveable(session, saver = ApConfigurationState.Saver) {
            ApConfigurationState(
                session.initial,
                session.readOnly,
                session.target,
            )
        }
    }
    val applyApConfiguration: (suspend (SoftApConfigurationCompat) -> Boolean)? = when (savedApSession?.target) {
        ApConfigurationTarget.System -> {
            { config: SoftApConfigurationCompat -> applySystemApConfiguration(config, snackbarHostState) }
        }
        ApConfigurationTarget.Repeater -> apState?.let { state ->
            { config: SoftApConfigurationCompat -> applyRepeaterApConfiguration(config, state.useFramework) }
        }
        null, ApConfigurationTarget.Temporary -> null
    }
    LaunchedEffect(rootDestination, appDestinationVisible) {
        if (rootDestination != null && !appDestinationVisible) {
            savedApSession = null
        }
    }
    BackHandler(rootDestination != null) {
        (appContext as? Activity)?.finish()
    }
    val navFadeSpec = MaterialTheme.motionScheme.defaultEffectsSpec<Float>()
    Box(
        Modifier
            .fillMaxSize()
            .background(MaterialTheme.colorScheme.background),
    ) {
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
                val scope = rememberCoroutineScope()
                val lifecycleOwner = LocalLifecycleOwner.current
                var apConfigurationJob by remember { mutableStateOf<Job?>(null) }
                val openApConfiguration:
                    suspend (ApConfigurationSession, AppDestination) -> Unit = { session, destination ->
                        lifecycleOwner.withResumed {
                            if (navController.currentBackStackEntry?.destination?.route == RootDestination.Tethering.route) {
                                savedApSession = session
                                navController.navigate(destination.route)
                            }
                        }
                    }
                var tetheringInterfaceRefreshVersion by remember { mutableIntStateOf(0) }
                val localOnlyIface by (localOnlyBinder?.iface)?.collectAsStateWithLifecycle(null)
                    ?: remember { mutableStateOf(null) }
                val repeaterActive by (repeaterBinder?.active)?.collectAsStateWithLifecycle(null)
                    ?: remember { mutableStateOf(null) }
                val repeaterGroup by (repeaterBinder?.group)?.collectAsStateWithLifecycle(null)
                    ?: remember { mutableStateOf(null) }
                RootDestinationScaffold(
                    title = R.string.app_name,
                    selectedDestination = RootDestination.Tethering,
                    navController = navController,
                    validClientCount = validClientCount,
                    activeSnackbarPadding = route == RootDestination.Tethering.route,
                    onSnackbarStartPaddingChanged = { snackbarStartPadding = it },
                    onSnackbarBottomPaddingChanged = { snackbarBottomPadding = it },
                ) {
                    TetheringScreen(
                        snackbarHostState,
                        repeaterActive,
                        repeaterGroup,
                        localOnlyIface,
                        tetherStates,
                        tetheringServiceState,
                        interfaceRefreshVersion = tetheringInterfaceRefreshVersion,
                        onRefresh = { tetheringInterfaceRefreshVersion++ },
                        onConfigureRepeater = {
                            if (apConfigurationJob == null) {
                                apConfigurationJob = scope.launch {
                                    try {
                                        val session = loadRepeaterApConfiguration(repeaterBinderState.value)
                                        ensureActive()
                                        openApConfiguration(session, AppDestination.RepeaterConfiguration)
                                    } finally {
                                        apConfigurationJob = null
                                    }
                                }
                            }
                        },
                        onConfigureTemporaryHotspot = temporaryHotspotConfiguration?.let { configuration ->
                            {
                                savedApSession = ApConfigurationSession(
                                    initial = configuration,
                                    target = ApConfigurationTarget.Temporary,
                                    readOnly = true,
                                )
                                navController.navigate(AppDestination.TemporaryHotspotConfiguration.route)
                            }
                        },
                        onConfigureAp = {
                            if (apConfigurationJob == null) {
                                apConfigurationJob = scope.launch {
                                    try {
                                        val session = loadSystemApConfiguration(snackbarHostState)
                                        ensureActive()
                                        session?.let {
                                            openApConfiguration(it, AppDestination.ApConfiguration)
                                        }
                                    } finally {
                                        apConfigurationJob = null
                                    }
                                }
                            }
                        },
                        onStopRepeater = { repeaterBinderState.value?.shutdown() },
                        onStopTemporaryHotspot = { localOnlyBinderState.value?.stop() },
                        onStartRepeaterWps = { repeaterBinderState.value?.startWps(it) },
                    )
                }
            }
            composable(RootDestination.Clients.route) {
                RootDestinationScaffold(
                    title = R.string.app_name,
                    selectedDestination = RootDestination.Clients,
                    navController = navController,
                    validClientCount = validClientCount,
                    activeSnackbarPadding = route == RootDestination.Clients.route,
                    onSnackbarStartPaddingChanged = { snackbarStartPadding = it },
                    onSnackbarBottomPaddingChanged = { snackbarBottomPadding = it },
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
                    activeSnackbarPadding = route == RootDestination.Settings.route,
                    onSnackbarStartPaddingChanged = { snackbarStartPadding = it },
                    onSnackbarBottomPaddingChanged = { snackbarBottomPadding = it },
                ) {
                    SettingsScreen(snackbarHostState)
                }
            }
            for (destination in AppDestination.entries) {
                composable(destination.route) {
                    ApConfigurationRoute(
                        route == destination.route,
                        apState,
                        applyApConfiguration,
                        navController,
                        snackbarHostState,
                        onSnackbarStartPaddingChanged = { snackbarStartPadding = it },
                        onSnackbarBottomPaddingChanged = { snackbarBottomPadding = it },
                    )
                }
            }
        }
        Box(
            Modifier
                .fillMaxSize()
                .padding(start = snackbarStartPadding),
        ) {
            SnackbarHost(
                snackbarHostState,
                Modifier
                    .align(Alignment.BottomCenter)
                    .padding(bottom = snackbarBottomPadding),
            ) { data ->
                SwipeToDismissBox(
                    state = rememberSwipeToDismissBoxState(),
                    backgroundContent = {},
                    onDismiss = { data.dismiss() },
                ) {
                    Snackbar(data)
                }
            }
        }
    }
}

@Composable
private fun <T : IBinder> rememberServiceBinder(enabled: Boolean, clazz: Class<out Service>): State<T?> {
    val context = LocalContext.current
    val lifecycleOwner = LocalLifecycleOwner.current
    return produceState<T?>(null, enabled, context, lifecycleOwner, clazz) {
        if (!enabled) {
            value = null
            return@produceState
        }
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
            value = null
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
@OptIn(ExperimentalMaterial3AdaptiveApi::class)
private fun RootDestinationScaffold(
    @StringRes title: Int,
    selectedDestination: RootDestination,
    navController: NavHostController,
    validClientCount: Int,
    activeSnackbarPadding: Boolean,
    onSnackbarStartPaddingChanged: (Dp) -> Unit,
    onSnackbarBottomPaddingChanged: (Dp) -> Unit,
    actions: @Composable RowScope.() -> Unit = {},
    onReselect: () -> Unit = {},
    content: @Composable () -> Unit,
) {
    val adaptiveInfo = currentWindowAdaptiveInfoV2()
    val useNavigationRail = adaptiveInfo.windowSizeClass.isWidthAtLeastBreakpoint(WIDTH_DP_MEDIUM_LOWER_BOUND)
    val navigateToRoot: (RootDestination) -> Unit = { destination ->
        if (destination == selectedDestination) onReselect()
        navController.navigate(destination.route) {
            popUpTo(navController.graph.findStartDestination().id) {
                saveState = true
            }
            launchSingleTop = true
            restoreState = true
        }
    }
    if (useNavigationRail) {
        val density = LocalDensity.current
        var navigationRailWidth by remember { mutableStateOf<Dp?>(null) }
        Row(Modifier.fillMaxSize()) {
            NavigationRail(
                modifier = Modifier.onSizeChanged { navigationRailWidth = with(density) { it.width.toDp() } },
            ) {
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
                activeSnackbarPadding = activeSnackbarPadding,
                snackbarStartPadding = navigationRailWidth,
                onSnackbarStartPaddingChanged = onSnackbarStartPaddingChanged,
                onSnackbarBottomPaddingChanged = onSnackbarBottomPaddingChanged,
                actions = actions,
                contentWindowInsets = WindowInsets.safeDrawing.only(WindowInsetsSides.Bottom),
                content = content,
            )
        }
    } else {
        DestinationScaffold(
            title = title,
            activeSnackbarPadding = activeSnackbarPadding,
            onSnackbarStartPaddingChanged = onSnackbarStartPaddingChanged,
            onSnackbarBottomPaddingChanged = onSnackbarBottomPaddingChanged,
            actions = actions,
            bottomBar = {
                NavigationBar(
                    modifier = Modifier.shadow(3.dp),
                    containerColor = MaterialTheme.colorScheme.surface,
                    tonalElevation = 3.dp,
                ) {
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
    activeSnackbarPadding: Boolean,
    state: ApConfigurationState?,
    onApply: (suspend (SoftApConfigurationCompat) -> Boolean)?,
    navController: NavHostController,
    snackbarHostState: SnackbarHostState,
    onSnackbarStartPaddingChanged: (Dp) -> Unit,
    onSnackbarBottomPaddingChanged: (Dp) -> Unit,
) {
    if (state == null) LaunchedEffect(Unit) {
        navController.popBackStack()
    } else {
        val save = if (state.readOnly) null else onApply
        DestinationScaffold(
            title = R.string.configuration_view,
            activeSnackbarPadding = activeSnackbarPadding,
            onSnackbarStartPaddingChanged = onSnackbarStartPaddingChanged,
            onSnackbarBottomPaddingChanged = onSnackbarBottomPaddingChanged,
            onNavigateUp = { navController.popBackStack() },
            actions = {
                if (onApply != null) ApConfigurationTopBarActions(
                    state = state,
                    snackbarHostState = snackbarHostState,
                )
            },
            floatingActionButton = if (save == null) null else {
                {
                    ApConfigurationSaveFab(
                        state = state,
                        onApply = save,
                        snackbarHostState = snackbarHostState,
                        onApplied = { navController.popBackStack() },
                    )
                }
            },
        ) {
            ApConfigurationScreen(
                state,
                snackbarHostState,
                floatingActionButtonPadding = if (save == null) 0.dp else 88.dp,
            )
        }
    }
}

@Composable
@OptIn(ExperimentalMaterial3Api::class)
private fun DestinationScaffold(
    @StringRes title: Int,
    modifier: Modifier = Modifier,
    activeSnackbarPadding: Boolean,
    snackbarStartPadding: Dp? = 0.dp,
    onSnackbarStartPaddingChanged: (Dp) -> Unit,
    onSnackbarBottomPaddingChanged: (Dp) -> Unit,
    onNavigateUp: (() -> Unit)? = null,
    actions: @Composable RowScope.() -> Unit = {},
    bottomBar: (@Composable () -> Unit)? = null,
    floatingActionButton: (@Composable () -> Unit)? = null,
    contentWindowInsets: WindowInsets = WindowInsets(),
    content: @Composable () -> Unit,
) {
    val density = LocalDensity.current
    var bottomBarHeight by remember { mutableStateOf<Dp?>(null) }
    var fabHeight by remember { mutableStateOf<Dp?>(null) }
    val bottomContentInset = contentWindowInsets.asPaddingValues().calculateBottomPadding()
    val snackbarBottomPadding = when {
        floatingActionButton != null && fabHeight == null -> null
        bottomBar != null && bottomBarHeight == null -> null
        floatingActionButton != null -> (bottomBarHeight ?: bottomContentInset) + fabHeight!! + 16.dp
        bottomBar != null -> bottomBarHeight ?: bottomContentInset
        else -> bottomContentInset
    }
    LaunchedEffect(activeSnackbarPadding, snackbarBottomPadding) {
        if (activeSnackbarPadding && snackbarBottomPadding != null) {
            onSnackbarBottomPaddingChanged(snackbarBottomPadding)
        }
    }
    LaunchedEffect(activeSnackbarPadding, snackbarStartPadding) {
        if (activeSnackbarPadding && snackbarStartPadding != null) {
            onSnackbarStartPaddingChanged(snackbarStartPadding)
        }
    }
    Scaffold(
        modifier = modifier,
        topBar = {
            TopAppBar(
                title = { Text(stringResource(title)) },
                navigationIcon = {
                    if (onNavigateUp != null) {
                        val tooltip = stringResource(R.string.action_bar_up_description)
                        TooltipIconButton(
                            tooltip = tooltip,
                            onClick = onNavigateUp,
                        ) {
                            NavIcon(R.drawable.ic_arrow_back, tooltip)
                        }
                    }
                },
                actions = actions,
            )
        },
        bottomBar = {
            if (bottomBar != null) {
                Box(Modifier.onSizeChanged { bottomBarHeight = with(density) { it.height.toDp() } }) {
                    bottomBar()
                }
            }
        },
        floatingActionButton = {
            if (floatingActionButton != null) {
                Box(Modifier.onSizeChanged { fabHeight = with(density) { it.height.toDp() } }) {
                    floatingActionButton()
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

@Composable
private fun RootNavigationIcon(destination: RootDestination, validClientCount: Int) {
    val description = stringResource(destination.title)
    if (destination == RootDestination.Clients && validClientCount > 0) {
        BadgedBox(badge = {
            Badge(
                containerColor = MaterialTheme.colorScheme.secondary,
                contentColor = MaterialTheme.colorScheme.onSecondary,
            ) {
                Text(validClientCount.toString())
            }
        }) {
            NavIcon(destination.icon, description)
        }
    } else NavIcon(destination.icon, description)
}

@Composable
private fun NavIcon(@DrawableRes icon: Int, description: String) {
    Icon(
        painter = painterResource(icon),
        contentDescription = description,
    )
}
