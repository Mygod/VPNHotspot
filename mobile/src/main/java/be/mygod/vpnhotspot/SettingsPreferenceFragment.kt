package be.mygod.vpnhotspot

import android.content.Context
import android.content.Intent
import android.os.Build
import android.os.Bundle
import androidx.core.content.FileProvider
import androidx.core.net.toUri
import androidx.core.os.bundleOf
import androidx.fragment.app.DialogFragment
import androidx.preference.Preference
import androidx.preference.SwitchPreference
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.Routing
import be.mygod.vpnhotspot.net.UpstreamMonitor
import be.mygod.vpnhotspot.preference.AlwaysAutoCompleteEditTextPreferenceDialogFragmentCompat
import be.mygod.vpnhotspot.preference.SharedPreferenceDataStore
import be.mygod.vpnhotspot.util.RootSession
import be.mygod.vpnhotspot.widget.SmartSnackbar
import com.takisoft.preferencex.PreferenceFragmentCompat
import timber.log.Timber
import java.io.File
import java.io.IOException
import java.io.PrintWriter
import java.net.NetworkInterface
import java.net.SocketException

class SettingsPreferenceFragment : PreferenceFragmentCompat() {
    override fun onCreatePreferencesFix(savedInstanceState: Bundle?, rootKey: String?) {
        preferenceManager.preferenceDataStore = SharedPreferenceDataStore(app.pref)
        addPreferencesFromResource(R.xml.pref_settings)
        val boot = findPreference("service.repeater.startOnBoot") as SwitchPreference
        boot.setOnPreferenceChangeListener { _, value ->
            BootReceiver.enabled = value as Boolean
            true
        }
        boot.isChecked = BootReceiver.enabled
        findPreference("service.clean").setOnPreferenceClickListener {
            val cleaned = try {
                Routing.clean()
                true
            } catch (e: RuntimeException) {
                Timber.d(e)
                SmartSnackbar.make(e.localizedMessage).show()
                false
            }
            if (cleaned) app.cleanRoutings()
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
                        Timber.w(e)
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
                        |echo dumpsys ${Context.WIFI_P2P_SERVICE}
                        |dumpsys ${Context.WIFI_P2P_SERVICE}
                        |echo
                        |echo dumpsys ${Context.CONNECTIVITY_SERVICE} tethering
                        |dumpsys ${Context.CONNECTIVITY_SERVICE} tethering
                        |echo
                        |echo iptables -t filter
                        |$iptablesSave -t filter
                        |echo
                        |echo iptables -t nat
                        |$iptablesSave -t nat
                        |echo
                        |echo ip rule
                        |ip rule
                        |echo
                        |echo logcat-su
                        |logcat -d
                    """.trimMargin())
                    try {
                        out.write(RootSession.use { it.execQuiet(commands.toString(), true).out }
                                .joinToString("\n").toByteArray())
                    } catch (e: Exception) {
                        e.printStackTrace(writer)
                        writer.flush()
                        Timber.i(e)
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
            (activity as MainActivity).launchUrl("https://github.com/Mygod/VPNHotspot".toUri())
            true
        }
        findPreference("misc.donate").setOnPreferenceClickListener {
            EBegFragment().apply { setStyle(DialogFragment.STYLE_NO_TITLE, 0) }.show(fragmentManager, "EBegFragment")
            true
        }
    }

    override fun onDisplayPreferenceDialog(preference: Preference) = when (preference.key) {
        UpstreamMonitor.KEY -> displayPreferenceDialog(
                AlwaysAutoCompleteEditTextPreferenceDialogFragmentCompat(), UpstreamMonitor.KEY,
                bundleOf(Pair(AlwaysAutoCompleteEditTextPreferenceDialogFragmentCompat.KEY_SUGGESTIONS,
                        try {
                            NetworkInterface.getNetworkInterfaces().asSequence()
                                    .filter { it.isUp && !it.isLoopback && it.interfaceAddresses.isNotEmpty() }
                                    .map { it.name }.sorted().toList().toTypedArray()
                        } catch (e: SocketException) {
                            Timber.w(e)
                            emptyArray<String>()
                        })))
        else -> super.onDisplayPreferenceDialog(preference)
    }
}
