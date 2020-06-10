package be.mygod.vpnhotspot.manage

import android.annotation.TargetApi
import android.content.*
import android.os.Build
import android.os.Bundle
import android.os.IBinder
import android.view.LayoutInflater
import android.view.MenuItem
import android.view.View
import android.view.ViewGroup
import androidx.activity.result.contract.ActivityResultContracts
import androidx.annotation.RequiresApi
import androidx.appcompat.widget.Toolbar
import androidx.core.content.ContextCompat
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
import be.mygod.vpnhotspot.net.wifi.WifiApDialogFragment
import be.mygod.vpnhotspot.net.wifi.WifiApManager
import be.mygod.vpnhotspot.util.ServiceForegroundConnector
import be.mygod.vpnhotspot.util.broadcastReceiver
import be.mygod.vpnhotspot.util.isNotGone
import be.mygod.vpnhotspot.util.showAllowingStateLoss
import be.mygod.vpnhotspot.widget.SmartSnackbar
import kotlinx.coroutines.CompletableDeferred
import timber.log.Timber
import java.lang.reflect.InvocationTargetException
import java.net.NetworkInterface
import java.net.SocketException

class TetheringFragment : Fragment(), ServiceConnection, Toolbar.OnMenuItemClickListener {
    inner class ManagerAdapter : ListAdapter<Manager, RecyclerView.ViewHolder>(Manager) {
        internal val repeaterManager by lazy { RepeaterManager(this@TetheringFragment) }
        @get:RequiresApi(26)
        internal val localOnlyHotspotManager by lazy @TargetApi(26) { LocalOnlyHotspotManager(this@TetheringFragment) }
        @get:RequiresApi(24)
        private val tetherManagers by lazy @TargetApi(24) {
            listOf(TetherManager.Wifi(this@TetheringFragment),
                    TetherManager.Usb(this@TetheringFragment),
                    TetherManager.Bluetooth(this@TetheringFragment))
        }
        @get:RequiresApi(30)
        private val tetherManagers30 by lazy @TargetApi(30) {
            listOf(TetherManager.Ethernet(this@TetheringFragment), TetherManager.Ncm(this@TetheringFragment))
        }
        private val wifiManagerLegacy by lazy @Suppress("Deprecation") {
            TetherManager.WifiLegacy(this@TetheringFragment)
        }

        private var enabledIfaces = emptyList<String>()
        private var listDeferred = CompletableDeferred<List<Manager>>(emptyList())
        private fun updateEnabledTypes() {
            this@TetheringFragment.enabledTypes = enabledIfaces.map { TetherType.ofInterface(it) }.toSet()
        }

        suspend fun notifyInterfaceChanged(lastList: List<Manager>? = null) {
            @Suppress("NAME_SHADOWING") val lastList = lastList ?: listDeferred.await()
            val first = lastList.indexOfFirst { it is InterfaceManager }
            if (first >= 0) notifyItemRangeChanged(first, lastList.indexOfLast { it is InterfaceManager } - first + 1)
        }
        suspend fun notifyTetherTypeChanged() {
            updateEnabledTypes()
            val lastList = listDeferred.await()
            notifyInterfaceChanged(lastList)
            val first = lastList.indexOfLast { it !is TetherManager } + 1
            notifyItemRangeChanged(first, lastList.size - first)
        }

        fun update(activeIfaces: List<String>, localOnlyIfaces: List<String>, erroredIfaces: List<String>) {
            val deferred = CompletableDeferred<List<Manager>>()
            listDeferred = deferred
            ifaceLookup = try {
                NetworkInterface.getNetworkInterfaces().asSequence().associateBy { it.name }
            } catch (e: SocketException) {
                Timber.d(e)
                emptyMap()
            }
            enabledIfaces = activeIfaces + localOnlyIfaces
            updateEnabledTypes()

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
            if (Build.VERSION.SDK_INT >= 30) {
                list.addAll(tetherManagers30)
                tetherManagers30.forEach { it.updateErrorMessage(erroredIfaces) }
            }
            if (Build.VERSION.SDK_INT < 26) {
                list.add(wifiManagerLegacy)
                wifiManagerLegacy.onTetheringStarted()
            }
            submitList(list) { deferred.complete(list) }
        }

        override fun getItemViewType(position: Int) = getItem(position).type
        override fun onCreateViewHolder(parent: ViewGroup, viewType: Int) =
                Manager.createViewHolder(LayoutInflater.from(parent.context), parent, viewType)
        override fun onBindViewHolder(holder: RecyclerView.ViewHolder, position: Int) = getItem(position).bindTo(holder)
    }

