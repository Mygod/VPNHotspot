package be.mygod.vpnhotspot.manage

import android.annotation.TargetApi
import android.content.ComponentName
import android.content.Intent
import android.content.IntentFilter
import android.content.ServiceConnection
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.os.IBinder
import android.view.LayoutInflater
import android.view.MenuItem
import android.view.View
import android.view.ViewGroup
import androidx.core.content.ContextCompat
import androidx.databinding.DataBindingUtil
import androidx.fragment.app.Fragment
import androidx.recyclerview.widget.DefaultItemAnimator
import androidx.recyclerview.widget.LinearLayoutManager
import androidx.recyclerview.widget.ListAdapter
import androidx.recyclerview.widget.RecyclerView
import be.mygod.vpnhotspot.LocalOnlyHotspotService
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.RepeaterService
import be.mygod.vpnhotspot.TetheringService
import be.mygod.vpnhotspot.databinding.FragmentTetheringBinding
import be.mygod.vpnhotspot.net.TetherType
import be.mygod.vpnhotspot.net.TetheringManager
import be.mygod.vpnhotspot.net.TetheringManager.localOnlyTetheredIfaces
import be.mygod.vpnhotspot.net.TetheringManager.tetheredIfaces
import be.mygod.vpnhotspot.util.ServiceForegroundConnector
import be.mygod.vpnhotspot.util.broadcastReceiver
import be.mygod.vpnhotspot.util.isNotGone
import kotlinx.android.synthetic.main.activity_main.*
import timber.log.Timber
import java.net.NetworkInterface
import java.net.SocketException

class TetheringFragment : Fragment(), ServiceConnection, MenuItem.OnMenuItemClickListener {
    companion object {
        const val START_REPEATER = 4
        const val START_LOCAL_ONLY_HOTSPOT = 1
        const val REPEATER_EDIT_CONFIGURATION = 2
        const val REPEATER_WPS = 3
    }

    inner class ManagerAdapter : ListAdapter<Manager, RecyclerView.ViewHolder>(Manager) {
        internal val repeaterManager by lazy { RepeaterManager(this@TetheringFragment) }
        private val localOnlyHotspotManager by lazy @TargetApi(26) { LocalOnlyHotspotManager(this@TetheringFragment) }
        private val tetherManagers by lazy @TargetApi(24) {
            listOf(TetherManager.Wifi(this@TetheringFragment),
                    TetherManager.Usb(this@TetheringFragment),
                    TetherManager.Bluetooth(this@TetheringFragment))
        }
        private val wifiManagerLegacy by lazy @Suppress("Deprecation") {
            TetherManager.WifiLegacy(this@TetheringFragment)
        }

        fun update(activeIfaces: List<String>, localOnlyIfaces: List<String>, erroredIfaces: List<String>) {
            ifaceLookup = try {
                NetworkInterface.getNetworkInterfaces().asSequence().associateBy { it.name }
            } catch (e: SocketException) {
                Timber.d(e)
                emptyMap()
            }
            this@TetheringFragment.enabledTypes =
                    (activeIfaces + localOnlyIfaces).map { TetherType.ofInterface(it) }.toSet()

            val list = ArrayList<Manager>()
            if (RepeaterService.supported) list.add(repeaterManager)
            if (Build.VERSION.SDK_INT >= 26) list.add(localOnlyHotspotManager)
            val monitoredIfaces = binder?.monitoredIfaces ?: emptyList()
            updateMonitorList(activeIfaces - monitoredIfaces)
            list.addAll((activeIfaces + monitoredIfaces).toSortedSet()
                    .map { InterfaceManager(this@TetheringFragment, it) })
            list.add(ManageBar)
            if (Build.VERSION.SDK_INT >= 24) {
                list.addAll(tetherManagers)
                tetherManagers.forEach { it.updateErrorMessage(erroredIfaces) }
            }
            if (Build.VERSION.SDK_INT < 26) {
                list.add(wifiManagerLegacy)
                wifiManagerLegacy.onTetheringStarted()
            }
            submitList(list)
        }

        override fun getItemViewType(position: Int) = getItem(position).type
        override fun onCreateViewHolder(parent: ViewGroup, viewType: Int) =
                Manager.createViewHolder(LayoutInflater.from(parent.context), parent, viewType)
        override fun onBindViewHolder(holder: RecyclerView.ViewHolder, position: Int) = getItem(position).bindTo(holder)
    }

