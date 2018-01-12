package be.mygod.vpnhotspot

import android.app.Application
import android.app.NotificationChannel
import android.app.NotificationManager
import android.content.SharedPreferences
import android.os.Build
import android.preference.PreferenceManager

class App : Application() {
    companion object {
        lateinit var app: App
    }

    override fun onCreate() {
        super.onCreate()
        app = this
        if (Build.VERSION.SDK_INT >= 26) getSystemService(NotificationManager::class.java)
                .createNotificationChannel(NotificationChannel(RepeaterService.CHANNEL,
                        "Hotspot Service", NotificationManager.IMPORTANCE_LOW))
    }

    val pref: SharedPreferences by lazy { PreferenceManager.getDefaultSharedPreferences(this) }
    val dns: String get() = app.pref.getString("service.dns", "8.8.8.8:53")
}
