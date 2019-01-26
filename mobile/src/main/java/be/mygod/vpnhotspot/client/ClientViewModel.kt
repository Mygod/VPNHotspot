package be.mygod.vpnhotspot.client

import android.content.ComponentName
import android.content.IntentFilter
import android.content.ServiceConnection
import android.net.wifi.p2p.WifiP2pDevice
import android.os.IBinder
import androidx.lifecycle.MutableLiveData
import androidx.lifecycle.ViewModel
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.RepeaterService
import be.mygod.vpnhotspot.net.IpNeighbour
import be.mygod.vpnhotspot.net.TetheringManager
import be.mygod.vpnhotspot.net.monitor.IpNeighbourMonitor
import be.mygod.vpnhotspot.room.macToLong
import be.mygod.vpnhotspot.util.broadcastReceiver

class ClientViewModel : ViewModel(), ServiceConnection, IpNeighbourMonitor.Callback {
    private var tetheredInterfaces = emptySet<String>()
    private val receiver = broadcastReceiver { _, intent ->
        val extras = intent.extras ?: return@broadcastReceiver
        tetheredInterfaces = TetheringManager.getTetheredIfaces(extras).toSet() +
                TetheringManager.getLocalOnlyTetheredIfaces(extras)
        populateClients()
    }

    private var repeater: RepeaterService.Binder? = null
    private var p2p: Collection<WifiP2pDevice> = emptyList()
    private var neighbours = emptyList<IpNeighbour>()
    val clients = MutableLiveData<List<Client>>()

    private fun populateClients() {
        val clients = HashMap<Pair<String, Long>, Client>()
        val group = repeater?.group
        val p2pInterface = group?.`interface`
        if (p2pInterface != null) {
            for (client in p2p) clients[Pair(p2pInterface, client.deviceAddress.macToLong())] =
                    WifiP2pClient(p2pInterface, client)
        }
        for (neighbour in neighbours) {
            val key = Pair(neighbour.dev, neighbour.lladdr)
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

    init {
        app.registerReceiver(receiver, IntentFilter(TetheringManager.ACTION_TETHER_STATE_CHANGED))
        IpNeighbourMonitor.registerCallback(this)
    }

    override fun onCleared() {
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

    override fun onIpNeighbourAvailable(neighbours: List<IpNeighbour>) {
        this.neighbours = neighbours
        populateClients()
    }
}
