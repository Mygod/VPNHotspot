package be.mygod.vpnhotspot.client

import android.net.wifi.p2p.WifiP2pDevice
import be.mygod.vpnhotspot.net.TetherType
import be.mygod.vpnhotspot.room.macToLong

class WifiP2pClient(p2pInterface: String, p2p: WifiP2pDevice) : Client(p2p.deviceAddress!!.macToLong(), p2pInterface) {
    override val icon: Int get() = TetherType.WIFI_P2P.icon
}
