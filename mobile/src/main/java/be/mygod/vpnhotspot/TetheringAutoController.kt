package be.mygod.vpnhotspot

import android.content.SharedPreferences
import android.net.TetheringManager
import android.os.Build
import androidx.core.content.edit
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.TetherStates
import be.mygod.vpnhotspot.net.TetheringManagerCompat
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.flow.map
import kotlinx.coroutines.launch
import timber.log.Timber

/** Event-driven replacement for the four one-second polling auto starters. */
object TetheringAutoController : SharedPreferences.OnSharedPreferenceChangeListener {
    const val KEY_WIFI = "service.auto.wifiTethering"
    const val KEY_BLUETOOTH = "service.auto.bluetoothTethering"
    const val KEY_USB = "service.auto.usbTethering"
    const val KEY_ETHERNET = "service.auto.ethernetTethering"

    private data class Target(val key: String, val type: Int, val matches: (String) -> Boolean)
    private val targets = listOf(
        Target(KEY_WIFI, TetheringManager.TETHERING_WIFI) { it.startsWith("wlan") || it.startsWith("ap") },
        Target(KEY_BLUETOOTH, TetheringManagerCompat.TETHERING_BLUETOOTH) { it.startsWith("bt-pan") || it.startsWith("bnep") },
        Target(KEY_USB, TetheringManagerCompat.TETHERING_USB) { it.contains("rndis") || it.startsWith("usb") },
        Target(KEY_ETHERNET, TetheringManagerCompat.TETHERING_ETHERNET) { it.startsWith("eth") },
    )
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default.limitedParallelism(1))
    // Bound missing OEM callbacks to 30 seconds, then allow 15 seconds for the actual
    // tether-state event. Each type owns its request and exponentially backs off to five minutes.
    private val retries = targets.associate { target ->
        target.key to AutoTetheringTarget(scope, 30_000, 15_000, 2_000, 300_000,
            { TetheringManagerCompat.startTethering(target.type, false) },
            { error, delayMs -> Timber.w(error, "Auto tethering start failed for ${target.key}; retrying in $delayMs ms") })
    }
    private var started = false
    private var lastTethered = emptySet<String>()

    @Synchronized fun start() {
        if (started) return
        started = true
        app.pref.registerOnSharedPreferenceChangeListener(this)
        scope.launch {
            TetherStates.flow.map { it.tethered.toSet() }.distinctUntilChanged().collect { tethered ->
                lastTethered = tethered
                reconcile(tethered)
            }
        }
    }

    fun setEnabled(key: String, enabled: Boolean) {
        app.pref.edit { putBoolean(key, enabled) }
    }

    override fun onSharedPreferenceChanged(sharedPreferences: SharedPreferences?, key: String?) {
        if (targets.any { it.key == key }) scope.launch { reconcile(lastTethered) }
    }

    private fun isEnabled(target: Target): Boolean {
        if (target.key == KEY_ETHERNET && Build.VERSION.SDK_INT < 30) return false
        return app.pref.getBoolean(target.key, false)
    }

    private fun reconcile(tethered: Set<String>) {
        // Auto recovery owns Android tethering only. VPN downstream routing and its reconnect
        // monitor are explicit per-interface user choices in TetheringScreen and must never be
        // inferred from service.auto.* tethering preferences.
        for (target in targets) {
            retries.getValue(target.key).update(isEnabled(target) && tethered.none(target.matches))
        }
    }
}
