package be.mygod.vpnhotspot

import android.Manifest
import android.annotation.SuppressLint
import android.annotation.TargetApi
import android.bluetooth.BluetoothAdapter
import android.bluetooth.BluetoothProfile
import android.content.*
import android.content.pm.PackageManager
import android.databinding.BaseObservable
import android.databinding.Bindable
import android.databinding.DataBindingUtil
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.IBinder
import android.provider.Settings
import android.support.annotation.RequiresApi
import android.support.v4.app.Fragment
import android.support.v4.content.ContextCompat
import android.support.v7.recyclerview.extensions.ListAdapter
import android.support.v7.util.DiffUtil
import android.support.v7.widget.DefaultItemAnimator
import android.support.v7.widget.LinearLayoutManager
import android.support.v7.widget.RecyclerView
import android.view.LayoutInflater
import android.view.View
import android.view.ViewGroup
import android.widget.Toast
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.databinding.FragmentTetheringBinding
import be.mygod.vpnhotspot.databinding.ListitemInterfaceBinding
import be.mygod.vpnhotspot.databinding.ListitemManageTetherBinding
import be.mygod.vpnhotspot.net.TetherType
import be.mygod.vpnhotspot.net.TetheringManager
import be.mygod.vpnhotspot.net.wifi.WifiApManager
import be.mygod.vpnhotspot.util.broadcastReceiver
import be.mygod.vpnhotspot.util.formatAddresses
import java.lang.reflect.InvocationTargetException
import java.net.NetworkInterface
import java.net.SocketException
import java.util.*

class TetheringFragment : Fragment(), ServiceConnection {
    companion object {
        private const val VIEW_TYPE_INTERFACE = 0
        private const val VIEW_TYPE_LOCAL_ONLY_HOTSPOT = 6
        private const val VIEW_TYPE_MANAGE = 1
        private const val VIEW_TYPE_WIFI = 2
        private const val VIEW_TYPE_USB = 3
        private const val VIEW_TYPE_BLUETOOTH = 4
        private const val VIEW_TYPE_WIFI_LEGACY = 5

        private const val START_LOCAL_ONLY_HOTSPOT = 1

        /**
         * PAN Profile
         * From BluetoothProfile.java.
         */
        private const val PAN = 5
        private val isTetheringOn by lazy @SuppressLint("PrivateApi") {
            Class.forName("android.bluetooth.BluetoothPan").getDeclaredMethod("isTetheringOn")
        }
    }

    interface Data {
        val icon: Int
        val title: CharSequence
        val text: CharSequence
        val active: Boolean
        val selectable: Boolean
    }
    inner class TetheredData(val iface: TetheredInterface) : Data {
        override val icon get() = TetherType.ofInterface(iface.name).icon
        override val title get() = iface.name
        override val text get() = iface.addresses
        override val active = tetheringBinder?.isActive(iface.name) == true
        override val selectable get() = true
    }
    inner class LocalHotspotData(private val lookup: Map<String, NetworkInterface>) : Data {
        override val icon: Int get() {
            val iface = hotspotBinder?.iface ?: return TetherType.WIFI.icon
            return TetherType.ofInterface(iface).icon
        }
        override val title get() = getString(R.string.tethering_temp_hotspot)
        override val text by lazy {
            val binder = hotspotBinder
            val configuration = binder?.configuration ?: return@lazy getText(R.string.service_inactive)
            val iface = binder.iface ?: return@lazy getText(R.string.service_inactive)
            "${configuration.SSID} - ${configuration.preSharedKey}\n${TetheredInterface(iface, lookup).addresses}"
        }
        override val active = hotspotBinder?.iface != null
        override val selectable get() = active
    }

