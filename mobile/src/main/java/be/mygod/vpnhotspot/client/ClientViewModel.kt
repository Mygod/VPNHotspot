package be.mygod.vpnhotspot.client

import android.content.ClipData
import android.content.Intent
import android.net.MacAddress
import android.net.wifi.WifiClient
import android.net.wifi.p2p.WifiP2pDevice
import android.os.Build
import android.system.OsConstants
import androidx.annotation.RequiresApi
import androidx.collection.LongObjectMap
import androidx.collection.MutableScatterMap
import androidx.collection.MutableScatterSet
import androidx.collection.ObjectList
import androidx.compose.ui.text.AnnotatedString
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.ViewModel
import androidx.lifecycle.coroutineScope
import androidx.lifecycle.repeatOnLifecycle
import androidx.lifecycle.viewModelScope
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.RepeaterService
import be.mygod.vpnhotspot.net.NetlinkNeighbour
import be.mygod.vpnhotspot.net.TetherStates
import be.mygod.vpnhotspot.net.TetherType
import be.mygod.vpnhotspot.net.TetheringManagerCompat
import be.mygod.vpnhotspot.net.monitor.TrafficRecorder
import be.mygod.vpnhotspot.net.wifi.WifiApManager
import be.mygod.vpnhotspot.net.wifi.apInstanceIdentifierOrNull
import be.mygod.vpnhotspot.room.AppDatabase
import be.mygod.vpnhotspot.room.ClientRecord
import be.mygod.vpnhotspot.room.ClientStats
import be.mygod.vpnhotspot.room.TrafficRecord
import be.mygod.vpnhotspot.root.RootManager
import be.mygod.vpnhotspot.root.TetheringCommands
import be.mygod.vpnhotspot.root.WifiApCommands
import be.mygod.vpnhotspot.root.daemon.NeighbourState
import be.mygod.vpnhotspot.ui.softApClientBlockReasonLabel
import be.mygod.vpnhotspot.ui.softApClientDisconnectReasonLabel
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.bindServiceFlow
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.collections.immutable.persistentMapOf
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.Job
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.SharingStarted
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.collectLatest
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.flatMapLatest
import kotlinx.coroutines.flow.flowOf
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.flow.merge
import kotlinx.coroutines.flow.stateIn
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import timber.log.Timber

class ClientViewModel : ViewModel(), DefaultLifecycleObserver {
    private enum class WifiApClientSource { Tethered, LocalOnly }
    private data class TetheredClientInfo(val fallbackType: TetherType, val addresses: List<ClientAddressInfo>)
    data class TrafficRate(val send: Long = 0, val receive: Long = 0)
    data class ClientRowState(val client: Client, val record: ClientRecord, val rate: TrafficRate?)

    private val tetherStatesState = MutableStateFlow(TetherStates())
    val tetherStates = tetherStatesState.asStateFlow()
    private var tetheredInterfaces = emptySet<String>()
    private var localOnlyInterfaces = emptySet<String>()
    private var startedJob: Job? = null

    private val repeater = MutableStateFlow<RepeaterService.Binder?>(null)
    private var p2p: Collection<WifiP2pDevice> = emptyList()
    private var wifiAp: MutableScatterSet<Pair<String, MacAddress>>? = null
    private var localOnlyHotspot: MutableScatterSet<Pair<String, MacAddress>>? = null
    private var netlinkSnapshot = NetlinkNeighbour.Snapshot(emptyList(), persistentMapOf())
    private var tetheringClients = MutableScatterMap<MacAddress, TetheredClientInfo>()
    private val clientsState = MutableStateFlow<List<Client>>(emptyList())
    val validClientCount = clientsState.map { clients ->
        clients.count { it.active }
    }.stateIn(viewModelScope, SharingStarted.WhileSubscribed(5_000), 0)
    private val trafficRates = MutableStateFlow<Map<Pair<String?, MacAddress>, TrafficRate>>(emptyMap())
    @OptIn(ExperimentalCoroutinesApi::class)
    val clientRows = clientsState.flatMapLatest { clients ->
        if (clients.isEmpty()) flowOf(emptyList()) else combine(clients.map { client ->
            AppDatabase.instance.clientRecordDao.lookupOrDefaultFlow(client.mac)
        }) { records ->
            clients.zip(records)
        }.combine(trafficRates) { rows, rates ->
            rows.map { (client, record) -> ClientRowState(client, record, rates[client.iface to client.mac]) }
        }
    }.stateIn(viewModelScope, SharingStarted.WhileSubscribed(5_000), emptyList())

