package be.mygod.vpnhotspot.client

import android.support.v7.util.DiffUtil
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.net.IpNeighbour
import be.mygod.vpnhotspot.net.TetherType
import java.util.*

abstract class Client {
    companion object DiffCallback : DiffUtil.ItemCallback<Client>() {
        override fun areItemsTheSame(oldItem: Client, newItem: Client) =
                oldItem.iface == newItem.iface && oldItem.mac == newItem.mac
        override fun areContentsTheSame(oldItem: Client, newItem: Client) = oldItem == newItem
    }

    abstract val iface: String
    abstract val mac: String
    val ip = TreeMap<String, IpNeighbour.State>()

    open val icon get() = TetherType.ofInterface(iface).icon
    val title get() = "$mac%$iface"
    val description get() = ip.entries.joinToString("\n") { (ip, state) ->
        app.getString(when (state) {
            IpNeighbour.State.INCOMPLETE -> R.string.connected_state_incomplete
            IpNeighbour.State.VALID -> R.string.connected_state_valid
            IpNeighbour.State.FAILED -> R.string.connected_state_failed
            else -> throw IllegalStateException("Invalid IpNeighbour.State: $state")
        }, ip)
    }

    override fun equals(other: Any?): Boolean {
        if (this === other) return true
        if (javaClass != other?.javaClass) return false

        other as Client

        if (iface != other.iface) return false
        if (mac != other.mac) return false
        if (ip != other.ip) return false

        return true
    }
    override fun hashCode() = Objects.hash(iface, mac, ip)
}