    private open class InterfaceViewHolder(val binding: ListitemInterfaceBinding) :
            RecyclerView.ViewHolder(binding.root), View.OnClickListener {
        init {
            itemView.setOnClickListener(this)
        }

        override fun onClick(view: View) {
            val context = itemView.context
            val data = binding.data as TetheredData
            if (data.active) context.startService(Intent(context, TetheringService::class.java)
                    .putExtra(TetheringService.EXTRA_REMOVE_INTERFACE, data.iface.name))
            else ContextCompat.startForegroundService(context, Intent(context, TetheringService::class.java)
                    .putExtra(TetheringService.EXTRA_ADD_INTERFACE, data.iface.name))
        }
    }
    @RequiresApi(26)
    private inner class LocalOnlyHotspotViewHolder(binding: ListitemInterfaceBinding) : InterfaceViewHolder(binding) {
        override fun onClick(view: View) {
            val binder = hotspotBinder
            if (binder?.iface != null) binder.stop() else {
                val context = requireContext()
                if (context.checkSelfPermission(Manifest.permission.ACCESS_COARSE_LOCATION) ==
                        PackageManager.PERMISSION_GRANTED) {
                    context.startForegroundService(Intent(context, LocalOnlyHotspotService::class.java))
                } else {
                    requestPermissions(arrayOf(Manifest.permission.ACCESS_COARSE_LOCATION), START_LOCAL_ONLY_HOTSPOT)
                }
            }
        }
    }
    private class ManageViewHolder(view: View) : RecyclerView.ViewHolder(view), View.OnClickListener {
        init {
            view.setOnClickListener(this)
        }

        override fun onClick(v: View?) = try {
            itemView.context.startActivity(Intent()
                    .setClassName("com.android.settings", "com.android.settings.Settings\$TetherSettingsActivity"))
        } catch (e: ActivityNotFoundException) {
            itemView.context.startActivity(Intent()
                    .setClassName("com.android.settings", "com.android.settings.TetherSettings"))
        }
    }
    private inner class ManageItemHolder(binding: ListitemManageTetherBinding, private val type: Int)
        : RecyclerView.ViewHolder(binding.root), View.OnClickListener, TetheringManager.OnStartTetheringCallback {
        val tetherType = when (type) {
            VIEW_TYPE_WIFI, VIEW_TYPE_WIFI_LEGACY -> TetherType.WIFI
            VIEW_TYPE_USB -> TetherType.USB
            VIEW_TYPE_BLUETOOTH -> TetherType.BLUETOOTH
            else -> TetherType.NONE
        }
        init {
            itemView.setOnClickListener(this)
            binding.icon = tetherType.icon
            binding.title = getString(when (type) {
                VIEW_TYPE_USB -> R.string.tethering_manage_usb
                VIEW_TYPE_WIFI -> R.string.tethering_manage_wifi
                VIEW_TYPE_WIFI_LEGACY -> R.string.tethering_manage_wifi_legacy
                VIEW_TYPE_BLUETOOTH -> R.string.tethering_manage_bluetooth
                else -> throw IllegalStateException("Unexpected view type")
            })
            binding.tetherListener = tetherListener
            binding.type = tetherType
        }

        override fun onClick(v: View?) {
            val context = requireContext()
            if (Build.VERSION.SDK_INT >= 23 && !Settings.System.canWrite(context)) {
                startActivity(Intent(Settings.ACTION_MANAGE_WRITE_SETTINGS,
                        Uri.parse("package:${context.packageName}")))
                return
            }
            val started = tetherListener.isStarted(tetherType)
            try {
                when (type) {
                    VIEW_TYPE_WIFI -> @RequiresApi(24) {
                        if (started) TetheringManager.stop(TetheringManager.TETHERING_WIFI)
                        else TetheringManager.start(TetheringManager.TETHERING_WIFI, true, this)
                    }
                    VIEW_TYPE_USB -> @RequiresApi(24) {
                        if (started) TetheringManager.stop(TetheringManager.TETHERING_USB)
                        else TetheringManager.start(TetheringManager.TETHERING_USB, true, this)
                    }
                    VIEW_TYPE_BLUETOOTH -> @RequiresApi(24) {
                        if (started) {
                            TetheringManager.stop(TetheringManager.TETHERING_BLUETOOTH)
                            Thread.sleep(1)         // give others a room to breathe
                            onTetheringStarted()    // force flush state
                        } else TetheringManager.start(TetheringManager.TETHERING_BLUETOOTH, true, this)
                    }
                    VIEW_TYPE_WIFI_LEGACY -> @Suppress("DEPRECATION") {
                        if (started) WifiApManager.stop() else WifiApManager.start()
                    }
                }
            } catch (e: InvocationTargetException) {
                e.printStackTrace()
                var cause: Throwable? = e
                while (cause != null) {
                    cause = cause.cause
                    if (cause != null && cause !is InvocationTargetException) {
                        Toast.makeText(context, cause.message, Toast.LENGTH_LONG).show()
                        break
                    }
                }
            }
        }

        override fun onTetheringStarted() = tetherListener.notifyPropertyChanged(BR.enabledTypes)
        override fun onTetheringFailed() {
            app.handler.post {
                Toast.makeText(requireContext(), R.string.tethering_manage_failed, Toast.LENGTH_SHORT).show()
            }
        }
    }