    private var clientsScreenJob: Job? = null
    val clientsScreenObserver = object : DefaultLifecycleObserver {
        override fun onStart(owner: LifecycleOwner) {
            clientsScreenJob?.cancel()
            clientsScreenJob = owner.lifecycle.coroutineScope.launch {
                if (Build.VERSION.SDK_INT >= 30) {
                    launch {
                        try {
                            RootManager.use {
                                handleClientsChanged(it.flow(TetheringCommands.ClientsFlow()))
                            }
                        } catch (_: CancellationException) {
                        } catch (e: Exception) {
                            Timber.w(e)
                            SmartSnackbar.make(e).show()
                        }
                    }
                    launch {
                        TetherType.changes.collect {
                            populateClients()
                        }
                    }
                }
                launch {
                    TrafficRecorder.foregroundUpdates.collect { (newRecords, oldRecords) ->
                        updateTrafficRates(newRecords, oldRecords)
                    }
                }
            }
        }

        override fun onStop(owner: LifecycleOwner) {
            stopClientsScreen()
        }
    }

    fun stopClientsScreen() {
        clientsScreenJob?.cancel()
        clientsScreenJob = null
        if (tetheringClients.isNotEmpty()) {
            tetheringClients = MutableScatterMap()
            populateClients()
        }
    }

    private suspend fun handleClientsChanged(
        flow: Flow<TetheringManagerCompat.Event.ClientsChanged>,
    ) = flow.collect { event ->
        val tetheringClients = MutableScatterMap<MacAddress, TetheredClientInfo>()
        for (client in event.clients) {
            tetheringClients[client.macAddress] = TetheredClientInfo(
                TetherType.fromTetheringType(client.tetheringType),
                client.addresses.map {
                    val address = it.address
                    ClientAddressInfo(
                        NeighbourState.NEIGHBOUR_STATE_UNSET,
                        address,
                        it.hostname,
                    ).also { info ->
                        // https://cs.android.com/android/platform/superproject/main/+/main:packages/modules/Connectivity/Tethering/src/android/net/ip/IpServer.java;l=516;drc=efb735f4d5a2f04550e33e8aa9485f906018fe4e
                        if (address.flags != 0 || address.scope != OsConstants.RT_SCOPE_UNIVERSE ||
                            info.deprecationTime != info.expirationTime) {
                            Timber.w("$address, ${address.flags}, ${address.scope}, ${info.deprecationTime}, ${
                                info.expirationTime}")
                        }
                    }
                },
            )
        }
        this.tetheringClients = tetheringClients
        populateClients()
    }

    @RequiresApi(30)
    private fun parseWifiApClients(clients: List<WifiClient>): MutableScatterSet<Pair<String, MacAddress>>? {
        return MutableScatterSet<Pair<String, MacAddress>>(clients.size).apply {
            // Without AP instance identifiers, keep netlink as the activity source.
            for (client in clients) add((client.apInstanceIdentifierOrNull ?: return null) to client.macAddress)
        }
    }
    @RequiresApi(30)
    private suspend fun collectSoftApCallbacks(source: WifiApClientSource, flow: Flow<WifiApManager.Event>) {
        try {
            flow.collect { event ->
                when (event) {
                    is WifiApManager.Event.OnConnectedClientsChanged -> {
                        when (source) {
                            WifiApClientSource.Tethered -> wifiAp = parseWifiApClients(event.clients)
                            WifiApClientSource.LocalOnly -> localOnlyHotspot = parseWifiApClients(event.clients)
                        }
                        populateClients()
                    }
                    is WifiApManager.Event.OnBlockedClientConnecting -> {
                        val macAddress = event.client.macAddress
                        var name = macAddress.toString()
                        event.client.apInstanceIdentifierOrNull?.let { name += "%$it" }
                        val reason = softApClientBlockReasonLabel(app, event.blockedReason)
                        Timber.i("$name blocked from connecting: $reason (${event.blockedReason})")
                        SmartSnackbar.make(
                            app.getString(R.string.tethering_manage_wifi_client_blocked, name, reason),
                        ).apply {
                            action(R.string.tethering_manage_wifi_copy_mac) {
                                app.clipboard.setPrimaryClip(ClipData.newPlainText(null, macAddress.toString()))
                            }
                        }.show()
                    }
                    is WifiApManager.Event.OnClientsDisconnected -> event.clients.forEach { client ->
                        val macAddress = client.macAddress
                        var name = macAddress.toString()
                        client.apInstanceIdentifierOrNull?.let { name += "%$it" }
                        val reason = softApClientDisconnectReasonLabel(app, client.disconnectReason)
                        Timber.i("$client disconnected: $reason")
                        SmartSnackbar.make(
                            app.getString(R.string.tethering_manage_wifi_client_disconnected, name, reason),
                        ).apply {
                            action(R.string.tethering_manage_wifi_copy_mac) {
                                app.clipboard.setPrimaryClip(ClipData.newPlainText(null, macAddress.toString()))
                            }
                        }.show()
                    }
                    else -> { }
                }
            }
        } catch (e: CancellationException) {
            throw e
        } catch (e: WifiApManager.SoftApCallbackUnavailableException) {
            if (e.cause == null) Timber.d(e) else Timber.w(e)
        } catch (e: Exception) {
            Timber.w(e)
        }
        when (source) {
            WifiApClientSource.Tethered -> wifiAp = null
            WifiApClientSource.LocalOnly -> localOnlyHotspot = null
        }
        populateClients()
    }

