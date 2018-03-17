package be.mygod.vpnhotspot

import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.support.customtabs.CustomTabsIntent
import android.support.v4.content.ContextCompat
import android.support.v4.content.FileProvider
import android.support.v4.content.LocalBroadcastManager
import android.widget.Toast
import be.mygod.vpnhotspot.net.Routing
import com.takisoft.fix.support.v7.preference.PreferenceFragmentCompatDividers
import java.io.File
import java.io.IOException
import java.io.PrintWriter

class SettingsPreferenceFragment : PreferenceFragmentCompatDividers() {
    private val customTabsIntent by lazy {
        CustomTabsIntent.Builder()
                .setToolbarColor(ContextCompat.getColor(requireContext(), R.color.colorPrimary))
                .build()
    }

    override fun onCreatePreferencesFix(savedInstanceState: Bundle?, rootKey: String?) {
        addPreferencesFromResource(R.xml.pref_settings)
        findPreference("service.clean").setOnPreferenceClickListener {
            if (Routing.clean() == null) {
                Toast.makeText(requireContext(), R.string.root_unavailable, Toast.LENGTH_SHORT).show()
            } else {
                LocalBroadcastManager.getInstance(requireContext()).sendBroadcastSync(Intent(App.ACTION_CLEAN_ROUTINGS))
            }
            true
        }
        findPreference("misc.logcat").setOnPreferenceClickListener {
            val context = requireContext()
            val logDir = File(context.cacheDir, "log")
            logDir.mkdir()
            val logFile = File.createTempFile("vpnhotspot-", ".log", logDir)
            logFile.outputStream().use { out ->
                PrintWriter(out.bufferedWriter()).use { writer ->
                    writer.write("${BuildConfig.VERSION_CODE} is running on API ${Build.VERSION.SDK_INT}\n\n")
                    writer.flush()
                    try {
                        Runtime.getRuntime().exec(arrayOf("logcat", "-d")).inputStream.use { it.copyTo(out) }
                    } catch (e: IOException) {
                        e.printStackTrace(writer)
                    }
                    writer.write("\n")
                    writer.flush()
                    try {
                        Routing.dump()?.use { it.copyTo(out) }
                    } catch (e: IOException) {
                        e.printStackTrace(writer)
                        writer.flush()
                    }
                }
            }
            startActivity(Intent.createChooser(Intent(Intent.ACTION_SEND)
                    .setType("text/x-log")
                    .setFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
                    .putExtra(Intent.EXTRA_STREAM,
                            FileProvider.getUriForFile(context, "be.mygod.vpnhotspot.log", logFile)),
                    getString(R.string.abc_shareactionprovider_share_with)))
            true
        }
        findPreference("misc.source").setOnPreferenceClickListener {
            customTabsIntent.launchUrl(activity, Uri.parse("https://github.com/Mygod/VPNHotspot"))
            true
        }
    }
}
