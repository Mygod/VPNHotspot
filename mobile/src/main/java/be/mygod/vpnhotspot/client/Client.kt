package be.mygod.vpnhotspot.client

import android.net.MacAddress
import androidx.recyclerview.widget.DiffUtil
import be.mygod.vpnhotspot.net.InetAddressComparator
import be.mygod.vpnhotspot.net.TetherType
import java.net.InetAddress
import java.util.Objects
import java.util.TreeMap

class Client(val mac: MacAddress, iface: String? = null, val type: TetherType = TetherType.ofInterface(iface)) {
    companion object DiffCallback : DiffUtil.ItemCallback<Client>() {
        override fun areItemsTheSame(oldItem: Client, newItem: Client) =
                oldItem.iface == newItem.iface && oldItem.type == newItem.type && oldItem.mac == newItem.mac
        override fun areContentsTheSame(oldItem: Client, newItem: Client) = oldItem == newItem
    }

    val ip = TreeMap<InetAddress, ClientAddressInfo>(InetAddressComparator)
    val ifaces = LinkedHashSet<String>().also { iface?.let(it::add) }
    val iface get() = ifaces.firstOrNull()
    val macString by lazy { mac.toString() }
    var active = false

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
        if (ip != other.ip) return false
        if (ifaces != other.ifaces) return false

        return true
    }
    override fun hashCode() = Objects.hash(mac, type, active, ip, ifaces)
}
