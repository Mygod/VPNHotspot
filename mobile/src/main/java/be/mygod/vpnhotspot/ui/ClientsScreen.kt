package be.mygod.vpnhotspot.ui

import android.content.Context
import android.net.MacAddress
import android.os.Build
import android.os.Parcelable
import android.text.SpannableStringBuilder
import android.text.TextUtils
import android.text.format.Formatter
import androidx.collection.LongObjectMap
import androidx.collection.MutableScatterMap
import androidx.collection.ObjectList
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.ListItem
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
import androidx.compose.ui.platform.LocalSoftwareKeyboardController
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.input.KeyboardCapitalization
import androidx.compose.ui.text.style.TextDecoration
import androidx.compose.ui.unit.dp
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LiveData
import androidx.lifecycle.compose.LocalLifecycleOwner
import androidx.lifecycle.repeatOnLifecycle
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.client.Client
import be.mygod.vpnhotspot.client.ClientViewModel
import be.mygod.vpnhotspot.client.MacLookup
import be.mygod.vpnhotspot.net.TetherType
import be.mygod.vpnhotspot.net.monitor.TrafficRecorder
import be.mygod.vpnhotspot.room.AppDatabase
import be.mygod.vpnhotspot.room.ClientStats
import be.mygod.vpnhotspot.room.TrafficRecord
import be.mygod.vpnhotspot.room.TrafficStatsSource
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
    val lifecycleOwner = LocalLifecycleOwner.current
    val clients by model.clients.observeAsState(emptyList())
    val rates = remember { mutableStateMapOf<Pair<String?, MacAddress>, TrafficRate>() }
    var tetherTypeRevision by remember { mutableIntStateOf(0) }
    val blockServiceInactive = stringResource(R.string.clients_popup_block_service_inactive)
    val clientsContentDescription = stringResource(R.string.title_clients)

    DisposableEffect(lifecycleOwner, model) {
        lifecycleOwner.lifecycle.addObserver(model.clientsFragmentObserver)
        onDispose { lifecycleOwner.lifecycle.removeObserver(model.clientsFragmentObserver) }
    }
    LaunchedEffect(clients) {
        clients.forEach { client ->
            val key = client.iface to client.mac
            if (rates[key] == null) rates[key] = TrafficRate()
        }
    }
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

    SettingsList(modifier = Modifier.semantics { contentDescription = clientsContentDescription }) {
        if (clients.isEmpty()) {
            item {
                PreferenceRow(
                    icon = R.drawable.ic_device_devices,
                    title = stringResource(R.string.clients_empty),
                )
            }
        } else for (client in clients) item(key = "${client.iface}/${client.mac}") {
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

@Composable
private fun ClientRow(
    client: Client,
    rate: TrafficRate?,
    snackbarHostState: SnackbarHostState,
    tetherTypeRevision: Int,
    onNickname: (CharSequence) -> Unit,
    onSetNicknameToVendor: () -> Unit,
    onToggleBlocked: () -> Unit,
) {
    val context = LocalContext.current
    val title by client.title.observeAsState(SpannableStringBuilder(client.macString))
    val titleSelectable by client.titleSelectable.observeAsState(true)
    val description by client.description.observeAsState(SpannableStringBuilder())
    val rateText = formatTrafficRate(context, rate)
    var expanded by remember { mutableStateOf(false) }
    var editingNickname by rememberSaveable(client.mac.toString()) { mutableStateOf(false) }
    var nicknameDraft by rememberSaveable(client.mac.toString(), client.nickname.toString(), editingNickname) {
        mutableStateOf(client.nickname.toString())
    }
    var statsDialog by rememberSaveable(client.mac.toString()) { mutableStateOf<ClientStatsDialog?>(null) }
    val scope = rememberCoroutineScope()
    val icon = remember(client, tetherTypeRevision) { client.icon }

    Box {
        ListItem(
            headlineContent = {
                if (titleSelectable) RowSelectionContainer {
                    LinkedText(
                        title,
                        textDecoration = if (client.blocked) TextDecoration.LineThrough else null,
                    )
                } else {
                    LinkedText(
                        title,
                        textDecoration = if (client.blocked) TextDecoration.LineThrough else null,
                    )
                }
            },
            supportingContent = {
                Column {
                    if (description.isNotEmpty()) {
                        RowSelectionContainer {
                            LinkedText(description)
                        }
                    }
                    rateText?.let { Text(it) }
                }
            },
            leadingContent = {
                Icon(
                    painter = painterResource(icon),
                    contentDescription = null,
                    modifier = Modifier.size(24.dp),
                )
            },
            modifier = Modifier
                .fillMaxWidth()
                .clickable { expanded = true },
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
                    Text(stringResource(if (client.blocked) R.string.clients_popup_unblock else R.string.clients_popup_block))
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
                            statsDialog = ClientStatsDialog(title, stats)
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
    HorizontalDivider()

    if (editingNickname) {
        val focusRequester = remember { FocusRequester() }
        val keyboard = LocalSoftwareKeyboardController.current
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
                    keyboardOptions = KeyboardOptions(capitalization = KeyboardCapitalization.Words),
                    singleLine = true,
                )
            },
            confirmButton = {
                TextButton(onClick = {
                    MacLookup.abort(client.mac)
                    onNickname(nicknameDraft)
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
                LinkedText(TextUtils.expandTemplate(stringResource(R.string.clients_stats_title), dialog.title))
            },
            text = { LinkedText(formatClientStats(context, dialog.stats)) },
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

@Parcelize
private data class ClientStatsDialog(val title: CharSequence, val stats: ClientStats) : Parcelable

private suspend fun updateNickname(
    mac: MacAddress,
    nickname: CharSequence,
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

private fun formatClientStats(context: Context, stats: ClientStats): CharSequence {
    val resources = context.resources
    val format = NumberFormat.getIntegerInstance(resources.configuration.locales[0])
    val builder = SpannableStringBuilder()
    if (stats.timestamp > 0) builder.append(
        TextUtils.expandTemplate(context.getText(R.string.clients_stats_since), context.formatTimestamp(stats.timestamp)),
    )
    for (entry in stats.entries) if (!entry.isEmpty) {
        if (builder.isNotEmpty()) builder.append("\n\n")
        builder.append(context.getText(when (entry.source) {
            TrafficStatsSource.IPV4 -> R.string.clients_stats_ipv4
            TrafficStatsSource.DNS -> R.string.clients_stats_dns
            TrafficStatsSource.NAT66_TCP -> R.string.clients_stats_nat66_tcp
            TrafficStatsSource.NAT66_UDP -> R.string.clients_stats_nat66_udp
            TrafficStatsSource.NAT66_ICMPV6 -> R.string.clients_stats_nat66_icmpv6
        }))
        when (entry.source) {
            TrafficStatsSource.NAT66_TCP -> {
                builder.append(TextUtils.expandTemplate(resources.getQuantityText(R.plurals.clients_stats_connections,
                    entry.sentPackets.toPluralInt()), format.format(entry.sentPackets)))
                builder.append(TextUtils.expandTemplate(context.getText(R.string.clients_stats_sent_bytes),
                    Formatter.formatFileSize(context, entry.sentBytes)))
                builder.append(TextUtils.expandTemplate(context.getText(R.string.clients_stats_received_bytes),
                    Formatter.formatFileSize(context, entry.receivedBytes)))
            }
            TrafficStatsSource.DNS -> {
                builder.append(TextUtils.expandTemplate(resources.getQuantityText(R.plurals.clients_stats_dns_queries,
                    entry.sentPackets.toPluralInt()), format.format(entry.sentPackets),
                    Formatter.formatFileSize(context, entry.sentBytes)))
                builder.append(TextUtils.expandTemplate(resources.getQuantityText(R.plurals.clients_stats_dns_responses,
                    entry.receivedPackets.toPluralInt()), format.format(entry.receivedPackets),
                    Formatter.formatFileSize(context, entry.receivedBytes)))
            }
            else -> {
                builder.append(TextUtils.expandTemplate(resources.getQuantityText(R.plurals.clients_stats_message_2,
                    entry.sentPackets.toPluralInt()), format.format(entry.sentPackets),
                    Formatter.formatFileSize(context, entry.sentBytes)))
                builder.append(TextUtils.expandTemplate(resources.getQuantityText(R.plurals.clients_stats_message_3,
                    entry.receivedPackets.toPluralInt()), format.format(entry.receivedPackets),
                    Formatter.formatFileSize(context, entry.receivedBytes)))
            }
        }
    }
    return builder.ifEmpty { context.getText(R.string.clients_stats_empty) }
}

private data class TrafficRate(val send: Long = -1, val receive: Long = -1)
