package be.mygod.vpnhotspot.client

import android.net.MacAddress
import android.os.SystemClock
import android.text.SpannableStringBuilder
import android.text.Spanned
import android.text.style.StrikethroughSpan
import androidx.lifecycle.map
import androidx.recyclerview.widget.DiffUtil
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.net.InetAddressComparator
import be.mygod.vpnhotspot.net.IpNeighbour
import be.mygod.vpnhotspot.net.TetherType
import be.mygod.vpnhotspot.room.AppDatabase
import be.mygod.vpnhotspot.room.ClientRecord
import be.mygod.vpnhotspot.util.formatTimestamp
import be.mygod.vpnhotspot.util.makeIpSpan
import be.mygod.vpnhotspot.util.makeMacSpan
import java.net.InetAddress
import java.util.Objects
import java.util.TreeMap

class Client(val mac: MacAddress, val iface: String? = null, val type: TetherType = TetherType.ofInterface(iface)) {
    companion object DiffCallback : DiffUtil.ItemCallback<Client>() {
        override fun areItemsTheSame(oldItem: Client, newItem: Client) =
                oldItem.iface == newItem.iface && oldItem.type == newItem.type && oldItem.mac == newItem.mac
        override fun areContentsTheSame(oldItem: Client, newItem: Client) = oldItem == newItem
    }

    val ip = TreeMap<InetAddress, ClientAddressInfo>(InetAddressComparator)
    val macString by lazy { mac.toString() }
    private val record = AppDatabase.instance.clientRecordDao.lookupOrDefaultSync(mac)
    private val macIface get() = SpannableStringBuilder(makeMacSpan(macString)).apply {
        iface?.let {
            append('%')
            append(it)
        }
    }

    val nickname get() = record.value?.nickname ?: ""
    val blocked get() = record.value?.blocked == true

    val icon get() = type.icon
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
            ip.entries.forEach { (ip, info) ->
                append(makeIpSpan(ip))
                info.address?.let { append("/${it.prefixLength}") }
                append(when (info.state) {
                    IpNeighbour.State.UNSET -> ""
                    IpNeighbour.State.INCOMPLETE -> app.getText(R.string.connected_state_incomplete)
                    IpNeighbour.State.VALID -> app.getText(R.string.connected_state_valid)
                    IpNeighbour.State.FAILED -> app.getText(R.string.connected_state_failed)
                    else -> error("Invalid IpNeighbour.State: ${info.state}")
                })
                if (info.address != null) {
                    info.hostname?.let { append(" →“$it”") }
                    val delta = System.currentTimeMillis() - SystemClock.elapsedRealtime()
                    append(" ⏳${app.formatTimestamp(info.deprecationTime + delta)}")
                }
                appendLine()
            }
        }.trimEnd()
    }

    fun obtainRecord() = record.value ?: ClientRecord(mac)

    override fun equals(other: Any?): Boolean {
        if (this === other) return true
        if (other !is Client) return false

        if (iface != other.iface) return false
        if (mac != other.mac) return false
        if (type != other.type) return false
        if (ip != other.ip) return false

        return true
    }
    override fun hashCode() = Objects.hash(iface, mac, type, ip)
}
