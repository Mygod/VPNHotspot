package be.mygod.vpnhotspot.manage

import android.annotation.TargetApi
import android.bluetooth.BluetoothManager
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
import androidx.core.content.getSystemService
import androidx.fragment.app.Fragment
import androidx.lifecycle.lifecycleScope
import androidx.lifecycle.withStarted
import androidx.recyclerview.widget.DefaultItemAnimator
import androidx.recyclerview.widget.LinearLayoutManager
import androidx.recyclerview.widget.ListAdapter
import androidx.recyclerview.widget.RecyclerView
import be.mygod.vpnhotspot.*
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.databinding.FragmentTetheringBinding
import be.mygod.vpnhotspot.net.TetherType
import be.mygod.vpnhotspot.net.TetheringManager
import be.mygod.vpnhotspot.net.TetheringManager.localOnlyTetheredIfaces
import be.mygod.vpnhotspot.net.TetheringManager.tetheredIfaces
import be.mygod.vpnhotspot.net.monitor.TetherTimeoutMonitor
import be.mygod.vpnhotspot.net.wifi.SoftApConfigurationCompat
import be.mygod.vpnhotspot.net.wifi.SoftApConfigurationCompat.Companion.toCompat
import be.mygod.vpnhotspot.net.wifi.WifiApDialogFragment
import be.mygod.vpnhotspot.net.wifi.WifiApManager
import be.mygod.vpnhotspot.root.RootManager
import be.mygod.vpnhotspot.root.WifiApCommands
import be.mygod.vpnhotspot.util.*
import be.mygod.vpnhotspot.widget.SmartSnackbar
import com.google.android.material.snackbar.Snackbar
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.launch
import timber.log.Timber
import java.lang.reflect.InvocationTargetException
import java.net.NetworkInterface
import java.net.SocketException

