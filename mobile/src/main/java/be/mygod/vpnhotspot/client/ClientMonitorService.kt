package be.mygod.vpnhotspot.client

import android.app.Service
import android.content.*
import android.net.wifi.p2p.WifiP2pDevice
import android.os.IBinder
import be.mygod.vpnhotspot.RepeaterService
import be.mygod.vpnhotspot.net.IpNeighbour
import be.mygod.vpnhotspot.net.IpNeighbourMonitor
import be.mygod.vpnhotspot.net.TetheringManager
import be.mygod.vpnhotspot.util.StickyEvent1
import be.mygod.vpnhotspot.util.broadcastReceiver
import be.mygod.vpnhotspot.util.stopAndUnbind

class ClientMonitorService : Service(), ServiceConnection, IpNeighbourMonitor.Callback {
    inner class Binder : android.os.Binder() {
        val clientsChanged = StickyEvent1 { clients }
    }
    private val binder = Binder()
    override fun onBind(intent: Intent?) = binder

    private var tetheredInterfaces = emptySet<String>()
    private val receiver = broadcastReceiver { _, intent ->
        val extras = intent.extras!!
        tetheredInterfaces = TetheringManager.getTetheredIfaces(extras).toSet() +
                TetheringManager.getLocalOnlyTetheredIfaces(extras)
        populateClients()
    }

    private var repeater: RepeaterService.Binder? = null
    private var p2p: Collection<WifiP2pDevice> = emptyList()
    private var neighbours = emptyList<IpNeighbour>()
    private var clients = emptyList<Client>()
        private set(value) {
            field = value
            binder.clientsChanged(value)
        }

    private fun populateClients() {
        val clients = HashMap<Pair<String, String>, Client>()
        val group = repeater?.service?.group
        val p2pInterface = group?.`interface`
        if (p2pInterface != null) {
            for (client in p2p) clients[Pair(p2pInterface, client.deviceAddress)] = WifiP2pClient(p2pInterface, client)
        }
        for (neighbour in neighbours) {
            val key = Pair(neighbour.dev, neighbour.lladdr)
            var client = clients[key]
            if (client == null) {
                if (!tetheredInterfaces.contains(neighbour.dev)) continue
                client = TetheringClient(neighbour)
                clients[key] = client
            }
            client.ip += Pair(neighbour.ip, neighbour.state)
        }
        this.clients = clients.values.sortedWith(compareBy<Client> { it.iface }.thenBy { it.mac })
    }

    private fun refreshP2p() {
        val repeater = repeater
        p2p = (if (repeater?.active != true) null else repeater.service.group?.clientList) ?: emptyList()
        populateClients()
    }

    override fun onCreate() {
        super.onCreate()
        bindService(Intent(this, RepeaterService::class.java), this, Context.BIND_AUTO_CREATE)
        IpNeighbourMonitor.registerCallback(this)
        registerReceiver(receiver, IntentFilter(TetheringManager.ACTION_TETHER_STATE_CHANGED))
    }

    override fun onDestroy() {
        unregisterReceiver(receiver)
        IpNeighbourMonitor.unregisterCallback(this)
        stopAndUnbind(this)
        super.onDestroy()
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