    var ifaceLookup: Map<String, NetworkInterface> = emptyMap()
    var enabledTypes = emptySet<TetherType>()
    private lateinit var binding: FragmentTetheringBinding
    var binder: TetheringService.Binder? = null
    private val adapter = ManagerAdapter()
    private val receiver = broadcastReceiver { _, intent ->
        adapter.update(intent.tetheredIfaces, intent.localOnlyTetheredIfaces,
                intent.getStringArrayListExtra(TetheringManager.EXTRA_ERRORED_TETHER))
    }

    private fun updateMonitorList(canMonitor: List<String> = emptyList()) {
        val item = requireActivity().toolbar.menu.findItem(R.id.monitor) ?: return  // assuming no longer foreground
        item.isNotGone = canMonitor.isNotEmpty()
        item.subMenu.apply {
            clear()
            canMonitor.sorted().forEach { add(it).setOnMenuItemClickListener(this@TetheringFragment) }
        }
    }
    override fun onMenuItemClick(item: MenuItem?): Boolean {
        ContextCompat.startForegroundService(requireContext(), Intent(context, TetheringService::class.java)
                .putExtra(TetheringService.EXTRA_ADD_INTERFACE_MONITOR, item?.title ?: return false))
        return true
    }

    override fun onCreateView(inflater: LayoutInflater, container: ViewGroup?, savedInstanceState: Bundle?): View? {
        binding = DataBindingUtil.inflate(inflater, R.layout.fragment_tethering, container, false)
        binding.lifecycleOwner = this
        binding.interfaces.layoutManager = LinearLayoutManager(context, RecyclerView.VERTICAL, false)
        binding.interfaces.itemAnimator = DefaultItemAnimator()
        binding.interfaces.adapter = adapter
        adapter.update(emptyList(), emptyList(), emptyList())
        ServiceForegroundConnector(this, this, TetheringService::class)
        requireActivity().toolbar.inflateMenu(R.menu.toolbar_tethering)
        return binding.root
    }

    override fun onDestroyView() {
        super.onDestroyView()
        requireActivity().toolbar.menu.clear()
    }

    override fun onResume() {
        super.onResume()
        if (Build.VERSION.SDK_INT >= 27) ManageBar.Data.notifyChange()
    }

    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) = when (requestCode) {
        REPEATER_WPS -> adapter.repeaterManager.onWpsResult(resultCode, data)
        REPEATER_EDIT_CONFIGURATION -> adapter.repeaterManager.onEditResult(resultCode, data)
        else -> super.onActivityResult(requestCode, resultCode, data)
    }

    override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, grantResults: IntArray) {
        when (requestCode) {
            START_REPEATER -> if (grantResults.firstOrNull() == PackageManager.PERMISSION_GRANTED) @TargetApi(29) {
                val context = requireContext()
                context.startForegroundService(Intent(context, RepeaterService::class.java))
            }
            START_LOCAL_ONLY_HOTSPOT -> {
                if (grantResults.firstOrNull() == PackageManager.PERMISSION_GRANTED) @TargetApi(26) {
                    val context = requireContext()
                    context.startForegroundService(Intent(context, LocalOnlyHotspotService::class.java))
                }
            }
            else -> super.onRequestPermissionsResult(requestCode, permissions, grantResults)
        }
    }

    override fun onServiceConnected(name: ComponentName?, service: IBinder?) {
        binder = service as TetheringService.Binder
        service.routingsChanged[this] = {
            requireContext().apply {
                // flush tethered interfaces
                unregisterReceiver(receiver)
                registerReceiver(receiver, IntentFilter(TetheringManager.ACTION_TETHER_STATE_CHANGED))
            }
        }
        requireContext().registerReceiver(receiver, IntentFilter(TetheringManager.ACTION_TETHER_STATE_CHANGED))
    }

    override fun onServiceDisconnected(name: ComponentName?) {
        (binder ?: return).routingsChanged -= this
        binder = null
        requireContext().unregisterReceiver(receiver)
    }
}
