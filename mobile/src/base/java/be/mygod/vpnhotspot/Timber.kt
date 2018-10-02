package be.mygod.vpnhotspot

import android.util.Log
import be.mygod.vpnhotspot.App.Companion.app
import com.crashlytics.android.Crashlytics
import io.fabric.sdk.android.Fabric
import timber.log.Timber

fun initTimber() {
    Fabric.with(app, Crashlytics())
    Timber.plant(object : Timber.Tree() {
        override fun log(priority: Int, tag: String?, message: String, t: Throwable?) {
            if (t == null) Crashlytics.log(priority, tag, message) else {
                // Crashlytics.logException doesn't print to logcat
                if (priority >= Log.WARN) Log.println(priority, tag, message)
                Crashlytics.logException(t)
            }
        }
    })
}
