package be.mygod.vpnhotspot.util

import android.annotation.SuppressLint
import android.app.Application
import android.content.ComponentCallbacks
import android.content.Context
import android.content.res.Configuration

@SuppressLint("MissingSuperCall", "Registered")
class DeviceStorageApp(private val app: Application) : Application() {
    init {
        attachBaseContext(app.createDeviceProtectedStorageContext())
    }

    /**
     * Thou shalt not get the REAL underlying application context which would no longer be operating under device
     * protected storage.
     */
    override fun getApplicationContext(): Context = this

    override fun onCreate() = app.onCreate()
    override fun onTerminate() = app.onTerminate()
    override fun onConfigurationChanged(newConfig: Configuration) = app.onConfigurationChanged(newConfig)
    override fun onLowMemory() = app.onLowMemory()
    override fun onTrimMemory(level: Int) = app.onTrimMemory(level)
    override fun registerComponentCallbacks(callback: ComponentCallbacks?) = app.registerComponentCallbacks(callback)
    override fun unregisterComponentCallbacks(callback: ComponentCallbacks?) =
        app.unregisterComponentCallbacks(callback)
    override fun registerActivityLifecycleCallbacks(callback: ActivityLifecycleCallbacks?) =
        app.registerActivityLifecycleCallbacks(callback)
    override fun unregisterActivityLifecycleCallbacks(callback: ActivityLifecycleCallbacks?) =
        app.unregisterActivityLifecycleCallbacks(callback)
    override fun registerOnProvideAssistDataListener(callback: OnProvideAssistDataListener?) =
        app.registerOnProvideAssistDataListener(callback)
    override fun unregisterOnProvideAssistDataListener(callback: OnProvideAssistDataListener?) =
        app.unregisterOnProvideAssistDataListener(callback)
}
