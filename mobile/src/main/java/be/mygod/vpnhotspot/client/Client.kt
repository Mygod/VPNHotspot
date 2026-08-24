package be.mygod.vpnhotspot.client

import android.net.MacAddress
import android.net.wifi.p2p.WifiP2pConnectionInfo
import be.mygod.vpnhotspot.net.InetAddressComparator
import be.mygod.vpnhotspot.net.TetherType
import java.net.InetAddress
import java.util.Objects
import java.util.TreeMap

class Client(val mac: MacAddress, iface: String? = null, val type: TetherType = TetherType.ofInterface(iface)) {
    val ip = TreeMap<InetAddress, ClientAddressInfo>(InetAddressComparator)
    val ifaces = LinkedHashSet<String>().also { iface?.let(it::add) }
    val iface get() = ifaces.firstOrNull()
    val macString by lazy { mac.toString() }
    var active = false
    var wifiP2pConnectionInfo: WifiP2pConnectionInfo? = null

    val icon get() = type.icon

    fun addSource(iface: String?) {
        ifaces.add(iface ?: return)
    }

    override fun equals(other: Any?): Boolean {
        if (this === other) return true
        if (other !is Client) return false

        if (iface != other.iface) return false
        if (mac != other.mac) return false
        if (type != other.type) return false
        if (active != other.active) return false
        if (wifiP2pConnectionInfo != other.wifiP2pConnectionInfo) return false
        if (ip != other.ip) return false
        if (ifaces != other.ifaces) return false

        return true
    }
    override fun hashCode() = Objects.hash(mac, type, active, wifiP2pConnectionInfo, ip, ifaces)
}
