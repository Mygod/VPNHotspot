package be.mygod.vpnhotspot

import android.annotation.SuppressLint
import android.app.Application
import android.content.SharedPreferences
import android.content.res.Configuration
import android.net.ConnectivityManager
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Handler
import android.preference.PreferenceManager
import android.widget.Toast
import androidx.annotation.StringRes
import androidx.core.content.getSystemService
import be.mygod.vpnhotspot.util.DeviceStorageApp
import be.mygod.vpnhotspot.util.Event0
import com.crashlytics.android.Crashlytics
import io.fabric.sdk.android.Fabric

class App : Application() {
    companion object {
        const val KEY_OPERATING_CHANNEL = "service.repeater.oc"
        private const val KEY_MASQUERADE = "service.masquerade"

        @SuppressLint("StaticFieldLeak")
        lateinit var app: App
    }

    override fun onCreate() {
        super.onCreate()
        app = this
        if (Build.VERSION.SDK_INT >= 24) {
            deviceStorage = DeviceStorageApp(this)
            deviceStorage.moveSharedPreferencesFrom(this, PreferenceManager.getDefaultSharedPreferencesName(this))
        } else deviceStorage = this
        Fabric.with(deviceStorage, Crashlytics())
        ServiceNotification.updateNotificationChannels()
    }

    override fun onConfigurationChanged(newConfig: Configuration?) {
        super.onConfigurationChanged(newConfig)
        ServiceNotification.updateNotificationChannels()
    }

    lateinit var deviceStorage: Application
    val handler = Handler()
    val pref: SharedPreferences by lazy { PreferenceManager.getDefaultSharedPreferences(deviceStorage) }
    val connectivity by lazy { getSystemService<ConnectivityManager>()!! }
    val wifi by lazy { getSystemService<WifiManager>()!! }

    val operatingChannel: Int get() {
        val result = pref.getString(KEY_OPERATING_CHANNEL, null)?.toIntOrNull() ?: 0
        return if (result in 1..165) result else 0
    }
    val masquerade: Boolean get() = pref.getBoolean(KEY_MASQUERADE, true)

    val cleanRoutings = Event0()

    fun toast(@StringRes resId: Int) = handler.post { Toast.makeText(this, resId, Toast.LENGTH_SHORT).show() }
}
