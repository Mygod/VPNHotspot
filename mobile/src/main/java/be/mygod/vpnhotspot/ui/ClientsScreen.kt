package be.mygod.vpnhotspot.ui

import android.content.Context
import android.content.res.Configuration
import android.net.MacAddress
import android.os.Build
import android.os.Parcelable
import android.os.SystemClock
import android.text.format.Formatter
import androidx.annotation.DrawableRes
import androidx.collection.LongObjectMap
import androidx.collection.MutableScatterMap
import androidx.collection.ObjectList
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.Icon
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.State
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateMapOf
import androidx.compose.runtime.mutableStateOf
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
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.AnnotatedString
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.TextLinkStyles
import androidx.compose.ui.text.TextRange
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.input.ImeAction
import androidx.compose.ui.text.input.KeyboardCapitalization
import androidx.compose.ui.text.input.TextFieldValue
import androidx.compose.ui.text.style.TextDecoration
import androidx.compose.ui.tooling.preview.Preview
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LiveData
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.repeatOnLifecycle
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.client.Client
import be.mygod.vpnhotspot.client.ClientViewModel
import be.mygod.vpnhotspot.client.MacLookup
import be.mygod.vpnhotspot.net.TetherType
import be.mygod.vpnhotspot.net.monitor.TrafficRecorder
import be.mygod.vpnhotspot.room.AppDatabase
import be.mygod.vpnhotspot.room.ClientRecord
import be.mygod.vpnhotspot.room.ClientStats
import be.mygod.vpnhotspot.room.TrafficRecord
import be.mygod.vpnhotspot.room.TrafficStatsSource
import be.mygod.vpnhotspot.root.daemon.NeighbourState
import be.mygod.vpnhotspot.ui.theme.VpnHotspotTheme
import be.mygod.vpnhotspot.util.formatTimestamp
import be.mygod.vpnhotspot.util.toPluralInt
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.DelicateCoroutinesApi
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.parcelize.Parcelize
import timber.log.Timber
import java.text.NumberFormat

@OptIn(DelicateCoroutinesApi::class)
@Composable
internal fun ClientsScreen(model: ClientViewModel, snackbarHostState: SnackbarHostState) {
    val context = LocalContext.current
    val inspectionMode = LocalInspectionMode.current
    val lifecycleOwner = LocalLifecycleOwner.current
    val clients by model.clients.observeAsState(emptyList())
    val rates = remember { mutableStateMapOf<Pair<String?, MacAddress>, TrafficRate>() }
    var tetherTypeRevision by remember { mutableIntStateOf(0) }
    val blockServiceInactive = stringResource(R.string.clients_popup_block_service_inactive)
    val clientsContentDescription = stringResource(R.string.title_clients)

    if (!inspectionMode) {
        DisposableEffect(lifecycleOwner, model) {
            lifecycleOwner.lifecycle.addObserver(model.clientsFragmentObserver)
            onDispose { lifecycleOwner.lifecycle.removeObserver(model.clientsFragmentObserver) }
        }
    }
    LaunchedEffect(clients) {
        clients.forEach { client ->
            val key = client.iface to client.mac
            if (rates[key] == null) rates[key] = TrafficRate()
        }
    }
    if (!inspectionMode) {
        LaunchedEffect(lifecycleOwner, clients) {
            lifecycleOwner.lifecycle.repeatOnLifecycle(Lifecycle.State.STARTED) {
                if (Build.VERSION.SDK_INT >= 30) launch {
                    TetherType.changes.collect { tetherTypeRevision++ }
                }
                launch {
                    TrafficRecorder.foregroundUpdates.collect { (newRecords, oldRecords) ->
                        updateTrafficRates(clients, rates, newRecords, oldRecords)
                    }
                }
                withContext(Dispatchers.Default) {
                    TrafficRecorder.rescheduleUpdate()
                }
            }
        }
    }

    SettingsList(modifier = Modifier.semantics { contentDescription = clientsContentDescription }) {
        if (clients.isEmpty()) item {
            Text(
                text = stringResource(R.string.clients_empty),
                modifier = Modifier
                    .fillParentMaxSize()
                    .padding(horizontal = 16.dp),
            )
        } else item {
            PreferenceGroup {
                for (client in clients) {
                    row(key = client.iface to client.mac) {
                        ClientRow(
                            client = client,
                            rate = rates[client.iface to client.mac],
                            snackbarHostState = snackbarHostState,
                            tetherTypeRevision = tetherTypeRevision,
                            onNickname = { nickname ->
                                GlobalScope.launch(Dispatchers.Main.immediate) {
                                    updateNickname(client.mac, nickname, snackbarHostState)
                                }
                            },
                            onSetNicknameToVendor = { MacLookup.perform(client.mac, true) },
                            onToggleBlocked = {
                                val wasWorking = TrafficRecorder.isWorking(client.mac)
                                val record = client.obtainRecord().apply { blocked = !blocked }
                                GlobalScope.launch(Dispatchers.Unconfined) {
                                    AppDatabase.instance.clientRecordDao.update(record)
                                }
                                if (!wasWorking && record.blocked) GlobalScope.launch(Dispatchers.Main.immediate) {
                                    snackbarHostState.showLongSnackbar(blockServiceInactive)
                                }
                            },
                        )
                    }
                }
            }
        }
    }
}

