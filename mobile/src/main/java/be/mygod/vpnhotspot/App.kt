package be.mygod.vpnhotspot

import android.annotation.SuppressLint
import android.app.Application
import android.app.UiModeManager
import android.content.SharedPreferences
import android.content.res.Configuration
import android.net.ConnectivityManager
import android.net.wifi.WifiManager
import android.os.Build
import android.preference.PreferenceManager
import androidx.browser.customtabs.CustomTabsIntent
import androidx.core.content.ContextCompat
import androidx.core.content.getSystemService
import be.mygod.vpnhotspot.room.AppDatabase
import be.mygod.vpnhotspot.util.DeviceStorageApp
import be.mygod.vpnhotspot.util.Event0
import be.mygod.vpnhotspot.util.RootSession

class App : Application() {
    companion object {
        const val KEY_OPERATING_CHANNEL = "service.repeater.oc"

        @SuppressLint("StaticFieldLeak")
        lateinit var app: App
    }

    override fun onCreate() {
        super.onCreate()
        app = this
        if (Build.VERSION.SDK_INT >= 24) {
            deviceStorage = DeviceStorageApp(this)
            deviceStorage.moveSharedPreferencesFrom(this, PreferenceManager.getDefaultSharedPreferencesName(this))
            deviceStorage.moveDatabaseFrom(this, AppDatabase.DB_NAME)
        } else deviceStorage = this
        DebugHelper.init()
        ServiceNotification.updateNotificationChannels()
    }

    override fun onConfigurationChanged(newConfig: Configuration?) {
        super.onConfigurationChanged(newConfig)
        ServiceNotification.updateNotificationChannels()
    }

    override fun onTrimMemory(level: Int) {
        super.onTrimMemory(level)
        if (level >= TRIM_MEMORY_RUNNING_CRITICAL) RootSession.trimMemory()
    }

    lateinit var deviceStorage: Application
    val pref: SharedPreferences by lazy { PreferenceManager.getDefaultSharedPreferences(deviceStorage) }
    val connectivity by lazy { getSystemService<ConnectivityManager>()!! }
    val uiMode by lazy { getSystemService<UiModeManager>()!! }
    val wifi by lazy { getSystemService<WifiManager>()!! }

    val hasTouch by lazy { packageManager.hasSystemFeature("android.hardware.faketouch") }
    val customTabsIntent by lazy {
        CustomTabsIntent.Builder()
                .setToolbarColor(ContextCompat.getColor(this, R.color.colorPrimary))
                .build()
    }

    val operatingChannel: Int get() {
        val result = pref.getString(KEY_OPERATING_CHANNEL, null)?.toIntOrNull() ?: 0
        return if (result in 1..165) result else 0
    }
    val masquerade get() = pref.getBoolean("service.masquerade", true)
    val dhcpWorkaround get() = pref.getBoolean("service.dhcpWorkaround", false)

    val onPreCleanRoutings = Event0()
    val onRoutingsCleaned = Event0()
}
