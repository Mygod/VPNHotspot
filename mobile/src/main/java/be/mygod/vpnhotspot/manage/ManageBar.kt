package be.mygod.vpnhotspot.manage

import android.content.Intent
import be.mygod.vpnhotspot.App.Companion.app
import be.mygod.vpnhotspot.net.TetherOffloadManager

object ManageBar {
    private const val TAG = "ManageBar"
    private const val SETTINGS_PACKAGE = "com.android.settings"
    private const val SETTINGS_1 = "com.android.settings.Settings\$TetherSettingsActivity"
    private const val SETTINGS_2 = "com.android.settings.TetherSettings"

    val offloadEnabled get() = TetherOffloadManager.enabled

    fun start(startActivity: (Intent) -> Unit) {
        val intent = Intent().setFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        try {
            startActivity(intent.setClassName(SETTINGS_PACKAGE, SETTINGS_1))
        } catch (e1: RuntimeException) {
            try {
                startActivity(intent.setClassName(SETTINGS_PACKAGE, SETTINGS_2))
                app.logEvent(TAG) { param(SETTINGS_1, e1.toString()) }
            } catch (e2: RuntimeException) {
                app.logEvent(TAG) {
                    param(SETTINGS_1, e1.toString())
                    param(SETTINGS_2, e2.toString())
                }
            }
        }
    }
}
