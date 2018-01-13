package be.mygod.vpnhotspot

import android.annotation.TargetApi
import android.app.Application
import android.app.NotificationChannel
import android.app.NotificationManager
import android.content.SharedPreferences
import android.content.res.Configuration
import android.os.Build
import android.preference.PreferenceManager

class App : Application() {
    companion object {
        const val ACTION_CLEAN_ROUTINGS = "be.mygod.vpnhotspot.CLEAN_ROUTINGS"

        lateinit var app: App
    }

    override fun onCreate() {
        super.onCreate()
        app = this
        updateNotificationChannels()
    }

    override fun onConfigurationChanged(newConfig: Configuration?) {
        super.onConfigurationChanged(newConfig)
        updateNotificationChannels()
    }

    private fun updateNotificationChannels() {
        if (Build.VERSION.SDK_INT >= 26) @TargetApi(26) {
            val nm = getSystemService(NotificationManager::class.java)
            nm.createNotificationChannel(NotificationChannel(RepeaterService.CHANNEL,
                    getText(R.string.notification_channel_repeater), NotificationManager.IMPORTANCE_LOW))
            nm.deleteNotificationChannel("hotspot") // remove old service channel
        }
    }

    val pref: SharedPreferences by lazy { PreferenceManager.getDefaultSharedPreferences(this) }
    val dns: String get() = app.pref.getString("service.dns", "8.8.8.8:53")
}