    private fun populateClients() {
        val clients = MutableScatterMap<Pair<String?, MacAddress>, Client>()
        fun canonicalIface(iface: String) = netlinkSnapshot.bridgeMasterByMember[iface] ?: iface
        fun rowIface(iface: String?) = iface?.let { canonicalIface(it) }
        fun getClient(mac: MacAddress, iface: String?, type: TetherType = TetherType.ofInterface(rowIface(iface))) =
            (rowIface(iface) to mac).let { key ->
                clients[key] ?: Client(mac, key.first, type).also { clients[key] = it }
            }
        val repeater = repeater.value
        val p2pInterface = if (repeater?.active?.value == true) repeater.group.value?.`interface` else null
        val wifiAp = wifiAp
        val localOnlyHotspot = localOnlyHotspot
        p2pInterface?.let { p2pInterface ->
            for (client in p2p) {
                val addr = MacAddress.fromString(client.deviceAddress!!)
                getClient(addr, p2pInterface, TetherType.WIFI_P2P).apply {
                    addSource(p2pInterface)
                    active = true
                    // WiFi mainline module might be backported to API 30
                    if (Build.VERSION.SDK_INT >= 30) try {
                        client.ipAddress
                    } catch (e: NoSuchMethodError) {
                        if (Build.VERSION.SDK_INT >= 35) Timber.w(e)
                        null
                    }?.let { ip[it] = ClientAddressInfo() }
                }
            }
        }
        wifiAp?.forEach { client ->
            getClient(client.second, client.first, TetherType.WIFI).apply {
                addSource(client.first)
                active = true
            }
        }
        localOnlyHotspot?.forEach { client ->
            getClient(client.second, client.first, TetherType.WIFI).apply {
                addSource(client.first)
                active = true
            }
        }
        for (neighbour in netlinkSnapshot.neighbours) {
            val lladdr = neighbour.lladdr ?: continue
            val iface = canonicalIface(neighbour.dev)
            var client = clients[iface to lladdr]
            if (client == null) {
                if (!tetheredInterfaces.contains(iface)) continue
                client = getClient(lladdr, neighbour.dev)
            }
            client.addSource(neighbour.dev)
            val type = TetherType.ofInterface(iface)
            if (neighbour.state == NeighbourState.NEIGHBOUR_STATE_VALID &&
                (p2pInterface == null || type != TetherType.WIFI_P2P) &&
                (!type.isWifi || type == TetherType.WIFI_P2P ||
                        (if (iface in localOnlyInterfaces) localOnlyHotspot else wifiAp) == null)) client.active = true
            client.ip.compute(neighbour.ip) { _, info ->
                info?.apply { state = neighbour.state } ?: ClientAddressInfo(neighbour.state)
            }
        }
        tetheringClients.forEach { mac, tetheringClient ->
            var bestClient: Client? = null
            clients.forEach { key, client ->
                if (key.second != mac) return@forEach
                if (key.first != null && TetherType.ofInterface(key.first).isA(tetheringClient.fallbackType)) {
                    bestClient = client
                } else if (bestClient == null) bestClient = client
            }
            if (bestClient == null) bestClient = Client(mac, null, tetheringClient.fallbackType).also {
                clients[null to mac] = it
            }
            bestClient.active = true
            for (info in tetheringClient.addresses) bestClient.ip.compute(info.address!!.address) { _, oldInfo ->
                info.copy(state = oldInfo?.state ?: NeighbourState.NEIGHBOUR_STATE_UNSET)
            }
        }
        val result = ArrayList<Client>(clients.size)
        clients.forEachValue { result.add(it) }
        result.sortWith(compareBy<Client> { it.iface }.thenBy { it.macString })
        clientsState.value = result
    }

