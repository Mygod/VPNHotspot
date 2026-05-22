package be.mygod.vpnhotspot.client

import android.content.ClipData
import android.content.ComponentName
import android.content.ServiceConnection
import android.net.LinkAddress
import android.net.MacAddress
import android.net.wifi.p2p.WifiP2pDevice
import android.os.Build
import android.os.IBinder
import android.os.Parcelable
import android.system.OsConstants
import androidx.annotation.RequiresApi
import androidx.collection.LongObjectMap
import androidx.collection.MutableScatterMap
import androidx.collection.ObjectList
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.ViewModel
import androidx.lifecycle.lifecycleScope
import androidx.lifecycle.repeatOnLifecycle
import androidx.lifecycle.viewModelScope
import androidx.compose.ui.text.AnnotatedString
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.RepeaterService
import be.mygod.vpnhotspot.net.NetlinkNeighbour
import be.mygod.vpnhotspot.net.TetherStates
import be.mygod.vpnhotspot.net.TetherType
import be.mygod.vpnhotspot.net.monitor.TrafficRecorder
import be.mygod.vpnhotspot.room.AppDatabase
import be.mygod.vpnhotspot.room.ClientRecord
import be.mygod.vpnhotspot.room.ClientStats
import be.mygod.vpnhotspot.room.TrafficRecord
import be.mygod.vpnhotspot.net.wifi.WifiApManager
import be.mygod.vpnhotspot.net.wifi.WifiClient
import be.mygod.vpnhotspot.root.RootManager
import be.mygod.vpnhotspot.root.TetheringCommands
import be.mygod.vpnhotspot.root.WifiApCommands
import be.mygod.vpnhotspot.root.daemon.NeighbourState
import be.mygod.vpnhotspot.ui.softApClientBlockReasonLabel
import be.mygod.vpnhotspot.ui.softApClientDisconnectReasonLabel
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.collections.immutable.persistentMapOf
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.Job
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.combine
import kotlinx.coroutines.flow.collectLatest
import kotlinx.coroutines.flow.flatMapLatest
import kotlinx.coroutines.flow.flowOf
import kotlinx.coroutines.flow.merge
import kotlinx.coroutines.flow.stateIn
import kotlinx.coroutines.flow.SharingStarted
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import timber.log.Timber

