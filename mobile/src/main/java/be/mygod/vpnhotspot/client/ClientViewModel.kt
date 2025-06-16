package be.mygod.vpnhotspot.client

import android.content.ClipData
import android.content.ComponentName
import android.content.IntentFilter
import android.content.ServiceConnection
import android.net.LinkAddress
import android.net.MacAddress
import android.net.wifi.p2p.WifiP2pDevice
import android.os.Build
import android.os.IBinder
import android.os.Parcelable
import android.system.OsConstants
import androidx.annotation.RequiresApi
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.MutableLiveData
import androidx.lifecycle.ViewModel
import androidx.lifecycle.lifecycleScope
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.RepeaterService
import be.mygod.vpnhotspot.net.IpNeighbour
import be.mygod.vpnhotspot.net.TetherType
import be.mygod.vpnhotspot.net.TetheringManagerCompat
import be.mygod.vpnhotspot.net.TetheringManagerCompat.localOnlyTetheredIfaces
import be.mygod.vpnhotspot.net.TetheringManagerCompat.tetheredIfaces
import be.mygod.vpnhotspot.net.monitor.IpNeighbourMonitor
import be.mygod.vpnhotspot.net.wifi.WifiApManager
import be.mygod.vpnhotspot.net.wifi.WifiClient
import be.mygod.vpnhotspot.root.RootManager
import be.mygod.vpnhotspot.root.TetheringCommands
import be.mygod.vpnhotspot.root.WifiApCommands
import be.mygod.vpnhotspot.util.broadcastReceiver
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Job
import kotlinx.coroutines.channels.ReceiveChannel
import kotlinx.coroutines.channels.consumeEach
import kotlinx.coroutines.launch
import timber.log.Timber

