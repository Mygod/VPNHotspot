package be.mygod.vpnhotspot.client

import androidx.lifecycle.Transformations
import androidx.recyclerview.widget.DiffUtil
import be.mygod.vpnhotspot.net.InetAddressComparator
import be.mygod.vpnhotspot.net.IpNeighbour
import be.mygod.vpnhotspot.net.TetherType
import be.mygod.vpnhotspot.room.AppDatabase
import be.mygod.vpnhotspot.room.ClientRecord
import be.mygod.vpnhotspot.room.macToString
import java.net.InetAddress
import java.util.*

open class Client(val mac: Long, val iface: String) {
    companion object DiffCallback : DiffUtil.ItemCallback<Client>() {
        override fun areItemsTheSame(oldItem: Client, newItem: Client) =
                oldItem.iface == newItem.iface && oldItem.mac == newItem.mac
        override fun areContentsTheSame(oldItem: Client, newItem: Client) = oldItem == newItem
    }

    val ip = TreeMap<InetAddress, IpNeighbour.State>(InetAddressComparator)
    val macString by lazy { mac.macToString() }
    val record = AppDatabase.instance.clientRecordDao.lookupSync(mac)

    val nickname get() = record.value?.nickname ?: ""
    val blocked get() = record.value?.blocked == true

    open val icon get() = TetherType.ofInterface(iface).icon
    val titleSelectable = Transformations.map(record) { it?.nickname.isNullOrEmpty() }

    fun obtainRecord() = record.value ?: ClientRecord(mac)

    override fun equals(other: Any?): Boolean {
        if (this === other) return true
        if (javaClass != other?.javaClass) return false

        other as Client

        if (iface != other.iface) return false
        if (mac != other.mac) return false
        if (ip != other.ip) return false
        if (record.value != other.record.value) return false

        return true
    }
    override fun hashCode() = Objects.hash(iface, mac, ip, record.value)
}
