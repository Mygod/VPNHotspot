package be.mygod.vpnhotspot.util

import android.annotation.SuppressLint
import android.app.Application
import android.content.Context

@SuppressLint("Registered")
class DeviceStorageApp(context: Context) : Application() {
    init {
        attachBaseContext(context.createDeviceProtectedStorageContext())
    }

    /**
     * Thou shalt not get the REAL underlying application context which would no longer be operating under device
     * protected storage.
     */
    override fun getApplicationContext(): Context = this
}