@Composable
private fun ClientRow(
    client: Client,
    rate: TrafficRate?,
    snackbarHostState: SnackbarHostState,
    tetherTypeRevision: Int,
    onNickname: (AnnotatedString) -> Unit,
    onSetNicknameToVendor: () -> Unit,
    onToggleBlocked: () -> Unit,
) {
    val context = LocalContext.current
    val record by client.record.observeAsState(ClientRecord(client.mac))
    val linkStyles = rememberNetworkAddressLinkStyles()
    val nickname = record.nickname
    LaunchedEffect(client.mac, nickname, record.macLookupPending) {
        if (nickname.isEmpty() && record.macLookupPending) MacLookup.perform(client.mac)
    }
    val title = buildAnnotatedString {
        if (nickname.isNotEmpty()) append(nickname) else appendClientAddress(client, client.iface, linkStyles)
    }
    val description = buildAnnotatedString {
        fun line(content: AnnotatedString.Builder.() -> Unit) {
            if (length > 0) append('\n')
            content()
        }
        if (nickname.isNotEmpty()) line {
            appendClientAddress(client, client.iface, linkStyles)
        }
        client.ifaces.forEach {
            if (it == client.iface) return@forEach
            line { appendClientAddress(client, it, linkStyles) }
        }
        client.ip.entries.forEach { (ip, info) ->
            line {
                appendIpAddress(ip, linkStyles)
                info.address?.let { append("/${it.prefixLength}") }
                append(when (info.state) {
                    NeighbourState.NEIGHBOUR_STATE_UNSET -> ""
                    NeighbourState.NEIGHBOUR_STATE_INCOMPLETE -> context.getString(R.string.connected_state_incomplete)
                    NeighbourState.NEIGHBOUR_STATE_VALID -> context.getString(R.string.connected_state_valid)
                    NeighbourState.NEIGHBOUR_STATE_FAILED -> context.getString(R.string.connected_state_failed)
                    is NeighbourState.Unrecognized -> error("Invalid neighbour state ${info.state.value}")
                })
                if (info.address != null) {
                    info.hostname?.let { append(" \u2192\u201c$it\u201d") }
                    val delta = System.currentTimeMillis() - SystemClock.elapsedRealtime()
                    append(" \u23f3${context.formatTimestamp(info.deprecationTime + delta)}")
                }
            }
        }
    }
    val rateText = formatTrafficRate(context, rate)
    var expanded by remember { mutableStateOf(false) }
    var editingNickname by rememberSaveable(client.mac.toString()) { mutableStateOf(false) }
    var statsDialog by rememberSaveable(client.mac.toString()) { mutableStateOf<ClientStatsDialog?>(null) }
    val scope = rememberCoroutineScope()
    val icon = remember(client, tetherTypeRevision) { client.icon }

    Box {
        ClientRowLayout(
            icon = icon,
            title = title,
            description = description,
            rateText = rateText,
            blocked = record.blocked,
            onClick = { expanded = true },
        )
        DropdownMenu(expanded = expanded, onDismissRequest = { expanded = false }) {
            DropdownMenuItem(
                text = { Text(stringResource(R.string.clients_popup_nickname)) },
                onClick = {
                    expanded = false
                    editingNickname = true
                },
            )
            DropdownMenuItem(
                text = {
                    Text(stringResource(if (record.blocked) R.string.clients_popup_unblock else R.string.clients_popup_block))
                },
                onClick = {
                    expanded = false
                    onToggleBlocked()
                },
            )
            DropdownMenuItem(
                text = { Text(stringResource(R.string.clients_popup_stats)) },
                onClick = {
                    expanded = false
                    scope.launch {
                        try {
                            val stats = withContext(Dispatchers.Unconfined) {
                                AppDatabase.instance.trafficRecordDao.queryStats(client.mac)
                            }
                            statsDialog = ClientStatsDialog(title.text, stats)
                        } catch (e: CancellationException) {
                            throw e
                        } catch (e: Exception) {
                            Timber.w(e)
                            snackbarHostState.showLongSnackbar(e.localizedMessage ?: e.javaClass.name)
                        }
                    }
                },
            )
        }
    }

    if (editingNickname) {
        val focusRequester = remember { FocusRequester() }
        val keyboard = LocalSoftwareKeyboardController.current
        val initialNickname = remember(client.mac.toString(), editingNickname) {
            record.nickname
        }
        var nicknameDraft by rememberSaveable(
            client.mac.toString(),
            editingNickname,
            stateSaver = TextFieldValue.Saver,
        ) {
            mutableStateOf(TextFieldValue(initialNickname, TextRange(initialNickname.length)))
        }
        LaunchedEffect(Unit) {
            focusRequester.requestFocus()
            keyboard?.show()
        }
        AlertDialog(
            onDismissRequest = { editingNickname = false },
            title = { Text(stringResource(R.string.clients_nickname_title, client.mac)) },
            text = {
                OutlinedTextField(
                    value = nicknameDraft,
                    onValueChange = { nicknameDraft = it },
                    modifier = Modifier
                        .fillMaxWidth()
                        .focusRequester(focusRequester),
                    placeholder = { Text(stringResource(R.string.clients_nickname_hint)) },
                    keyboardOptions = KeyboardOptions(
                        capitalization = KeyboardCapitalization.Words,
                        imeAction = ImeAction.Done,
                    ),
                    singleLine = true,
                )
            },
            confirmButton = {
                TextButton(onClick = {
                    MacLookup.abort(client.mac)
                    onNickname(nicknameDraft.annotatedString)
                    editingNickname = false
                }) {
                    Text(stringResource(android.R.string.ok))
                }
            },
            dismissButton = {
                TextButton(onClick = {
                    onSetNicknameToVendor()
                    editingNickname = false
                }) {
                    Text(stringResource(R.string.clients_nickname_set_to_vendor))
                }
                TextButton(onClick = { editingNickname = false }) {
                    Text(stringResource(android.R.string.cancel))
                }
            },
        )
    }
    statsDialog?.let { dialog ->
        AlertDialog(
            onDismissRequest = {
                statsDialog = null
            },
            title = {
                Text(stringResource(R.string.clients_stats_title, dialog.title))
            },
            text = { Text(formatClientStats(context, dialog.stats)) },
            confirmButton = {
                TextButton(onClick = {
                    statsDialog = null
                }) {
                    Text(stringResource(android.R.string.ok))
                }
            },
        )
    }
}

