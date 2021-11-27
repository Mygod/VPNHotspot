package be.mygod.vpnhotspot.client

import android.text.SpannableStringBuilder
import android.text.Spanned
import android.text.style.StrikethroughSpan
import androidx.lifecycle.map
import androidx.recyclerview.widget.DiffUtil
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.net.InetAddressComparator
import be.mygod.vpnhotspot.net.IpNeighbour
import be.mygod.vpnhotspot.net.MacAddressCompat
import be.mygod.vpnhotspot.net.TetherType
import be.mygod.vpnhotspot.room.AppDatabase
import be.mygod.vpnhotspot.room.ClientRecord
import be.mygod.vpnhotspot.util.makeIpSpan
import be.mygod.vpnhotspot.util.makeMacSpan
import java.net.InetAddress
import java.util.*

open class Client(val mac: MacAddressCompat, val iface: String) {
    companion object DiffCallback : DiffUtil.ItemCallback<Client>() {
        override fun areItemsTheSame(oldItem: Client, newItem: Client) =
                oldItem.iface == newItem.iface && oldItem.mac == newItem.mac
        override fun areContentsTheSame(oldItem: Client, newItem: Client) = oldItem == newItem
    }

    val ip = TreeMap<InetAddress, IpNeighbour.State>(InetAddressComparator)
    val macString by lazy { mac.toString() }
    private val record = AppDatabase.instance.clientRecordDao.lookupOrDefaultSync(mac)
    private val macIface get() = SpannableStringBuilder(makeMacSpan(macString)).apply {
        append('%')
        append(iface)
    }

    val nickname get() = record.value?.nickname ?: ""
    val blocked get() = record.value?.blocked == true

    open val icon get() = TetherType.ofInterface(iface).icon
    val title = record.map { record ->
        /**
         * we hijack the get title process to check if we need to perform MacLookup,
         * as record might not be initialized in other more appropriate places
         */
        SpannableStringBuilder(record.nickname.ifEmpty {
            if (record.macLookupPending) MacLookup.perform(mac)
            macIface
        }).apply {
            if (record.blocked) setSpan(StrikethroughSpan(), 0, length, Spanned.SPAN_INCLUSIVE_INCLUSIVE)
        }
    }
    val titleSelectable = record.map { it.nickname.isEmpty() }
    val description = record.map { record ->
        SpannableStringBuilder().apply {
            if (record.nickname.isNotEmpty()) appendLine(macIface)
            ip.entries.forEach { (ip, state) ->
                append(makeIpSpan(ip))
                appendLine(app.getText(when (state) {
                    IpNeighbour.State.INCOMPLETE -> R.string.connected_state_incomplete
                    IpNeighbour.State.VALID -> R.string.connected_state_valid
                    IpNeighbour.State.FAILED -> R.string.connected_state_failed
                    else -> error("Invalid IpNeighbour.State: $state")
                }))
            }
        }.trimEnd()
    }

    fun obtainRecord() = record.value ?: ClientRecord(mac.addr)

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