class TetheringFragment : Fragment(), ServiceConnection, Toolbar.OnMenuItemClickListener {
    inner class ManagerAdapter : ListAdapter<Manager, RecyclerView.ViewHolder>(Manager),
        TetheringManager.TetheringEventCallback {
        internal val repeaterManager by lazy { RepeaterManager(this@TetheringFragment) }
        internal val localOnlyHotspotManager by lazy { LocalOnlyHotspotManager(this@TetheringFragment) }
        internal val bluetoothManager by lazy {
            requireContext().getSystemService<BluetoothManager>()?.adapter?.let {
                TetherManager.Bluetooth(this@TetheringFragment, it)
            }
        }
        private val tetherManagers by lazy {
            listOfNotNull(
                TetherManager.Wifi(this@TetheringFragment),
                TetherManager.Usb(this@TetheringFragment),
                bluetoothManager,
            )
        }
        @get:RequiresApi(30)
        private val ethernetManager by lazy @TargetApi(30) { TetherManager.Ethernet(this@TetheringFragment) }

        var activeIfaces = emptyList<String>()
        var localOnlyIfaces = emptyList<String>()
        var erroredIfaces = emptyList<String>()
        private var listDeferred = CompletableDeferred<List<Manager>>(emptyList())
        fun updateEnabledTypes() {
            this@TetheringFragment.enabledTypes =
                (activeIfaces + localOnlyIfaces).map { TetherType.ofInterface(it) }.toSet()
        }

        val lastErrors = mutableMapOf<String, Int>()
        override fun onError(ifName: String, error: Int) {
            if (error == 0) lastErrors.remove(ifName) else lastErrors[ifName] = error
        }

        suspend fun notifyTetherTypeChanged() {
            updateEnabledTypes()
            val lastList = listDeferred.await()
            var first = lastList.indexOfFirst { it is InterfaceManager }
            withStarted {
                if (first >= 0) {
                    notifyItemRangeChanged(first, lastList.indexOfLast { it is InterfaceManager } - first + 1)
                }
                first = lastList.indexOfLast { it !is TetherManager } + 1
                notifyItemRangeChanged(first, lastList.size - first)
            }
        }

        fun update() {
            val deferred = CompletableDeferred<List<Manager>>()
            listDeferred = deferred
            ifaceLookup = try {
                NetworkInterface.getNetworkInterfaces().asSequence().associateBy { it.name }
            } catch (e: Exception) {
                if (e is SocketException) Timber.d(e) else Timber.w(e)
                emptyMap()
            }

            val list = ArrayList<Manager>()
            if (Services.p2p != null) list.add(repeaterManager)
            list.add(localOnlyHotspotManager)
            val monitoredIfaces = binder?.monitoredIfaces ?: emptyList()
            updateMonitorList(activeIfaces - monitoredIfaces.toSet())
            list.addAll((activeIfaces + monitoredIfaces).toSortedSet()
                    .map { InterfaceManager(this@TetheringFragment, it) })
            list.add(ManageBar)
            list.addAll(tetherManagers)
            tetherManagers.forEach { it.updateErrorMessage(erroredIfaces, lastErrors) }
            if (Build.VERSION.SDK_INT >= 30) {
                list.add(ethernetManager)
                ethernetManager.updateErrorMessage(erroredIfaces, lastErrors)
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
        if (granted) app.startServiceWithLocation<RepeaterService>(requireContext()) else {
            Snackbar.make((activity as MainActivity).binding.fragmentHolder,
                R.string.repeater_missing_location_permissions, Snackbar.LENGTH_LONG).show()
        }
    }
    val startLocalOnlyHotspot = registerForActivityResult(ActivityResultContracts.RequestPermission()) {
        adapter.localOnlyHotspotManager.start(requireContext())
    }
    @RequiresApi(31)
    val requestBluetooth = registerForActivityResult(ActivityResultContracts.RequestPermission()) { granted ->
        if (granted) adapter.bluetoothManager!!.ensureInit(requireContext())
    }

    var ifaceLookup: Map<String, NetworkInterface> = emptyMap()
    var enabledTypes = emptySet<TetherType>()
    private lateinit var binding: FragmentTetheringBinding
    var binder: TetheringService.Binder? = null
    private val adapter = ManagerAdapter()
    private val receiver = broadcastReceiver { _, intent ->
        adapter.activeIfaces = intent.tetheredIfaces ?: return@broadcastReceiver
        adapter.localOnlyIfaces = intent.localOnlyTetheredIfaces ?: return@broadcastReceiver
        adapter.erroredIfaces = intent.getStringArrayListExtra(TetheringManager.EXTRA_ERRORED_TETHER)
            ?: return@broadcastReceiver
        adapter.updateEnabledTypes()
        adapter.update()
    }

    private fun updateMonitorList(canMonitor: List<String> = emptyList()) {
        val activity = activity as? MainActivity
        val item = activity?.binding?.toolbar?.menu?.findItem(R.id.monitor) ?: return   // assuming no longer foreground
        item.isNotGone = canMonitor.isNotEmpty()
        item.subMenu!!.apply {
            clear()
            for (iface in canMonitor.sorted()) add(iface).setOnMenuItemClickListener {
                activity.startForegroundService(Intent(activity, TetheringService::class.java)
                        .putExtra(TetheringService.EXTRA_ADD_INTERFACE_MONITOR, iface))
                true
            }
        }
    }

    private var apConfigurationRunning = false
    override fun onMenuItemClick(item: MenuItem?): Boolean {
        return when (item?.itemId) {
            R.id.configuration -> item.subMenu!!.run {
                findItem(R.id.configuration_repeater).isNotGone = Services.p2p != null
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
            R.id.configuration_ap -> if (apConfigurationRunning) false else {
                apConfigurationRunning = true
                viewLifecycleOwner.lifecycleScope.launch {
                    val configuration = try {
                        if (Build.VERSION.SDK_INT < 30) @Suppress("DEPRECATION") {
                            WifiApManager.configurationLegacy?.toCompat() ?: SoftApConfigurationCompat()
                        } else WifiApManager.configuration.toCompat()
                    } catch (e: InvocationTargetException) {
                        if (e.targetException !is SecurityException) Timber.w(e)
                        try {
                            if (Build.VERSION.SDK_INT < 30) @Suppress("DEPRECATION") {
                                RootManager.use { it.execute(WifiApCommands.GetConfigurationLegacy()) }?.toCompat()
                                    ?: SoftApConfigurationCompat()
                            } else RootManager.use { it.execute(WifiApCommands.GetConfiguration()) }.toCompat()
                        } catch (_: CancellationException) {
                            return@launch
                        } catch (eRoot: Exception) {
                            eRoot.addSuppressed(e)
                            if (Build.VERSION.SDK_INT >= 29 || eRoot.getRootCause() !is SecurityException) {
                                Timber.w(eRoot)
                            }
                            SmartSnackbar.make(eRoot).show()
                            return@launch
                        }
                    } catch (e: IllegalArgumentException) {
                        Timber.w(e)
                        SmartSnackbar.make(e).show()
                        return@launch
                    }
                    withStarted {
                        WifiApDialogFragment().apply {
                            arg(WifiApDialogFragment.Arg(configuration))
                            key()
                        }.showAllowingStateLoss(parentFragmentManager)
                    }
                    apConfigurationRunning = false
                }
                true
            }
            else -> false
        }
    }

    override fun onCreateView(inflater: LayoutInflater, container: ViewGroup?, savedInstanceState: Bundle?): View {
        AlertDialogFragment.setResultListener<WifiApDialogFragment, WifiApDialogFragment.Arg>(this) { which, ret ->
            if (which == DialogInterface.BUTTON_POSITIVE) GlobalScope.launch {
                val configuration = ret!!.configuration
                if (Build.VERSION.SDK_INT < 30) @Suppress("DEPRECATION") {
                    if (configuration.isAutoShutdownEnabled != TetherTimeoutMonitor.enabled) try {
                        TetherTimeoutMonitor.setEnabled(configuration.isAutoShutdownEnabled)
                    } catch (e: Exception) {
                        Timber.w(e)
                        SmartSnackbar.make(e).show()
                    }
                    val wc = configuration.toWifiConfiguration()
                    try {
                        if (WifiApManager.setConfiguration(wc)) return@launch
                    } catch (e: InvocationTargetException) {
                        try {
                            if (RootManager.use { it.execute(WifiApCommands.SetConfigurationLegacy(wc)) }
                                .value) return@launch
                        } catch (e: CancellationException) {
                            return@launch SmartSnackbar.make(e).show()
                        } catch (eRoot: Exception) {
                            eRoot.addSuppressed(e)
                            Timber.w(eRoot)
                            return@launch SmartSnackbar.make(eRoot).show()
                        }
                    }
                } else {
                    val platform = try {
                        configuration.toPlatform()
                    } catch (e: InvocationTargetException) {
                        Timber.w(e)
                        return@launch SmartSnackbar.make(e).show()
                    }
                    try {
                        if (WifiApManager.setConfiguration(platform)) return@launch
                    } catch (e: InvocationTargetException) {
                        try {
                            if (RootManager.use { it.execute(WifiApCommands.SetConfiguration(platform)) }
                                    .value) return@launch
                        } catch (e: CancellationException) {
                            return@launch SmartSnackbar.make(e).show()
                        } catch (eRoot: Exception) {
                            eRoot.addSuppressed(e)
                            Timber.w(eRoot)
                            return@launch SmartSnackbar.make(eRoot).show()
                        }
                    }
                }
                SmartSnackbar.make(R.string.configuration_rejected).show()
            }
        }
        binding = FragmentTetheringBinding.inflate(inflater, container, false)
        binding.interfaces.layoutManager = LinearLayoutManager(context, RecyclerView.VERTICAL, false)
        binding.interfaces.itemAnimator = DefaultItemAnimator()
        binding.interfaces.adapter = adapter
        adapter.update()
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
        ManageBar.Data.notifyChange()
    }

    override fun onServiceConnected(name: ComponentName?, service: IBinder?) {
        binder = service as TetheringService.Binder
        service.routingsChanged[this] = {
            lifecycleScope.launch {
                withStarted { adapter.update() }
            }
        }
        requireContext().registerReceiver(receiver, IntentFilter(TetheringManager.ACTION_TETHER_STATE_CHANGED))
        if (Build.VERSION.SDK_INT >= 30) {
            TetheringManager.registerTetheringEventCallback(null, adapter)
            TetherType.listener[this] = {
                lifecycleScope.launch { adapter.notifyTetherTypeChanged() }
            }
        }
    }

    override fun onServiceDisconnected(name: ComponentName?) {
        (binder ?: return).routingsChanged -= this
        binder = null
        if (Build.VERSION.SDK_INT >= 30) {
            TetherType.listener -= this
            TetheringManager.unregisterTetheringEventCallback(adapter)
            adapter.lastErrors.clear()
        }
        requireContext().unregisterReceiver(receiver)
    }
}
