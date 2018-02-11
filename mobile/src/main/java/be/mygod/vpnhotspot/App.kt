package be.mygod.vpnhotspot

import android.app.Application
import android.content.SharedPreferences
import android.content.res.Configuration
import android.os.Handler
import android.preference.PreferenceManager
import android.support.annotation.StringRes
import android.widget.Toast

class App : Application() {
    companion object {
        const val ACTION_CLEAN_ROUTINGS = "be.mygod.vpnhotspot.CLEAN_ROUTINGS"

        lateinit var app: App
    }

    override fun onCreate() {
        super.onCreate()
        app = this
        ServiceNotification.updateNotificationChannels()
    }

    override fun onConfigurationChanged(newConfig: Configuration?) {
        super.onConfigurationChanged(newConfig)
        ServiceNotification.updateNotificationChannels()
    }

    val handler = Handler()
    val pref: SharedPreferences by lazy { PreferenceManager.getDefaultSharedPreferences(this) }
    val dns: String get() = pref.getString("service.dns", "8.8.8.8")

    fun toast(@StringRes resId: Int) = handler.post { Toast.makeText(this, resId, Toast.LENGTH_SHORT).show() }
}
