package be.mygod.vpnhotspot.manage

import android.content.Intent
import android.support.v4.content.ContextCompat
import android.support.v7.widget.RecyclerView
import android.view.View
import be.mygod.vpnhotspot.TetheringService
import be.mygod.vpnhotspot.databinding.ListitemInterfaceBinding
import be.mygod.vpnhotspot.net.TetherType
import be.mygod.vpnhotspot.util.formatAddresses
import java.util.*

class InterfaceManager(private val parent: TetheringFragment, val iface: String) : Manager() {
    class ViewHolder(val binding: ListitemInterfaceBinding) : RecyclerView.ViewHolder(binding.root),
            View.OnClickListener {
        init {
            itemView.setOnClickListener(this)
        }

        lateinit var iface: String

        override fun onClick(view: View) {
            val context = itemView.context
            val data = binding.data as Data
            if (data.active) context.startService(Intent(context, TetheringService::class.java)
                    .putExtra(TetheringService.EXTRA_REMOVE_INTERFACE, iface))
            else ContextCompat.startForegroundService(context, Intent(context, TetheringService::class.java)
                    .putExtra(TetheringService.EXTRA_ADD_INTERFACE, iface))
        }
    }
    private inner class Data : be.mygod.vpnhotspot.manage.Data() {
        override val icon get() = TetherType.ofInterface(iface).icon
        override val title get() = iface
        override val text get() = addresses
        override val active get() = parent.tetheringBinder?.isActive(iface) == true
        override val selectable get() = true
    }

    val addresses = parent.ifaceLookup[iface]?.formatAddresses() ?: ""
    override val type get() = VIEW_TYPE_INTERFACE
    private val data = Data()

    override fun bindTo(viewHolder: RecyclerView.ViewHolder) {
        viewHolder as ViewHolder
        viewHolder.binding.data = data
        viewHolder.iface = iface
    }

    override fun isSameItemAs(other: Manager) = when (other) {
        is InterfaceManager -> iface == other.iface
        else -> false
    }

    override fun equals(other: Any?): Boolean {
        if (this === other) return true
        if (javaClass != other?.javaClass) return false
        other as InterfaceManager
        if (iface != other.iface) return false
        if (addresses != other.addresses) return false
        return true
    }
    override fun hashCode(): Int = Objects.hash(iface, addresses)
}
