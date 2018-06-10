package be.mygod.vpnhotspot.manage

import android.Manifest
import android.annotation.TargetApi
import android.content.ActivityNotFoundException
import android.content.ComponentName
import android.content.Intent
import android.content.ServiceConnection
import android.content.pm.PackageManager
import android.location.LocationManager
import android.os.Build
import android.os.IBinder
import android.provider.Settings
import android.support.v7.widget.RecyclerView
import android.view.View
import android.widget.Toast
import be.mygod.vpnhotspot.LocalOnlyHotspotService
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.databinding.ListitemInterfaceBinding
import be.mygod.vpnhotspot.net.TetherType
import be.mygod.vpnhotspot.util.ServiceForegroundConnector
import be.mygod.vpnhotspot.util.formatAddresses
import be.mygod.vpnhotspot.util.systemService
import com.crashlytics.android.Crashlytics
import java.net.NetworkInterface

@TargetApi(26)
class LocalOnlyHotspotManager(private val parent: TetheringFragment) : Manager(), ServiceConnection {
    class ViewHolder(val binding: ListitemInterfaceBinding) : RecyclerView.ViewHolder(binding.root),
            View.OnClickListener {
        init {
            itemView.setOnClickListener(this)
        }

        lateinit var manager: LocalOnlyHotspotManager

        override fun onClick(view: View) {
            val binder = manager.binder
            if (binder?.iface != null) binder.stop() else {
                val context = manager.parent.requireContext()
                if (context.checkSelfPermission(Manifest.permission.ACCESS_COARSE_LOCATION) !=
                        PackageManager.PERMISSION_GRANTED) {
                    manager.parent.requestPermissions(arrayOf(Manifest.permission.ACCESS_COARSE_LOCATION),
                            TetheringFragment.START_LOCAL_ONLY_HOTSPOT)
                    return
                }
                /**
                 * LOH also requires location to be turned on. Source:
                 * https://android.googlesource.com/platform/frameworks/opt/net/wifi/+/53e0284/service/java/com/android/server/wifi/WifiServiceImpl.java#1204
                 * https://android.googlesource.com/platform/frameworks/opt/net/wifi/+/53e0284/service/java/com/android/server/wifi/WifiSettingsStore.java#228
                 */
                if (if (Build.VERSION.SDK_INT < 28) @Suppress("DEPRECATION") {
                            Settings.Secure.getInt(view.context.contentResolver, Settings.Secure.LOCATION_MODE,
                                    Settings.Secure.LOCATION_MODE_OFF) == Settings.Secure.LOCATION_MODE_OFF
                        } else !context.systemService<LocationManager>().isLocationEnabled) {
                    Toast.makeText(view.context, R.string.tethering_temp_hotspot_location, Toast.LENGTH_LONG).show()
                    try {
                        view.context.startActivity(Intent(Settings.ACTION_LOCATION_SOURCE_SETTINGS))
                    } catch (exc: ActivityNotFoundException) {
                        exc.printStackTrace()
                        Crashlytics.logException(exc)
                    }
                    return
                }
                context.startForegroundService(Intent(context, LocalOnlyHotspotService::class.java))
            }
        }
    }
    private inner class Data : be.mygod.vpnhotspot.manage.Data() {
        private val lookup: Map<String, NetworkInterface> get() = parent.ifaceLookup

        override val icon: Int get() {
            val iface = binder?.iface ?: return TetherType.WIFI.icon
            return TetherType.ofInterface(iface).icon
        }
        override val title: CharSequence get() {
            val configuration = binder?.configuration ?: return parent.getString(R.string.tethering_temp_hotspot)
            return "${configuration.SSID} - ${configuration.preSharedKey}"
        }
        override val text: CharSequence get() {
            return lookup[binder?.iface ?: return ""]?.formatAddresses() ?: ""
        }
        override val active get() = binder?.iface != null
        override val selectable get() = active
    }

    init {
        ServiceForegroundConnector(parent, this, LocalOnlyHotspotService::class)
    }

    override val type get() = VIEW_TYPE_LOCAL_ONLY_HOTSPOT
    private val data = Data()
    private var binder: LocalOnlyHotspotService.Binder? = null

    override fun bindTo(viewHolder: RecyclerView.ViewHolder) {
        viewHolder as ViewHolder
        viewHolder.binding.data = data
        viewHolder.manager = this
    }

    fun update() = data.notifyChange()

    override fun onServiceConnected(name: ComponentName?, service: IBinder?) {
        binder = service as LocalOnlyHotspotService.Binder
        service.manager = this
        update()
    }

    override fun onServiceDisconnected(name: ComponentName?) {
        binder?.manager = null
        binder = null
    }
}