    @RequiresApi(29)
    val startRepeater = registerForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
        if (granted) requireContext().apply { startForegroundService(Intent(this, RepeaterService::class.java)) }
    }
    @RequiresApi(26)
    val startLocalOnlyHotspot = registerForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
        if (granted) adapter.localOnlyHotspotManager.start(requireContext())
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
        val activity = activity as? MainActivity
        val item = activity?.binding?.toolbar?.menu?.findItem(R.id.monitor) ?: return   // assuming no longer foreground
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
                adapter.repeaterManager.configure()
                true
            }
            R.id.configuration_temp_hotspot -> {
                WifiApDialogFragment().apply {
                    arg(WifiApDialogFragment.Arg(adapter.localOnlyHotspotManager.binder?.configuration ?: return false,
                            readOnly = true))
                    // no need for callback
                }.showAllowingStateLoss(parentFragmentManager)
                true
            }
            R.id.configuration_ap -> try {
                WifiApDialogFragment().apply {
                    arg(WifiApDialogFragment.Arg(WifiApManager.configuration))
                    key()
                }.showAllowingStateLoss(parentFragmentManager)
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
        AlertDialogFragment.setResultListener<WifiApDialogFragment, WifiApDialogFragment.Arg>(this) { which, ret ->
            if (which == DialogInterface.BUTTON_POSITIVE) try {
                WifiApManager.configuration = ret!!.configuration
            } catch (e: IllegalArgumentException) {
                Timber.d(e)
                SmartSnackbar.make(R.string.configuration_rejected).show()
            } catch (e: InvocationTargetException) {
                SmartSnackbar.make(e.targetException).show()
            }
        }
        binding = FragmentTetheringBinding.inflate(inflater, container, false)
        binding.interfaces.layoutManager = LinearLayoutManager(context, RecyclerView.VERTICAL, false)
        binding.interfaces.itemAnimator = DefaultItemAnimator()
        binding.interfaces.adapter = adapter
        adapter.update(emptyList(), emptyList(), emptyList())
        ServiceForegroundConnector(this, this, TetheringService::class)
        (activity as MainActivity).binding.toolbar.apply {
            inflateMenu(R.menu.toolbar_tethering)
            setOnMenuItemClickListener(this@TetheringFragment)
        }
        return binding.root
    }

    override fun onDestroyView() {
        super.onDestroyView()
        (activity as MainActivity).binding.toolbar.apply {
            menu.clear()
            setOnMenuItemClickListener(null)
        }
    }

    override fun onResume() {
        super.onResume()
        if (Build.VERSION.SDK_INT >= 27) ManageBar.Data.notifyChange()
    }

    override fun onServiceConnected(name: ComponentName?, service: IBinder?) {
        binder = service as TetheringService.Binder
        service.routingsChanged[this] = {
            lifecycleScope.launchWhenStarted { adapter.notifyInterfaceChanged() }
        }
        requireContext().registerReceiver(receiver, IntentFilter(TetheringManager.ACTION_TETHER_STATE_CHANGED))
        if (Build.VERSION.SDK_INT >= 30) TetherType.listener[this] = {
            lifecycleScope.launchWhenStarted { adapter.notifyTetherTypeChanged() }
        }
    }

    override fun onServiceDisconnected(name: ComponentName?) {
        (binder ?: return).routingsChanged -= this
        binder = null
        if (Build.VERSION.SDK_INT >= 30) TetherType.listener -= this
        requireContext().unregisterReceiver(receiver)
    }
}
