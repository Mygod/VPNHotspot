package be.mygod.vpnhotspot.manage

import android.Manifest
import android.content.ComponentName
import android.content.Context
import android.content.ServiceConnection
import android.os.Build
import android.os.IBinder
import android.view.View
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.lifecycleScope
import androidx.lifecycle.repeatOnLifecycle
import androidx.recyclerview.widget.RecyclerView
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.LocalOnlyHotspotService
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.databinding.ListitemInterfaceBinding
import be.mygod.vpnhotspot.util.ServiceForegroundConnector
import be.mygod.vpnhotspot.util.formatAddresses
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.flow.collectLatest
import kotlinx.coroutines.launch
import java.net.NetworkInterface

class LocalOnlyHotspotManager(private val parent: TetheringFragment) : Manager(), ServiceConnection {
    companion object {
        val permission = if (Build.VERSION.SDK_INT >= 33) {
            Manifest.permission.NEARBY_WIFI_DEVICES
        } else Manifest.permission.ACCESS_FINE_LOCATION
    }

    class ViewHolder(val binding: ListitemInterfaceBinding) : RecyclerView.ViewHolder(binding.root),
            View.OnClickListener {
        init {
            itemView.setOnClickListener(this)
        }

        lateinit var manager: LocalOnlyHotspotManager

        override fun onClick(view: View) {
            val binder = manager.binder.value
            if (binder?.iface?.value == null) manager.parent.startLocalOnlyHotspot.launch(permission) else binder.stop()
        }
    }
    private inner class Data : be.mygod.vpnhotspot.manage.Data() {
        private val lookup: Map<String, NetworkInterface> get() = parent.ifaceLookup

        override val icon get() = R.drawable.ic_action_perm_scan_wifi
        override val title: CharSequence get() = parent.getString(R.string.tethering_temp_hotspot)
        override val text: CharSequence get() {
            return lookup[binder.value?.iface?.value ?: return ""]?.formatAddresses() ?: ""
        }
        override val active get() = binder.value?.iface?.value != null
        override val selectable get() = active
    }

    override val type get() = VIEW_TYPE_LOCAL_ONLY_HOTSPOT
    private val data = Data()
    private val mutableBinder = MutableStateFlow<LocalOnlyHotspotService.Binder?>(null)
    val binder = mutableBinder.asStateFlow()

    init {
        ServiceForegroundConnector(parent, this, LocalOnlyHotspotService::class)
        parent.viewLifecycleOwner.lifecycleScope.launch {
            parent.viewLifecycleOwner.repeatOnLifecycle(Lifecycle.State.STARTED) {
                binder.collectLatest { service ->
                    if (service == null) {
                        data.notifyChange()
                        return@collectLatest
                    }
                    service.iface.collect {
                        data.notifyChange()
                    }
                }
            }
        }
    }

    fun start(context: Context) = app.startServiceWithLocation<LocalOnlyHotspotService>(context)

    override fun bindTo(viewHolder: RecyclerView.ViewHolder) {
        viewHolder as ViewHolder
        viewHolder.binding.data = data
        viewHolder.manager = this
    }

    fun update() = data.notifyChange()

    override fun onServiceConnected(name: ComponentName?, service: IBinder?) {
        mutableBinder.value = service as LocalOnlyHotspotService.Binder
    }

    override fun onServiceDisconnected(name: ComponentName?) {
        mutableBinder.value = null
        data.notifyChange()
    }
}
