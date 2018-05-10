package be.mygod.vpnhotspot.client

import android.net.wifi.p2p.WifiP2pDevice
import be.mygod.vpnhotspot.net.TetherType

class WifiP2pClient(p2pInterface: String, p2p: WifiP2pDevice) : Client() {
    override val iface = p2pInterface
    override val mac = p2p.deviceAddress ?: ""
    override val icon: Int get() = TetherType.WIFI_P2P.icon
}