    private fun updateTrafficRates(
        newRecords: ObjectList<TrafficRecord>,
        oldRecords: LongObjectMap<TrafficRecord>,
    ) {
        val clients = clientsState.value
        val rates = HashMap<Pair<String?, MacAddress>, TrafficRate>()
        val rateKeys = MutableScatterMap<Pair<String, MacAddress>, Pair<String?, MacAddress>>()
        clients.forEach { client ->
            client.ifaces.forEach { rateKeys[it to client.mac] = client.iface to client.mac }
        }
        newRecords.forEach { newRecord ->
            val oldRecord = oldRecords[newRecord.previousId ?: return@forEach] ?: return@forEach
            val elapsed = newRecord.timestamp - oldRecord.timestamp
            if (elapsed <= 0L) {
                if (elapsed < 0L ||
                    newRecord.sentPackets != oldRecord.sentPackets || newRecord.sentBytes != oldRecord.sentBytes ||
                    newRecord.receivedPackets != oldRecord.receivedPackets ||
                    newRecord.receivedBytes != oldRecord.receivedBytes) {
                    Timber.w(Exception("Traffic counters changed without positive elapsed time ($elapsed ms): old=${
                        oldRecord} new=$newRecord"))
                }
                return@forEach
            }
            val key = rateKeys[newRecord.downstream to newRecord.mac] ?: (newRecord.downstream to newRecord.mac)
            val rate = rates[key] ?: TrafficRate()
            rates[key] = TrafficRate(
                send = rate.send + (newRecord.sentBytes - oldRecord.sentBytes) * 1000 / elapsed,
                receive = rate.receive + (newRecord.receivedBytes - oldRecord.receivedBytes) * 1000 / elapsed,
            )
        }
        trafficRates.value = rates
    }

    suspend fun updateNickname(mac: MacAddress, nickname: AnnotatedString) {
        MacLookup.abort(mac)
        withContext(Dispatchers.IO) {
            AppDatabase.instance.clientRecordDao.upsert(mac) {
                this.nickname = nickname
            }
        }
    }

    suspend fun toggleBlocked(row: ClientRowState) = withContext(Dispatchers.IO) {
        val wasWorking = TrafficRecorder.isWorking(row.client.mac)
        AppDatabase.instance.clientRecordDao.update(row.record.copy(blocked = !row.record.blocked))
        !wasWorking && !row.record.blocked
    }

    suspend fun queryStats(mac: MacAddress): ClientStats = withContext(Dispatchers.IO) {
        AppDatabase.instance.trafficRecordDao.queryStats(mac)
    }

    fun performMacLookup(mac: MacAddress, explicit: Boolean = false) = MacLookup.perform(viewModelScope, mac, explicit)

    private fun refreshP2p() {
        val repeater = repeater.value
        p2p = (if (repeater?.active?.value == true) repeater.group.value?.clientList else null) ?: emptyList()
        populateClients()
    }

    override fun onCreate(owner: LifecycleOwner) {
        owner.lifecycle.coroutineScope.launch {
            owner.repeatOnLifecycle(Lifecycle.State.STARTED) {
                launch {
                    NetlinkNeighbour.monitorSnapshots.collect {
                        netlinkSnapshot = it
                        populateClients()
                    }
                }
                launch {
                    repeater.collectLatest { service ->
                        service ?: return@collectLatest
                        merge(service.active, service.group).collect { refreshP2p() }
                    }
                }
                if (Build.VERSION.SDK_INT >= 30) {
                    launch {
                        collectSoftApCallbacks(
                            WifiApClientSource.Tethered,
                            WifiApCommands.softApCallbackFlow(expensive = true),
                        )
                    }
                    if (Build.VERSION.SDK_INT >= 33) launch {
                        collectSoftApCallbacks(
                            WifiApClientSource.LocalOnly,
                            WifiApCommands.localOnlyHotspotSoftApCallbackFlow(expensive = true),
                        )
                    }
                }
            }
        }
    }

    override fun onStart(owner: LifecycleOwner) {
        startedJob = viewModelScope.launch {
            if (Services.p2p != null) launch {
                try {
                    app.bindServiceFlow(Intent(app, RepeaterService::class.java)).collect {
                        repeater.value = it as RepeaterService.Binder?
                    }
                } finally {
                    repeater.value = null
                }
            }
            TetherStates.flow.collect { states ->
                tetherStatesState.value = states
                localOnlyInterfaces = states.localOnly
                tetheredInterfaces = states.tethered + states.localOnly
                populateClients()
            }
        }
    }
    override fun onStop(owner: LifecycleOwner) {
        startedJob?.cancel()
        startedJob = null
    }
}
