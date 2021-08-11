package be.mygod.vpnhotspot.client

import android.content.ComponentName
import android.content.IntentFilter
import android.content.ServiceConnection
import android.net.wifi.p2p.WifiP2pDevice
import android.os.Build
import android.os.IBinder
import android.os.Parcelable
import androidx.annotation.RequiresApi
import androidx.lifecycle.DefaultLifecycleObserver
import androidx.lifecycle.LifecycleOwner
import androidx.lifecycle.MutableLiveData
import androidx.lifecycle.ViewModel
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.RepeaterService
import be.mygod.vpnhotspot.net.IpNeighbour
import be.mygod.vpnhotspot.net.MacAddressCompat
import be.mygod.vpnhotspot.net.MacAddressCompat.Companion.toCompat
import be.mygod.vpnhotspot.net.TetherType
import be.mygod.vpnhotspot.net.TetheringManager
import be.mygod.vpnhotspot.net.TetheringManager.localOnlyTetheredIfaces
import be.mygod.vpnhotspot.net.TetheringManager.tetheredIfaces
import be.mygod.vpnhotspot.net.monitor.IpNeighbourMonitor
import be.mygod.vpnhotspot.net.wifi.WifiApManager
import be.mygod.vpnhotspot.net.wifi.WifiClient
import be.mygod.vpnhotspot.root.WifiApCommands
import be.mygod.vpnhotspot.util.broadcastReceiver

class ClientViewModel : ViewModel(), ServiceConnection, IpNeighbourMonitor.Callback, DefaultLifecycleObserver,
    WifiApManager.SoftApCallbackCompat {
    private var tetheredInterfaces = emptySet<String>()
    private val receiver = broadcastReceiver { _, intent ->
        tetheredInterfaces = (intent.tetheredIfaces ?: return@broadcastReceiver).toSet() +
                (intent.localOnlyTetheredIfaces ?: return@broadcastReceiver)
        populateClients()
    }

    private var repeater: RepeaterService.Binder? = null
    private var p2p: Collection<WifiP2pDevice> = emptyList()
    private var wifiAp = emptyList<Pair<String, MacAddressCompat>>()
    private var neighbours: Collection<IpNeighbour> = emptyList()
    val clients = MutableLiveData<List<Client>>()
    val fullMode = object : DefaultLifecycleObserver {
        override fun onStart(owner: LifecycleOwner) {
            IpNeighbourMonitor.registerCallback(this@ClientViewModel, true)
        }
        override fun onStop(owner: LifecycleOwner) {
            IpNeighbourMonitor.registerCallback(this@ClientViewModel, false)
        }
    }

    private fun populateClients() {
        val clients = HashMap<Pair<String, MacAddressCompat>, Client>()
        repeater?.group?.`interface`?.let { p2pInterface ->
            for (client in p2p) {
                val addr = MacAddressCompat.fromString(client.deviceAddress!!)
                clients[p2pInterface to addr] = object : Client(addr, p2pInterface) {
                    override val icon: Int get() = TetherType.WIFI_P2P.icon
                }
            }
        }
        for (client in wifiAp) {
            clients[client] = object : Client(client.second, client.first) {
                override val icon: Int get() = TetherType.WIFI.icon
            }
        }
        for (neighbour in neighbours) {
            val key = neighbour.dev to neighbour.lladdr
            var client = clients[key]
            if (client == null) {
                if (!tetheredInterfaces.contains(neighbour.dev)) continue
                client = Client(neighbour.lladdr, neighbour.dev)
                clients[key] = client
            }
            client.ip += Pair(neighbour.ip, neighbour.state)
        }
        this.clients.postValue(clients.values.sortedWith(compareBy<Client> { it.iface }.thenBy { it.macString }))
    }

    private fun refreshP2p() {
        val repeater = repeater
        p2p = (if (repeater?.active != true) null else repeater.group?.clientList) ?: emptyList()
        populateClients()
    }

    override fun onStart(owner: LifecycleOwner) {
        app.registerReceiver(receiver, IntentFilter(TetheringManager.ACTION_TETHER_STATE_CHANGED))
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
            client.apInstanceIdentifier?.run { this to client.macAddress.toCompat() }
        }
    }
}
