package be.mygod.vpnhotspot.manage

import android.annotation.TargetApi
import android.content.*
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.os.IBinder
import android.view.LayoutInflater
import android.view.MenuItem
import android.view.View
import android.view.ViewGroup
import androidx.appcompat.widget.Toolbar
import androidx.core.content.ContextCompat
import androidx.databinding.DataBindingUtil
import androidx.fragment.app.Fragment
import androidx.lifecycle.lifecycleScope
import androidx.recyclerview.widget.DefaultItemAnimator
import androidx.recyclerview.widget.LinearLayoutManager
import androidx.recyclerview.widget.ListAdapter
import androidx.recyclerview.widget.RecyclerView
import be.mygod.vpnhotspot.*
import be.mygod.vpnhotspot.databinding.FragmentTetheringBinding
import be.mygod.vpnhotspot.net.TetherType
import be.mygod.vpnhotspot.net.TetheringManager
import be.mygod.vpnhotspot.net.TetheringManager.localOnlyTetheredIfaces
import be.mygod.vpnhotspot.net.TetheringManager.tetheredIfaces
import be.mygod.vpnhotspot.net.wifi.WifiApManager
import be.mygod.vpnhotspot.net.wifi.configuration.WifiApDialogFragment
import be.mygod.vpnhotspot.util.ServiceForegroundConnector
import be.mygod.vpnhotspot.util.broadcastReceiver
import be.mygod.vpnhotspot.util.isNotGone
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.android.synthetic.main.activity_main.*
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import timber.log.Timber
import java.lang.IllegalArgumentException
import java.lang.reflect.InvocationTargetException
import java.net.NetworkInterface
import java.net.SocketException

class TetheringFragment : Fragment(), ServiceConnection, Toolbar.OnMenuItemClickListener {
    companion object {
        const val START_REPEATER = 4
        const val START_LOCAL_ONLY_HOTSPOT = 1
        const val REPEATER_WPS = 3
        const val CONFIGURE_REPEATER = 2
        const val CONFIGURE_AP = 4
    }

    inner class ManagerAdapter : ListAdapter<Manager, RecyclerView.ViewHolder>(Manager) {
        internal val repeaterManager by lazy { RepeaterManager(this@TetheringFragment) }
        internal val localOnlyHotspotManager by lazy @TargetApi(26) { LocalOnlyHotspotManager(this@TetheringFragment) }
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
        adapter.update(intent.tetheredIfaces ?: return@broadcastReceiver,
                intent.localOnlyTetheredIfaces ?: return@broadcastReceiver,
                intent.getStringArrayListExtra(TetheringManager.EXTRA_ERRORED_TETHER) ?: return@broadcastReceiver)
    }

    private fun updateMonitorList(canMonitor: List<String> = emptyList()) {
        val activity = activity
        val item = activity?.toolbar?.menu?.findItem(R.id.monitor) ?: return    // assuming no longer foreground
        item.isNotGone = canMonitor.isNotEmpty()
        item.subMenu.apply {
            clear()
            for (iface in canMonitor.sorted()) add(iface).setOnMenuItemClickListener {
                ContextCompat.startForegroundService(activity, Intent(activity, TetheringService::class.java)
                        .putExtra(TetheringService.EXTRA_ADD_INTERFACE_MONITOR, iface))
                true
            }
        }
    }
    override fun onMenuItemClick(item: MenuItem?): Boolean {
        return when (item?.itemId) {
            R.id.configuration -> item.subMenu.run {
                findItem(R.id.configuration_repeater).isNotGone = RepeaterService.supported
                findItem(R.id.configuration_temp_hotspot).isNotGone =
                        adapter.localOnlyHotspotManager.binder?.configuration != null
                true
            }
            R.id.configuration_repeater -> {
                lifecycleScope.launchWhenCreated {
                    WifiApDialogFragment().withArg(WifiApDialogFragment.Arg(withContext(Dispatchers.Default) {
                        adapter.repeaterManager.configuration
                    } ?: return@launchWhenCreated, p2pMode = true)).show(this@TetheringFragment, CONFIGURE_REPEATER)
                }
                true
            }
            R.id.configuration_temp_hotspot -> {
                WifiApDialogFragment().withArg(WifiApDialogFragment.Arg(
                        adapter.localOnlyHotspotManager.binder?.configuration ?: return false,
                        readOnly = true
                )).show(this, 0)    // read-only, no callback needed
                true
            }
            R.id.configuration_ap -> try {
                WifiApDialogFragment().withArg(WifiApDialogFragment.Arg(
                        WifiApManager.configuration
                )).show(this, CONFIGURE_AP)
                true
            } catch (e: InvocationTargetException) {
                if (e.targetException !is SecurityException) Timber.w(e)
                SmartSnackbar.make(e.targetException).show()
                false
            }
            else -> false
        }
    }

    override fun onCreateView(inflater: LayoutInflater, container: ViewGroup?, savedInstanceState: Bundle?): View? {
        binding = DataBindingUtil.inflate(inflater, R.layout.fragment_tethering, container, false)
        binding.lifecycleOwner = this
        binding.interfaces.layoutManager = LinearLayoutManager(context, RecyclerView.VERTICAL, false)
        binding.interfaces.itemAnimator = DefaultItemAnimator()
        binding.interfaces.adapter = adapter
        adapter.update(emptyList(), emptyList(), emptyList())
        ServiceForegroundConnector(this, this, TetheringService::class)
        requireActivity().toolbar.apply {
            inflateMenu(R.menu.toolbar_tethering)
            setOnMenuItemClickListener(this@TetheringFragment)
        }
        return binding.root
    }

    override fun onDestroyView() {
        super.onDestroyView()
        requireActivity().toolbar.apply {
            menu.clear()
            setOnMenuItemClickListener(null)
        }
    }

    override fun onResume() {
        super.onResume()
        if (Build.VERSION.SDK_INT >= 27) ManageBar.Data.notifyChange()
    }

    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        val configuration by lazy { AlertDialogFragment.getRet<WifiApDialogFragment.Arg>(data!!).configuration }
        when (requestCode) {
            REPEATER_WPS -> adapter.repeaterManager.onWpsResult(resultCode, data)
            CONFIGURE_REPEATER -> if (resultCode == DialogInterface.BUTTON_POSITIVE) lifecycleScope.launchWhenCreated {
                adapter.repeaterManager.updateConfiguration(configuration)
            }
            CONFIGURE_AP -> if (resultCode == DialogInterface.BUTTON_POSITIVE) try {
                WifiApManager.configuration = configuration
            } catch (e: IllegalArgumentException) {
                SmartSnackbar.make(R.string.configuration_rejected).show()
            } catch (e: InvocationTargetException) {
                SmartSnackbar.make(e.targetException).show()
            }
            else -> super.onActivityResult(requestCode, resultCode, data)
        }
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