class ClientViewModel : ViewModel(), ServiceConnection, IpNeighbourMonitor.Callback, DefaultLifecycleObserver,
    WifiApManager.SoftApCallbackCompat, TetheringManagerCompat.TetheringEventCallback {
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

    private var tetheredInterfaces = emptySet<String>()
    private val receiver = broadcastReceiver { _, intent ->
        tetheredInterfaces = (intent.tetheredIfaces ?: return@broadcastReceiver).toSet() +
                (intent.localOnlyTetheredIfaces ?: return@broadcastReceiver)
        populateClients()
    }

    private var repeater: RepeaterService.Binder? = null
    private var p2p: Collection<WifiP2pDevice> = emptyList()
    private var wifiAp = emptyList<Pair<String, MacAddress>>()
    private var neighbours: Collection<IpNeighbour> = emptyList()
    private var tetheringClients = emptyMap<MacAddress, TetheredClient>()
    val clients = MutableLiveData<List<Client>>()
    private var rootCallbackJob: Job? = null
    val fullMode = object : DefaultLifecycleObserver {
        override fun onStart(owner: LifecycleOwner) {
            IpNeighbourMonitor.registerCallback(this@ClientViewModel, true)
            if (Build.VERSION.SDK_INT >= 30) rootCallbackJob = owner.lifecycleScope.launch {
                try {
                    RootManager.use {
                        handleClientsChanged(it.create(TetheringCommands.RegisterTetheringEventCallback(),
                            owner.lifecycleScope))
                    }
                } catch (_: CancellationException) {
                } catch (e: Exception) {
                    Timber.w(e)
                    SmartSnackbar.make(e).show()
                }
            }
        }
        override fun onStop(owner: LifecycleOwner) {
            IpNeighbourMonitor.registerCallback(this@ClientViewModel, false)
        }
    }

    private suspend fun handleClientsChanged(
        channel: ReceiveChannel<TetheringCommands.OnClientsChanged>,
    ) = channel.consumeEach { event ->
        tetheringClients = event.clients.associate { client ->
            getMacAddress(client) as MacAddress to TetheredClient(
                TetherType.fromTetheringType(getTetheringType(client) as Int), (getAddresses(client) as List<*>).map {
                    val address = getAddress(it) as LinkAddress
                    ClientAddressInfo(IpNeighbour.State.UNSET, address, getHostname(it) as String?).also { info ->
                        // https://cs.android.com/android/platform/superproject/main/+/main:packages/modules/Connectivity/Tethering/src/android/net/ip/IpServer.java;l=516;drc=efb735f4d5a2f04550e33e8aa9485f906018fe4e
                        if (address.flags != 0 || address.scope != OsConstants.RT_SCOPE_UNIVERSE ||
                            info.deprecationTime != info.expirationTime) {
                            Timber.w("$address, ${address.flags}, ${address.scope}, ${info.deprecationTime}, " +
                                    info.expirationTime)
                        }
                    }
                })
        }
        populateClients()
    }

    private fun populateClients() {
        val clients = HashMap<Pair<String?, MacAddress>, Client>()
        repeater?.group?.`interface`?.let { p2pInterface ->
            for (client in p2p) {
                val addr = MacAddress.fromString(client.deviceAddress!!)
                clients[p2pInterface to addr] = Client(addr, p2pInterface, TetherType.WIFI_P2P).apply {
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
        for (client in wifiAp) clients[client] = Client(client.second, client.first, TetherType.WIFI)
        for (neighbour in neighbours) {
            val key = neighbour.dev to neighbour.lladdr
            var client = clients[key]
            if (client == null) {
                if (!tetheredInterfaces.contains(neighbour.dev)) continue
                client = Client(neighbour.lladdr, neighbour.dev)
                clients[key] = client
            }
            client.ip.compute(neighbour.ip) { _, info ->
                info?.apply { state = neighbour.state } ?: ClientAddressInfo(neighbour.state)
            }
        }
        for ((mac, tetheringClient) in tetheringClients) {
            var bestClient: Client? = null
            for ((key, client) in clients) if (key.second == mac) {
                if (key.first != null && TetherType.ofInterface(key.first).isA(tetheringClient.fallbackType)) {
                    bestClient = client
                    break
                }
                if (bestClient == null) bestClient = client
            }
            if (bestClient == null) bestClient = Client(mac, null, tetheringClient.fallbackType).also {
                clients[null to mac] = it
            }
            for (info in tetheringClient.addresses) bestClient.ip.compute(info.address!!.address) { _, oldInfo ->
                oldInfo?.apply { info.state = state }
                info
            }
        }
        this.clients.postValue(clients.values.sortedWith(compareBy<Client> { it.iface }.thenBy { it.macString }))
    }

    private fun refreshP2p() {
        val repeater = repeater
        p2p = (if (repeater?.active != true) null else repeater.group?.clientList) ?: emptyList()
        populateClients()
    }

    override fun onStart(owner: LifecycleOwner) {
        app.registerReceiver(receiver, IntentFilter(TetheringManagerCompat.ACTION_TETHER_STATE_CHANGED))
        IpNeighbourMonitor.registerCallback(this, false)
        if (Build.VERSION.SDK_INT >= 31) WifiApCommands.registerSoftApCallback(this)
    }
    override fun onStop(owner: LifecycleOwner) {
        if (Build.VERSION.SDK_INT >= 31) WifiApCommands.unregisterSoftApCallback(this)
        IpNeighbourMonitor.unregisterCallback(this)
        app.unregisterReceiver(receiver)
    }

    override fun onServiceConnected(name: ComponentName?, service: IBinder?) {
        val binder = service as RepeaterService.Binder
        repeater = binder
        binder.statusChanged[this] = this::refreshP2p
        binder.groupChanged[this] = { refreshP2p() }
    }

    override fun onServiceDisconnected(name: ComponentName?) {
        val binder = repeater ?: return
        repeater = null
        binder.statusChanged -= this
        binder.groupChanged -= this
    }

    override fun onIpNeighbourAvailable(neighbours: Collection<IpNeighbour>) {
        this.neighbours = neighbours
        populateClients()
    }

    @RequiresApi(31)
    override fun onConnectedClientsChanged(clients: List<Parcelable>) {
        wifiAp = clients.mapNotNull {
            val client = WifiClient(it)
            client.apInstanceIdentifier?.run { this to client.macAddress }
        }
    }

    @RequiresApi(30)
    override fun onBlockedClientConnecting(client: Parcelable, blockedReason: Int) {
        val client = WifiClient(client)
        val macAddress = client.macAddress
        var name = macAddress.toString()
        if (Build.VERSION.SDK_INT >= 31) client.apInstanceIdentifier?.let { name += "%$it" }
        val reason = WifiApManager.clientBlockLookup(blockedReason, true)
        Timber.i("$name blocked from connecting: $reason ($blockedReason)")
        SmartSnackbar.make(app.getString(R.string.tethering_manage_wifi_client_blocked, name, reason)).apply {
            action(R.string.tethering_manage_wifi_copy_mac) {
                app.clipboard.setPrimaryClip(ClipData.newPlainText(null, macAddress.toString()))
            }
        }.show()
    }
}
