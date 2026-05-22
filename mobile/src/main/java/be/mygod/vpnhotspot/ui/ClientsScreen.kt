package be.mygod.vpnhotspot.ui

import android.content.Context
import android.content.res.Configuration
import android.os.Parcelable
import android.os.SystemClock
import android.text.format.Formatter
import androidx.annotation.DrawableRes
import androidx.compose.foundation.layout.Arrangement
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
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
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
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextDecoration
import androidx.compose.ui.tooling.preview.Preview
import androidx.compose.ui.unit.dp
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.compose.collectAsStateWithLifecycle
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.client.Client
import be.mygod.vpnhotspot.client.ClientViewModel
import be.mygod.vpnhotspot.room.ClientStats
import be.mygod.vpnhotspot.room.TrafficStatsSource
import be.mygod.vpnhotspot.root.daemon.NeighbourState
import be.mygod.vpnhotspot.ui.theme.VpnHotspotPreviewSurface
import be.mygod.vpnhotspot.util.formatTimestamp
import be.mygod.vpnhotspot.util.toPluralInt
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.launch
import kotlinx.parcelize.Parcelize
import timber.log.Timber
import java.text.NumberFormat

@Composable
fun ClientsScreen(model: ClientViewModel, snackbarHostState: SnackbarHostState) {
    val inspectionMode = LocalInspectionMode.current
    val lifecycleOwner = LocalLifecycleOwner.current
    val rows by model.clientRows.collectAsStateWithLifecycle()
    val scope = rememberCoroutineScope()
    val blockServiceInactive = stringResource(R.string.clients_popup_block_service_inactive)
    val clientsContentDescription = stringResource(R.string.title_clients)

    if (!inspectionMode) {
        DisposableEffect(lifecycleOwner, model) {
            lifecycleOwner.lifecycle.addObserver(model.clientsFragmentObserver)
            onDispose {
                lifecycleOwner.lifecycle.removeObserver(model.clientsFragmentObserver)
                model.stopClientsScreen()
            }
        }
    }

    SettingsList(modifier = Modifier.semantics { contentDescription = clientsContentDescription }) {
        if (rows.isEmpty()) item {
            Column(
                modifier = Modifier
                    .fillParentMaxSize()
                    .padding(32.dp),
                horizontalAlignment = Alignment.CenterHorizontally,
                verticalArrangement = Arrangement.Center,
            ) {
                Icon(
                    painter = painterResource(R.drawable.ic_device_devices),
                    contentDescription = null,
                    modifier = Modifier.size(64.dp),
                    tint = MaterialTheme.colorScheme.secondary,
                )
                Text(
                    text = stringResource(R.string.clients_empty),
                    modifier = Modifier.padding(top = 16.dp),
                    color = MaterialTheme.colorScheme.onSurfaceVariant,
                    textAlign = TextAlign.Center,
                    style = MaterialTheme.typography.bodyLarge,
                )
            }
        } else preferenceGroup(key = "clients") {
            for (row in rows) {
                val client = row.client
                row(key = client.iface to client.mac) {
                    ClientRow(
                        row = row,
                        snackbarHostState = snackbarHostState,
                        onPendingMacLookup = { model.performMacLookup(client.mac) },
                        onNickname = { nickname ->
                            scope.launch {
                                try {
                                    model.updateNickname(client.mac, nickname)
                                } catch (e: CancellationException) {
                                    throw e
                                } catch (e: Exception) {
                                    snackbarHostState.showClientError(e)
                                }
                            }
                        },
                        onSetNicknameToVendor = { model.performMacLookup(client.mac, true) },
                        onToggleBlocked = {
                            scope.launch {
                                try {
                                    if (model.toggleBlocked(row)) snackbarHostState.showLongSnackbar(blockServiceInactive)
                                } catch (e: CancellationException) {
                                    throw e
                                } catch (e: Exception) {
                                    snackbarHostState.showClientError(e)
                                }
                            }
                        },
                        onQueryStats = { model.queryStats(client.mac) },
                    )
                }
            }
        }
    }
}

@Composable
private fun ClientRow(
    row: ClientViewModel.ClientRowState,
    snackbarHostState: SnackbarHostState,
    onPendingMacLookup: () -> Unit,
    onNickname: (AnnotatedString) -> Unit,
    onSetNicknameToVendor: () -> Unit,
    onToggleBlocked: () -> Unit,
    onQueryStats: suspend () -> ClientStats,
) {
    val client = row.client
    val record = row.record
    val context = LocalContext.current
    val linkStyles = rememberNetworkAddressLinkStyles()
    val neighbourStateIncomplete = stringResource(R.string.connected_state_incomplete)
    val neighbourStateValid = stringResource(R.string.connected_state_valid)
    val neighbourStateFailed = stringResource(R.string.connected_state_failed)
    val nickname = record.nickname
    LaunchedEffect(client.mac, record) {
        if (record.nickname.isEmpty() && record.macLookupPending) onPendingMacLookup()
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
                    NeighbourState.NEIGHBOUR_STATE_INCOMPLETE -> neighbourStateIncomplete
                    NeighbourState.NEIGHBOUR_STATE_VALID -> neighbourStateValid
                    NeighbourState.NEIGHBOUR_STATE_FAILED -> neighbourStateFailed
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
    val rateText = formatTrafficRate(context, row.rate)
    var expanded by remember { mutableStateOf(false) }
    var editingNickname by rememberSaveable(client.mac.toString()) { mutableStateOf(false) }
    var statsDialog by rememberSaveable(client.mac.toString()) { mutableStateOf<ClientStatsDialog?>(null) }
    val scope = rememberCoroutineScope()
    val icon = client.icon

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
                            statsDialog = ClientStatsDialog(title.text, onQueryStats())
                        } catch (e: CancellationException) {
                            throw e
                        } catch (e: Exception) {
                            snackbarHostState.showClientError(e)
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
                DialogConfirmButton(onClick = {
                    onNickname(nicknameDraft.annotatedString)
                    editingNickname = false
                }) {
                    Text(stringResource(android.R.string.ok))
                }
            },
            dismissButton = {
                DialogNeutralButton(onClick = {
                    onSetNicknameToVendor()
                    editingNickname = false
                }) {
                    Text(stringResource(R.string.clients_nickname_set_to_vendor))
                }
                DialogDismissButton(onClick = { editingNickname = false }) {
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
                DialogConfirmButton(onClick = {
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

private suspend fun SnackbarHostState.showClientError(e: Exception) {
    Timber.w(e)
    showLongSnackbar(e.localizedMessage ?: e.javaClass.name)
}

private fun formatTrafficRate(context: Context, rate: ClientViewModel.TrafficRate?): String? {
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
    VpnHotspotPreviewSurface {
        ClientsScreen(
            model = remember { ClientViewModel() },
            snackbarHostState = remember { SnackbarHostState() },
        )
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
    VpnHotspotPreviewSurface {
        val clientsContentDescription = stringResource(R.string.title_clients)
        SettingsList(modifier = Modifier.semantics { contentDescription = clientsContentDescription }) {
            preferenceGroup(key = "clients_preview") {
                row("pixel") {
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
                row("usb") {
                    ClientRowLayout(
                        icon = R.drawable.ic_device_usb,
                        title = AnnotatedString("7a:3f:11:90:2c:0d%rndis0"),
                        description = AnnotatedString("172.20.10.4 (reachable)"),
                        rateText = "${'\u25B2'} 8 KB/s\t\t${'\u25BC'} 64 KB/s",
                        blocked = true,
                        onClick = {},
                    )
                }
                row("laptop") {
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
