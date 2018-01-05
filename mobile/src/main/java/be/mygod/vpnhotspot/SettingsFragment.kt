package be.mygod.vpnhotspot

import android.content.Intent
import android.net.Uri
import android.os.Bundle
import android.support.customtabs.CustomTabsIntent
import android.support.v4.content.ContextCompat
import android.support.v7.preference.Preference
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.preference.AlwaysAutoCompleteEditTextPreferenceDialogFragmentCompat
import com.takisoft.fix.support.v7.preference.PreferenceFragmentCompatDividers
import java.net.NetworkInterface

class SettingsFragment : PreferenceFragmentCompatDividers() {
    private val customTabsIntent by lazy {
        CustomTabsIntent.Builder().setToolbarColor(ContextCompat.getColor(activity!!, R.color.colorPrimary)).build()
    }

    override fun onCreatePreferencesFix(savedInstanceState: Bundle?, rootKey: String?) {
        addPreferencesFromResource(R.xml.pref_settings)
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

    override fun onDisplayPreferenceDialog(preference: Preference) = when (preference.key) {
        HotspotService.KEY_UPSTREAM -> displayPreferenceDialog(
                AlwaysAutoCompleteEditTextPreferenceDialogFragmentCompat(), HotspotService.KEY_UPSTREAM,
                Bundle().put(AlwaysAutoCompleteEditTextPreferenceDialogFragmentCompat.KEY_SUGGESTIONS,
                        NetworkInterface.getNetworkInterfaces().asSequence()
                                .filter { it.isUp && !it.isLoopback && it.interfaceAddresses.isNotEmpty() }
                                .map { it.name }.sorted().toList().toTypedArray()))
        HotspotService.KEY_WIFI -> displayPreferenceDialog(
                AlwaysAutoCompleteEditTextPreferenceDialogFragmentCompat(), HotspotService.KEY_WIFI, Bundle()
                    .put(AlwaysAutoCompleteEditTextPreferenceDialogFragmentCompat.KEY_SUGGESTIONS, app.wifiInterfaces))
        else -> super.onDisplayPreferenceDialog(preference)
    }
}
