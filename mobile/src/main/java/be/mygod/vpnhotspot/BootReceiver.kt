package be.mygod.vpnhotspot

import android.content.BroadcastReceiver
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import androidx.core.content.ContextCompat
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.util.Services

class BootReceiver : BroadcastReceiver() {
    companion object {
        private val componentName by lazy { ComponentName(app, BootReceiver::class.java) }
        var enabled: Boolean
            get() = app.packageManager.getComponentEnabledSetting(componentName) ==
                    PackageManager.COMPONENT_ENABLED_STATE_ENABLED
            set(value) = app.packageManager.setComponentEnabledSetting(componentName,
                    if (value) PackageManager.COMPONENT_ENABLED_STATE_ENABLED
                    else PackageManager.COMPONENT_ENABLED_STATE_DISABLED, PackageManager.DONT_KILL_APP)

        private var started = false
    }

    override fun onReceive(context: Context, intent: Intent) {
        if (started) return
        when (intent.action) {
            Intent.ACTION_BOOT_COMPLETED, Intent.ACTION_LOCKED_BOOT_COMPLETED -> started = true
            else -> return
        }
        if (Services.p2p != null) {
            ContextCompat.startForegroundService(context, Intent(context, RepeaterService::class.java))
        }
    }
}
