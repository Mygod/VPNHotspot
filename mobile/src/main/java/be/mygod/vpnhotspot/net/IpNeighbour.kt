package be.mygod.vpnhotspot.net

import android.net.MacAddress
import java.net.InetAddress

data class IpNeighbour(val ip: InetAddress, val dev: String, val lladdr: MacAddress, val state: State) {
    enum class State {
        UNSET, INCOMPLETE, VALID, FAILED, DELETING
    }
}

data class IpDev(val ip: InetAddress, val dev: String) {
    override fun toString() = "$ip%$dev"
}
fun IpDev(neighbour: IpNeighbour) = IpDev(neighbour.ip, neighbour.dev)
