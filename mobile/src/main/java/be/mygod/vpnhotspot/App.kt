package be.mygod.vpnhotspot

import android.app.Application
import android.app.NotificationChannel
import android.app.NotificationManager
import android.content.SharedPreferences
import android.content.res.Resources
import android.os.Build
import android.preference.PreferenceManager
import java.net.NetworkInterface

class App : Application() {
    companion object {
        lateinit var app: App
    }

    override fun onCreate() {
        super.onCreate()
        app = this
        if (Build.VERSION.SDK_INT >= 26) getSystemService(NotificationManager::class.java)
                .createNotificationChannel(NotificationChannel(HotspotService.CHANNEL,
                        "Hotspot Service", NotificationManager.IMPORTANCE_LOW))
        pref = PreferenceManager.getDefaultSharedPreferences(this)
        val wifiRegexes = resources.getStringArray(Resources.getSystem()
                .getIdentifier("config_tether_wifi_regexs", "array", "android"))
                .map { it.toPattern() }
        wifiInterfaces = NetworkInterface.getNetworkInterfaces().asSequence()
                .map { it.name }
                .filter { ifname -> wifiRegexes.any { it.matcher(ifname).matches() } }
                .sorted().toList().toTypedArray()
        val wifiInterface = wifiInterfaces.singleOrNull()
        if (wifiInterface != null && pref.getString(HotspotService.KEY_WIFI, null) == null)
            pref.edit().putString(HotspotService.KEY_WIFI, wifiInterface).apply()
    }

    lateinit var pref: SharedPreferences
    lateinit var wifiInterfaces: Array<String>
}
