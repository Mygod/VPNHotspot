package be.mygod.vpnhotspot

import android.content.Intent
import android.os.Build
import android.os.Bundle
import android.os.ext.SdkExtensions
import androidx.core.content.FileProvider
import androidx.lifecycle.lifecycleScope
import androidx.preference.Preference
import androidx.preference.PreferenceFragmentCompat
import androidx.preference.TwoStatePreference
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.TetherOffloadManager
import be.mygod.vpnhotspot.net.monitor.FallbackUpstreamMonitor
import be.mygod.vpnhotspot.net.monitor.IpMonitor
import be.mygod.vpnhotspot.net.monitor.UpstreamMonitor
import be.mygod.vpnhotspot.net.wifi.WifiDoubleLock
import be.mygod.vpnhotspot.preference.AutoCompleteNetworkPreferenceDialogFragment
import be.mygod.vpnhotspot.preference.SharedPreferenceDataStore
import be.mygod.vpnhotspot.preference.SummaryFallbackProvider
import be.mygod.vpnhotspot.root.Dump
import be.mygod.vpnhotspot.root.RootManager
import be.mygod.vpnhotspot.util.Services
import be.mygod.vpnhotspot.util.launchUrl
import be.mygod.vpnhotspot.util.showAllowingStateLoss
import be.mygod.vpnhotspot.widget.SmartSnackbar
import com.google.android.gms.oss.licenses.OssLicensesMenuActivity
import com.google.android.material.snackbar.Snackbar
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.launch
import timber.log.Timber
import java.io.File
import java.io.FileOutputStream
import java.io.IOException
import java.io.PrintWriter
import kotlin.system.exitProcess

class SettingsPreferenceFragment : PreferenceFragmentCompat() {
    private fun Preference.remove() = parent!!.removePreference(this)
    override fun onCreatePreferences(savedInstanceState: Bundle?, rootKey: String?) {
        // handle complicated default value and possible system upgrades
        WifiDoubleLock.mode = WifiDoubleLock.mode
        RoutingManager.masqueradeMode = RoutingManager.masqueradeMode
        IpMonitor.currentMode = IpMonitor.currentMode
        preferenceManager.preferenceDataStore = SharedPreferenceDataStore(app.pref)
        addPreferencesFromResource(R.xml.pref_settings)
        SummaryFallbackProvider(findPreference(UpstreamMonitor.KEY)!!)
        SummaryFallbackProvider(findPreference(FallbackUpstreamMonitor.KEY)!!)
        findPreference<TwoStatePreference>("system.enableTetherOffload")!!.apply {
            isChecked = TetherOffloadManager.enabled
            setOnPreferenceChangeListener { _, newValue ->
                if (TetherOffloadManager.enabled != newValue) viewLifecycleOwner.lifecycleScope.launch {
                    isEnabled = false
                    try {
                        TetherOffloadManager.setEnabled(newValue as Boolean)
                    } catch (_: CancellationException) {
                    } catch (e: Exception) {
                        Timber.w(e)
                        SmartSnackbar.make(e).show()
                    }
                    isChecked = TetherOffloadManager.enabled
                    isEnabled = true
                }
                false
            }
        }
        findPreference<TwoStatePreference>(BootReceiver.KEY)!!.setOnPreferenceChangeListener { _, value ->
            BootReceiver.onUserSettingUpdated(value as Boolean)
            true
        }
        if (Services.p2p == null || !RepeaterService.safeModeConfigurable) {
            val safeMode = findPreference<Preference>(RepeaterService.KEY_SAFE_MODE)!!
            safeMode.remove()
        }
        if (Build.VERSION.SDK_INT < 30) findPreference<Preference>(LocalOnlyHotspotService.KEY_USE_SYSTEM)!!.remove()
        findPreference<Preference>("service.clean")!!.setOnPreferenceClickListener {
            GlobalScope.launch { RoutingManager.clean() }
            true
        }
        findPreference<Preference>(IpMonitor.KEY)!!.setOnPreferenceChangeListener { _, _ ->
            Snackbar.make(requireView(), R.string.settings_restart_required, Snackbar.LENGTH_LONG).apply {
                setAction(R.string.settings_exit_app) {
                    GlobalScope.launch {
                        RoutingManager.clean(false)
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
                        writer.println("${BuildConfig.VERSION_CODE} is running on API ${Build.VERSION.SDK_INT}")
                        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) writer.println(
                            "S extension ${SdkExtensions.getExtensionVersion(Build.VERSION_CODES.S)}")
                        writer.println()
                        writer.flush()
                        try {
                            Runtime.getRuntime().exec(arrayOf("logcat", "-d")).inputStream.use { it.copyTo(out) }
                        } catch (e: IOException) {
                            Timber.w(e)
                            e.printStackTrace(writer)
                        }
                        writer.println()
                    }
                }
                try {
                    RootManager.use {
                        it.execute(Dump(logFile.absolutePath))
                    }
                } catch (e: Exception) {
                    if (e !is CancellationException) Timber.w(e)
                    PrintWriter(FileOutputStream(logFile, true)).use { e.printStackTrace(it) }
                }
                context.startActivity(Intent.createChooser(Intent(Intent.ACTION_SEND)
                        .setType("text/x-log")
                        .setFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
                        .putExtra(Intent.EXTRA_STREAM,
                                FileProvider.getUriForFile(context, "be.mygod.vpnhotspot.log", logFile)),
                        context.getString(androidx.appcompat.R.string.abc_shareactionprovider_share_with)))
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

    override fun onDisplayPreferenceDialog(preference: Preference) = when (preference.key) {
        UpstreamMonitor.KEY, FallbackUpstreamMonitor.KEY ->
            AutoCompleteNetworkPreferenceDialogFragment().apply {
                setArguments(preference.key)
                setTargetFragment(this@SettingsPreferenceFragment, 0)
            }.showAllowingStateLoss(parentFragmentManager, preference.key)
        else -> super.onDisplayPreferenceDialog(preference)
    }
}