@Composable
private fun ClientRowLayout(
    @DrawableRes icon: Int,
    title: AnnotatedString,
    description: AnnotatedString,
    rateText: String?,
    blocked: Boolean,
    onClick: () -> Unit,
) {
    PreferenceRow(
        titleContent = {
            RowSelectionContainer {
                Text(
                    text = title,
                    textDecoration = if (blocked) TextDecoration.LineThrough else null,
                )
            }
        },
        summaryContent = {
            Column {
                if (description.text.isNotEmpty()) {
                    RowSelectionContainer {
                        Text(description)
                    }
                }
                rateText?.let { Text(it) }
            }
        },
        iconContent = {
            Icon(
                painter = painterResource(icon),
                contentDescription = null,
                modifier = Modifier.size(24.dp),
            )
        },
        onClick = onClick,
    )
}

@Parcelize
private data class ClientStatsDialog(val title: String, val stats: ClientStats) : Parcelable

private fun AnnotatedString.Builder.appendClientAddress(client: Client, iface: String?, linkStyles: TextLinkStyles) {
    appendMacAddress(client.macString, linkStyles)
    iface?.let {
        append('%')
        append(it)
    }
}

private suspend fun updateNickname(
    mac: MacAddress,
    nickname: AnnotatedString,
    snackbarHostState: SnackbarHostState,
) {
    try {
        withContext(Dispatchers.Unconfined) {
            AppDatabase.instance.clientRecordDao.upsert(mac) { this.nickname = nickname }
        }
    } catch (e: CancellationException) {
        throw e
    } catch (e: Exception) {
        Timber.w(e)
        snackbarHostState.showLongSnackbar(e.localizedMessage ?: e.javaClass.name)
    }
}

