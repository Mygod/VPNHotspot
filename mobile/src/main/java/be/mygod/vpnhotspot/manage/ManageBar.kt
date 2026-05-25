package be.mygod.vpnhotspot.manage

import android.content.Intent
import android.os.Build
import timber.log.Timber

object ManageBar {
    fun start(startActivity: (Intent) -> Unit) {
        if (Build.VERSION.SDK_INT >= 30) try {
            startActivity(Intent("android.settings.TETHER_SETTINGS").setFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
            return
        } catch (e: RuntimeException) {
            Timber.w(e)
        }
        try {
            startActivity(Intent().setFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
                .setClassName("com.android.settings", "com.android.settings.Settings\$TetherSettingsActivity"))
        } catch (e: RuntimeException) {
            Timber.w(e)
        }
    }
}
