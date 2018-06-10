package be.mygod.vpnhotspot

import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.support.customtabs.CustomTabsIntent
import android.support.v4.content.ContextCompat
import android.support.v4.content.FileProvider
import android.support.v7.preference.Preference
import android.widget.Toast
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.Routing
import be.mygod.vpnhotspot.net.UpstreamMonitor
import be.mygod.vpnhotspot.preference.AlwaysAutoCompleteEditTextPreferenceDialogFragmentCompat
import be.mygod.vpnhotspot.preference.SharedPreferenceDataStore
import be.mygod.vpnhotspot.util.loggerSuStream
import be.mygod.vpnhotspot.util.put
import com.crashlytics.android.Crashlytics
import com.takisoft.fix.support.v7.preference.PreferenceFragmentCompat
import java.io.File
import java.io.IOException
import java.io.PrintWriter
import java.net.NetworkInterface
import java.net.SocketException

class SettingsPreferenceFragment : PreferenceFragmentCompat() {
    private val customTabsIntent by lazy {
        CustomTabsIntent.Builder()
                .setToolbarColor(ContextCompat.getColor(requireContext(), R.color.colorPrimary))
                .build()
    }

    override fun onCreatePreferencesFix(savedInstanceState: Bundle?, rootKey: String?) {
        preferenceManager.preferenceDataStore = SharedPreferenceDataStore(app.pref)
        addPreferencesFromResource(R.xml.pref_settings)
        findPreference("service.clean").setOnPreferenceClickListener {
            if (Routing.clean() == null) {
                Toast.makeText(requireContext(), R.string.root_unavailable, Toast.LENGTH_SHORT).show()
            } else app.cleanRoutings()
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
                        Crashlytics.logException(e)
                    }
                    writer.write("\n")
                    writer.flush()
                    val commands = StringBuilder()
                    // https://android.googlesource.com/platform/external/iptables/+/android-7.0.0_r1/iptables/Android.mk#34
                    val iptablesSave = if (Build.VERSION.SDK_INT >= 24) "iptables-save" else {
                        commands.appendln("ln -sf /system/bin/iptables ./iptables-save")
                        "./iptables-save"
                    }
                    commands.append("""
                        |echo logcat-su
                        |logcat -d
                        |echo
                        |echo dumpsys ${Context.WIFI_P2P_SERVICE}
                        |dumpsys ${Context.WIFI_P2P_SERVICE}
                        |echo
                        |echo iptables -t filter
                        |$iptablesSave -t filter
                        |echo
                        |echo iptables -t nat
                        |$iptablesSave -t nat
                        |echo
                        |echo ip rule
                        |ip rule
                    """.trimMargin())
                    try {
                        loggerSuStream(commands.toString())?.use { it.copyTo(out) }
                    } catch (e: IOException) {
                        e.printStackTrace(writer)
                        Crashlytics.logException(e)
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

    override fun onDisplayPreferenceDialog(preference: Preference) = when (preference.key) {
        UpstreamMonitor.KEY -> displayPreferenceDialog(
                AlwaysAutoCompleteEditTextPreferenceDialogFragmentCompat(), UpstreamMonitor.KEY,
                Bundle().put(AlwaysAutoCompleteEditTextPreferenceDialogFragmentCompat.KEY_SUGGESTIONS,
                        try {
                            NetworkInterface.getNetworkInterfaces().asSequence()
                                    .filter { it.isUp && !it.isLoopback && it.interfaceAddresses.isNotEmpty() }
                                    .map { it.name }.sorted().toList().toTypedArray()
                        } catch (e: SocketException) {
                            e.printStackTrace()
                            Crashlytics.logException(e)
                            emptyArray<String>()
                        }))
        else -> super.onDisplayPreferenceDialog(preference)
    }
}
