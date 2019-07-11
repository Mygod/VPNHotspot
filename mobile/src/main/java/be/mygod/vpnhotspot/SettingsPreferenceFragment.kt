package be.mygod.vpnhotspot

import android.content.Context
import android.content.Intent
import android.os.Build
import android.os.Bundle
import androidx.core.content.FileProvider
import androidx.preference.Preference
import androidx.preference.PreferenceFragmentCompat
import androidx.preference.SwitchPreference
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.Routing.Companion.IPTABLES
import be.mygod.vpnhotspot.net.monitor.IpMonitor
import be.mygod.vpnhotspot.net.monitor.UpstreamMonitor
import be.mygod.vpnhotspot.net.wifi.WifiDoubleLock
import be.mygod.vpnhotspot.preference.AlwaysAutoCompleteEditTextPreferenceDialogFragmentCompat
import be.mygod.vpnhotspot.preference.SharedPreferenceDataStore
import be.mygod.vpnhotspot.util.RootSession
import be.mygod.vpnhotspot.util.launchUrl
import com.google.android.material.snackbar.Snackbar
import timber.log.Timber
import java.io.File
import java.io.IOException
import java.io.PrintWriter
import java.net.NetworkInterface
import java.net.SocketException
import kotlin.system.exitProcess

class SettingsPreferenceFragment : PreferenceFragmentCompat() {
    override fun onCreatePreferences(savedInstanceState: Bundle?, rootKey: String?) {
        WifiDoubleLock.mode = WifiDoubleLock.mode   // handle complicated default value and possible system upgrades
        preferenceManager.preferenceDataStore = SharedPreferenceDataStore(app.pref)
        RoutingManager.masqueradeMode = RoutingManager.masqueradeMode   // flush default value
        addPreferencesFromResource(R.xml.pref_settings)
        val boot = findPreference<SwitchPreference>("service.repeater.startOnBoot")!!
        if (RepeaterService.supported) {
            boot.setOnPreferenceChangeListener { _, value ->
                BootReceiver.enabled = value as Boolean
                true
            }
            boot.isChecked = BootReceiver.enabled
        } else boot.parent!!.removePreference(boot)
        findPreference<Preference>("service.clean")!!.setOnPreferenceClickListener {
            RoutingManager.clean()
            true
        }
        findPreference<Preference>(IpMonitor.KEY)!!.setOnPreferenceChangeListener { _, _ ->
            Snackbar.make(requireView(), R.string.settings_restart_required, Snackbar.LENGTH_LONG).apply {
                setAction(R.string.settings_exit_app) {
                    RoutingManager.clean(false)
                    RootSession.trimMemory()
                    exitProcess(0)
                }
            }.show()
            true
        }
        findPreference<Preference>("misc.logcat")!!.setOnPreferenceClickListener {
            val context = requireContext()
            val logDir = File(context.cacheDir, "log")
            logDir.mkdir()
            val logFile = File.createTempFile("vpnhotspot-", ".log", logDir)
            logFile.outputStream().use { out ->
                PrintWriter(out.bufferedWriter()).use { writer ->
                    writer.println("${BuildConfig.VERSION_CODE} is running on API ${Build.VERSION.SDK_INT}\n")
                    writer.flush()
                    try {
                        Runtime.getRuntime().exec(arrayOf("logcat", "-d")).inputStream.use { it.copyTo(out) }
                    } catch (e: IOException) {
                        Timber.w(e)
                    }
                    writer.println()
                    val commands = StringBuilder()
                    // https://android.googlesource.com/platform/external/iptables/+/android-7.0.0_r1/iptables/Android.mk#34
                    val iptablesSave = if (Build.VERSION.SDK_INT >= 24) "iptables-save" else
                        File(app.deviceStorage.cacheDir, "iptables-save").absolutePath.also {
                            commands.appendln("ln -sf /system/bin/iptables $it")
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
                        |echo ip neigh
                        |ip neigh
                        |echo
                        |echo iptables -nvx -L vpnhotspot_fwd
                        |$IPTABLES -nvx -L vpnhotspot_fwd
                        |echo
                        |echo logcat-su
                        |logcat -d
                    """.trimMargin())
                    try {
                        RootSession.use { it.execQuiet(commands.toString(), true).out.forEach(writer::println) }
                    } catch (e: Exception) {
                        e.printStackTrace(writer)
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
        findPreference<Preference>("misc.source")!!.setOnPreferenceClickListener {
            requireContext().launchUrl("https://github.com/Mygod/VPNHotspot/blob/master/README.md")
            true
        }
        findPreference<Preference>("misc.donate")!!.setOnPreferenceClickListener {
            EBegFragment().show(fragmentManager ?: return@setOnPreferenceClickListener false, "EBegFragment")
            true
        }
    }

    override fun onDisplayPreferenceDialog(preference: Preference) {
        when (preference.key) {
            UpstreamMonitor.KEY -> AlwaysAutoCompleteEditTextPreferenceDialogFragmentCompat().apply {
                setArguments(preference.key, try {
                    NetworkInterface.getNetworkInterfaces().asSequence()
                            .filter {
                                try {
                                    it.isUp && !it.isLoopback && it.interfaceAddresses.isNotEmpty()
                                } catch (_: SocketException) {
                                    false
                                }
                            }
                            .map { it.name }.sorted().toList().toTypedArray()
                } catch (e: SocketException) {
                    Timber.d(e)
                    emptyArray<String>()
                })
                setTargetFragment(this@SettingsPreferenceFragment, 0)
            }.show(fragmentManager ?: return, preference.key)
            else -> super.onDisplayPreferenceDialog(preference)
        }
    }
}