@Composable
private fun <T> LiveData<T>.observeAsState(initial: T): State<T> {
    val lifecycleOwner = LocalLifecycleOwner.current
    val state = remember(this) { mutableStateOf(value ?: initial) }
    DisposableEffect(this, lifecycleOwner) {
        val observer = androidx.lifecycle.Observer<T> { state.value = it ?: initial }
        observe(lifecycleOwner, observer)
        onDispose { removeObserver(observer) }
    }
    return state
}

private fun updateTrafficRates(
    clients: List<Client>,
    rates: androidx.compose.runtime.snapshots.SnapshotStateMap<Pair<String?, MacAddress>, TrafficRate>,
    newRecords: ObjectList<TrafficRecord>,
    oldRecords: LongObjectMap<TrafficRecord>,
) {
    for (key in rates.keys.toList()) rates[key] = TrafficRate()
    val rateKeys = MutableScatterMap<Pair<String, MacAddress>, Pair<String?, MacAddress>>()
    clients.forEach { client ->
        client.ifaces.forEach { rateKeys[it to client.mac] = client.iface to client.mac }
    }
    newRecords.forEach { newRecord ->
        val oldRecord = oldRecords[newRecord.previousId ?: return@forEach] ?: return@forEach
        val elapsed = newRecord.timestamp - oldRecord.timestamp
        if (elapsed == 0L) {
            if (newRecord.sentPackets != oldRecord.sentPackets || newRecord.sentBytes != oldRecord.sentBytes ||
                newRecord.receivedPackets != oldRecord.receivedPackets || newRecord.receivedBytes != oldRecord.receivedBytes
            ) Timber.w(Exception("wtf"))
            return@forEach
        }
        val key = rateKeys[newRecord.downstream to newRecord.mac] ?: (newRecord.downstream to newRecord.mac)
        val rate = rates[key] ?: TrafficRate()
        rates[key] = TrafficRate(
            send = (if (rate.send < 0 || rate.receive < 0) 0 else rate.send) +
                    (newRecord.sentBytes - oldRecord.sentBytes) * 1000 / elapsed,
            receive = (if (rate.send < 0 || rate.receive < 0) 0 else rate.receive) +
                    (newRecord.receivedBytes - oldRecord.receivedBytes) * 1000 / elapsed,
        )
    }
}

private fun formatTrafficRate(context: Context, rate: TrafficRate?): String? {
    if (rate == null || rate.send < 0 || rate.receive < 0) return null
    return "${'\u25B2'} ${Formatter.formatFileSize(context, rate.send)}/s\t\t" +
            "${'\u25BC'} ${Formatter.formatFileSize(context, rate.receive)}/s"
}

private fun formatClientStats(context: Context, stats: ClientStats): AnnotatedString {
    val resources = context.resources
    val format = NumberFormat.getIntegerInstance(resources.configuration.locales[0])
    val sectionTitleStyle = SpanStyle(fontWeight = FontWeight.Bold)
    return buildAnnotatedString {
        if (stats.timestamp > 0) append(context.getString(
            R.string.clients_stats_since,
            context.formatTimestamp(stats.timestamp),
        ))
        for (entry in stats.entries) if (!entry.isEmpty) {
            if (length > 0) append("\n\n")
            val sectionStart = length
            append(context.getString(when (entry.source) {
                TrafficStatsSource.IPV4 -> R.string.clients_stats_ipv4
                TrafficStatsSource.DNS -> R.string.clients_stats_dns
                TrafficStatsSource.NAT66_TCP -> R.string.clients_stats_nat66_tcp
                TrafficStatsSource.NAT66_UDP -> R.string.clients_stats_nat66_udp
                TrafficStatsSource.NAT66_ICMPV6 -> R.string.clients_stats_nat66_icmpv6
            }))
            addStyle(sectionTitleStyle, sectionStart, length)
            when (entry.source) {
                TrafficStatsSource.NAT66_TCP -> {
                    append(resources.getQuantityString(
                        R.plurals.clients_stats_connections,
                        entry.sentPackets.toPluralInt(),
                        format.format(entry.sentPackets),
                    ))
                    append(context.getString(
                        R.string.clients_stats_sent_bytes,
                        Formatter.formatFileSize(context, entry.sentBytes),
                    ))
                    append(context.getString(
                        R.string.clients_stats_received_bytes,
                        Formatter.formatFileSize(context, entry.receivedBytes),
                    ))
                }
                TrafficStatsSource.DNS -> {
                    append(resources.getQuantityString(
                        R.plurals.clients_stats_dns_queries,
                        entry.sentPackets.toPluralInt(),
                        format.format(entry.sentPackets),
                        Formatter.formatFileSize(context, entry.sentBytes),
                    ))
                    append(resources.getQuantityString(
                        R.plurals.clients_stats_dns_responses,
                        entry.receivedPackets.toPluralInt(),
                        format.format(entry.receivedPackets),
                        Formatter.formatFileSize(context, entry.receivedBytes),
                    ))
                }
                else -> {
                    append(resources.getQuantityString(
                        R.plurals.clients_stats_message_2,
                        entry.sentPackets.toPluralInt(),
                        format.format(entry.sentPackets),
                        Formatter.formatFileSize(context, entry.sentBytes),
                    ))
                    append(resources.getQuantityString(
                        R.plurals.clients_stats_message_3,
                        entry.receivedPackets.toPluralInt(),
                        format.format(entry.receivedPackets),
                        Formatter.formatFileSize(context, entry.receivedBytes),
                    ))
                }
            }
        }
        if (length == 0) append(context.getString(R.string.clients_stats_empty))
    }
}

