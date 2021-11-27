package be.mygod.vpnhotspot.net.monitor

import android.content.res.Resources
import android.os.Build
import android.provider.Settings
import androidx.annotation.RequiresApi
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.wifi.WifiApManager
import be.mygod.vpnhotspot.root.SettingsGlobalPut
import be.mygod.vpnhotspot.util.findIdentifier
import kotlinx.coroutines.*
import timber.log.Timber
import kotlin.coroutines.CoroutineContext

class TetherTimeoutMonitor(private val timeout: Long = 0,
                           private val context: CoroutineContext = Dispatchers.Main.immediate,
                           private val onTimeout: () -> Unit) : AutoCloseable {
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

        @Deprecated("Use SoftApConfigurationCompat instead")
        @get:RequiresApi(28)
        val enabled get() = Settings.Global.getInt(app.contentResolver, SOFT_AP_TIMEOUT_ENABLED, 1) == 1
        @Deprecated("Use SoftApConfigurationCompat instead")
        suspend fun setEnabled(value: Boolean) = SettingsGlobalPut.int(SOFT_AP_TIMEOUT_ENABLED, if (value) 1 else 0)

        val defaultTimeout: Int get() {
            val delay = if (Build.VERSION.SDK_INT >= 28) try {
                if (Build.VERSION.SDK_INT < 30) Resources.getSystem().run {
                    getInteger(getIdentifier("config_wifi_framework_soft_ap_timeout_delay", "integer", "android"))
                } else {
                    val info = WifiApManager.resolvedActivity.activityInfo
                    val resources = app.packageManager.getResourcesForApplication(info.applicationInfo)
                    resources.getInteger(resources.findIdentifier(
                        "config_wifiFrameworkSoftApShutDownTimeoutMilliseconds", "integer",
                        WifiApManager.RESOURCES_PACKAGE, info.packageName))
                }
            } catch (e: RuntimeException) {
                Timber.w(e)
                MIN_SOFT_AP_TIMEOUT_DELAY_MS
            } else MIN_SOFT_AP_TIMEOUT_DELAY_MS
            return if (Build.VERSION.SDK_INT < 30 && delay < MIN_SOFT_AP_TIMEOUT_DELAY_MS) {
                Timber.w("Overriding timeout delay with minimum limit value: $delay < $MIN_SOFT_AP_TIMEOUT_DELAY_MS")
                MIN_SOFT_AP_TIMEOUT_DELAY_MS
            } else delay
        }
    }

    private var noClient = true
    private var timeoutJob: Job? = null

    init {
        onClientsChanged(true)
    }

    override fun close() {
        timeoutJob?.cancel()
        timeoutJob = null
    }

    fun onClientsChanged(noClient: Boolean) {
        this.noClient = noClient
        if (!noClient) close() else if (timeoutJob == null) timeoutJob = GlobalScope.launch(context) {
            delay(if (timeout == 0L) defaultTimeout.toLong() else timeout)
            onTimeout()
        }
    }
}
