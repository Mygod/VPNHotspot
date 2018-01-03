package be.mygod.vpnhotspot

import android.app.Application
import android.app.NotificationChannel
import android.app.NotificationManager
import android.os.Build

class App : Application() {
    override fun onCreate() {
        super.onCreate()
        if (Build.VERSION.SDK_INT >= 26) getSystemService(NotificationManager::class.java)
                .createNotificationChannel(NotificationChannel(HotspotService.CHANNEL,
                        "Hotspot Service", NotificationManager.IMPORTANCE_LOW))
    }
}
