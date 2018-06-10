package be.mygod.vpnhotspot.manage

import android.annotation.TargetApi
import android.content.ComponentName
import android.content.Intent
import android.content.IntentFilter
import android.content.ServiceConnection
import android.content.pm.PackageManager
import android.databinding.DataBindingUtil
import android.os.Build
import android.os.Bundle
import android.os.IBinder
import android.support.v4.app.Fragment
import android.support.v7.recyclerview.extensions.ListAdapter
import android.support.v7.widget.DefaultItemAnimator
import android.support.v7.widget.LinearLayoutManager
import android.support.v7.widget.RecyclerView
import android.view.LayoutInflater
import android.view.View
import android.view.ViewGroup
import be.mygod.vpnhotspot.LocalOnlyHotspotService
import be.mygod.vpnhotspot.R
import be.mygod.vpnhotspot.TetheringService
import be.mygod.vpnhotspot.databinding.FragmentTetheringBinding
import be.mygod.vpnhotspot.net.TetherType
import be.mygod.vpnhotspot.net.TetheringManager
import be.mygod.vpnhotspot.util.ServiceForegroundConnector
import be.mygod.vpnhotspot.util.broadcastReceiver
import com.crashlytics.android.Crashlytics
import java.net.NetworkInterface
import java.net.SocketException

class TetheringFragment : Fragment(), ServiceConnection {
    companion object {
        const val START_LOCAL_ONLY_HOTSPOT = 1
    }

    inner class ManagerAdapter : ListAdapter<Manager, RecyclerView.ViewHolder>(Manager) {
        private val repeaterManager by lazy { RepeaterManager(this@TetheringFragment) }
        private val localOnlyHotspotManager by lazy @TargetApi(26) { LocalOnlyHotspotManager(this@TetheringFragment) }
        private val tetherManagers by lazy @TargetApi(24) {
            listOf(TetherManager.Wifi(this@TetheringFragment),
                    TetherManager.Usb(this@TetheringFragment),
                    TetherManager.Bluetooth(this@TetheringFragment))
        }
        private val wifiManagerLegacy by lazy @Suppress("Deprecation") {
            TetherManager.WifiLegacy(this@TetheringFragment)
        }

        fun update(activeIfaces: List<String>, localOnlyIfaces: List<String>) {
            ifaceLookup = try {
                NetworkInterface.getNetworkInterfaces().asSequence().associateBy { it.name }
            } catch (e: SocketException) {
                e.printStackTrace()
                Crashlytics.logException(e)
                emptyMap()
            }
            this@TetheringFragment.enabledTypes =
                    (activeIfaces + localOnlyIfaces).map { TetherType.ofInterface(it) }.toSet()

            val list = arrayListOf<Manager>(repeaterManager)
            if (Build.VERSION.SDK_INT >= 26) {
                list.add(localOnlyHotspotManager)
                localOnlyHotspotManager.update()
            }
            list.addAll(activeIfaces.map { InterfaceManager(this@TetheringFragment, it) }.sortedBy { it.iface })
            list.add(ManageBar)
            if (Build.VERSION.SDK_INT >= 24) {
                list.addAll(tetherManagers)
                tetherManagers.forEach { it.onTetheringStarted() }
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
    var tetheringBinder: TetheringService.Binder? = null
    val adapter = ManagerAdapter()
    private val receiver = broadcastReceiver { _, intent ->
        adapter.update(TetheringManager.getTetheredIfaces(intent.extras),
                TetheringManager.getLocalOnlyTetheredIfaces(intent.extras))
    }

    override fun onCreateView(inflater: LayoutInflater, container: ViewGroup?, savedInstanceState: Bundle?): View? {
        binding = DataBindingUtil.inflate(inflater, R.layout.fragment_tethering, container, false)
        binding.interfaces.layoutManager = LinearLayoutManager(context, LinearLayoutManager.VERTICAL, false)
        binding.interfaces.itemAnimator = DefaultItemAnimator()
        binding.interfaces.adapter = adapter
        adapter.update(emptyList(), emptyList())
        ServiceForegroundConnector(this, this, TetheringService::class)
        return binding.root
    }

    override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, grantResults: IntArray) {
        if (requestCode == START_LOCAL_ONLY_HOTSPOT) @TargetApi(26) {
            if (grantResults.firstOrNull() == PackageManager.PERMISSION_GRANTED) {
                val context = requireContext()
                context.startForegroundService(Intent(context, LocalOnlyHotspotService::class.java))
            }
        } else super.onRequestPermissionsResult(requestCode, permissions, grantResults)
    }

    override fun onServiceConnected(name: ComponentName?, service: IBinder?) {
        tetheringBinder = service as TetheringService.Binder
        service.fragment = this
        requireContext().registerReceiver(receiver, IntentFilter(TetheringManager.ACTION_TETHER_STATE_CHANGED))
    }

    override fun onServiceDisconnected(name: ComponentName?) {
        (tetheringBinder ?: return).fragment = null
        tetheringBinder = null
        requireContext().unregisterReceiver(receiver)
    }
}
