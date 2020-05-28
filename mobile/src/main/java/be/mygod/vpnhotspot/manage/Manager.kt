package be.mygod.vpnhotspot.manage

import android.annotation.SuppressLint
import android.annotation.TargetApi
import android.view.LayoutInflater
import android.view.ViewGroup
import androidx.recyclerview.widget.DiffUtil
import androidx.recyclerview.widget.RecyclerView
import be.mygod.vpnhotspot.databinding.ListitemInterfaceBinding
import be.mygod.vpnhotspot.databinding.ListitemManageBinding
import be.mygod.vpnhotspot.databinding.ListitemRepeaterBinding

abstract class Manager {
    companion object DiffCallback : DiffUtil.ItemCallback<Manager>() {
        const val VIEW_TYPE_INTERFACE = 0
        const val VIEW_TYPE_MANAGE = 1
        const val VIEW_TYPE_WIFI = 2
        const val VIEW_TYPE_USB = 3
        const val VIEW_TYPE_BLUETOOTH = 4
        const val VIEW_TYPE_ETHERNET = 8
        const val VIEW_TYPE_NCM = 9
        const val VIEW_TYPE_WIFI_LEGACY = 5
        const val VIEW_TYPE_LOCAL_ONLY_HOTSPOT = 6
        const val VIEW_TYPE_REPEATER = 7

        override fun areItemsTheSame(oldItem: Manager, newItem: Manager) = oldItem.isSameItemAs(newItem)
        @SuppressLint("DiffUtilEquals")
        override fun areContentsTheSame(oldItem: Manager, newItem: Manager) = oldItem === newItem

        fun createViewHolder(inflater: LayoutInflater, parent: ViewGroup, type: Int) = when (type) {
            VIEW_TYPE_INTERFACE ->
                InterfaceManager.ViewHolder(ListitemInterfaceBinding.inflate(inflater, parent, false))
            VIEW_TYPE_MANAGE -> ManageBar.ViewHolder(ListitemManageBinding.inflate(inflater, parent, false))
            VIEW_TYPE_WIFI,
            VIEW_TYPE_USB,
            VIEW_TYPE_BLUETOOTH,
            VIEW_TYPE_ETHERNET,
            VIEW_TYPE_NCM,
            VIEW_TYPE_WIFI_LEGACY -> {
                TetherManager.ViewHolder(ListitemInterfaceBinding.inflate(inflater, parent, false))
            }
            VIEW_TYPE_LOCAL_ONLY_HOTSPOT -> @TargetApi(26) {
                LocalOnlyHotspotManager.ViewHolder(ListitemInterfaceBinding.inflate(inflater, parent, false))
            }
            VIEW_TYPE_REPEATER -> RepeaterManager.ViewHolder(ListitemRepeaterBinding.inflate(inflater, parent, false))
            else -> throw IllegalArgumentException("Invalid view type")
        }
    }

    abstract val type: Int

    open fun bindTo(viewHolder: RecyclerView.ViewHolder) { }

    open fun isSameItemAs(other: Manager) = javaClass == other.javaClass
}
