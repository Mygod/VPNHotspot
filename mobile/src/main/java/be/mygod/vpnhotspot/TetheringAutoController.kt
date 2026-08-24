package be.mygod.vpnhotspot

import android.content.Intent
import android.content.SharedPreferences
import android.net.TetheringManager
import android.os.Build
import androidx.core.content.edit
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.TetherStates
import be.mygod.vpnhotspot.net.TetheringManagerCompat
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.Job
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.delay
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
        Target(KEY_BLUETOOTH, TetheringManager.TETHERING_BLUETOOTH) { it.startsWith("bt-pan") || it.startsWith("bnep") },
        Target(KEY_USB, TetheringManager.TETHERING_USB) { it.contains("rndis") || it.startsWith("usb") },
        Target(KEY_ETHERNET, TetheringManager.TETHERING_ETHERNET) { it.startsWith("eth") },
    )
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Default.limitedParallelism(1))
    private val retries = mutableMapOf<String, Job>()
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
        scope.launch { reconcile(lastTethered) }
    }

    override fun onSharedPreferenceChanged(sharedPreferences: SharedPreferences?, key: String?) {
        if (targets.any { it.key == key }) scope.launch { reconcile(lastTethered) }
    }

    private fun isEnabled(target: Target): Boolean {
        if (target.key == KEY_ETHERNET && Build.VERSION.SDK_INT < 30) return false
        return app.pref.getBoolean(target.key, false)
    }

    private fun reconcile(tethered: Set<String>) {
        val monitored = targets.filter(::isEnabled).flatMap { target -> tethered.filter(target.matches) }.distinct()
        if (monitored.isNotEmpty()) {
            val intent = Intent(app, TetheringService::class.java)
                .putStringArrayListExtra(TetheringService.EXTRA_ADD_INTERFACES_MONITOR, ArrayList(monitored))
            app.startForegroundService(intent)
        }
        for (target in targets) {
            if (!isEnabled(target) || tethered.any(target.matches)) {
                retries.remove(target.key)?.cancel()
                continue
            }
            if (retries[target.key]?.isActive == true) continue
            retries[target.key] = scope.launch {
                var delayMs = 2_000L
                while (isEnabled(target) && lastTethered.none(target.matches)) {
                    try {
                        TetheringManagerCompat.startTethering(target.type, false)
                        Timber.i("Auto tethering start succeeded for ${target.key}")
                        return@launch
                    } catch (e: CancellationException) {
                        throw e
                    } catch (e: Exception) {
                        Timber.w(e, "Auto tethering start failed for ${target.key}; retrying in $delayMs ms")
                        delay(delayMs)
                        delayMs = (delayMs * 2).coerceAtMost(300_000L)
                    }
                }
            }
        }
    }
}
