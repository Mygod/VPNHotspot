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
import be.mygod.vpnhotspot.net.TetherOffloadManager
import be.mygod.vpnhotspot.net.monitor.FallbackUpstreamMonitor
import be.mygod.vpnhotspot.net.monitor.IpMonitor
import be.mygod.vpnhotspot.net.monitor.UpstreamMonitor
import be.mygod.vpnhotspot.net.wifi.WifiDoubleLock
import be.mygod.vpnhotspot.preference.AlwaysAutoCompleteEditTextPreferenceDialogFragment
import be.mygod.vpnhotspot.preference.SharedPreferenceDataStore
import be.mygod.vpnhotspot.preference.SummaryFallbackProvider
import be.mygod.vpnhotspot.util.RootSession
import be.mygod.vpnhotspot.util.launchUrl
import be.mygod.vpnhotspot.util.showAllowingStateLoss
import be.mygod.vpnhotspot.widget.SmartSnackbar
import com.google.android.gms.oss.licenses.OssLicensesMenuActivity
import com.google.android.material.snackbar.Snackbar
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import timber.log.Timber
import java.io.File
import java.io.IOException
import java.io.PrintWriter
import kotlin.system.exitProcess

class SettingsPreferenceFragment : PreferenceFragmentCompat() {
    override fun onCreatePreferences(savedInstanceState: Bundle?, rootKey: String?) {
        // handle complicated default value and possible system upgrades
        WifiDoubleLock.mode = WifiDoubleLock.mode
        RoutingManager.masqueradeMode = RoutingManager.masqueradeMode
        preferenceManager.preferenceDataStore = SharedPreferenceDataStore(app.pref)
        addPreferencesFromResource(R.xml.pref_settings)
        SummaryFallbackProvider(findPreference(UpstreamMonitor.KEY)!!)
        SummaryFallbackProvider(findPreference(FallbackUpstreamMonitor.KEY)!!)
        findPreference<SwitchPreference>("system.enableTetherOffload")!!.apply {
            if (Build.VERSION.SDK_INT >= 27) {
                isChecked = TetherOffloadManager.enabled
                setOnPreferenceChangeListener { _, newValue ->
                    if (TetherOffloadManager.enabled != newValue) {
                        isEnabled = false
                        GlobalScope.launch {
                            try {
                                TetherOffloadManager.enabled = newValue as Boolean
                            } catch (e: Exception) {
                                Timber.d(e)
                                SmartSnackbar.make(e).show()
                            }
                            withContext(Dispatchers.Main) {
                                isChecked = TetherOffloadManager.enabled
                                isEnabled = true
                            }
                        }
                    }
                    false
                }
            } else parent!!.removePreference(this)
        }
        val boot = findPreference<SwitchPreference>("service.repeater.startOnBoot")!!
        if (RepeaterService.supported) {
            boot.setOnPreferenceChangeListener { _, value ->
                BootReceiver.enabled = value as Boolean
                true
            }
            boot.isChecked = BootReceiver.enabled
        } else boot.parent!!.removePreference(boot)
        if (!RepeaterService.supported || !RepeaterService.safeModeConfigurable) {
            val safeMode = findPreference<Preference>(RepeaterService.KEY_SAFE_MODE)!!
            safeMode.parent!!.removePreference(safeMode)
        }
        findPreference<Preference>("service.clean")!!.setOnPreferenceClickListener {
            GlobalScope.launch { RoutingManager.clean() }
            true
        }
        findPreference<Preference>(IpMonitor.KEY)!!.setOnPreferenceChangeListener { _, _ ->
            Snackbar.make(requireView(), R.string.settings_restart_required, Snackbar.LENGTH_LONG).apply {
                setAction(R.string.settings_exit_app) {
                    GlobalScope.launch {
                        RoutingManager.clean(false)
                        RootSession.trimMemory()
                        exitProcess(0)
                    }
                }
            }.show()
            true
        }
        findPreference<Preference>("misc.logcat")!!.setOnPreferenceClickListener {
            GlobalScope.launch(Dispatchers.IO) {
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
                        val ip6tablesSave = if (Build.VERSION.SDK_INT >= 24) "ip6tables-save" else
                            File(app.deviceStorage.cacheDir, "ip6tables-save").absolutePath.also {
                                commands.appendln("ln -sf /system/bin/ip6tables $it")
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
                            |echo ip6tables-save
                            |$ip6tablesSave
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
                            |echo iptables -nvx -L vpnhotspot_acl
                            |$IPTABLES -nvx -L vpnhotspot_acl
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
                context.startActivity(Intent.createChooser(Intent(Intent.ACTION_SEND)
                        .setType("text/x-log")
                        .setFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
                        .putExtra(Intent.EXTRA_STREAM,
                                FileProvider.getUriForFile(context, "be.mygod.vpnhotspot.log", logFile)),
                        context.getString(R.string.abc_shareactionprovider_share_with)))
            }
            true
        }
        findPreference<Preference>("misc.source")!!.setOnPreferenceClickListener {
            requireContext().launchUrl("https://github.com/Mygod/VPNHotspot/blob/master/README.md")
            true
        }
        findPreference<Preference>("misc.donate")!!.setOnPreferenceClickListener {
            EBegFragment().showAllowingStateLoss(parentFragmentManager, "EBegFragment")
            true
        }
        findPreference<Preference>("misc.licenses")!!.setOnPreferenceClickListener {
            startActivity(Intent(context, OssLicensesMenuActivity::class.java))
            true
        }
    }

    override fun onDisplayPreferenceDialog(preference: Preference) {
        when (preference.key) {
            UpstreamMonitor.KEY, FallbackUpstreamMonitor.KEY ->
                AlwaysAutoCompleteEditTextPreferenceDialogFragment().apply {
                    setArguments(preference.key, app.connectivity.allNetworks.mapNotNull {
                        app.connectivity.getLinkProperties(it)?.interfaceName
                    }.toTypedArray())
                    setTargetFragment(this@SettingsPreferenceFragment, 0)
                }.showAllowingStateLoss(parentFragmentManager, preference.key)
            else -> super.onDisplayPreferenceDialog(preference)
        }
    }
}
