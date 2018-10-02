package be.mygod.vpnhotspot.client

import be.mygod.vpnhotspot.net.monitor.IpNeighbour

class TetheringClient(private val neighbour: IpNeighbour) : Client() {
    override val iface get() = neighbour.dev
    override val mac get() = neighbour.lladdr
}
