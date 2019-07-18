package be.mygod.vpnhotspot.net.monitor

import android.content.Context
import android.content.Intent
import android.content.res.Resources
import android.database.ContentObserver
import android.os.BatteryManager
import android.os.Handler
import android.provider.Settings
import androidx.annotation.RequiresApi
import androidx.core.os.postDelayed
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.util.broadcastReceiver
import be.mygod.vpnhotspot.util.intentFilter
import timber.log.Timber
import java.lang.IllegalArgumentException

@RequiresApi(28)
class TetherTimeoutMonitor(private val context: Context, private val handler: Handler, private val onTimeout: () -> Unit):
        ContentObserver(handler), AutoCloseable {
    /**
     * config_wifi_framework_soft_ap_timeout_delay was introduced in Android 9.
     *
     * Source: https://android.googlesource.com/platform/frameworks/opt/net/wifi/+/87ed136/service/java/com/android/server/wifi/SoftApManager.java
     */
    companion object {
        /**
         * Whether soft AP will shut down after a timeout period when no devices are connected.
         *
         * Type: int (0 for false, 1 for true)
         */
        private const val SOFT_AP_TIMEOUT_ENABLED = "soft_ap_timeout_enabled"
        /**
         * Minimum limit to use for timeout delay if the value from overlay setting is too small.
         */
        private const val MIN_SOFT_AP_TIMEOUT_DELAY_MS = 600_000    // 10 minutes

        private val enabled get() = Settings.Global.getInt(app.contentResolver, SOFT_AP_TIMEOUT_ENABLED, 1) == 1
        private val timeout by lazy {
            app.resources.getInteger(Resources.getSystem().getIdentifier(
                    "config_wifi_framework_soft_ap_timeout_delay", "integer", "android")).let { delay ->
                if (delay < MIN_SOFT_AP_TIMEOUT_DELAY_MS) {
                    Timber.w("Overriding timeout delay with minimum limit value: $delay < $MIN_SOFT_AP_TIMEOUT_DELAY_MS")
                    MIN_SOFT_AP_TIMEOUT_DELAY_MS
                } else delay
            }
        }
    }

    private var charging = when (context.registerReceiver(null, intentFilter(Intent.ACTION_BATTERY_CHANGED))
            ?.getIntExtra(BatteryManager.EXTRA_STATUS, -1)) {
        BatteryManager.BATTERY_STATUS_CHARGING, BatteryManager.BATTERY_STATUS_FULL -> true
        null, -1 -> false.also { Timber.w(Exception("Battery status not found")) }
        else -> false
    }
    private var noClient = true
    private var timeoutPending = false

    private val receiver = broadcastReceiver { _, intent ->
        charging = when (intent.action) {
            Intent.ACTION_POWER_CONNECTED -> true
            Intent.ACTION_POWER_DISCONNECTED -> false
            else -> throw IllegalArgumentException("Invalid intent.action")
        }
        onChange(true)
    }.also {
        context.registerReceiver(it, intentFilter(Intent.ACTION_POWER_CONNECTED, Intent.ACTION_POWER_DISCONNECTED))
        context.contentResolver.registerContentObserver(Settings.Global.getUriFor(SOFT_AP_TIMEOUT_ENABLED), true, this)
    }

    override fun close() {
        context.unregisterReceiver(receiver)
        context.contentResolver.unregisterContentObserver(this)
    }

    fun onClientsChanged(noClient: Boolean) {
        this.noClient = noClient
        onChange(true)
    }

    override fun onChange(selfChange: Boolean) {
        // super.onChange(selfChange) should not do anything
        if (enabled && noClient && !charging) {
            if (!timeoutPending) {
                handler.postDelayed(timeout.toLong(), this, onTimeout)
                timeoutPending = true
            }
        } else {
            if (timeoutPending) {
                handler.removeCallbacksAndMessages(this)
                timeoutPending = false
            }
        }
    }
}