@Preview(name = "Clients", showBackground = true, widthDp = 420, heightDp = 720)
@Composable
private fun ClientsPreview() = ClientsPreviewContent()

@Preview(
    name = "Clients - dark",
    showBackground = true,
    widthDp = 420,
    heightDp = 720,
    uiMode = Configuration.UI_MODE_NIGHT_YES,
)
@Composable
private fun ClientsDarkPreview() = ClientsPreviewContent()

@Composable
private fun ClientsPreviewContent() {
    VpnHotspotTheme(dynamicColor = false) {
        Surface {
            ClientsScreen(
                model = remember { ClientViewModel() },
                snackbarHostState = remember { SnackbarHostState() },
            )
        }
    }
}

@Preview(name = "Clients - connected", showBackground = true, widthDp = 420, heightDp = 720)
@Composable
private fun ClientsConnectedPreview() = ClientsConnectedPreviewContent()

@Preview(
    name = "Clients - connected dark",
    showBackground = true,
    widthDp = 420,
    heightDp = 720,
    uiMode = Configuration.UI_MODE_NIGHT_YES,
)
@Composable
private fun ClientsConnectedDarkPreview() = ClientsConnectedPreviewContent()

@Composable
private fun ClientsConnectedPreviewContent() {
    VpnHotspotTheme(dynamicColor = false) {
        Surface {
            val clientsContentDescription = stringResource(R.string.title_clients)
            SettingsList(modifier = Modifier.semantics { contentDescription = clientsContentDescription }) {
                item {
                    PreferenceGroup {
                        row {
                            ClientRowLayout(
                                icon = R.drawable.ic_device_network_wifi,
                                title = AnnotatedString("Pixel 9"),
                                description = AnnotatedString(
                                    "02:00:00:12:34:56%wlan0\n192.168.43.23 (reachable)\nfd00::23 (reachable)",
                                ),
                                rateText = "${'\u25B2'} 128 KB/s\t\t${'\u25BC'} 2.1 MB/s",
                                blocked = false,
                                onClick = {},
                            )
                        }
                        row {
                            ClientRowLayout(
                                icon = R.drawable.ic_device_usb,
                                title = AnnotatedString("7a:3f:11:90:2c:0d%rndis0"),
                                description = AnnotatedString("172.20.10.4 (reachable)"),
                                rateText = "${'\u25B2'} 8 KB/s\t\t${'\u25BC'} 64 KB/s",
                                blocked = true,
                                onClick = {},
                            )
                        }
                        row {
                            ClientRowLayout(
                                icon = R.drawable.ic_content_inbox,
                                title = AnnotatedString("Work laptop"),
                                description = AnnotatedString(
                                    "3c:22:fb:01:aa:90%eth0\n192.168.50.12 (reachable)",
                                ),
                                rateText = null,
                                blocked = false,
                                onClick = {},
                            )
                        }
                    }
                }
            }
        }
    }
}

private data class TrafficRate(val send: Long = -1, val receive: Long = -1)
