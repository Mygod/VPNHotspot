package be.mygod.vpnhotspot.client

import androidx.recyclerview.widget.DiffUtil
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.net.IpNeighbour
import be.mygod.vpnhotspot.net.TetherType
import be.mygod.vpnhotspot.room.AppDatabase
import be.mygod.vpnhotspot.room.lookup
import be.mygod.vpnhotspot.room.macToLong
import be.mygod.vpnhotspot.util.onEmpty
import java.util.*

abstract class Client {
    companion object DiffCallback : DiffUtil.ItemCallback<Client>() {
        override fun areItemsTheSame(oldItem: Client, newItem: Client) =
                oldItem.iface == newItem.iface && oldItem.mac == newItem.mac
        override fun areContentsTheSame(oldItem: Client, newItem: Client) = oldItem == newItem
    }

    abstract val iface: String
    abstract val mac: String
    private val macIface get() = "$mac%$iface"
    val ip = TreeMap<String, IpNeighbour.State>()
    val record by lazy { AppDatabase.instance.clientRecordDao.lookup(mac.macToLong()) }

    open val icon get() = TetherType.ofInterface(iface).icon
    val title get() = record.nickname.onEmpty(macIface)
    val description: String get() {
        val result = StringBuilder(if (record.nickname.isEmpty()) "" else "$macIface\n")
        ip.entries.forEach { (ip, state) ->
            result.appendln(app.getString(when (state) {
                IpNeighbour.State.INCOMPLETE -> R.string.connected_state_incomplete
                IpNeighbour.State.VALID -> R.string.connected_state_valid
                IpNeighbour.State.FAILED -> R.string.connected_state_failed
                else -> throw IllegalStateException("Invalid IpNeighbour.State: $state")
            }, ip))
        }
        return result.toString().trimEnd()
    }

    override fun equals(other: Any?): Boolean {
        if (this === other) return true
        if (javaClass != other?.javaClass) return false

        other as Client

        if (iface != other.iface) return false
        if (mac != other.mac) return false
        if (ip != other.ip) return false
        if (record != other.record) return false

        return true
    }
    override fun hashCode() = Objects.hash(iface, mac, ip)
}
