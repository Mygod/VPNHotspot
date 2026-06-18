package be.mygod.vpnhotspot.manage

import android.content.Intent
import android.os.Build
import androidx.annotation.DoNotInline
import be.mygod.vpnhotspot.App.Companion.app
import timber.log.Timber

object ManageBar {
    private const val ACTION_TETHER_SETTINGS = "android.settings.TETHER_SETTINGS"
    @DoNotInline
    fun start(startActivity: (Intent) -> Unit) {
        var eSuppressed: RuntimeException? = null
        if (Build.VERSION.SDK_INT >= 30) try {
            startActivity(Intent(ACTION_TETHER_SETTINGS).setFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
            return
        } catch (e: RuntimeException) {
            app.logEvent(ACTION_TETHER_SETTINGS)
            eSuppressed = e
        }
        try {
            startActivity(Intent().setFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                .setClassName("com.android.settings", "com.android.settings.Settings\$TetherSettingsActivity"))
        } catch (e: RuntimeException) {
            eSuppressed?.let { e.addSuppressed(eSuppressed) }
            Timber.w(e)
        }
    }
}
