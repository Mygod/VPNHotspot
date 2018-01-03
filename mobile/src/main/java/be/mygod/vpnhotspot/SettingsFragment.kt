package be.mygod.vpnhotspot

import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.ServiceConnection
import android.net.Uri
import android.os.Bundle
import android.os.IBinder
import android.support.customtabs.CustomTabsIntent
import android.support.v4.content.ContextCompat
import android.support.v4.content.LocalBroadcastManager
import android.support.v7.preference.Preference
import com.takisoft.fix.support.v7.preference.PreferenceFragmentCompatDividers

class SettingsFragment : PreferenceFragmentCompatDividers(), ServiceConnection {
    private lateinit var service: Preference
    private var binder: HotspotService.HotspotBinder? = null
    private val statusListener = broadcastReceiver { _, _ -> onStatusChanged() }
    private val customTabsIntent by lazy {
        CustomTabsIntent.Builder().setToolbarColor(ContextCompat.getColor(activity!!, R.color.colorPrimary)).build()
    }

    override fun onCreatePreferencesFix(savedInstanceState: Bundle?, rootKey: String?) {
        addPreferencesFromResource(R.xml.pref_settings)
        service = findPreference("service")
        findPreference("service.clean").setOnPreferenceClickListener {
            noisySu("iptables -t nat -F PREROUTING",
                    "while iptables -D FORWARD -j vpnhotspot_fwd; do done",
                    "iptables -F vpnhotspot_fwd",
                    "iptables -X vpnhotspot_fwd",
                    "ip rule del lookup 62",
                    "ip route flush table 62")
            true
        }
        findPreference("misc.logcat").setOnPreferenceClickListener {
            val intent = Intent(Intent.ACTION_SEND)
                    .setType("text/plain")
                    .putExtra(Intent.EXTRA_TEXT, Runtime.getRuntime().exec(arrayOf("logcat", "-d"))
                            .inputStream.bufferedReader().use { it.readText() })
            startActivity(Intent.createChooser(intent, getString(R.string.abc_shareactionprovider_share_with)))
            true
        }
        findPreference("misc.source").setOnPreferenceClickListener {
            customTabsIntent.launchUrl(activity, Uri.parse("https://github.com/Mygod/VPNHotspot"))
            true
        }
        findPreference("misc.donate").setOnPreferenceClickListener {
            customTabsIntent.launchUrl(activity, Uri.parse("https://mygod.be/donate/"))
            true
        }
    }

    override fun onStart() {
        super.onStart()
        val activity = activity!!
        activity.bindService(Intent(activity, HotspotService::class.java), this, Context.BIND_AUTO_CREATE)
    }

    override fun onStop() {
        onServiceDisconnected(null)
        activity!!.unbindService(this)
        super.onStop()
    }

    override fun onServiceConnected(name: ComponentName?, service: IBinder?) {
        binder = service as HotspotService.HotspotBinder
        onStatusChanged()
        LocalBroadcastManager.getInstance(activity!!)
                .registerReceiver(statusListener, intentFilter(HotspotService.STATUS_CHANGED))
    }

    override fun onServiceDisconnected(name: ComponentName?) {
        LocalBroadcastManager.getInstance(activity!!).unregisterReceiver(statusListener)
        binder = null
        service.isEnabled = false
    }

    private fun onStatusChanged() {
        service.isEnabled = binder!!.service.status == HotspotService.Status.IDLE
    }
}