    inner class TetherListener : BaseObservable(), BluetoothProfile.ServiceListener {
        var enabledTypes = emptySet<TetherType>()
            @Bindable get
            set(value) {
                field = value
                notifyPropertyChanged(BR.enabledTypes)
            }
        var pan: BluetoothProfile? = null

        override fun onServiceDisconnected(profile: Int) {
            pan = null
        }
        override fun onServiceConnected(profile: Int, proxy: BluetoothProfile) {
            pan = proxy
        }

        /**
         * Based on: https://android.googlesource.com/platform/packages/apps/Settings/+/78d5efd/src/com/android/settings/TetherSettings.java
         */
        fun isStarted(type: TetherType, enabledTypes: Set<TetherType> = this.enabledTypes) =
                if (type == TetherType.BLUETOOTH) {
                    val pan = pan
                    BluetoothAdapter.getDefaultAdapter()?.state == BluetoothAdapter.STATE_ON && pan != null &&
                            isTetheringOn.invoke(pan) as Boolean
                } else enabledTypes.contains(type)
    }
    class TetheredInterface(val name: String, lookup: Map<String, NetworkInterface>) : Comparable<TetheredInterface> {
        val addresses = lookup[name]?.formatAddresses() ?: ""

        override fun compareTo(other: TetheredInterface) = name.compareTo(other.name)
        override fun equals(other: Any?): Boolean {
            if (this === other) return true
            if (javaClass != other?.javaClass) return false
            other as TetheredInterface
            if (name != other.name) return false
            if (addresses != other.addresses) return false
            return true
        }
        override fun hashCode(): Int = Objects.hash(name, addresses)

        object DiffCallback : DiffUtil.ItemCallback<TetheredInterface>() {
            override fun areItemsTheSame(oldItem: TetheredInterface, newItem: TetheredInterface) =
                    oldItem.name == newItem.name
            override fun areContentsTheSame(oldItem: TetheredInterface, newItem: TetheredInterface) = oldItem == newItem
        }
    }
    inner class TetheringAdapter :
            ListAdapter<TetheredInterface, RecyclerView.ViewHolder>(TetheredInterface.DiffCallback) {
        private var lookup: Map<String, NetworkInterface> = emptyMap()

        fun update(activeIfaces: List<String>, localOnlyIfaces: List<String>) {
            lookup = try {
                NetworkInterface.getNetworkInterfaces().asSequence().associateBy { it.name }
            } catch (e: SocketException) {
                e.printStackTrace()
                emptyMap()
            }
            this@TetheringFragment.tetherListener.enabledTypes =
                    (activeIfaces + localOnlyIfaces).map { TetherType.ofInterface(it) }.toSet()
            submitList(activeIfaces.map { TetheredInterface(it, lookup) }.sorted())
            if (Build.VERSION.SDK_INT >= 26) updateLocalOnlyViewHolder()
        }

        override fun getItemCount() = super.getItemCount() + if (Build.VERSION.SDK_INT < 24) 2 else 5
        override fun getItemViewType(position: Int) = if (Build.VERSION.SDK_INT < 26) {
            when (position - super.getItemCount()) {
                0 -> VIEW_TYPE_MANAGE
                1 -> if (Build.VERSION.SDK_INT >= 24) VIEW_TYPE_USB else VIEW_TYPE_WIFI_LEGACY
                2 -> VIEW_TYPE_WIFI
                3 -> VIEW_TYPE_BLUETOOTH
                4 -> VIEW_TYPE_WIFI_LEGACY
                else -> VIEW_TYPE_INTERFACE
            }
        } else {
            when (position - super.getItemCount()) {
                0 -> VIEW_TYPE_LOCAL_ONLY_HOTSPOT
                1 -> VIEW_TYPE_MANAGE
                2 -> VIEW_TYPE_USB
                3 -> VIEW_TYPE_WIFI
                4 -> VIEW_TYPE_BLUETOOTH
                else -> VIEW_TYPE_INTERFACE
            }
        }
        override fun onCreateViewHolder(parent: ViewGroup, viewType: Int): RecyclerView.ViewHolder {
            val inflater = LayoutInflater.from(parent.context)
            return when (viewType) {
                VIEW_TYPE_INTERFACE -> InterfaceViewHolder(ListitemInterfaceBinding.inflate(inflater, parent, false))
                VIEW_TYPE_MANAGE -> ManageViewHolder(inflater.inflate(R.layout.listitem_manage, parent, false))
                VIEW_TYPE_WIFI, VIEW_TYPE_USB, VIEW_TYPE_BLUETOOTH, VIEW_TYPE_WIFI_LEGACY ->
                    ManageItemHolder(ListitemManageTetherBinding.inflate(inflater, parent, false), viewType)
                VIEW_TYPE_LOCAL_ONLY_HOTSPOT -> @TargetApi(26) {
                    LocalOnlyHotspotViewHolder(ListitemInterfaceBinding.inflate(inflater, parent, false))
                }
                else -> throw IllegalArgumentException("Invalid view type")
            }
        }
        override fun onBindViewHolder(holder: RecyclerView.ViewHolder, position: Int) {
            when (holder) {
                is LocalOnlyHotspotViewHolder -> holder.binding.data = LocalHotspotData(lookup)
                is InterfaceViewHolder -> holder.binding.data = TetheredData(getItem(position))
            }
        }
        @RequiresApi(26)
        fun updateLocalOnlyViewHolder() {
            notifyItemChanged(super.getItemCount())
            notifyItemChanged(super.getItemCount() + 3)
        }
    }

