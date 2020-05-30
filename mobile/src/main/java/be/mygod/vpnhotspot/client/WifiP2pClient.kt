package be.mygod.vpnhotspot.client

import android.net.wifi.p2p.WifiP2pDevice
import be.mygod.vpnhotspot.net.MacAddressCompat
import be.mygod.vpnhotspot.net.TetherType

class WifiP2pClient(p2pInterface: String, p2p: WifiP2pDevice) :
        Client(MacAddressCompat.fromString(p2p.deviceAddress!!), p2pInterface) {
    override val icon: Int get() = TetherType.WIFI_P2P.icon
}