class ClientViewModel : ViewModel(), ServiceConnection, DefaultLifecycleObserver, WifiApManager.SoftApCallbackCompat,
    TetherStates.Callback {
    companion object {
        private val classTetheredClient by lazy { Class.forName("android.net.TetheredClient") }
        private val getMacAddress by lazy { classTetheredClient.getDeclaredMethod("getMacAddress") }
        private val getAddresses by lazy { classTetheredClient.getDeclaredMethod("getAddresses") }
        private val getTetheringType by lazy { classTetheredClient.getDeclaredMethod("getTetheringType") }

        private val classAddressInfo by lazy { Class.forName("android.net.TetheredClient\$AddressInfo") }
        private val getAddress by lazy { classAddressInfo.getDeclaredMethod("getAddress") }
        private val getHostname by lazy { classAddressInfo.getDeclaredMethod("getHostname") }
    }

    data class TetheredClient(val fallbackType: TetherType, val addresses: List<ClientAddressInfo>)
    data class TrafficRate(val send: Long = -1, val receive: Long = -1)
    data class ClientRowState(val client: Client, val record: ClientRecord, val rate: TrafficRate?)

    private val tetherStatesState = MutableStateFlow(TetherStates())
    val tetherStates = tetherStatesState.asStateFlow()
    private var tetheredInterfaces = emptySet<String>()
    override fun onTetherStatesChanged(states: TetherStates) {
        tetherStatesState.value = states
        tetheredInterfaces = states.tethered + states.localOnly
        populateClients()
    }

    private val repeater = MutableStateFlow<RepeaterService.Binder?>(null)
    private var p2p: Collection<WifiP2pDevice> = emptyList()
    private var wifiAp = MutableScatterMap<Pair<String, MacAddress>, Unit>()
    private var netlinkSnapshot = NetlinkNeighbour.Snapshot(emptyList(), persistentMapOf())
    private var tetheringClients = MutableScatterMap<MacAddress, TetheredClient>()
    private val clientsState = MutableStateFlow<List<Client>>(emptyList())
    val clients = clientsState.asStateFlow()
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
    val clientsFragmentObserver = object : DefaultLifecycleObserver {
        override fun onStart(owner: LifecycleOwner) {
            clientsScreenJob?.cancel()
            clientsScreenJob = owner.lifecycleScope.launch {
                if (Build.VERSION.SDK_INT >= 30) {
                    launch {
                        try {
                            RootManager.use {
                                handleClientsChanged(it.flow(TetheringCommands.RegisterTetheringEventCallback()))
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
                launch(Dispatchers.Default) {
                    TrafficRecorder.rescheduleUpdate()
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
    }

    private suspend fun handleClientsChanged(
        flow: Flow<TetheringCommands.OnClientsChanged>,
    ) = flow.collect { event ->
        val tetheringClients = MutableScatterMap<MacAddress, TetheredClient>()
        for (client in event.clients) {
            tetheringClients[getMacAddress(client) as MacAddress] = TetheredClient(
                TetherType.fromTetheringType(getTetheringType(client) as Int),
                (getAddresses(client) as List<*>).map {
                    val address = getAddress(it) as LinkAddress
                    ClientAddressInfo(
                        NeighbourState.NEIGHBOUR_STATE_UNSET,
                        address,
                        getHostname(it) as String?,
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

    private fun populateClients() {
        val clients = MutableScatterMap<Pair<String?, MacAddress>, Client>()
        fun canonicalIface(iface: String) = netlinkSnapshot.bridgeMasterByMember[iface] ?: iface
        fun rowIface(iface: String?) = iface?.let { canonicalIface(it) }
        fun getClient(mac: MacAddress, iface: String?, type: TetherType = TetherType.ofInterface(rowIface(iface))) =
            (rowIface(iface) to mac).let { key ->
                clients[key] ?: Client(mac, key.first, type).also { clients[key] = it }
            }
        repeater.value?.group?.value?.`interface`?.let { p2pInterface ->
            for (client in p2p) {
                val addr = MacAddress.fromString(client.deviceAddress!!)
                getClient(addr, p2pInterface, TetherType.WIFI_P2P).apply {
                    addSource(p2pInterface)
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
        wifiAp.forEach { client, _ ->
            getClient(client.second, client.first, TetherType.WIFI).addSource(client.first)
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
            for (info in tetheringClient.addresses) bestClient.ip.compute(info.address!!.address) { _, oldInfo ->
                oldInfo?.apply { info.state = state }
                info
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
        clients.forEach { rates[it.iface to it.mac] = TrafficRate() }
        val rateKeys = MutableScatterMap<Pair<String, MacAddress>, Pair<String?, MacAddress>>()
        clients.forEach { client ->
            client.ifaces.forEach { rateKeys[it to client.mac] = client.iface to client.mac }
        }
        newRecords.forEach { newRecord ->
            val oldRecord = oldRecords[newRecord.previousId ?: return@forEach] ?: return@forEach
            val elapsed = newRecord.timestamp - oldRecord.timestamp
            if (elapsed == 0L) {
                if (newRecord.sentPackets != oldRecord.sentPackets || newRecord.sentBytes != oldRecord.sentBytes ||
                    newRecord.receivedPackets != oldRecord.receivedPackets ||
                    newRecord.receivedBytes != oldRecord.receivedBytes) {
                    Timber.w(Exception("Traffic counters changed without elapsed time: old=$oldRecord new=$newRecord"))
                }
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
        p2p = (if (repeater?.active != true) null else repeater.group.value?.clientList) ?: emptyList()
        populateClients()
    }

    override fun onCreate(owner: LifecycleOwner) {
        owner.lifecycleScope.launch {
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
                        merge(service.status, service.group).collect { refreshP2p() }
                    }
                }
            }
        }
    }

    override fun onStart(owner: LifecycleOwner) {
        TetherStates.registerCallback(this)
        if (Build.VERSION.SDK_INT >= 31) WifiApCommands.registerSoftApCallback(this)
    }
    override fun onStop(owner: LifecycleOwner) {
        if (Build.VERSION.SDK_INT >= 31) WifiApCommands.unregisterSoftApCallback(this)
        TetherStates.unregisterCallback(this)
    }

    override fun onServiceConnected(name: ComponentName?, service: IBinder?) {
        val binder = service as RepeaterService.Binder
        repeater.value = binder
    }

    override fun onServiceDisconnected(name: ComponentName?) {
        repeater.value = null
    }

    @RequiresApi(31)
    override fun onConnectedClientsChanged(clients: List<Parcelable>) {
        val wifiAp = MutableScatterMap<Pair<String, MacAddress>, Unit>()
        for (it in clients) {
            val client = WifiClient(it)
            client.apInstanceIdentifier?.let { wifiAp[it to client.macAddress] = Unit }
        }
        this.wifiAp = wifiAp
        populateClients()
    }

    @RequiresApi(31)
    override fun onInfoChanged(info: List<Parcelable>) = populateClients()

    @RequiresApi(31)
    override fun onBlockedClientConnecting(client: Parcelable, blockedReason: Int) {
        val client = WifiClient(client)
        val macAddress = client.macAddress
        var name = macAddress.toString()
        client.apInstanceIdentifier?.let { name += "%$it" }
        val reason = softApClientBlockReasonLabel(app, blockedReason)
        Timber.i("$name blocked from connecting: $reason ($blockedReason)")
        SmartSnackbar.make(app.getString(R.string.tethering_manage_wifi_client_blocked, name, reason)).apply {
            action(R.string.tethering_manage_wifi_copy_mac) {
                app.clipboard.setPrimaryClip(ClipData.newPlainText(null, macAddress.toString()))
            }
        }.show()
    }

    @RequiresApi(36)
    override fun onClientsDisconnected(info: Parcelable, clients: List<Parcelable>) = clients.forEach { client ->
        val client = WifiClient(client)
        val macAddress = client.macAddress
        var name = macAddress.toString()
        client.apInstanceIdentifier?.let { name += "%$it" }
        val reason = softApClientDisconnectReasonLabel(app, client.disconnectReason)
        Timber.i("$client disconnected: $reason")
        SmartSnackbar.make(app.getString(R.string.tethering_manage_wifi_client_disconnected, name, reason)).apply {
            action(R.string.tethering_manage_wifi_copy_mac) {
                app.clipboard.setPrimaryClip(ClipData.newPlainText(null, macAddress.toString()))
            }
        }.show()
    }
}