    private val tetherListener = TetherListener()
    private lateinit var binding: FragmentTetheringBinding
    private var hotspotBinder: LocalOnlyHotspotService.HotspotBinder? = null
    private var tetheringBinder: TetheringService.TetheringBinder? = null
    val adapter = TetheringAdapter()
    private val receiver = broadcastReceiver { _, intent ->
        adapter.update(TetheringManager.getTetheredIfaces(intent.extras),
                TetheringManager.getLocalOnlyTetheredIfaces(intent.extras))
    }

    override fun onCreateView(inflater: LayoutInflater, container: ViewGroup?, savedInstanceState: Bundle?): View? {
        binding = DataBindingUtil.inflate(inflater, R.layout.fragment_tethering, container, false)
        binding.interfaces.layoutManager = LinearLayoutManager(context, LinearLayoutManager.VERTICAL, false)
        binding.interfaces.itemAnimator = DefaultItemAnimator()
        binding.interfaces.adapter = adapter
        BluetoothAdapter.getDefaultAdapter()?.getProfileProxy(requireContext(), tetherListener, PAN)
        return binding.root
    }

    override fun onStart() {
        super.onStart()
        val context = requireContext()
        context.bindService(Intent(context, TetheringService::class.java), this, Context.BIND_AUTO_CREATE)
        if (Build.VERSION.SDK_INT >= 26) {
            context.bindService(Intent(context, LocalOnlyHotspotService::class.java), this, Context.BIND_AUTO_CREATE)
        }
    }

    override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, grantResults: IntArray) {
        if (requestCode == START_LOCAL_ONLY_HOTSPOT) @TargetApi(26) {
            if (grantResults.firstOrNull() == PackageManager.PERMISSION_GRANTED) {
                val context = requireContext()
                context.startForegroundService(Intent(context, LocalOnlyHotspotService::class.java))
            }
        } else super.onRequestPermissionsResult(requestCode, permissions, grantResults)
    }

    override fun onStop() {
        requireContext().unbindService(this)
        super.onStop()
    }

    override fun onDestroy() {
        tetherListener.pan = null
        super.onDestroy()
    }

    override fun onServiceConnected(name: ComponentName?, service: IBinder?) = when (service) {
        is TetheringService.TetheringBinder -> {
            tetheringBinder = service
            service.fragment = this
            requireContext().registerReceiver(receiver, IntentFilter(TetheringManager.ACTION_TETHER_STATE_CHANGED))
            while (false) { }
        }
        is LocalOnlyHotspotService.HotspotBinder -> @TargetApi(26) {
            hotspotBinder = service
            service.fragment = this
            adapter.updateLocalOnlyViewHolder()
        }
        else -> throw IllegalArgumentException("service")
    }

    override fun onServiceDisconnected(name: ComponentName?) {
        val context = requireContext()
        when (name) {
            ComponentName(context, TetheringService::class.java) -> {
                tetheringBinder?.fragment = null
                tetheringBinder = null
                context.unregisterReceiver(receiver)
            }
            ComponentName(context, LocalOnlyHotspotService::class.java) -> {
                hotspotBinder?.fragment = null
                hotspotBinder = null
            }
            else -> throw IllegalArgumentException("name")
        }
    }
}
